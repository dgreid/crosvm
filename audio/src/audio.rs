// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::error;
use std::fmt::{self, Display};
use std::io::{self, Write};
use std::result::Result;
use std::time::{Duration, Instant};

/// `StreamSource` creates streams for playback or capture of audio.
pub trait StreamSource: Send {
    /// Returns a stream control and buffer generator object. These are separate as the buffer
    /// generator might want to be passed to the audio stream.
    fn new_playback_stream(
        &mut self,
        num_channels: usize,
        frame_rate: usize,
        buffer_size: usize,
    ) -> Result<(Box<dyn StreamControl>, Box<dyn PlaybackBufferStream>), Box<error::Error>>;
}

/// `PlaybackBufferStream` provides `PlaybackBuffer`s to fill with audio samples for playback.
pub trait PlaybackBufferStream: Send {
    fn next_playback_buffer<'a>(&'a mut self) -> Result<PlaybackBuffer<'a>, Box<error::Error>>;
}

/// `StreamControl` provides a way to set the volume and mute states of a stream. `StreamControl`
/// is separate from the stream so it can be owned by a different thread if needed.
pub trait StreamControl: Send + Sync {
    fn set_volume(&mut self, _scaler: f64) {}
    fn set_mute(&mut self, _mute: bool) {}
}

/// `BufferDrop` is used as a callback mechanism for `PlaybackBuffer` objects. It is meant to be
/// implemented by the audio stream, allowing arbitrary code to be run after a buffer is filled or
/// read by the user.
pub trait BufferDrop {
    /// Called when an audio buffer is dropped. `nframes` indicates the number of audio frames that
    /// were read or written to the device.
    fn trigger(&mut self, nframes: usize);
}

/// Errors that are possible from a `PlaybackBuffer`.
#[derive(Debug)]
pub enum PlaybackBufferError {
    InvalidLength,
}

impl error::Error for PlaybackBufferError {}

impl Display for PlaybackBufferError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PlaybackBufferError::InvalidLength => write!(f, "Invalid buffer length"),
        }
    }
}

/// `PlaybackBuffer` is one buffer that holds buffer_size audio frames. It is used to temporarily
/// allow access to an audio buffer and notifes the owning stream of write completion when dropped.
pub struct PlaybackBuffer<'a> {
    pub buffer: &'a mut [u8],
    offset: usize,     // Write offset in frames.
    frame_size: usize, // Size of a frame in bytes.
    drop: &'a mut BufferDrop,
}

impl<'a> PlaybackBuffer<'a> {
    /// Creates a new `PlaybackBuffer` that holds a reference to the backing memory specified in
    /// `buffer`.
    pub fn new<F>(
        frame_size: usize,
        buffer: &'a mut [u8],
        drop: &'a mut F,
    ) -> Result<Self, PlaybackBufferError>
    where
        F: BufferDrop,
    {
        if buffer.len() % frame_size != 0 {
            return Err(PlaybackBufferError::InvalidLength);
        }

        Ok(PlaybackBuffer {
            buffer,
            offset: 0,
            frame_size,
            drop,
        })
    }

    /// Returns the number of audio frames that fit in the buffer.
    pub fn frame_capacity(&self) -> usize {
        self.buffer.len() / self.frame_size
    }
}

impl<'a> Write for PlaybackBuffer<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // only write complete frames.
        let len = buf.len() / self.frame_size * self.frame_size;
        let written = (&mut self.buffer[self.offset..]).write(&buf[..len])?;
        self.offset += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> Drop for PlaybackBuffer<'a> {
    fn drop(&mut self) {
        self.drop.trigger(self.offset);
    }
}

/// Stream that accepts playback samples but drops them.
pub struct DummyStream {
    buffer: Vec<u8>,
    frame_size: usize,
    interval: Duration,
    next_frame: Duration,
    start_time: Option<Instant>,
    buffer_drop: DummyBufferDrop,
}

/// DummyStream data that is needed from the buffer complete callback.
struct DummyBufferDrop {
    which_buffer: bool,
}

impl BufferDrop for DummyBufferDrop {
    fn trigger(&mut self, _nwritten: usize) {
        // When a buffer completes, switch to the other one.
        self.which_buffer ^= true;
    }
}

impl DummyStream {
    // TODO(allow other formats)
    pub fn new(num_channels: usize, frame_rate: usize, buffer_size: usize) -> Self {
        const S16LE_SIZE: usize = 2;
        let frame_size = S16LE_SIZE * num_channels;
        let interval = Duration::from_millis(buffer_size as u64 * 1000 / frame_rate as u64);
        DummyStream {
            buffer: vec![0; buffer_size * frame_size],
            frame_size,
            interval,
            next_frame: interval,
            start_time: None,
            buffer_drop: DummyBufferDrop {
                which_buffer: false,
            },
        }
    }
}

impl PlaybackBufferStream for DummyStream {
    fn next_playback_buffer<'a>(&'a mut self) -> Result<PlaybackBuffer<'a>, Box<error::Error>> {
        if let Some(start_time) = self.start_time {
            if start_time.elapsed() < self.next_frame {
                std::thread::sleep(self.next_frame - start_time.elapsed());
            }
            self.next_frame += self.interval;
        } else {
            self.start_time = Some(Instant::now());
            self.next_frame = self.interval;
        }
        Ok(PlaybackBuffer::new(
            self.frame_size,
            &mut self.buffer,
            &mut self.buffer_drop,
        )?)
    }
}

#[derive(Default)]
pub struct DummyStreamControl;

impl DummyStreamControl {
    pub fn new() -> Self {
        DummyStreamControl {}
    }
}

impl StreamControl for DummyStreamControl {}

#[derive(Default)]
pub struct DummyStreamSource;

impl DummyStreamSource {
    pub fn new() -> Self {
        DummyStreamSource {}
    }
}

impl StreamSource for DummyStreamSource {
    fn new_playback_stream(
        &mut self,
        num_channels: usize,
        frame_rate: usize,
        buffer_size: usize,
    ) -> Result<(Box<dyn StreamControl>, Box<dyn PlaybackBufferStream>), Box<error::Error>> {
        Ok((
            Box::new(DummyStreamControl::new()),
            Box::new(DummyStream::new(num_channels, frame_rate, buffer_size)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_buffer_length() {
        // Playback buffers can't be created with a size that isn't divisible by the frame size.
        let mut pb_buf = [0xa5u8; 480 * 2 * 2 + 1];
        let mut buffer_drop = DummyBufferDrop {
            which_buffer: false,
        };
        assert!(PlaybackBuffer::new(2, &mut pb_buf, &mut buffer_drop).is_err());
    }

    #[test]
    fn sixteen_bit_stereo() {
        let mut server = DummyStreamSource::new();
        let (_, mut stream) = server.new_playback_stream(2, 48000, 480).unwrap();
        let mut stream_buffer = stream.next_playback_buffer().unwrap();
        assert_eq!(stream_buffer.frame_capacity(), 480);
        let pb_buf = [0xa5u8; 480 * 2 * 2];
        assert_eq!(stream_buffer.write(&pb_buf).unwrap(), 480 * 2 * 2);
    }

    #[test]
    fn consumption_rate() {
        let mut server = DummyStreamSource::new();
        let (_, mut stream) = server.new_playback_stream(2, 48000, 480).unwrap();
        let start = Instant::now();
        {
            let mut stream_buffer = stream.next_playback_buffer().unwrap();
            let pb_buf = [0xa5u8; 480 * 2 * 2];
            assert_eq!(stream_buffer.write(&pb_buf).unwrap(), 480 * 2 * 2);
        }
        // The second call should block until the first buffer is consumed.
        let _stream_buffer = stream.next_playback_buffer().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed > Duration::from_millis(10),
            "next_playback_buffer didn't block long enough {}",
            elapsed.subsec_millis()
        );
    }
}
