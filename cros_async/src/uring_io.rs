// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::convert::AsRef;
use std::future::Future;
use std::io::{IoSlice, IoSliceMut};
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::uring_executor::{self, Error, MemVec, PendingOperation, RegisteredIoMem, Result};
use crate::uring_mem::BackingMemory;

/// Wrapper to be used for passing a Vec in as backing memory for asynchronous operations.
pub struct VecCompleteIo {
    inner: Vec<u8>,
}

impl Clone for VecCompleteIo {
    fn clone(&self) -> VecCompleteIo {
        VecCompleteIo {
            inner: self.inner.clone(),
        }
    }
}

impl From<Vec<u8>> for VecCompleteIo {
    fn from(vec: Vec<u8>) -> Self {
        VecCompleteIo { inner: vec }
    }
}

impl Into<Vec<u8>> for VecCompleteIo {
    fn into(self) -> Vec<u8> {
        self.inner
    }
}

impl AsRef<Vec<u8>> for VecCompleteIo {
    fn as_ref(&self) -> &Vec<u8> {
        &self.inner
    }
}

impl VecCompleteIo {
    fn check_addrs(&self, mem_off: &MemVec) -> Result<()> {
        let end = mem_off
            .offset
            .checked_add(mem_off.len as u64)
            .ok_or(Error::InvalidOffset)?;
        if end > self.inner.len() as u64 {
            return Err(Error::InvalidOffset);
        }
        Ok(())
    }
}

pub trait MemLen {
    fn get_len(&self) -> usize;
}

impl MemLen for VecCompleteIo {
    fn get_len(&self) -> usize {
        self.inner.len()
    }
}

// Safe to implement BackingMemory as the vec is owned and ref counted.
// Nothing else can get a reference to the vec until all slice are dropped because they borrow Self.
// Nothing can borrow the owned inner vec until self is consumed by `to_inner`, which can't happen
// if there are outstanding mut borrows.
unsafe impl BackingMemory for VecCompleteIo {
    fn io_slice_mut(&self, mem_off: &MemVec) -> Result<IoSliceMut<'_>> {
        // Safe because the vector is valid and will be kept alive longer than this IoSliceMut and
        // this memory is fully owned so it can be modified for the lifetime of this IoSlice safely.
        // The mem_off ranges are checked.
        unsafe {
            self.check_addrs(mem_off)?;
            Ok(IoSliceMut::new(std::slice::from_raw_parts_mut(
                self.inner.as_ptr().add(mem_off.offset as usize) as *mut _,
                mem_off.len,
            )))
        }
    }

    fn io_slice(&self, mem_off: &MemVec) -> Result<IoSlice<'_>> {
        // Safe because the vector is valid and will be kept alive longer than this IoSlice.
        // The mem_off ranges are checked.
        self.check_addrs(mem_off)?;
        Ok(IoSlice::new(
            &self.inner[mem_off.offset as usize..mem_off.offset as usize + mem_off.len],
        ))
    }
}

pub struct AsyncData<F, M, T>
where
    F: AsRawFd,
    M: BackingMemory,
    M: From<T>,
    M: Into<T>,
    M: AsRef<T>,
{
    async_io: AsyncIo<F, M>,
    mem: Rc<M>,
    _t: PhantomData<T>,
}

impl<F, M, T> AsyncData<F, M, T>
where
    F: AsRawFd + Unpin,
    M: BackingMemory + 'static + MemLen + Clone,
    M: From<T>,
    M: Into<T>,
    M: AsRef<T>,
{
    pub fn new(source: F, data: T) -> Result<AsyncData<F, M, T>> {
        let mem = Rc::new(M::from(data));
        Ok(AsyncData {
            async_io: AsyncIo::new(source, mem.clone())?,
            mem,
            _t: PhantomData,
        })
    }

    // Consumes self and returns the original T used to create the struct.
    // Use this to avoid making a copy of T.
    async fn read_once(self, file_offset: u64) -> Result<T> {
        self.async_io
            .read_to_vectored(
                file_offset,
                &[MemVec {
                    offset: 0,
                    len: self.mem.get_len(),
                }],
            )
            .await?;
        drop(self.async_io);
        Ok(Rc::try_unwrap(self.mem)
            .map_err(|_| Error::InvalidOffset)?
            .into()) // TODO unwrap - should handle a hangling reference somehow...
    }

    // Read a T from the file asynchronously. A reference to T will be created and returned.
    // Useful for small T's where the cost of clone is small compared to the setup time for the IO.
    // This way many T values can be read with one registered FD. This saves on internal dup calls.
    async fn read_from(&self, file_offset: u64) -> Result<&T> {
        self.async_io
            .read_to_vectored(
                file_offset,
                &[MemVec {
                    offset: 0,
                    len: self.mem.get_len(),
                }],
            )
            .await?;

        Ok((*self.mem).as_ref())
    }
}

pub struct AsyncIo<F: AsRawFd, M: BackingMemory> {
    registered_io: RegisteredIoMem,
    io: F,
    mem: Rc<M>,
}

impl<F: AsRawFd, M: BackingMemory + 'static> AsyncIo<F, M> {
    pub fn new(io_source: F, mem: Rc<M>) -> Result<AsyncIo<F, M>> {
        let r = uring_executor::register_io(&io_source, mem.clone())?;
        Ok(AsyncIo {
            registered_io: r,
            io: io_source,
            mem,
        })
    }

    pub fn source(&self) -> &F {
        &self.io
    }

    pub fn mem(self) -> std::result::Result<M, Rc<M>> {
        drop(self.registered_io);
        Rc::try_unwrap(self.mem)
    }
}

impl<F: AsRawFd, M: BackingMemory> CompleteIo for AsyncIo<F, M> {
    type CompleteToken = PendingOperation;

    fn read_vec(
        self: Pin<&Self>,
        file_offset: u64,
        mem_offsets: &[MemVec],
    ) -> Result<Self::CompleteToken> {
        self.registered_io.start_readv(file_offset, mem_offsets)
    }

    fn write_vec(
        self: Pin<&Self>,
        file_offset: u64,
        mem_offsets: &[MemVec],
    ) -> Result<Self::CompleteToken> {
        self.registered_io.start_writev(file_offset, mem_offsets)
    }

    fn poll_complete(
        self: Pin<&Self>,
        cx: &mut Context,
        token: &mut Self::CompleteToken,
    ) -> Poll<Result<u32>> {
        self.registered_io.poll_complete(cx, token)
    }
}

pub trait CompleteIo {
    type CompleteToken;

    fn read_vec(
        self: Pin<&Self>,
        file_offset: u64,
        mem_offsets: &[MemVec],
    ) -> Result<Self::CompleteToken>;

    fn write_vec(
        self: Pin<&Self>,
        file_offset: u64,
        mem_offsets: &[MemVec],
    ) -> Result<Self::CompleteToken>;

    fn poll_complete(
        self: Pin<&Self>,
        cx: &mut Context,
        token: &mut Self::CompleteToken,
    ) -> Poll<Result<u32>>;
}

macro_rules! deref_complete_io {
    () => {
        type CompleteToken = T::CompleteToken;

        fn read_vec(
            self: Pin<&Self>,
            file_offset: u64,
            mem_offsets: &[MemVec],
        ) -> Result<Self::CompleteToken> {
            Pin::new(&**self).read_vec(file_offset, mem_offsets)
        }

        fn write_vec(
            self: Pin<&Self>,
            file_offset: u64,
            mem_offsets: &[MemVec],
        ) -> Result<Self::CompleteToken> {
            Pin::new(&**self).write_vec(file_offset, mem_offsets)
        }

        fn poll_complete(
            self: Pin<&Self>,
            cx: &mut Context,
            token: &mut Self::CompleteToken,
        ) -> Poll<Result<u32>> {
            Pin::new(&**self).poll_complete(cx, token)
        }
    };
}

impl<T: ?Sized + CompleteIo + Unpin> CompleteIo for Box<T> {
    deref_complete_io!();
}

impl<T: ?Sized + CompleteIo + Unpin> CompleteIo for &T {
    deref_complete_io!();
}

impl<T: ?Sized + CompleteIo + Unpin> CompleteIo for &mut T {
    deref_complete_io!();
}

impl<P> CompleteIo for Pin<P>
where
    P: Deref + Unpin,
    P::Target: CompleteIo,
{
    type CompleteToken = <<P as std::ops::Deref>::Target as CompleteIo>::CompleteToken;

    fn read_vec(
        self: Pin<&Self>,
        file_offset: u64,
        mem_offsets: &[MemVec],
    ) -> Result<Self::CompleteToken> {
        self.get_ref().as_ref().read_vec(file_offset, mem_offsets)
    }

    fn write_vec(
        self: Pin<&Self>,
        file_offset: u64,
        mem_offsets: &[MemVec],
    ) -> Result<Self::CompleteToken> {
        self.get_ref().as_ref().write_vec(file_offset, mem_offsets)
    }

    fn poll_complete(
        self: Pin<&Self>,
        cx: &mut Context,
        token: &mut Self::CompleteToken,
    ) -> Poll<Result<u32>> {
        self.get_ref().as_ref().poll_complete(cx, token)
    }
}

pub trait CompleteIoExt: CompleteIo {
    fn read_to_vectored<'a>(
        &'a self,
        file_offset: u64,
        mem_offsets: &'a [MemVec],
    ) -> ReadRanges<'a, Self>
    where
        Self: Unpin,
    {
        ReadRanges::new(self, file_offset, mem_offsets)
    }

    fn write_from_vectored<'a>(
        &'a self,
        file_offset: u64,
        mem_offsets: &'a [MemVec],
    ) -> WriteRanges<'a, Self>
    where
        Self: Unpin,
    {
        WriteRanges::new(self, file_offset, mem_offsets)
    }
}

impl<T: CompleteIo + ?Sized> CompleteIoExt for T {}

/// Future for the `read_to_vectored` function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadRanges<'a, R: CompleteIo + ?Sized> {
    reader: &'a R,
    file_offset: u64,
    mem_offsets: &'a [MemVec],
    token: Option<R::CompleteToken>,
}

impl<R: CompleteIo + ?Sized + Unpin> Unpin for ReadRanges<'_, R> {}

impl<'a, R: CompleteIo + ?Sized + Unpin> ReadRanges<'a, R> {
    pub(super) fn new(reader: &'a R, file_offset: u64, mem_offsets: &'a [MemVec]) -> Self {
        ReadRanges {
            reader,
            file_offset,
            mem_offsets,
            token: None,
        }
    }
}

impl<R: CompleteIo + ?Sized + Unpin> Future for ReadRanges<'_, R> {
    type Output = Result<u32>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut token = match std::mem::replace(&mut self.token, None) {
            None => match Pin::new(&self.reader).read_vec(self.file_offset, self.mem_offsets) {
                Ok(t) => Some(t),
                Err(e) => return Poll::Ready(Err(e)),
            },
            Some(t) => Some(t),
        };

        let ret = Pin::new(&self.reader).poll_complete(cx, token.as_mut().unwrap());
        self.token = token;
        ret
    }
}

/// Future for the `read_to_vectored` function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct WriteRanges<'a, R: CompleteIo + ?Sized> {
    reader: &'a R,
    file_offset: u64,
    mem_offsets: &'a [MemVec],
    token: Option<R::CompleteToken>,
}

impl<R: CompleteIo + ?Sized + Unpin> Unpin for WriteRanges<'_, R> {}

impl<'a, R: CompleteIo + ?Sized + Unpin> WriteRanges<'a, R> {
    pub(super) fn new(reader: &'a R, file_offset: u64, mem_offsets: &'a [MemVec]) -> Self {
        WriteRanges {
            reader,
            file_offset,
            mem_offsets,
            token: None,
        }
    }
}

impl<R: CompleteIo + ?Sized + Unpin> Future for WriteRanges<'_, R> {
    type Output = Result<u32>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut token = match std::mem::replace(&mut self.token, None) {
            None => match Pin::new(&self.reader).write_vec(self.file_offset, self.mem_offsets) {
                Ok(t) => Some(t),
                Err(e) => return Poll::Ready(Err(e)),
            },
            Some(t) => Some(t),
        };

        let ret = Pin::new(&self.reader).poll_complete(cx, token.as_mut().unwrap());
        self.token = token;
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    use data_model::{GetVolatileRef, VolatileMemory};
    use futures::pin_mut;

    #[test]
    fn writev() {
        use sys_util::{GuestAddress, GuestMemory};

        // Read one subsection from /dev/zero to the buffer.
        async fn write_out() {
            // Write from source to target. 'target' is used as the backing file because GuestMemory
            // implements 'AsRawFd'.
            let source = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
            let target = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
            source.get_slice(0, 8192).unwrap().write_bytes(0x55);
            target.get_slice(0, 8192).unwrap().write_bytes(0);
            let async_writer = AsyncIo::new(target, source.clone()).unwrap();
            async_writer
                .write_from_vectored(
                    0,
                    &[MemVec {
                        offset: 0,
                        len: 4096,
                    }],
                )
                .await
                .unwrap();

            let target = async_writer.source();
            for i in 0..4096 {
                assert_eq!(0x55u8, target.get_ref(i).unwrap().load());
            }
        }

        let async_fut = write_out();
        pin_mut!(async_fut);
        crate::run_one(async_fut).unwrap();
    }

    #[test]
    fn read_zeros() {
        use data_model::GetVolatileRef;
        use sys_util::{GuestAddress, GuestMemory};

        let buf = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());

        // Read one subsection from /dev/zero to the buffer.
        buf.get_slice(0, 8192).unwrap().write_bytes(0x55);
        async fn zero_buf(buf: Rc<GuestMemory>, addrs: &[MemVec]) -> u32 {
            let f = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
            let async_reader = AsyncIo::new(f, buf.clone()).unwrap();
            async_reader.read_to_vectored(0, addrs).await.unwrap()
        }

        let async_fut = zero_buf(
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

        // Fill two subregions with a joined future.
        buf.get_slice(0, 8192).unwrap().write_bytes(0x55);
        async fn zero2<'a>(
            buf: Rc<GuestMemory>,
            addrs1: &'a [MemVec],
            addrs2: &'a [MemVec],
        ) -> (u32, u32) {
            let f = File::open("/dev/zero").unwrap();
            let async_reader = AsyncIo::new(f, buf.clone()).unwrap();
            let res = futures::future::join(
                async_reader.read_to_vectored(0, addrs1),
                async_reader.read_to_vectored(0, addrs2),
            )
            .await;
            (res.0.unwrap(), res.1.unwrap())
        }
        let async_fut2 = zero2(
            buf.clone(),
            &[MemVec {
                offset: 0,
                len: 1024,
            }],
            &[MemVec {
                offset: 4096,
                len: 1024,
            }],
        );
        pin_mut!(async_fut2);
        assert_eq!((1024, 1024), crate::run_one(async_fut2).unwrap());
        for i in 0..1024 {
            assert_eq!(0u8, buf.get_ref(i).unwrap().load());
        }
        for i in 1024..4096 {
            assert_eq!(0x55u8, buf.get_ref(i).unwrap().load());
        }
        for i in 4096..(4096 + 1024) {
            assert_eq!(0u8, buf.get_ref(i).unwrap().load());
        }
        for i in (4096 + 1024)..8192 {
            assert_eq!(0x55u8, buf.get_ref(i).unwrap().load());
        }
    }

    use data_model::{DataInit, VolatileSlice};
    struct VolatileDataInit<T: DataInit + Clone> {
        data: T,
    }

    impl<T: DataInit + Clone> VolatileDataInit<T> {
        fn new(init_val: T) -> Self {
            VolatileDataInit { data: init_val }
        }

        fn get(&mut self) -> T {
            self.data.clone()
        }
    }

    impl<T: DataInit> VolatileMemory for VolatileDataInit<T> {
        fn get_slice(
            &self,
            offset: u64,
            count: u64,
        ) -> data_model::VolatileMemoryResult<VolatileSlice> {
            let mem_end = match offset.checked_add(count) {
                None => {
                    return Err(data_model::VolatileMemoryError::Overflow {
                        base: offset,
                        offset: count,
                    })
                }
                Some(m) => m,
            };
            if mem_end > std::mem::size_of::<T>() as u64 {
                return Err(data_model::VolatileMemoryError::OutOfBounds { addr: mem_end });
            }
            let slice = self.data.as_slice();
            Ok(unsafe { VolatileSlice::new((slice.as_ptr() as u64 + offset) as *mut _, count) })
        }
    }

    async fn read_u64<T: AsRawFd + Unpin>(e: T) -> u64 {
        let mut data = Rc::new(VolatileDataInit::new(0u64));
        {
            let a = AsyncIo::new(e, data.clone()).unwrap();
            a.read_to_vectored(
                0,
                &[MemVec {
                    offset: 0,
                    len: std::mem::size_of::<u64>(),
                }],
            )
            .await
            .unwrap();
        }
        Rc::get_mut(&mut data).unwrap().get()
    }

    #[test]
    fn eventfd() {
        use sys_util::EventFd;

        async fn write_event(ev: EventFd) {
            ev.write(55).unwrap();
        }
        let eventfd = EventFd::new().unwrap();
        let write_task = write_event(eventfd.try_clone().unwrap());
        let read_task = read_u64(eventfd);
        let joined = futures::future::join(read_task, write_task);
        pin_mut!(joined);
        let (read_res, _) = crate::run_one(joined).unwrap();
        assert_eq!(read_res, 55u64);
    }

    #[test]
    fn read_t() {
        use sys_util::{GuestAddress, GuestMemory};

        let source = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
        source.get_slice(0, 8192).unwrap().write_bytes(0x55);
        let fut = read_u64(source);
        pin_mut!(fut);
        let res = crate::run_one(fut).unwrap();
        assert_eq!(res, 0x5555555555555555);
    }

    #[test]
    fn vec_read() {
        use sys_util::{GuestAddress, GuestMemory};

        async fn fill(vec: Vec<u8>, source: GuestMemory) -> Vec<u8> {
            let async_data: AsyncData<_, VecCompleteIo, _> = AsyncData::new(source, vec).unwrap();
            async_data.read_once(0).await.unwrap()
        }

        let v = vec![0u8; 128];
        let source = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
        source.get_slice(0, 8192).unwrap().write_bytes(0x55);
        let fut = fill(v, source);
        pin_mut!(fut);
        let v = crate::run_one(fut).unwrap();
        assert!(v.iter().all(|&v| v == 0x55));

        let source = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
        source.get_slice(0, 8192).unwrap().write_bytes(0x44);
        let fut = fill(v, source);
        pin_mut!(fut);
        let v = crate::run_one(fut).unwrap();
        assert!(v.iter().all(|&v| v == 0x44));
    }

    #[test]
    fn vec_clone_read() {
        async fn fill_clone() {
            use sys_util::{GuestAddress, GuestMemory};
            let v = vec![0u8; 128];
            let source = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
            source.get_slice(0, 4096).unwrap().write_bytes(0x55);
            source.get_slice(4096, 4096).unwrap().write_bytes(0x44);
            let async_data: AsyncData<_, VecCompleteIo, _> = AsyncData::new(source, v).unwrap();
            let v1 = async_data.read_from(0).await.unwrap();
            assert!(v1.iter().all(|&v| v == 0x55));
            let v2 = async_data.read_from(4096).await.unwrap();
            assert!(v2.iter().all(|&v| v == 0x44));
        }

        let fut = fill_clone();
        pin_mut!(fut);
        crate::run_one(fut).unwrap();
    }
}
