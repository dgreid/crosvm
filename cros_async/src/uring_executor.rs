// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The executor runs all given futures to completion. Futures register wakers associated with
//! io_uring operations. A waker is called when the set of uring ops the waker is waiting on
//! completes.
//!
//! `URingExecutor` is meant to be used with the `futures-rs` crate that provides combinators and
//! utility functions to combine futures.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Display};
use std::fs::File;
use std::future::Future;
use std::io::{self, IoSlice};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::rc::Rc;
use std::task::Waker;
use std::task::{Context, Poll};

use futures::pin_mut;

use data_model::VolatileMemory;
use io_uring::URingContext;

use crate::executor::{ExecutableFuture, Executor, FutureList};
use crate::WakerToken;

#[derive(Debug)]
pub enum Error {
    /// Attempts to create two Executors on the same thread fail.
    AttemptedDuplicateExecutor,
    /// Failed to copy the FD for the polling context.
    DuplicatingFd(sys_util::Error),
    /// Failed accessing the thread local storage for wakers.
    InvalidContext,
    /// Invalid IoPair.
    InvalidPair,
    /// Error doing the IO.
    Io(io::Error),
    /// Creating a context to wait on FDs failed.
    CreatingContext(io_uring::Error),
    /// Failed to remove the waker remove the polling context.
    RemovingWaker(io_uring::Error),
    /// Failed to submit the operation to the polling context.
    SubmittingOp(io_uring::Error),
    /// URingContext failure.
    URingContextError(io_uring::Error),
    /// Failed to submit or wait for io_uring events.
    URingEnter(io_uring::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            AttemptedDuplicateExecutor => write!(f, "Cannot have two executors on one thread."),
            DuplicatingFd(e) => write!(f, "Failed to copy the FD for the polling context: {}", e),
            InvalidContext => write!(
                f,
                "Invalid context, was the Fd executor created successfully?"
            ),
            InvalidPair => write!(f, "Invalid or unregistered the memory/FD pair."),
            Io(e) => write!(f, "Error during IO: {}", e),
            CreatingContext(e) => write!(f, "Error creating the fd waiting context: {}.", e),
            RemovingWaker(e) => write!(f, "Error removing from the URing context: {}.", e),
            SubmittingOp(e) => write!(f, "Error adding to the URing context: {}.", e),
            URingContextError(e) => write!(f, "URingContext failure: {}", e),
            URingEnter(e) => write!(f, "URing::enter: {}", e),
        }
    }
}

/// Checks if the uring executor can be used on this system.
pub(crate) fn supported() -> bool {
    // Create a dummy uring context to check that the kernel understands the syscalls.
    URingContext::new(8).is_ok()
}

/// Register a file and memory pair for buffered asynchronous operation.
pub fn register_io<F: AsRawFd>(fd: &F, mem: Rc<dyn VolatileMemory>) -> Result<Rc<RegisteredIo>> {
    RingWakerState::with(move |state| state.register_io(fd, mem))?
}

// Tracks active wakers and manages waking pending operations after completion.
thread_local!(static STATE: RefCell<Option<RingWakerState>> = RefCell::new(None));
// Tracks new futures that have been added while running the executor.
thread_local!(static NEW_FUTURES: RefCell<VecDeque<ExecutableFuture<()>>>
              = RefCell::new(VecDeque::new()));

struct IoPair {
    mem: Rc<dyn VolatileMemory>,
    fd: File,
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
struct RegisteredIoToken(u64);
pub struct RegisteredIo {
    io_pair: Rc<IoPair>, // Ref counted so each op can ensure memory and fd live.
    tag: RegisteredIoToken,
}
impl RegisteredIo {
    pub fn start_readv(&self, file_offset: u64, iovecs: &[MemVec]) -> Result<PendingOperation> {
        let op = IoOperation::ReadVectored {
            file_offset,
            addrs: iovecs,
        };
        op.submit(&self.tag)
    }

    pub fn start_writev(&self, file_offset: u64, iovecs: &[MemVec]) -> Result<PendingOperation> {
        let op = IoOperation::WriteVectored {
            file_offset,
            addrs: iovecs,
        };
        op.submit(&self.tag)
    }

    pub fn poll_complete(&self, cx: &mut Context, op: &mut PendingOperation) -> Poll<Result<u32>> {
        pin_mut!(op);
        op.poll(cx)
    }

    pub fn deregister(&self) {
        let _ = RingWakerState::with(|state| state.deregister_io(&self.tag));
    }
}

impl Drop for RegisteredIo {
    fn drop(&mut self) {
        println!("drop reg io");
    }
}

// Tracks active wakers and associates wakers with the futures that registered them.
struct RingWakerState {
    ctx: URingContext,
    pending_ops: BTreeMap<WakerToken, Rc<IoPair>>,
    waiting_ops: BTreeMap<WakerToken, Waker>,
    next_op_token: u64, // Next token for adding to the context.
    completed_ops: BTreeMap<WakerToken, std::io::Result<u32>>,
    registered_io: BTreeMap<RegisteredIoToken, Rc<RegisteredIo>>,
    next_io_token: u64, // Next token for registering IO pairs.
}

impl RingWakerState {
    fn new() -> Result<Self> {
        Ok(RingWakerState {
            ctx: URingContext::new(256).map_err(Error::CreatingContext)?,
            pending_ops: BTreeMap::new(),
            waiting_ops: BTreeMap::new(),
            next_op_token: 0,
            completed_ops: BTreeMap::new(),
            registered_io: BTreeMap::new(),
            next_io_token: 0,
        })
    }

    fn register_io<F: AsRawFd>(
        &mut self,
        fd: &F,
        mem: Rc<dyn VolatileMemory>,
    ) -> Result<Rc<RegisteredIo>> {
        let duped_fd = unsafe {
            // Safe because duplicating an FD doesn't affect memory safety, and the dup'd FD
            // will only be added to the poll loop.
            File::from_raw_fd(dup_fd(fd.as_raw_fd())?)
        };
        let tag = RegisteredIoToken(self.next_io_token);
        let registered_io = Rc::new(RegisteredIo {
            io_pair: Rc::new(IoPair { mem, fd: duped_fd }),
            tag: tag.clone(),
        });
        self.registered_io.insert(tag, registered_io.clone());
        self.next_io_token += 1;
        Ok(registered_io)
    }

    fn deregister_io(&mut self, tag: &RegisteredIoToken) {
        // RegisteredIo is refcounted. There isn't any need to pull pending ops out, let them
        // complete. deregister is not a common path.
        let _ = self.registered_io.remove(tag);
    }

    fn submit_writev(
        &mut self,
        io_tag: &RegisteredIoToken,
        offset: u64,
        addrs: &[MemVec],
    ) -> Result<WakerToken> {
        if let Some(registered_io) = self.registered_io.get(io_tag) {
            unsafe {
                let iovecs = addrs.iter().map(|mem_off| {
                    let vs = registered_io
                        .io_pair
                        .mem
                        .get_slice(mem_off.offset, mem_off.len as u64)
                        .unwrap();
                    // Safe because 'vs' is valid in the backing memory and that will be kept
                    // alive longer than this iterator.
                    IoSlice::new(std::slice::from_raw_parts(vs.as_ptr(), vs.size() as usize))
                });
                // Safe because all the addresses are within the Memory that an Rc is kept for the
                // duration to ensure the memory is valid while the kernel accesses it.
                self.ctx
                    .add_writev(
                        iovecs,
                        registered_io.io_pair.fd.as_raw_fd(),
                        offset,
                        self.next_op_token,
                    )
                    .map_err(Error::SubmittingOp)?;
            }
            let next_op_token = WakerToken(self.next_op_token);
            self.pending_ops
                .insert(next_op_token.clone(), registered_io.io_pair.clone());
            self.next_op_token += 1;
            Ok(next_op_token)
        } else {
            Err(Error::InvalidPair)
        }
    }

    fn submit_readv(
        &mut self,
        io_tag: &RegisteredIoToken,
        offset: u64,
        addrs: &[MemVec],
    ) -> Result<WakerToken> {
        if let Some(registered_io) = self.registered_io.get(io_tag) {
            unsafe {
                let iovecs = addrs.iter().map(|mem_off| {
                    let vs = registered_io
                        .io_pair
                        .mem
                        .get_slice(mem_off.offset, mem_off.len as u64)
                        .unwrap();
                    // Safe because 'vs' is valid in the backing memory and that will be kept
                    // alive longer than this iterator.
                    IoSlice::new(std::slice::from_raw_parts(vs.as_ptr(), vs.size() as usize))
                });
                // Safe because all the addresses are within the Memory that an Rc is kept for the
                // duration to ensure the memory is valid while the kernel accesses it.
                self.ctx
                    .add_readv(
                        iovecs,
                        registered_io.io_pair.fd.as_raw_fd(),
                        offset,
                        self.next_op_token,
                    )
                    .map_err(Error::SubmittingOp)?;
            }
            let next_op_token = WakerToken(self.next_op_token);
            self.pending_ops
                .insert(next_op_token.clone(), registered_io.io_pair.clone());
            self.next_op_token += 1;
            Ok(next_op_token)
        } else {
            Err(Error::InvalidPair)
        }
    }

    // Remove the waker for the given token if it hasn't fired yet.
    fn cancel_waker(&mut self, token: &WakerToken) -> Result<()> {
        if let Some(_) = self.pending_ops.remove(token) {
            // TODO - handle canceling ops in the uring
            // For now the op will complete but the response will be dropped.
        }
        let _ = self.waiting_ops.remove(token);
        let _ = self.completed_ops.remove(token);
        Ok(())
    }

    // Waits until one of the FDs is readable and wakes the associated waker.
    fn wait_wake_event(&mut self) -> Result<()> {
        let events = self.ctx.wait().map_err(Error::URingEnter)?;
        for (raw_token, result) in events {
            let token = WakerToken(raw_token);
            // if the op is still in pending_ops then it hasn't been cancelled and someone is
            // interested in the result, so save it. Otherwise, drop it.
            if let Some(_) = self.pending_ops.remove(&token) {
                if let Some(waker) = self.waiting_ops.remove(&token) {
                    waker.wake_by_ref();
                }
                self.completed_ops.insert(token, result);
            }
        }
        Ok(())
    }

    fn get_result(&mut self, token: &WakerToken, waker: Waker) -> Option<io::Result<u32>> {
        if let Some(result) = self.completed_ops.remove(token) {
            Some(result)
        } else {
            if self.pending_ops.contains_key(token) && !self.waiting_ops.contains_key(token) {
                self.waiting_ops.insert(token.clone(), waker);
            }
            None
        }
    }

    fn with<R, F: FnOnce(&mut RingWakerState) -> R>(f: F) -> Result<R> {
        STATE.with(|state| {
            if state.borrow().is_none() {
                state.replace(Some(RingWakerState::new()?));
            }
            let mut state = state.borrow_mut();
            if let Some(state) = state.as_mut() {
                Ok(f(state))
            } else {
                Err(Error::InvalidContext)
            }
        })
    }
}

/// Runs futures to completion on a single thread. Futures are allowed to block on file descriptors
/// only. Futures can only block on FDs becoming readable or writable. `URingExecutor` is meant to be
/// used where a poll or select loop would be used otherwise.
pub(crate) struct URingExecutor<T: FutureList> {
    futures: T,
}

impl<T: FutureList> Executor for URingExecutor<T> {
    type Output = Result<T::Output>;

    fn run(&mut self) -> Self::Output {
        self.append_futures();

        loop {
            if let Some(output) = self.futures.poll_results() {
                return Ok(output);
            }

            self.append_futures();

            // If no futures are ready, sleep until a waker is signaled.
            if !self.futures.any_ready() {
                RingWakerState::with(|state| state.wait_wake_event())??;
            }
        }
    }

    fn add_future(&self, future: Pin<Box<dyn Future<Output = ()>>>) {
        NEW_FUTURES.with(|new_futures| {
            let mut new_futures = new_futures.borrow_mut();
            new_futures.push_back(ExecutableFuture::new(future));
        });
    }
}

impl<T: FutureList> URingExecutor<T> {
    /// Create a new executor.
    pub fn new(futures: T) -> Result<URingExecutor<T>> {
        RingWakerState::with(|_| ())?;
        Ok(URingExecutor { futures })
    }

    // Add any new futures and wakers to the lists.
    fn append_futures(&mut self) {
        let _ = NEW_FUTURES.with(|new_futures| {
            let mut new_futures = new_futures.borrow_mut();
            self.futures.futures_mut().append(&mut new_futures);
        });
    }
}

impl<T: FutureList> Drop for URingExecutor<T> {
    fn drop(&mut self) {
        STATE.with(|state| {
            state.replace(None);
        });
        // Drop any pending futures that were added.
        NEW_FUTURES.with(|new_futures| {
            let mut new_futures = new_futures.borrow_mut();
            new_futures.clear();
        });
    }
}

// Used to dup the FDs passed to the executor so there is a guarantee they aren't closed while
// waiting in TLS to be added to the main polling context.
unsafe fn dup_fd(fd: RawFd) -> Result<RawFd> {
    let ret = libc::dup(fd);
    if ret < 0 {
        Err(Error::DuplicatingFd(sys_util::Error::last()))
    } else {
        Ok(ret)
    }
}

#[derive(Debug)]
pub struct MemVec {
    pub offset: u64,
    pub len: usize,
}

enum IoOperation<'a> {
    ReadVectored {
        file_offset: u64,
        addrs: &'a [MemVec],
    },
    WriteVectored {
        file_offset: u64,
        addrs: &'a [MemVec],
    },
}

impl<'a> IoOperation<'a> {
    fn submit(self, tag: &RegisteredIoToken) -> Result<PendingOperation> {
        let waker_token = match self {
            IoOperation::ReadVectored { file_offset, addrs } => STATE.with(|state| {
                let mut state = state.borrow_mut();
                if let Some(state) = state.as_mut() {
                    state.submit_readv(tag, file_offset, addrs)
                } else {
                    Err(Error::InvalidContext)
                }
            })?,
            IoOperation::WriteVectored { file_offset, addrs } => STATE.with(|state| {
                let mut state = state.borrow_mut();
                if let Some(state) = state.as_mut() {
                    state.submit_writev(tag, file_offset, addrs)
                } else {
                    Err(Error::InvalidContext)
                }
            })?,
        };
        Ok(PendingOperation {
            waker_token: Some(waker_token),
        })
    }
}

pub struct PendingOperation {
    waker_token: Option<WakerToken>,
}

impl Future for PendingOperation {
    type Output = Result<u32>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if let Some(waker_token) = &self.waker_token {
            if let Some(result) =
                RingWakerState::with(|state| state.get_result(waker_token, cx.waker().clone()))?
            {
                self.waker_token = None;
                return Poll::Ready(result.map_err(Error::Io));
            }
        }
        Poll::Pending
    }
}

impl Drop for PendingOperation {
    fn drop(&mut self) {
        if let Some(waker_token) = self.waker_token.take() {
            let _ = RingWakerState::with(|state| state.cancel_waker(&waker_token));
        }
    }
}

#[cfg(test)]
mod test {
    use std::future::Future;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use futures::future::Either;

    use super::*;

    struct TestFut {
        registered_io: Rc<RegisteredIo>,
        pending_operation: Option<PendingOperation>,
    }

    impl TestFut {
        fn new<T: AsRawFd>(io_source: T, mem: Rc<dyn VolatileMemory>) -> TestFut {
            TestFut {
                registered_io: crate::uring_executor::register_io(&io_source, mem.clone()).unwrap(),
                pending_operation: None,
            }
        }
    }

    impl Future for TestFut {
        type Output = Result<u32>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let mut token = match std::mem::replace(&mut self.pending_operation, None) {
                None => Some(
                    self.registered_io
                        .start_readv(0, &[MemVec { offset: 0, len: 8 }])
                        .unwrap(),
                ),
                Some(t) => Some(t),
            };

            let ret = self
                .registered_io
                .poll_complete(cx, token.as_mut().unwrap());
            self.pending_operation = token;
            ret
        }
    }

    #[test]
    fn pend_on_pipe() {
        use sys_util::{GuestAddress, GuestMemory};

        async fn do_test() {
            let read_target = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
            let (read_source, _w) = sys_util::pipe(true).unwrap();
            let done = Box::pin(async { 5usize });
            let pending = Box::pin(TestFut::new(read_source, read_target.clone()));
            match futures::future::select(pending, done).await {
                Either::Right((5, pending)) => std::mem::drop(pending),
                _ => panic!("unexpected select result"),
            }
        }

        let fut = do_test();

        crate::run_one(Box::pin(fut)).unwrap();
    }

    #[test]
    fn pend_on_enventfd() {
        use sys_util::{EventFd, GuestAddress, GuestMemory};

        async fn do_test() {
            let read_target = Rc::new(GuestMemory::new(&[(GuestAddress(0), 8192)]).unwrap());
            let read_source = EventFd::new().unwrap();
            let done = Box::pin(async { 5usize });
            let pending = Box::pin(TestFut::new(read_source, read_target.clone()));
            match futures::future::select(pending, done).await {
                Either::Right((5, pending)) => std::mem::drop(pending),
                _ => panic!("unexpected select result"),
            }
        }

        let fut = do_test();

        crate::run_one(Box::pin(fut)).unwrap();
    }
}
