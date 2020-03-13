// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt;
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};

use sys_util::{MemoryMapping, Protection, WatchingEvents};

use crate::bindings::*;
use crate::syscalls::*;

#[derive(Debug)]
pub enum Error {
    /// The call to `io_uring_enter` failed with the given errno.
    RingEnter(libc::c_int),
    /// The call to `io_uring_setup` failed with the given errno.
    Setup(libc::c_int),
    MappingCompleteRing(sys_util::MmapError),
    MappingSubmitRing(sys_util::MmapError),
    MappingSubmitEntries(sys_util::MmapError),
    NoSpace,
}
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            RingEnter(e) => write!(f, "Failed to enter io uring {}", e),
            Setup(e) => write!(f, "Failed to setup io uring {}", e),
            MappingCompleteRing(e) => write!(f, "Failed to mmap completion ring {}", e),
            MappingSubmitRing(e) => write!(f, "Failed to mmap submit ring {}", e),
            MappingSubmitEntries(e) => write!(f, "Failed to mmap submit entries {}", e),
            NoSpace => write!(
                f,
                "No space for more ring entries, try increasing the size passed to `new`",
            ),
        }
    }
}

/// Basic statistics about the operations that have been submitted to the uring.
#[derive(Default)]
pub struct URingStats {
    total_enter_calls: u64,
    total_ops: u64,
    total_complete: u64,
}

/// Unsafe wrapper for the kernel's io_uring interface. Allows for queueing multiple I/O operations
/// to the kernel and asynchronously handling the completion of these operations.
/// Use the various `add_*` functions to configure operations, then call `enter` to start the
/// operations and get any completed results. Each op is given a u64 user_data argument that is used
/// to identify the result when returned in the iterator provided by `enter`.
///
/// # Example polling an FD for readable status.
///
/// ```
/// # use std::fs::File;
/// # use std::os::unix::io::AsRawFd;
/// # use std::path::Path;
/// # use sys_util::WatchingEvents;
/// # use io_uring::URingContext;
/// let f = File::open(Path::new("/dev/zero")).unwrap();
/// let mut uring = URingContext::new(16).unwrap();
/// uring
///   .add_poll_fd(f.as_raw_fd(), WatchingEvents::empty().set_read(), 454)
/// .unwrap();
/// assert_eq!(uring.enter().unwrap().next(), Some((454, 1)));
///
/// ```
pub struct URingContext {
    ring_file: File, // Holds the io_uring context FD returned from io_uring_setup.
    submit_ring: SubmitQueueState,
    submit_queue_entries: SubmitQueueEntries,
    complete_ring: CompleteQueueState,
    io_vecs: Vec<libc::iovec>,
    in_flight: usize, // The number of pending operations.
    added: usize,     // The number of ops added since the last call to `io_uring_enter`.
    stats: URingStats,
}

impl URingContext {
    pub fn new(num_entries: usize) -> Result<URingContext> {
        let ring_params = io_uring_params::default();
        unsafe {
            // Safe because the kernel is trusted to only modify params and `File` is created with
            // an FD that it takes complete ownership of.
            let fd = io_uring_setup(num_entries, &ring_params).map_err(Error::Setup)?;
            let ring_file = File::from_raw_fd(fd);

            // Safe because we trust the kernel to set valid sizes in `io_uring_setup` and any error
            // is checked.
            let submit_ring = SubmitQueueState::new(
                MemoryMapping::from_fd_offset_flags(
                    &ring_file,
                    ring_params.sq_off.array as usize
                        + ring_params.sq_entries as usize * std::mem::size_of::<u32>(),
                    IORING_OFF_SQ_RING as usize,
                    libc::MAP_SHARED | libc::MAP_POPULATE,
                    Protection::read_write(),
                )
                .map_err(Error::MappingSubmitRing)?,
                &ring_params,
            );

            let submit_queue_entries = SubmitQueueEntries {
                mmap: MemoryMapping::from_fd_offset_flags(
                    &ring_file,
                    ring_params.sq_entries as usize * std::mem::size_of::<io_uring_sqe>(),
                    IORING_OFF_SQES as usize,
                    libc::MAP_SHARED | libc::MAP_POPULATE,
                    Protection::read_write(),
                )
                .map_err(Error::MappingSubmitEntries)?,
                len: ring_params.sq_entries as usize,
            };

            let complete_ring = CompleteQueueState::new(
                MemoryMapping::from_fd_offset_flags(
                    &ring_file,
                    ring_params.cq_off.cqes as usize
                        + ring_params.cq_entries as usize * std::mem::size_of::<u32>(),
                    IORING_OFF_CQ_RING as usize,
                    libc::MAP_SHARED | libc::MAP_POPULATE,
                    Protection::read_write(),
                )
                .map_err(Error::MappingCompleteRing)?,
                &ring_params,
            );

            Ok(URingContext {
                ring_file,
                submit_ring,
                submit_queue_entries,
                complete_ring,
                io_vecs: vec![
                    libc::iovec {
                        iov_base: null_mut(),
                        iov_len: 0
                    };
                    num_entries
                ],
                added: 0,
                in_flight: 0,
                stats: Default::default(),
            })
        }
    }

    // Call `f` with the next available sqe or return an error if none are free.
    fn with_next_sqe<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut io_uring_sqe, &mut libc::iovec),
    {
        // find next free submission entry in the submit ring and fill it with an iovec.
        // The below raw pointer derefs are safe because the memory the pointers use lives as long
        // as the mmap in self.
        let tail = unsafe { (*self.submit_ring.tail).load(Ordering::Relaxed) };
        let next_tail = tail.wrapping_add(1);
        if next_tail == unsafe { (*self.submit_ring.head).load(Ordering::Acquire) } {
            return Err(Error::NoSpace);
        }
        // fill in iovec of tail and put it on the queue.
        let index = (tail & self.submit_ring.ring_mask) as usize;
        let sqe = self.submit_queue_entries.get_mut(index).unwrap();

        f(sqe, &mut self.io_vecs[index]);

        unsafe {
            *(self.submit_ring.array.offset(index as isize)) = index as u32;

            // Ensure the above writes to sqe are seen before the tail is updated.
            (*self.submit_ring.tail).store(next_tail, Ordering::Release);
        }

        self.added += 1;

        Ok(())
    }

    unsafe fn add_rw_op(
        &mut self,
        ptr: *const u8,
        len: usize,
        fd: RawFd,
        offset: u64,
        user_data: u64,
        op: u8,
    ) -> Result<()> {
        self.with_next_sqe(|sqe, iovec| {
            iovec.iov_base = ptr as *const libc::c_void as *mut _;
            iovec.iov_len = len;
            sqe.opcode = op;
            sqe.addr = iovec as *const _ as *const libc::c_void as u64;
            sqe.len = 1;
            sqe.__bindgen_anon_1.off = offset;
            sqe.__bindgen_anon_3.__bindgen_anon_1.buf_index = 0;
            sqe.ioprio = 0;
            sqe.user_data = user_data;
            sqe.flags = 0;
            sqe.fd = fd;
        })?;

        Ok(())
    }

    /// Asynchronously writes to `fd` from the address given in `ptr`.
    /// # Safety
    /// `add_write` will write up to `len` bytes of data from the address given by `ptr`. This is
    /// only safe if the caller guarantees that the memory lives until the transaction is complete
    /// and that completion has been returned from the `enter` function.
    /// Ensure that the fd remains open until the op completes as well.
    pub unsafe fn add_write(
        &mut self,
        ptr: *const u8,
        len: usize,
        fd: RawFd,
        offset: usize,
        user_data: u64,
    ) -> Result<()> {
        self.add_rw_op(
            ptr,
            len,
            fd,
            offset as u64,
            user_data,
            IORING_OP_WRITEV as u8,
        )
    }

    /// Asynchronously reads from `fd` to the address given in `ptr`.
    /// # Safety
    /// `add_read` will write up to `len` bytes of data to the address given by `ptr`. This is only
    /// safe if the caller guarantees there are no other references to that memory and that the
    /// memory lives until the transaction is complete and that completion has been returned from
    /// the `enter` function.
    /// Ensure that the fd remains open until the op completes as well.
    pub unsafe fn add_read(
        &mut self,
        ptr: *mut u8,
        len: usize,
        fd: RawFd,
        offset: usize,
        user_data: u64,
    ) -> Result<()> {
        self.add_rw_op(
            ptr,
            len,
            fd,
            offset as u64,
            user_data,
            IORING_OP_READV as u8,
        )
    }

    /// `add_fsync` syncs all completed operations, the ordering with in-flight async ops is not
    /// defined.
    pub fn add_fsync(&mut self, fd: RawFd, user_data: u64) -> Result<()> {
        self.with_next_sqe(|sqe, _iovec| {
            sqe.opcode = IORING_OP_FSYNC as u8;
            sqe.fd = fd;
            sqe.user_data = user_data;

            sqe.addr = 0;
            sqe.len = 0;
            sqe.__bindgen_anon_1.off = 0;
            sqe.__bindgen_anon_3.__bindgen_anon_1.buf_index = 0;
            sqe.__bindgen_anon_2.rw_flags = 0;
            sqe.ioprio = 0;
            sqe.flags = 0;
        })
    }

    /// `add_fallocate` adds the fallocate operation to io_uring.
    /// # Safety
    /// TODO - Are some ops on a file unsafe?
    /// Ensure that the fd remains open until the op completes as well.
    pub fn add_fallocate(
        &mut self,
        fd: RawFd,
        offset: usize,
        len: usize,
        mode: u64,
        user_data: u64,
    ) -> Result<()> {
        // Note that len for fallocate in passed in the addr field of the sqe and the mode uses the
        // len field.
        self.with_next_sqe(|sqe, _iovec| {
            sqe.opcode = IORING_OP_FALLOCATE as u8;

            sqe.fd = fd;
            sqe.addr = len as u64;
            sqe.len = mode as u32;
            sqe.__bindgen_anon_1.off = offset as u64;
            sqe.user_data = user_data;

            sqe.__bindgen_anon_3.__bindgen_anon_1.buf_index = 0;
            sqe.__bindgen_anon_2.rw_flags = 0;
            sqe.ioprio = 0;
            sqe.flags = 0;
        })
    }

    /// Adds an FD to be polled based on the given flags.
    /// The user must keep the FD open until the operation completion is returned from `enter`.
    pub fn add_poll_fd(&mut self, fd: RawFd, events: WatchingEvents, user_data: u64) -> Result<()> {
        self.with_next_sqe(|sqe, _iovec| {
            sqe.opcode = IORING_OP_POLL_ADD as u8;
            sqe.fd = fd;
            sqe.user_data = user_data;
            sqe.__bindgen_anon_2.poll_events = events.get_raw() as u16;

            sqe.addr = 0;
            sqe.len = 0;
            sqe.__bindgen_anon_1.off = 0;
            sqe.__bindgen_anon_3.__bindgen_anon_1.buf_index = 0;
            sqe.ioprio = 0;
            sqe.flags = 0;
        })
    }

    /// Removes an FD that was previously added with `add_poll_fd`.
    pub fn remove_poll_fd(
        &mut self,
        fd: RawFd,
        events: WatchingEvents,
        user_data: u64,
    ) -> Result<()> {
        self.with_next_sqe(|sqe, _iovec| {
            sqe.opcode = IORING_OP_POLL_REMOVE as u8;
            sqe.fd = fd;
            sqe.user_data = user_data;
            sqe.__bindgen_anon_2.poll_events = events.get_raw() as u16;

            sqe.addr = 0;
            sqe.len = 0;
            sqe.__bindgen_anon_1.off = 0;
            sqe.__bindgen_anon_3.__bindgen_anon_1.buf_index = 0;
            sqe.ioprio = 0;
            sqe.flags = 0;
        })
    }

    /// Send operations added with the `add_*` functions to the kernel.
    pub fn submit<'a>(&'a mut self) -> Result<()> {
        self.in_flight += self.added;
        self.stats.total_ops = self.stats.total_ops.wrapping_add(self.added as u64);
        if self.added > 0 {
            unsafe {
                self.stats.total_enter_calls = self.stats.total_enter_calls.wrapping_add(1);
                // Safe because the only memory modified is in the completion queue.
                io_uring_enter(self.ring_file.as_raw_fd(), self.added as u64, 1, 0)
                    .map_err(Error::RingEnter)?;
            }
        }
        self.added = 0;

        Ok(())
    }

    /// Send operations added with the `add_*` functions to the kerenl and return an iterator to any
    /// completed operations.
    pub fn enter<'a>(&'a mut self) -> Result<impl Iterator<Item = (u64, i32)> + 'a> {
        let completed = self.complete_ring.num_completed();
        self.stats.total_complete = self.stats.total_complete.wrapping_add(completed as u64);
        self.in_flight -= completed;
        self.in_flight += self.added;
        self.stats.total_ops = self.stats.total_ops.wrapping_add(self.added as u64);
        if self.in_flight > 0 {
            unsafe {
                self.stats.total_enter_calls = self.stats.total_enter_calls.wrapping_add(1);
                // Safe because the only memory modified is in the completion queue.
                io_uring_enter(
                    self.ring_file.as_raw_fd(),
                    self.added as u64,
                    1,
                    IORING_ENTER_GETEVENTS,
                )
                .map_err(Error::RingEnter)?;
            }
        }
        self.added = 0;

        // The CompletionQueue will iterate all completed ops.
        Ok(&mut self.complete_ring)
    }
}

impl AsRawFd for URingContext {
    fn as_raw_fd(&self) -> RawFd {
        self.ring_file.as_raw_fd()
    }
}

struct SubmitQueueEntries {
    mmap: MemoryMapping,
    len: usize,
}

impl SubmitQueueEntries {
    fn get_mut(&mut self, index: usize) -> Option<&mut io_uring_sqe> {
        if index >= self.len {
            return None;
        }
        unsafe {
            // Safe because we trust that the kernel has returned enough memory in io_uring_setup
            // and mmap.
            Some(&mut *(self.mmap.as_ptr() as *mut io_uring_sqe).offset(index as isize))
        }
    }
}

struct SubmitQueueState {
    _mmap: MemoryMapping,
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    _flags: *const AtomicU32, // Will be needed for checing if wakeup is required.
    array: *mut u32,
}

impl SubmitQueueState {
    /// # Safety
    /// Safe iff `mmap` is created by mapping from a uring FD at the SQ_RING offset and params is
    /// the params struct passed to io_uring_setup.
    unsafe fn new(mmap: MemoryMapping, params: &io_uring_params) -> SubmitQueueState {
        let ptr = mmap.as_ptr();
        // Transmutes are safe because a u32 is atomic on all supported architectures and the
        // pointer will live until after self is dropped because mmap is owned.
        let head = ptr.offset(params.sq_off.head as isize) as *const AtomicU32;
        let tail = ptr.offset(params.sq_off.tail as isize) as *const AtomicU32;
        let ring_mask = *(ptr.offset(params.sq_off.ring_mask as isize) as *const u32);
        let flags = ptr.offset(params.sq_off.flags as isize) as *const AtomicU32;
        let array = ptr.offset(params.sq_off.array as isize) as *mut u32;
        SubmitQueueState {
            _mmap: mmap,
            head,
            tail,
            ring_mask,
            _flags: flags,
            array,
        }
    }
}

struct CompleteQueueState {
    mmap: MemoryMapping,
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    cqes_offset: usize,
    len: usize,
    completed: usize,
}

impl CompleteQueueState {
    /// # Safety
    /// Safe iff `mmap` is created by mapping from a uring FD at the CQ_RING offset and params is
    /// the params struct passed to io_uring_setup.
    unsafe fn new(mmap: MemoryMapping, params: &io_uring_params) -> CompleteQueueState {
        let ptr = mmap.as_ptr();
        let head = ptr.offset(params.cq_off.head as isize) as *const AtomicU32;
        let tail = ptr.offset(params.cq_off.tail as isize) as *const AtomicU32;
        let ring_mask = *(ptr.offset(params.cq_off.ring_mask as isize) as *const u32);
        CompleteQueueState {
            mmap,
            head,
            tail,
            ring_mask,
            cqes_offset: params.cq_off.cqes as usize,
            len: params.cq_entries as usize,
            completed: 0,
        }
    }

    fn get_cqe(&mut self, index: usize) -> Option<&io_uring_cqe> {
        if index >= self.len {
            return None;
        }

        unsafe {
            // Safe because we trust that the kernel has returned enough memory in io_uring_setup
            // and mmap.
            let cqes = (self.mmap.as_ptr() as *const u8).offset(self.cqes_offset as isize)
                as *const io_uring_cqe;
            Some(&*cqes.offset(index as isize))
        }
    }

    fn num_completed(&mut self) -> usize {
        std::mem::replace(&mut self.completed, 0)
    }
}

// Return the completed ops with their result.
impl Iterator for CompleteQueueState {
    type Item = (u64, i32);

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            // Safe because the pointers to the atomics are valid and the cqe must be in range
            // because the kernel provided mask is applied to the index.
            let head = (*self.head).load(Ordering::Relaxed);

            // Synchronize the read of tail after the read of head.
            if head == (*self.tail).load(Ordering::Acquire) {
                return None;
            }

            self.completed += 1;

            let cqe = self.get_cqe((head & self.ring_mask) as usize).unwrap();
            let user_data = cqe.user_data;
            let res = cqe.res;

            // Store the new head and ensure the reads above complete before the kernel sees the
            // update to head.
            let new_head = head.wrapping_add(1);
            (*self.head).store(new_head, Ordering::Release);

            Some((user_data, res))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use sys_util::PollContext;
    use tempfile::TempDir;

    use super::*;

    fn append_file_name(path: &Path, name: &str) -> PathBuf {
        let mut joined = path.to_path_buf();
        joined.push(name);
        joined
    }

    #[test]
    fn read_one_block_at_a_time() {
        let tempdir = TempDir::new().unwrap();
        let file_path = append_file_name(tempdir.path(), "test");

        let mut uring = URingContext::new(16).unwrap();
        let mut buf = [0u8; 4096];
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&file_path)
            .unwrap();
        f.write(&buf).unwrap();
        f.write(&buf).unwrap();
        f.write(&buf).unwrap();

        unsafe {
            // Safe because the `enter` call waits until the kernel is done mutating `buf`.
            uring
                .add_read(buf.as_mut_ptr(), buf.len(), f.as_raw_fd(), 0, 55)
                .unwrap();
            assert_eq!(uring.enter().unwrap().next(), Some((55, buf.len() as i32)));
            uring
                .add_read(buf.as_mut_ptr(), buf.len(), f.as_raw_fd(), 0, 56)
                .unwrap();
            assert_eq!(uring.enter().unwrap().next(), Some((56, buf.len() as i32)));
            uring
                .add_read(buf.as_mut_ptr(), buf.len(), f.as_raw_fd(), 0, 57)
                .unwrap();
            assert_eq!(uring.enter().unwrap().next(), Some((57, buf.len() as i32)));
        }
    }

    #[test]
    fn write_one_block() {
        let tempdir = TempDir::new().unwrap();
        let file_path = append_file_name(tempdir.path(), "test");

        let mut uring = URingContext::new(16).unwrap();
        let mut buf = [0u8; 4096];
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&file_path)
            .unwrap();
        f.write(&buf).unwrap();
        f.write(&buf).unwrap();

        unsafe {
            // Safe because the `enter` call waits until the kernel is done mutating `buf`.
            uring
                .add_write(buf.as_mut_ptr(), buf.len(), f.as_raw_fd(), 0, 55)
                .unwrap();
            assert_eq!(uring.enter().unwrap().next(), Some((55, buf.len() as i32)));
        }
    }

    #[test]
    fn write_one_submit_poll() {
        let tempdir = TempDir::new().unwrap();
        let file_path = append_file_name(tempdir.path(), "test");

        let mut uring = URingContext::new(16).unwrap();
        let mut buf = [0u8; 4096];
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&file_path)
            .unwrap();
        f.write(&buf).unwrap();
        f.write(&buf).unwrap();

        let ctx: PollContext<u64> = PollContext::build_with(&[(&uring, 1)]).unwrap();
        {
            // Test that the uring context isn't readable before any events are complete.
            let events = ctx.wait_timeout(Duration::from_millis(1)).unwrap();
            assert!(events.iter_readable().next().is_none());
        }

        unsafe {
            // Safe because the `enter` call waits until the kernel is done mutating `buf`.
            uring
                .add_write(buf.as_mut_ptr(), buf.len(), f.as_raw_fd(), 0, 55)
                .unwrap();
            uring.submit().unwrap();
            // TODO poll for completion with epoll.
            let events = ctx.wait().unwrap();
            let event = events.iter_readable().next().unwrap();
            assert_eq!(event.token(), 1);
            assert_eq!(uring.enter().unwrap().next(), Some((55, buf.len() as i32)));
        }
    }
    #[test]
    fn fallocate_fsync() {
        let tempdir = TempDir::new().unwrap();
        let file_path = append_file_name(tempdir.path(), "test");

        {
            let buf = [0u8; 4096];
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&file_path)
                .unwrap();
            f.write(&buf).unwrap();
        }

        let init_size = std::fs::metadata(&file_path).unwrap().len() as usize;
        let set_size = init_size + 1024 * 1024 * 50;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&file_path)
            .unwrap();

        let mut uring = URingContext::new(16).unwrap();
        uring
            .add_fallocate(f.as_raw_fd(), 0, set_size, 0, 66)
            .unwrap();
        assert_eq!(uring.enter().unwrap().next(), Some((66, 0)));

        uring.add_fsync(f.as_raw_fd(), 67).unwrap();
        assert_eq!(uring.enter().unwrap().next(), Some((67, 0)));

        uring
            .add_fallocate(
                f.as_raw_fd(),
                init_size,
                set_size - init_size,
                (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u64,
                68,
            )
            .unwrap();
        assert_eq!(uring.enter().unwrap().next(), Some((68, 0)));

        drop(f); // Close to ensure directory entires for metadata are updated.

        let new_size = std::fs::metadata(&file_path).unwrap().len() as usize;
        assert_eq!(new_size, set_size);
    }

    #[test]
    fn dev_zero_readable() {
        let f = File::open(Path::new("/dev/zero")).unwrap();
        let mut uring = URingContext::new(16).unwrap();
        uring
            .add_poll_fd(f.as_raw_fd(), WatchingEvents::empty().set_read(), 454)
            .unwrap();
        assert_eq!(uring.enter().unwrap().next(), Some((454, 1)));
    }
}
