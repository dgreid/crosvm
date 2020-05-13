// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::marker::Unpin;
use std::ops::DerefMut;
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

pub trait AsyncVolatileReadExt: AsyncVolatileRead {
    /// Returns a future that will return the number of bytes read when complete or an error.
    fn read_to_vectored<'a>(
        &'a mut self,
        offset: u64,
        addrs: &'a [MemVec],
    ) -> ReadVectored<'a, Self>
    where
        Self: Unpin,
    {
        ReadVectored::new(self, offset, addrs)
    }
}
impl<R: AsyncVolatileRead + ?Sized> AsyncVolatileReadExt for R {}

/// Future for the `read_to_vectored` method.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadVectored<'a, R: ?Sized> {
    reader: &'a mut R,
    offset: u64,
    addrs: &'a [MemVec],
}

impl<R: ?Sized + Unpin> Unpin for ReadVectored<'_, R> {}

impl<'a, R: AsyncVolatileRead + ?Sized + Unpin> ReadVectored<'a, R> {
    pub(super) fn new(reader: &'a mut R, offset: u64, addrs: &'a [MemVec]) -> Self {
        ReadVectored {
            reader,
            offset,
            addrs,
        }
    }
}

impl<R: AsyncVolatileRead + ?Sized + Unpin> Future for ReadVectored<'_, R> {
    type Output = Result<u32>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        Pin::new(&mut this.reader).poll_vec_read(cx, this.offset, this.addrs)
    }
}

macro_rules! deref_async_volatile_read {
    () => {
        fn poll_vec_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context,
            file_offset: u64,
            iovecs: &[MemVec],
        ) -> Poll<Result<u32>> {
            Pin::new(&mut **self).poll_vec_read(cx, file_offset, iovecs)
        }
    };
}

impl<T: ?Sized + AsyncVolatileRead + Unpin> AsyncVolatileRead for Box<T> {
    deref_async_volatile_read!();
}

impl<T: ?Sized + AsyncVolatileRead + Unpin> AsyncVolatileRead for &mut T {
    deref_async_volatile_read!();
}

impl<P> AsyncVolatileRead for Pin<P>
where
    P: DerefMut + Unpin,
    P::Target: AsyncVolatileRead,
{
    fn poll_vec_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        file_offset: u64,
        iovecs: &[MemVec],
    ) -> Poll<Result<u32>> {
        self.get_mut()
            .as_mut()
            .poll_vec_read(cx, file_offset, iovecs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    async fn zero_buf(buf: Rc<dyn VolatileMemory>, addrs: &[MemVec]) -> u32 {
        let f = File::open("/dev/zero").unwrap();
        let mut async_reader = AsyncIo::new(f, buf.clone()).unwrap();
        async_reader.read_to_vectored(0, addrs).await.unwrap()
    }

    #[test]
    fn read_zeros() {
        use data_model::GetVolatileRef;
        use sys_util::{GuestAddress, GuestMemory};
        let b = [55u8; 8192];
        let buf = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
        buf.get_slice(0, 8192).unwrap().write_bytes(0x55);
        let mut async_fut = zero_buf(
            buf.clone(),
            &[MemVec {
                offset: 1024,
                len: 4096,
            }],
        );
        pin_mut!(async_fut);
        assert_eq!(4096, crate::run_one(async_fut).unwrap());
        for i in 1024..(1024 + 4096) {
            assert_eq!(0u8, buf.get_ref(i).unwrap().load());
        }
    }
}
