// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Sampling rates for audio streams.
pub enum SampleRate {
    Rate8000,
    Rate16000,
    Rate32000,
    Rate44100,
    Rate48000,
    Rate96000,
    Rate192000,
}

/// Available formats for audio samples.
pub enum SampleFormat {
    S16LE,
    S24LE,
    S32LE,
}

type Result<T> = std::result::Result<T, Box<std::error::Error>>;

/// Represents an audio server that can create playback and capture streams.
pub trait AudioServer<'a, T> {
    fn create_playback_stream(&mut self, rate: SampleRate, sample_format: SampleFormat, num_channels: usize, buffer_size: usize) -> Result<Box<PlaybackStream<'a, T>>>;
    fn create_capture_stream(&mut self, rate: SampleRate, sample_format: SampleFormat, num_channels: usize, buffer_size: usize) -> Result<Box<CaptureStream<'a, T>>>;
}

/// A stream for playing back audio.
pub trait PlaybackStream<'a, T> {
    /// Return the next available output bufer to fill with playback data.
    fn next_playback_buffer(&'a mut self) -> PlaybackBuffer<'a, T>;
}

/// A stream for capturing audio.
pub trait CaptureStream<'a, T> {
    /// Return the next input bufer filled with caputred audio data.
    fn next_captured_buffer(&'a mut self) -> CaptureBuffer<'a, T>;
}

/// A buffer to be filled with audio samples. When dropped, the data is committed to the host.
pub struct PlaybackBuffer<'a, T> {
    pub buffer: &'a mut [T],
}

impl<'a, T> Drop for PlaybackBuffer<'a, T> {
    fn drop(&mut self) {
    }
}

/// A buffer filled with audio samples. When dropped, the buffer space is returned to the host.
pub struct CaptureBuffer<'a, T> {
    pub buffer: &'a [T],
}

impl<'a, T> Drop for CaptureBuffer<'a, T> {
    fn drop(&mut self) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    enum DummyError {
        TestError1
    }

    impl std::error::Error for DummyError {
        fn description(&self) -> &str {
            "It's an error"
        }
    }

    impl fmt::Display for DummyError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let prefix = "Libfdt Error: ";
            write!(f, "desc: {}", std::error::Error::description(self))
        }
    }

    struct DummyServer {
        pub num_streams: usize,
    }

    impl<'a, T> AudioServer<'a, T> for DummyServer {
        fn create_playback_stream(&mut self, rate: SampleRate, sample_format: SampleFormat, num_channels: usize, buffer_size: usize) -> Result<Box<PlaybackStream<'a, T>>> {
            Err(Box::new(DummyError::TestError1))
        }

        fn create_capture_stream(&mut self, rate: SampleRate, sample_format: SampleFormat, num_channels: usize, buffer_size: usize) -> Result<Box<CaptureStream<'a,T>>> {
            Err(Box::new(DummyError::TestError1))
        }

    }

    #[test]
    fn play_s16le() {
        let mut server = DummyServer{ num_streams: 0 };
        let mut pb_stream: Box<PlaybackStream<i16>> = server.create_playback_stream(SampleRate::Rate48000, SampleFormat::S16LE, 2, 480).unwrap();

        for i in 1..10 {
            let this_buff = pb_stream.next_playback_buffer();
            for sample in this_buff.buffer.iter_mut() {
                *sample = i as i16;
            }
        }
    }
}
