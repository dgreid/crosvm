// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use audio_streams::capture::AsyncCaptureBuffer;
use audio_streams::capture::AsyncCaptureBufferStream;
use audio_streams::capture::CaptureBuffer;
use audio_streams::capture::CaptureBufferStream;
use audio_streams::BoxError;
use audio_streams::SampleFormat;
use audio_streams::StreamControl;

use crate::convert;
use crate::coreaudio_sys::*;
use crate::ring_buffer::RingBuffer;

struct CoreAudioCaptureDevice {
    audio_unit: AudioUnit,
}

// SAFETY: AudioUnit is thread-safe when used with the AudioToolbox API.
unsafe impl Send for CoreAudioCaptureDevice {}

impl Drop for CoreAudioCaptureDevice {
    fn drop(&mut self) {
        if !self.audio_unit.is_null() {
            // SAFETY: audio_unit was created by AudioComponentInstanceNew.
            unsafe {
                AudioOutputUnitStop(self.audio_unit);
                AudioUnitUninitialize(self.audio_unit);
                AudioComponentInstanceDispose(self.audio_unit);
            }
        }
    }
}

struct CaptureCallbackData {
    ring_buffer: Arc<RingBuffer>,
    channels: usize,
    audio_unit: AudioUnit,
}

unsafe extern "C" fn capture_input_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    _io_data: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: in_ref_con points to CaptureCallbackData, which outlives the AudioUnit.
    let data = &*(in_ref_con as *const CaptureCallbackData);

    let buf_size = (in_number_frames as usize) * data.channels * 4;
    let mut render_buf = vec![0u8; buf_size];

    let mut audio_buffer = AudioBuffer {
        number_channels: data.channels as u32,
        data_byte_size: buf_size as u32,
        data: render_buf.as_mut_ptr() as *mut c_void,
    };

    let mut buffer_list = AudioBufferList {
        number_buffers: 1,
        buffers: [audio_buffer],
    };

    // SAFETY: AudioUnitRender fills the buffer list with captured audio data.
    let status = AudioUnitRender(
        data.audio_unit,
        _io_action_flags,
        in_time_stamp,
        in_bus_number,
        in_number_frames,
        &mut buffer_list,
    );

    if status != noErr {
        return status;
    }

    let actual_size = buffer_list.buffers[0].data_byte_size as usize;
    let written = data.ring_buffer.write(&render_buf[..actual_size]);
    if written < actual_size {
        // Overrun: drop oldest data by reading and discarding.
        let excess = actual_size - written;
        let mut discard = vec![0u8; excess];
        data.ring_buffer.read(&mut discard);
        data.ring_buffer.write(&render_buf[written..actual_size]);
    }

    noErr
}

pub struct CoreAudioCaptureStream {
    _device: CoreAudioCaptureDevice,
    _callback_data: Box<CaptureCallbackData>,
    ring_buffer: Arc<RingBuffer>,
    buffer: Vec<u8>,
    float_conv_buf: Vec<f32>,
    frame_size: usize,
    guest_format: SampleFormat,
    num_channels: usize,
    interval: Duration,
    next_frame: Duration,
    start_time: Option<Instant>,
    buffer_commit: CaptureNoopCommit,
}

// SAFETY: All fields are Send. The raw pointer in _callback_data is used only
// by the CoreAudio capture thread via the ring buffer's atomic operations.
unsafe impl Send for CoreAudioCaptureStream {}

struct CaptureNoopCommit;

impl audio_streams::BufferCommit for CaptureNoopCommit {
    fn commit(&mut self, _nframes: usize) {}
}

#[async_trait(?Send)]
impl audio_streams::AsyncBufferCommit for CaptureNoopCommit {
    async fn commit(&mut self, _nframes: usize) {}
}

fn create_input_audio_unit(
    sample_rate: u32,
    channels: usize,
    ring_buffer: &Arc<RingBuffer>,
) -> Result<(CoreAudioCaptureDevice, Box<CaptureCallbackData>), BoxError> {
    // SAFETY: All CoreAudio API calls follow the documented lifecycle for input.
    unsafe {
        let desc = AudioComponentDescription {
            component_type: kAudioUnitType_Output,
            component_sub_type: kAudioUnitSubType_HALOutput,
            component_manufacturer: kAudioUnitManufacturer_Apple,
            component_flags: 0,
            component_flags_mask: 0,
        };

        let component = AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if component.is_null() {
            return Err("No HAL audio component found".into());
        }

        let mut audio_unit: AudioUnit = std::ptr::null_mut();
        let status = AudioComponentInstanceNew(component, &mut audio_unit);
        if status != noErr {
            return Err(format!("AudioComponentInstanceNew failed: {}", status).into());
        }

        let device = CoreAudioCaptureDevice { audio_unit };

        // Enable input on bus 1.
        let enable_io: u32 = 1;
        let status = AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            1, // bus 1 = input
            &enable_io as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if status != noErr {
            return Err(format!("Failed to enable input: {}", status).into());
        }

        // Disable output on bus 0 (we only want capture).
        let disable_io: u32 = 0;
        let status = AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            0, // bus 0 = output
            &disable_io as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if status != noErr {
            return Err(format!("Failed to disable output: {}", status).into());
        }

        // Set the format we want to receive on bus 1's output scope.
        let asbd = AudioStreamBasicDescription::float32_interleaved(
            sample_rate as f64,
            channels as u32,
        );

        let status = AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            1, // bus 1
            &asbd as *const _ as *const c_void,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        );
        if status != noErr {
            return Err(format!("Failed to set input stream format: {}", status).into());
        }

        let callback_data = Box::new(CaptureCallbackData {
            ring_buffer: ring_buffer.clone(),
            channels,
            audio_unit,
        });

        let callback_struct = AURenderCallbackStruct {
            input_proc: capture_input_callback,
            input_proc_ref_con: &*callback_data as *const _ as *mut c_void,
        };

        let status = AudioUnitSetProperty(
            audio_unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            0,
            &callback_struct as *const _ as *const c_void,
            std::mem::size_of::<AURenderCallbackStruct>() as u32,
        );
        if status != noErr {
            return Err(format!("Failed to set input callback: {}", status).into());
        }

        let status = AudioUnitInitialize(audio_unit);
        if status != noErr {
            return Err(format!("AudioUnitInitialize failed: {}", status).into());
        }

        let status = AudioOutputUnitStart(audio_unit);
        if status != noErr {
            return Err(format!("AudioOutputUnitStart failed: {}", status).into());
        }

        Ok((device, callback_data))
    }
}

pub fn make_capture_stream(
    num_channels: usize,
    format: SampleFormat,
    frame_rate: u32,
    buffer_size: usize,
) -> Result<(CoreAudioCaptureControl, CoreAudioCaptureStream), BoxError> {
    let frame_size = format.sample_bytes() * num_channels;
    let period_bytes = buffer_size * frame_size;
    let ring_capacity = 4 * num_channels * buffer_size * 4;
    let ring_buffer = Arc::new(RingBuffer::new(ring_capacity));

    let (device, callback_data) =
        create_input_audio_unit(frame_rate, num_channels, &ring_buffer)?;

    let interval = Duration::from_millis(
        (buffer_size as u64) * 1000 / (frame_rate as u64),
    );

    let max_samples = buffer_size * num_channels;

    let stream = CoreAudioCaptureStream {
        _device: device,
        _callback_data: callback_data,
        ring_buffer,
        buffer: vec![0u8; period_bytes],
        float_conv_buf: vec![0.0f32; max_samples],
        frame_size,
        guest_format: format,
        num_channels,
        interval,
        next_frame: interval,
        start_time: None,
        buffer_commit: CaptureNoopCommit,
    };

    Ok((CoreAudioCaptureControl, stream))
}

impl CoreAudioCaptureStream {
    fn fill_buffer_from_ring(&mut self) {
        let float_bytes_needed = self.float_conv_buf.len() * 4;
        // SAFETY: Reinterpret f32 slice as bytes for ring buffer read.
        let float_bytes = unsafe {
            std::slice::from_raw_parts_mut(
                self.float_conv_buf.as_mut_ptr() as *mut u8,
                float_bytes_needed,
            )
        };
        self.ring_buffer.read_or_zero_fill(float_bytes);
        convert::float32_to_guest_format(&self.float_conv_buf, &mut self.buffer, self.guest_format);
    }
}

impl CaptureBufferStream for CoreAudioCaptureStream {
    fn next_capture_buffer<'b, 's: 'b>(&'s mut self) -> Result<CaptureBuffer<'b>, BoxError> {
        if let Some(start_time) = self.start_time {
            let elapsed = start_time.elapsed();
            if elapsed < self.next_frame {
                std::thread::sleep(self.next_frame - elapsed);
            }
            self.next_frame += self.interval;
        } else {
            self.start_time = Some(Instant::now());
            self.next_frame = self.interval;
        }

        self.fill_buffer_from_ring();

        Ok(CaptureBuffer::new(
            self.frame_size,
            &mut self.buffer,
            &mut self.buffer_commit,
        )?)
    }
}

#[async_trait(?Send)]
impl AsyncCaptureBufferStream for CoreAudioCaptureStream {
    async fn next_capture_buffer<'a>(
        &'a mut self,
        ex: &dyn audio_streams::AudioStreamsExecutor,
    ) -> Result<AsyncCaptureBuffer<'a>, BoxError> {
        if let Some(start_time) = self.start_time {
            let elapsed = start_time.elapsed();
            if elapsed < self.next_frame {
                ex.delay(self.next_frame - elapsed).await?;
            }
            self.next_frame += self.interval;
        } else {
            self.start_time = Some(Instant::now());
            self.next_frame = self.interval;
        }

        self.fill_buffer_from_ring();

        Ok(AsyncCaptureBuffer::new(
            self.frame_size,
            &mut self.buffer,
            &mut self.buffer_commit,
        )?)
    }
}

pub struct CoreAudioCaptureControl;

impl StreamControl for CoreAudioCaptureControl {}
