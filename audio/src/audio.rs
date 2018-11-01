use std::io::{self, Write};
use std::time::{self, Duration, Instant};

pub trait StreamSource: Send {
    fn new_playback_stream(&mut self, num_channels: usize, frame_rate: usize, buffer_size: usize) -> Box<dyn PlaybackBufferStream>;
}

pub trait PlaybackBufferStream: Send {
    fn next_playback_buffer<'a>(&'a mut self) -> PlaybackBuffer<'a>;
}

pub struct PlaybackBuffer<'a> {
    done_toggle: &'a mut bool,
    pub buffer: &'a mut [u8],
    offset: usize, // Write offset in frames.
    frame_size: usize, // Size of a frame in bytes.
}

impl<'a> PlaybackBuffer<'a> {
    pub fn new(frame_size: usize, done_toggle: &'a mut bool, buffer: &'a mut [u8]) -> Self {
        PlaybackBuffer {
            done_toggle,
            buffer,
            offset: 0,
            frame_size,
        }
    }

    /// Returns the number of audio frames that fit in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.len() / self.frame_size
    }
}

impl<'a> Write for PlaybackBuffer<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = (&mut self.buffer[self.offset..]).write(buf)?;
        self.offset += written;
        Ok(written/self.frame_size)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> Drop for PlaybackBuffer<'a> {
    fn drop(&mut self) {
        *self.done_toggle = !*self.done_toggle;
    }
}

/// Stream that accepts playback samples but drops them.
pub struct DummyStream {
    buffer: Vec<u8>,
    which_buffer: bool,
    frame_size: usize,
    frame_rate: usize,
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
            frame_rate,
            interval,
            next_frame: interval,
            start_time: None,
        }
    }
}

impl PlaybackBufferStream for DummyStream {
    fn next_playback_buffer<'a>(&'a mut self) -> PlaybackBuffer<'a> {
        if let Some(start_time) = self.start_time {
            if start_time.elapsed() < self.next_frame {
                std::thread::sleep(self.next_frame - start_time.elapsed());
            }
            self.next_frame += self.interval;
        } else {
            self.start_time = Some(Instant::now());
            self.next_frame = self.interval;
        }
        PlaybackBuffer::new(self.frame_size, &mut self.which_buffer, &mut self.buffer)
    }
}

pub struct DummyStreamSource {
}

impl DummyStreamSource {
    pub fn new() -> Self {
        DummyStreamSource {}
    }
}

impl StreamSource for DummyStreamSource {
    fn new_playback_stream(&mut self, num_channels: usize, frame_rate: usize, buffer_size: usize) -> Box<dyn PlaybackBufferStream> {
        Box::new(DummyStream::new(num_channels, frame_rate, buffer_size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_bit_stereo() {
        let mut server = DummyStreamSource::new();
        let mut stream = server.new_playback_stream(2, 48000, 480);
        let mut stream_buffer = stream.next_playback_buffer();
        assert_eq!(stream_buffer.len(), 480);
        let pb_buf = [0xa5u8; 480 * 2 * 2];
        assert_eq!(stream_buffer.write(&pb_buf).unwrap(), 480);
    }

    #[test]
    fn consumption_rate() {
        let mut server = DummyStreamSource::new();
        let mut stream = server.new_playback_stream(2, 48000, 480);
        let start = Instant::now();
        {
            let mut stream_buffer = stream.next_playback_buffer();
            let pb_buf = [0xa5u8; 480 * 2 * 2];
            assert_eq!(stream_buffer.write(&pb_buf).unwrap(), 480);
        }
        // The second call should block until the first buffer is consumed.
        let stream_buffer = stream.next_playback_buffer();
        let elapsed = start.elapsed();
        assert!(elapsed > Duration::from_millis(10),
               "next_playback_buffer didn't block long enough {}", elapsed.subsec_millis());
    }
}
