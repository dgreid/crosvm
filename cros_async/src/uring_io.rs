// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::pin_mut;

use data_model::VolatileMemory;

use crate::uring_executor::{self, MemVec, RegisteredIo, Result};

struct AsyncIo<T: AsRawFd> {
    registered_io: Rc<RegisteredIo>,
    io: T,
    mem: Rc<dyn VolatileMemory>,
}

impl<T: AsRawFd> AsyncIo<T> {
    pub fn new(io_source: T, mem: Rc<dyn VolatileMemory>) -> Result<AsyncIo<T>> {
        Ok(AsyncIo {
            registered_io: uring_executor::register_io(&io_source, mem.clone())?,
            io: io_source,
            mem,
        })
    }

    pub fn into_parts(self) -> (T, Rc<dyn VolatileMemory>) {
        (self.io, self.mem)
    }
}

pub trait AsyncVolatileRead {
    /// `file_offset` is the offset in the reading source `self`, most commonly a backing file.
    /// Pass Rc<M> so that the lifetime of the memory can be ensured to live longer than the
    /// asynchronous op. A ref is taken by the executor and not released until the op is completed,
    /// uring ops aren't cancelled synchronously, requiring the ref to be held beyond the scope of
    /// the returned future.
    /// Note that the `iovec`s are be relative to the start of the memory region the implementer of
    /// the trait is working with.
    fn poll_vec_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        file_offset: u64,
        iovecs: &[MemVec],
    ) -> Poll<Result<u32>>;
}

impl<T: AsRawFd> AsyncVolatileRead for AsyncIo<T> {
    fn poll_vec_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        file_offset: u64,
        iovecs: &[MemVec],
    ) -> Poll<Result<u32>> {
        poll_future(cx, self.registered_io.do_readv(file_offset, iovecs))
    }
}

/// Pins a future and then polls it.
fn poll_future<T>(cx: &mut Context<'_>, fut: impl Future<Output = T>) -> Poll<T> {
    pin_mut!(fut);
    fut.poll(cx)
}

#[cfg(test)]
mod tests {
    async fn zero_buf(buf: Rc<Box<dyn VolatileMemory>>, addrs: &[MemVec]) -> u32 {
        let f = File::open("/dev/zero").unwrap();
        let async_reader = AsyncIo::new(f, buf.clone());
        async_reader.read_to_vectored(0, addrs).await.unwrap()
    }

    #[test]
    fn read_zeros() {
        let buf: Box<[u8]> = Rc::new(Box::new([55u8; 8192]));
        let mut async_fut = zero_buf(
            buf.clone(),
            &[MemVec {
                offset: 1024,
                len: 4096,
            }],
        );
        pin_mut!(async_fut);
        assert_eq!(4096, run_one(async_fut));
        assert!(buf[1024..(1024 + 4096)].iter().all(|v| v == 0));
        assert_eq(0, buf[1024]);
    }
}
