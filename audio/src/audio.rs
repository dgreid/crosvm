extern crate alsa;

mod alsa_streams;

use std::io::{self, Write};
use std::time::{Duration, Instant};

/// `StreamSource` creates streams for playback or capture of audio.
pub trait StreamSource: Send {
    /// Returns a stream control and buffer generator object. These are separate as the buffer
    /// generator might want to be passed to the audio stream.
    fn new_playback_stream(&mut self, num_channels: usize, frame_rate: usize, buffer_size: usize)
        -> (Box<dyn StreamControl>, Box<dyn PlaybackBufferStream>);
}

/// `PlaybackBufferStream` provides `PlaybackBuffer`s to fill with audio samples for playback.
pub trait PlaybackBufferStream<F: FnOnce()>: Send {
    fn next_playback_buffer<'a>(&'a mut self) -> PlaybackBuffer<'a, F>;
}

/// `StreamControl` provides a way to set the volume and mute states of a stream. `StreamControl`
/// is separate from the stream so it can be owned by a different thread if needed.
pub trait StreamControl: Send + Sync {
    fn set_volume(&mut self, _scaler: f64) {}
    fn set_mute(&mut self, _mute: bool) {}
}

/// `PlaybackBuffer` is one buffer that holds buffer_size audio frames. It is used to temporarily
/// allow access to an audio buffer and notifes the owning stream of write completion when dropped.
pub struct PlaybackBuffer<'a, F>
where
    F: 'a + FnOnce()
{
    pub buffer: &'a mut [u8],
    offset: usize, // Write offset in frames.
    frame_size: usize, // Size of a frame in bytes.
    done_cb: FnOnce(),
}

impl<'a, F:'a + FnOnce()> PlaybackBuffer<'a, F> {
    /// Creates a new `PlaybackBuffer` that holds a reference to the backing memory specified in
    /// `buffer`. When dropped, the `done_toggle` will be inverted.
    pub fn new<F>(frame_size: usize, buffer: &'a mut [u8], done_cb: F) -> Self
    where
        F: 'a + FnOnce()
    {
        PlaybackBuffer {
            buffer,
            offset: 0,
            frame_size,
            done_cb,
        }
    }

    /// Returns the number of audio frames that fit in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.len() / self.frame_size
    }
}

impl<'a, F> Write for PlaybackBuffer<'a, F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = (&mut self.buffer[self.offset..]).write(buf)?;
        self.offset += written;
        Ok(written/self.frame_size)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a, F> Drop for PlaybackBuffer<'a, F> {
    fn drop(&mut self) {
        self.done_cb();
    }
}

/// Stream that accepts playback samples but drops them.
pub struct DummyStream {
    buffer: Vec<u8>,
    which_buffer: bool,
    frame_size: usize,
    interval: Duration,
    next_frame: Duration,
    start_time: Option<Instant>,
}

impl DummyStream {
    // TODO(allow other formats)
    pub fn new(num_channels: usize, frame_rate: usize, buffer_size: usize) -> Self {
        const S16LE_SIZE: usize = 2;
        let frame_size = S16LE_SIZE * num_channels;
        let interval = Duration::from_millis(buffer_size as u64 * 1000 / frame_rate as u64);
        DummyStream {
            buffer: vec![Default::default(); buffer_size * frame_size],
            which_buffer: false,
            frame_size,
            interval,
            next_frame: interval,
            start_time: None,
        }
    }
}

impl<F: FnOnce()> PlaybackBufferStream<F> for DummyStream {
    fn next_playback_buffer<'a>(&'a mut self) -> PlaybackBuffer<'a, F> {
        if let Some(start_time) = self.start_time {
            if start_time.elapsed() < self.next_frame {
                std::thread::sleep(self.next_frame - start_time.elapsed());
            }
            self.next_frame += self.interval;
        } else {
            self.start_time = Some(Instant::now());
            self.next_frame = self.interval;
        }
        PlaybackBuffer::new(self.frame_size, &mut self.buffer, || {self.which_buffer = !self.which_buffer;})
    }
}

pub struct DummyStreamControl {
}

impl DummyStreamControl {
    pub fn new() -> Self {
        DummyStreamControl {}
    }
}

impl StreamControl for DummyStreamControl {
}

pub struct DummyStreamSource {
}

impl DummyStreamSource {
    pub fn new() -> Self {
        DummyStreamSource {}
    }
}

impl StreamSource for DummyStreamSource {
    fn new_playback_stream(&mut self, num_channels: usize, frame_rate: usize, buffer_size: usize)
        -> (Box<dyn StreamControl>, Box<dyn PlaybackBufferStream>)
    {
        (Box::new(DummyStreamControl::new()),
         Box::new(DummyStream::new(num_channels, frame_rate, buffer_size)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_bit_stereo() {
        let mut server = DummyStreamSource::new();
        let (_, mut stream) = server.new_playback_stream(2, 48000, 480);
        let mut stream_buffer = stream.next_playback_buffer();
        assert_eq!(stream_buffer.len(), 480);
        let pb_buf = [0xa5u8; 480 * 2 * 2];
        assert_eq!(stream_buffer.write(&pb_buf).unwrap(), 480);
    }

    #[test]
    fn consumption_rate() {
        let mut server = DummyStreamSource::new();
        let (_, mut stream) = server.new_playback_stream(2, 48000, 480);
        let start = Instant::now();
        {
            let mut stream_buffer = stream.next_playback_buffer();
            let pb_buf = [0xa5u8; 480 * 2 * 2];
            assert_eq!(stream_buffer.write(&pb_buf).unwrap(), 480);
        }
        // The second call should block until the first buffer is consumed.
        let _stream_buffer = stream.next_playback_buffer();
        let elapsed = start.elapsed();
        assert!(elapsed > Duration::from_millis(10),
               "next_playback_buffer didn't block long enough {}", elapsed.subsec_millis());
    }
}
