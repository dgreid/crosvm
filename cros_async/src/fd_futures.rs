// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use data_model::volatile_memory::VolatileMemory;
use sys_util::{GuestAddress, GuestMemory};

use crate::fd_executor::{start_write, Error, Result};

/// Core futures build around the FD executor.

/// Represents an asynchronous write.
/// Only guest memory can be written from. This allows the guarantee that the target address won't
/// be freed before the krenel completes the read.
pub struct AsyncWrite {
    out_file: RawFd,
    offset: i64,
    write_submitted: bool,
    addr: GuestAddress,
    len: u64,
    mem: Rc<GuestMemory>,
}

impl Future for AsyncWrite {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        println!("poll write");
        // The next time the future is polled, the write should have completed.
        // Note this is only true with fd_executor, other executors may call poll early requiring
        // there to be a method of polling this particular write from the kernel.
        if self.write_submitted {
            return Poll::Ready(Ok(()));
        }

        let vol_slice = match self.mem.get_slice(self.addr.0, self.len) {
            Err(e) => return Poll::Ready(Err(Error::ReadWriteRange(e))),
            Ok(v) => v,
        };

        unsafe {
            // Safe because the buffer passed will live until the future completes. The buffer is
            // from guest memory and that is tied to this future's lifetime. Also since a volatile
            // slice is passed, it is already assumed to be mutable by actors outside of the
            // program.
            if let Err(e) = start_write(
                self.out_file,
                self.offset,
                vol_slice.as_ptr(),
                vol_slice.size(),
                cx.waker().clone(),
            ) {
                return Poll::Ready(Err(e));
            }
        }
        self.as_mut().write_submitted = true;
        Poll::Pending
    }
}

/// Returns a future that will complete when the async write is finished.
pub fn write_mem(
    file: RawFd,
    offset: i64,
    addr: GuestAddress,
    len: u64,
    mem: Rc<GuestMemory>,
) -> AsyncWrite {
    AsyncWrite {
        out_file: file,
        offset,
        write_submitted: false,
        addr,
        len,
        mem,
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::io::AsRawFd;

    use super::*;

    use crate::executor::Executor;

    #[test]
    fn write_file() {
        let mut f = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open("/tmp/foo")
            .unwrap();
        sys_util::add_fd_flags(f.as_raw_fd(), libc::O_DIRECT).unwrap();
        let mem_size = 64 * 1024 * 1024;
        let mem = Rc::new(GuestMemory::new(&[(GuestAddress(0), mem_size)]).unwrap());

        mem.get_slice(0x1000, 0x100).unwrap().write_bytes(0xa5);
        let fd = f.as_raw_fd();
        let m = mem.clone();
        let this_write = async move {
            write_mem(fd, 0, GuestAddress(0x1000), 0x100, m)
                .await
                .unwrap();
        };

        mem.get_slice(0x2000, 0x100).unwrap().write_bytes(0x5a);
        let fd = f.as_raw_fd();
        let m = mem.clone();
        let that_write = async move {
            write_mem(fd, 0x200, GuestAddress(0x2000), 0x100, m)
                .await
                .unwrap();
        };

        let mut ex = crate::empty_executor().unwrap();
        crate::fd_executor::add_future(Box::pin(this_write)).unwrap();
        crate::fd_executor::add_future(Box::pin(that_write)).unwrap();
        ex.run();

        f.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 0x100];
        f.read(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xa5));
        let mut buf = [0u8; 0x100];
        f.read(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        let mut buf = [0u8; 0x100];
        f.read(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x5a));
    }
}
