// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use audio_streams::AsyncPlaybackBuffer;
use audio_streams::AsyncPlaybackBufferStream;
use audio_streams::BoxError;
use audio_streams::PlaybackBuffer;
use audio_streams::PlaybackBufferStream;
use audio_streams::SampleFormat;
use audio_streams::StreamControl;
use audio_streams::StreamEffect;
use audio_streams::StreamSource;

use crate::convert;
use crate::coreaudio_sys::*;
use crate::ring_buffer::RingBuffer;

struct CoreAudioDevice {
    audio_unit: AudioUnit,
}

// SAFETY: AudioUnit (an opaque pointer) is thread-safe when used with the
// AudioToolbox API. We only call stop/uninitialize/dispose in Drop.
unsafe impl Send for CoreAudioDevice {}

impl Drop for CoreAudioDevice {
    fn drop(&mut self) {
        if !self.audio_unit.is_null() {
            // SAFETY: audio_unit was created by AudioComponentInstanceNew and is non-null.
            // Stop the unit before uninitializing. AudioOutputUnitStop waits for
            // any in-flight render callback to complete before returning. This
            // ensures _callback_data (dropped after _device) is still valid
            // during the final callback invocation.
            unsafe {
                AudioOutputUnitStop(self.audio_unit);
                AudioUnitUninitialize(self.audio_unit);
                AudioComponentInstanceDispose(self.audio_unit);
            }
        }
    }
}

pub struct CoreAudioStreamSource;

impl CoreAudioStreamSource {
    pub fn new() -> Self {
        CoreAudioStreamSource
    }
}

struct PlaybackCallbackData {
    ring_buffer: Arc<RingBuffer>,
    channels: usize,
}

unsafe extern "C" fn playback_render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: in_ref_con points to a PlaybackCallbackData owned by CoreAudioPlaybackStream
    // via a heap-allocated Box. io_data is provided by CoreAudio with valid buffer pointers.
    let data = &*(in_ref_con as *const PlaybackCallbackData);
    let buf_list = &mut *io_data;

    if buf_list.number_buffers == 0 {
        return noErr;
    }

    let buffer = &mut buf_list.buffers[0];
    let out_ptr = buffer.data as *mut u8;
    let calculated_len = (in_number_frames as usize) * data.channels * 4;
    let out_len = calculated_len.min(buffer.data_byte_size as usize);
    let out_slice = std::slice::from_raw_parts_mut(out_ptr, out_len);

    data.ring_buffer.read_or_zero_fill(out_slice);

    noErr
}

pub struct CoreAudioPlaybackStream {
    _device: CoreAudioDevice,
    _callback_data: Box<PlaybackCallbackData>,
    ring_buffer: Arc<RingBuffer>,
    buffer: Vec<u8>,
    float_conv_buf: Vec<f32>,
    frame_size: usize,
    guest_format: SampleFormat,
    num_channels: usize,
    interval: Duration,
    next_frame: Duration,
    start_time: Option<Instant>,
}

// SAFETY: All fields are Send. The raw pointer in _callback_data is used only
// by the CoreAudio render thread via the ring buffer's atomic operations.
unsafe impl Send for CoreAudioPlaybackStream {}

impl audio_streams::BufferCommit for CoreAudioPlaybackStream {
    fn commit(&mut self, nframes: usize) {
        let nwritten = nframes * self.guest_format.sample_bytes() * self.num_channels;
        if nwritten == 0 {
            return;
        }
        let guest_data = &self.buffer[..nwritten];
        let sample_count = nwritten / self.guest_format.sample_bytes();
        let float_buf = &mut self.float_conv_buf[..sample_count];
        convert::guest_format_to_float32(guest_data, float_buf, self.guest_format);
        // SAFETY: Reinterpret the f32 slice as bytes for the ring buffer.
        let float_bytes = unsafe {
            std::slice::from_raw_parts(float_buf.as_ptr() as *const u8, float_buf.len() * 4)
        };
        let written = self.ring_buffer.write(float_bytes);
        if written < float_bytes.len() {
            base::warn!(
                "CoreAudio ring buffer full, dropped {} bytes of audio",
                float_bytes.len() - written
            );
        }
    }
}

#[async_trait(?Send)]
impl audio_streams::AsyncBufferCommit for CoreAudioPlaybackStream {
    async fn commit(&mut self, nframes: usize) {
        audio_streams::BufferCommit::commit(self, nframes);
    }
}

fn create_output_audio_unit(
    sample_rate: u32,
    channels: usize,
    ring_buffer: &Arc<RingBuffer>,
) -> Result<(CoreAudioDevice, Box<PlaybackCallbackData>), BoxError> {
    // SAFETY: All CoreAudio API calls follow the documented lifecycle:
    // find component -> instantiate -> set format -> set callback -> init -> start.
    // Each call's return status is checked. The callback_data Box is pinned by the
    // caller (stored in CoreAudioPlaybackStream) and outlives the AudioUnit.
    unsafe {
        let desc = AudioComponentDescription {
            component_type: kAudioUnitType_Output,
            component_sub_type: kAudioUnitSubType_DefaultOutput,
            component_manufacturer: kAudioUnitManufacturer_Apple,
            component_flags: 0,
            component_flags_mask: 0,
        };

        let component = AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if component.is_null() {
            return Err("No default audio output component found".into());
        }

        let mut audio_unit: AudioUnit = std::ptr::null_mut();
        let status = AudioComponentInstanceNew(component, &mut audio_unit);
        if status != noErr {
            return Err(format!("AudioComponentInstanceNew failed: {}", status).into());
        }

        let device = CoreAudioDevice { audio_unit };

        let asbd =
            AudioStreamBasicDescription::float32_interleaved(sample_rate as f64, channels as u32);

        let status = AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            0,
            &asbd as *const _ as *const c_void,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        );
        if status != noErr {
            return Err(format!("Failed to set stream format: {}", status).into());
        }

        let callback_data = Box::new(PlaybackCallbackData {
            ring_buffer: ring_buffer.clone(),
            channels,
        });

        let callback_struct = AURenderCallbackStruct {
            input_proc: playback_render_callback,
            input_proc_ref_con: &*callback_data as *const _ as *mut c_void,
        };

        let status = AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            0,
            &callback_struct as *const _ as *const c_void,
            std::mem::size_of::<AURenderCallbackStruct>() as u32,
        );
        if status != noErr {
            return Err(format!("Failed to set render callback: {}", status).into());
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

fn make_stream(
    num_channels: usize,
    format: SampleFormat,
    frame_rate: u32,
    buffer_size: usize,
) -> Result<(CoreAudioStreamControl, CoreAudioPlaybackStream), BoxError> {
    let frame_size = format.sample_bytes() * num_channels;
    let period_bytes = buffer_size * frame_size;
    let ring_capacity = 4 * num_channels * buffer_size * 4;
    let ring_buffer = Arc::new(RingBuffer::new(ring_capacity));

    let (device, callback_data) = create_output_audio_unit(frame_rate, num_channels, &ring_buffer)?;

    let au = device.audio_unit;

    let interval = Duration::from_nanos((buffer_size as u64) * 1_000_000_000 / (frame_rate as u64));

    let max_samples = buffer_size * num_channels;

    let stream = CoreAudioPlaybackStream {
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
    };

    Ok((CoreAudioStreamControl { audio_unit: au }, stream))
}

impl StreamSource for CoreAudioStreamSource {
    fn new_playback_stream(
        &mut self,
        num_channels: usize,
        format: SampleFormat,
        frame_rate: u32,
        buffer_size: usize,
    ) -> Result<(Box<dyn StreamControl>, Box<dyn PlaybackBufferStream>), BoxError> {
        let (control, stream) = make_stream(num_channels, format, frame_rate, buffer_size)?;
        Ok((Box::new(control), Box::new(stream)))
    }

    fn new_async_playback_stream(
        &mut self,
        num_channels: usize,
        format: SampleFormat,
        frame_rate: u32,
        buffer_size: usize,
        _ex: &dyn audio_streams::AudioStreamsExecutor,
    ) -> Result<(Box<dyn StreamControl>, Box<dyn AsyncPlaybackBufferStream>), BoxError> {
        let (control, stream) = make_stream(num_channels, format, frame_rate, buffer_size)?;
        Ok((Box::new(control), Box::new(stream)))
    }

    fn new_capture_stream(
        &mut self,
        num_channels: usize,
        format: SampleFormat,
        frame_rate: u32,
        buffer_size: usize,
        _effects: &[StreamEffect],
    ) -> Result<
        (
            Box<dyn StreamControl>,
            Box<dyn audio_streams::capture::CaptureBufferStream>,
        ),
        BoxError,
    > {
        let (control, stream) =
            crate::capture::make_capture_stream(num_channels, format, frame_rate, buffer_size)?;
        Ok((Box::new(control), Box::new(stream)))
    }

    fn new_async_capture_stream(
        &mut self,
        num_channels: usize,
        format: SampleFormat,
        frame_rate: u32,
        buffer_size: usize,
        _effects: &[StreamEffect],
        _ex: &dyn audio_streams::AudioStreamsExecutor,
    ) -> Result<
        (
            Box<dyn StreamControl>,
            Box<dyn audio_streams::capture::AsyncCaptureBufferStream>,
        ),
        BoxError,
    > {
        let (control, stream) =
            crate::capture::make_capture_stream(num_channels, format, frame_rate, buffer_size)?;
        Ok((Box::new(control), Box::new(stream)))
    }
}

impl PlaybackBufferStream for CoreAudioPlaybackStream {
    fn next_playback_buffer<'b, 's: 'b>(&'s mut self) -> Result<PlaybackBuffer<'b>, BoxError> {
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

        // SAFETY: Create a raw slice aliasing self.buffer so we can pass both the
        // slice (for writing) and self (as BufferCommit) to PlaybackBuffer::new.
        // This is the same pattern used by win_audio's DeviceRendererWrapper.
        // The invariant: commit() only reads from self.buffer, never writes.
        let slice =
            unsafe { std::slice::from_raw_parts_mut(self.buffer.as_mut_ptr(), self.buffer.len()) };
        Ok(PlaybackBuffer::new(self.frame_size, slice, self)?)
    }
}

#[async_trait(?Send)]
impl AsyncPlaybackBufferStream for CoreAudioPlaybackStream {
    async fn next_playback_buffer<'a>(
        &'a mut self,
        ex: &dyn audio_streams::AudioStreamsExecutor,
    ) -> Result<AsyncPlaybackBuffer<'a>, BoxError> {
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

        // SAFETY: Same aliasing pattern as the sync path above.
        let slice =
            unsafe { std::slice::from_raw_parts_mut(self.buffer.as_mut_ptr(), self.buffer.len()) };
        Ok(AsyncPlaybackBuffer::new(self.frame_size, slice, self)?)
    }
}

struct CoreAudioStreamControl {
    audio_unit: AudioUnit,
}

// SAFETY: AudioUnit is thread-safe when accessed via AudioToolbox API.
unsafe impl Send for CoreAudioStreamControl {}
// SAFETY: set_volume/set_mute use AudioUnitSetParameter which is thread-safe.
unsafe impl Sync for CoreAudioStreamControl {}

impl StreamControl for CoreAudioStreamControl {}
