// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::AsRawFd;
use std::rc::Rc;

use data_model::VolatileMemory;

use crate::uring_executor::{self, MemVec, RegisteredIo, Result};

pub struct AsyncIo<T: AsRawFd> {
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

    /// `file_offset` is the offset in the reading source `self`, most commonly a backing file.
    /// Note that the `iovec`s are relative to the start of the memory region the AsyncIO was
    /// created with.
    pub async fn read_to_vectored(&self, file_offset: u64, iovecs: &[MemVec]) -> Result<u32> {
        self.registered_io.do_readv(file_offset, iovecs).await
    }

    /// `file_offset` is the offset in the reading source `self`, most commonly a backing file.
    /// Note that the `iovec`s are relative to the start of the memory region the AsyncIO was
    /// created with.
    pub async fn write_from_vectored(&self, file_offset: u64, iovecs: &[MemVec]) -> Result<u32> {
        self.registered_io.do_writev(file_offset, iovecs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    use futures::pin_mut;

    #[test]
    fn writev() {
        use data_model::GetVolatileRef;
        use sys_util::{GuestAddress, GuestMemory};

        // Read one subsection from /dev/zero to the buffer.
        async fn write_out(addrs: &[MemVec]) {
            // Write from source to target. 'target' is used as the backing file because GuestMemory
            // implements 'AsRawFd'.
            let source = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
            let target = GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap();
            source.get_slice(0, 8192).unwrap().write_bytes(0x55);
            target.get_slice(0, 8192).unwrap().write_bytes(0);
            let async_writer = AsyncIo::new(target, source.clone()).unwrap();
            async_writer.write_from_vectored(0, addrs).await.unwrap();

            let (target, _) = async_writer.into_parts();
            for i in 0..4096 {
                assert_eq!(0x55u8, target.get_ref(i).unwrap().load());
            }
        }

        let memvecs = vec![MemVec {
            offset: 1024,
            len: 4096,
        }];
        let async_fut = write_out(&memvecs);
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
        async fn zero_buf(buf: Rc<dyn VolatileMemory>, addrs: &[MemVec]) -> u32 {
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
            buf: Rc<dyn VolatileMemory>,
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
}
