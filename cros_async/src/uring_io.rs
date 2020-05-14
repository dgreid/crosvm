// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::AsRawFd;
use std::rc::Rc;

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

    /// `file_offset` is the offset in the reading source `self`, most commonly a backing file.
    /// Note that the `iovec`s are relative to the start of the memory region the AsyncIO was
    /// created with.
    async fn read_to_vectored(&mut self, file_offset: u64, iovecs: &[MemVec]) -> Result<u32> {
        self.registered_io.do_readv(file_offset, iovecs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    use futures::pin_mut;

    async fn zero_buf(buf: Rc<dyn VolatileMemory>, addrs: &[MemVec]) -> u32 {
        let f = File::open("/dev/zero").unwrap();
        let mut async_reader = AsyncIo::new(f, buf.clone()).unwrap();
        async_reader.read_to_vectored(0, addrs).await.unwrap()
    }

    #[test]
    fn read_zeros() {
        use data_model::GetVolatileRef;
        use sys_util::{GuestAddress, GuestMemory};

        let buf = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
        buf.get_slice(0, 8192).unwrap().write_bytes(0x55);
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
    }
}
