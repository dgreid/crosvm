// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Lock-free single-producer single-consumer ring buffer for real-time audio.
//!
//! The consumer (CoreAudio callback thread) must never block, so this uses
//! atomic operations instead of mutexes.

use std::cell::UnsafeCell;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

pub struct RingBuffer {
    data: UnsafeCell<Vec<u8>>,
    capacity: usize,
    read_idx: AtomicUsize,
    write_idx: AtomicUsize,
}

// SAFETY: RingBuffer is designed for single-producer single-consumer use.
// The atomic indices enforce disjoint access: the producer only writes to
// slots between write_idx and write_idx + available_write, while the consumer
// only reads from read_idx to read_idx + available_read. UnsafeCell correctly
// communicates interior mutability to the compiler.
unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        RingBuffer {
            data: UnsafeCell::new(vec![0u8; capacity]),
            capacity,
            read_idx: AtomicUsize::new(0),
            write_idx: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn available_read(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    pub fn available_write(&self) -> usize {
        self.capacity - self.available_read()
    }

    /// Write data into the ring buffer. Returns the number of bytes actually written.
    /// If there isn't enough space, writes as much as possible.
    pub fn write(&self, src: &[u8]) -> usize {
        let available = self.available_write();
        let to_write = src.len().min(available);
        if to_write == 0 {
            return 0;
        }

        let w = self.write_idx.load(Ordering::Relaxed);
        let start = w % self.capacity;

        // SAFETY: Single-producer guarantee ensures only one thread calls write().
        // The available_write() check ensures we don't write into slots the reader
        // is currently accessing.
        let data_ptr = unsafe { (*self.data.get()).as_mut_ptr() };

        if start + to_write <= self.capacity {
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), data_ptr.add(start), to_write);
            }
        } else {
            let first = self.capacity - start;
            let second = to_write - first;
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), data_ptr.add(start), first);
                std::ptr::copy_nonoverlapping(src.as_ptr().add(first), data_ptr, second);
            }
        }

        self.write_idx
            .store(w.wrapping_add(to_write), Ordering::Release);
        to_write
    }

    /// Read data from the ring buffer. Returns the number of bytes actually read.
    /// If there isn't enough data, reads as much as possible.
    pub fn read(&self, dst: &mut [u8]) -> usize {
        let available = self.available_read();
        let to_read = dst.len().min(available);
        if to_read == 0 {
            return 0;
        }

        let r = self.read_idx.load(Ordering::Relaxed);
        let start = r % self.capacity;

        // SAFETY: Single-consumer guarantee ensures only one thread calls read().
        // The available_read() check ensures we don't read into slots the writer
        // is currently writing.
        let data_ptr = unsafe { (*self.data.get()).as_ptr() };

        if start + to_read <= self.capacity {
            unsafe {
                std::ptr::copy_nonoverlapping(data_ptr.add(start), dst.as_mut_ptr(), to_read);
            }
        } else {
            let first = self.capacity - start;
            let second = to_read - first;
            unsafe {
                std::ptr::copy_nonoverlapping(data_ptr.add(start), dst.as_mut_ptr(), first);
                std::ptr::copy_nonoverlapping(data_ptr, dst.as_mut_ptr().add(first), second);
            }
        }

        self.read_idx
            .store(r.wrapping_add(to_read), Ordering::Release);
        to_read
    }

    /// Read data into a destination buffer, filling with zeros if not enough data is available.
    /// Always fills exactly `dst.len()` bytes. Returns the number of real bytes read.
    pub fn read_or_zero_fill(&self, dst: &mut [u8]) -> usize {
        let read = self.read(dst);
        if read < dst.len() {
            dst[read..].fill(0);
        }
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ring_buffer_is_empty() {
        let rb = RingBuffer::new(1024);
        assert_eq!(rb.available_read(), 0);
        assert_eq!(rb.available_write(), 1024);
    }

    #[test]
    fn write_then_read() {
        let rb = RingBuffer::new(1024);
        let data = [1u8, 2, 3, 4, 5];
        assert_eq!(rb.write(&data), 5);
        assert_eq!(rb.available_read(), 5);
        assert_eq!(rb.available_write(), 1024 - 5);

        let mut out = [0u8; 5];
        assert_eq!(rb.read(&mut out), 5);
        assert_eq!(out, [1, 2, 3, 4, 5]);
        assert_eq!(rb.available_read(), 0);
    }

    #[test]
    fn wrap_around() {
        let rb = RingBuffer::new(8);

        let data = [1u8, 2, 3, 4, 5, 6];
        assert_eq!(rb.write(&data), 6);

        let mut out = [0u8; 4];
        assert_eq!(rb.read(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);

        // Now write wraps around
        let data2 = [7u8, 8, 9, 10, 11, 12];
        assert_eq!(rb.write(&data2), 6);

        let mut out2 = [0u8; 8];
        assert_eq!(rb.read(&mut out2), 8);
        assert_eq!(out2, [5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn full_buffer_rejects_write() {
        let rb = RingBuffer::new(4);
        let data = [1u8, 2, 3, 4];
        assert_eq!(rb.write(&data), 4);
        assert_eq!(rb.available_write(), 0);

        let more = [5u8];
        assert_eq!(rb.write(&more), 0);
    }

    #[test]
    fn partial_read_on_empty() {
        let rb = RingBuffer::new(1024);
        let mut out = [0u8; 10];
        assert_eq!(rb.read(&mut out), 0);
    }

    #[test]
    fn read_or_zero_fill_pads() {
        let rb = RingBuffer::new(1024);
        let data = [1u8, 2, 3];
        rb.write(&data);

        let mut out = [0xffu8; 8];
        let real = rb.read_or_zero_fill(&mut out);
        assert_eq!(real, 3);
        assert_eq!(out, [1, 2, 3, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn concurrent_producer_consumer() {
        use std::sync::Arc;
        use std::thread;

        let rb = Arc::new(RingBuffer::new(256));
        let rb_writer = rb.clone();
        let rb_reader = rb.clone();

        let writer = thread::spawn(move || {
            let mut total = 0usize;
            for i in 0u8..200 {
                let chunk = [i; 4];
                loop {
                    let written = rb_writer.write(&chunk[total % 4..]);
                    total += written;
                    if written > 0 || total >= 800 {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
        });

        let reader = thread::spawn(move || {
            let mut total_read = 0usize;
            let mut buf = [0u8; 16];
            while total_read < 800 {
                let n = rb_reader.read(&mut buf);
                total_read += n;
                if n == 0 {
                    std::thread::yield_now();
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}
