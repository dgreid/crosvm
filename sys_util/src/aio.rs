// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::marker::PhantomData;
use std::os::unix::io::RawFd;
use std::ptr::null;

use crate::aio_abi_bindings::{aio_context_t, io_event, iocb, IOCB_CMD_POLL};
use crate::{errno_result, Result};
use crate::{PollToken, WatchingEvents};

use libc::{c_long, c_void, syscall, timespec};

use syscall_defines::linux::LinuxSyscall::{
    SYS_io_destroy, SYS_io_getevents, SYS_io_setup, SYS_io_submit,
};

pub struct Aio<T: PollToken> {
    context: aio_context_t,
    events_ret: Vec<io_event>,
    phantom: PhantomData<T>,
}

impl<T: PollToken> Aio<T> {
    pub fn new(nr_events: usize) -> Result<Aio<T>> {
        let context = io_setup(nr_events)?;
        Ok(Aio {
            context,
            events_ret: vec![Default::default(); nr_events],
            phantom: Default::default(),
        })
    }

    pub fn submit_cb(&mut self, cb: AioCb<T>) -> Result<()> {
        let cbs = [cb.cb];
        unsafe {
            // Safe because of the checks when creating an AioCb.
            io_submit(self.context, &cbs)?;
        }
        Ok(())
    }

    pub fn events(&mut self) -> Result<impl Iterator<Item = &io_event>> {
        let num_events = io_getevents(self.context, &mut self.events_ret)?;
        Ok(self.events_ret[..num_events].iter())
    }
}

impl<T: PollToken> Drop for Aio<T> {
    fn drop(&mut self) {
        io_destroy(self.context);
    }
}

pub struct AioCb<T: PollToken> {
    cb: iocb,
    phantom: PhantomData<T>,
}

impl<T: PollToken> AioCb<T> {
    pub fn new(fd: RawFd, events: WatchingEvents, token: T) -> AioCb<T> {
        AioCb {
            cb: poll_iocb(token.as_raw_token(), fd, events.get_raw().into()),
            phantom: Default::default(),
        }
    }
}

// Creates a iocb for polling the given FD.
fn poll_iocb(token: u64, fd: RawFd, flag_events: u64) -> iocb {
    let mut cb: iocb = Default::default();
    cb.aio_lio_opcode = IOCB_CMD_POLL as u16;
    cb.aio_data = token;
    cb.aio_buf = flag_events;
    cb.aio_fildes = fd as u32;
    cb
}

// Wrapper around the io_setup syscall.
pub fn io_setup(max_events: usize) -> Result<aio_context_t> {
    let context: aio_context_t = Default::default();
    unsafe {
        // Safe because the kernel is trusted to only touch the memory owned by `context`.
        let ret = libc::syscall(
            SYS_io_setup as c_long,
            max_events,
            &context as *const _ as *mut c_void,
        );
        if ret < 0 {
            return errno_result();
        }
    }
    Ok(context)
}

// Destroys a context create with `io_submit`.
pub fn io_destroy(context: aio_context_t) {
    unsafe {
        // Safe because the context can't be accessed after drop, making it OK for the kernel to
        // destroy it.
        syscall(SYS_io_destroy as c_long, context);
    }
}

// Wrapper around io_submit syscall.
// To use io_submit safely, the callbacks passed in must write to only areas of memory that they own
// exclusively or memory contained within a volatile slice.
pub unsafe fn io_submit(context: aio_context_t, cbs: &[iocb]) -> Result<()> {
    // TODO MaybeUninit
    let mut cb_ptrs = [null(); 128]; // TODO - don't limit this to 128?.
    for (ptr, cb) in cb_ptrs.iter_mut().zip(cbs.iter()) {
        *ptr = cb as *const _ as *const c_void;
    }
    let ret = libc::syscall(
        SYS_io_submit as c_long,
        context,
        cbs.len() as c_long,
        &cb_ptrs as *const _ as *const *const iocb,
    );
    if ret < 0 {
        return errno_result();
    }
    Ok(())
}

// Wrapper around io_getevents.
// Only support blocking mode, polling is not supported.
pub fn io_getevents(context: aio_context_t, events: &mut [io_event]) -> Result<usize> {
    unsafe {
        // Safe becuase the kernel is trusted to only write within io_events.
        let ret = syscall(
            SYS_io_getevents as c_long,
            context,
            1,
            events.len(),
            events.as_mut_ptr() as *mut io_event,
            null::<timespec>(),
        );
        if ret < 0 {
            return errno_result();
        }
        Ok(ret as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    #[test]
    fn dev_zero_readable() {
        let file = File::open(Path::new("/dev/zero")).unwrap();
        let cb = AioCb::new(file.as_raw_fd(), WatchingEvents::empty().set_read(), 1u32);

        let mut aio: Aio<u32> = Aio::new(8).unwrap();
        aio.submit_cb(cb).unwrap();
        assert!(aio.events().unwrap().next().is_some())
    }

    #[test]
    fn close_fd_after_submit() {
        let mut aio: Aio<u32> = Aio::new(8).unwrap();

        let mut writef = {
            let (file0, file1) = crate::pipe(true).unwrap();
            let cb = AioCb::new(file0.as_raw_fd(), WatchingEvents::empty().set_read(), 1u32);
            aio.submit_cb(cb).unwrap();
            file1
        };

        writef.write(&[1u8]).unwrap();

        let event = aio.events().unwrap().next().unwrap();
        println!(
            "{:x} {:x} {:x} {:x}",
            event.res,
            event.res2,
            libc::EPOLLHUP,
            libc::EPOLLIN
        );
    }
}
