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
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::task::Waker;

use io_uring::URingContext;
use sys_util::WatchingEvents;

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
    /// Creating a context to wait on FDs failed.
    CreatingContext(io_uring::Error),
    /// Failed to remove the waker remove the polling context.
    RemovingWaker(io_uring::Error),
    /// Failed to submit the waker to the polling context.
    SubmittingWaker(io_uring::Error),
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
            CreatingContext(e) => write!(f, "Error creating the fd waiting context: {}.", e),
            RemovingWaker(e) => write!(f, "Error removing from the URing context: {}.", e),
            SubmittingWaker(e) => write!(f, "Error adding to the URing context: {}.", e),
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

// Tracks active wakers and the futures they are associated with.
thread_local!(static STATE: RefCell<Option<RingWakerState>> = RefCell::new(None));

fn notify_fd_ready(fd: RawFd, waker: Waker, events: WatchingEvents) -> Result<WakerToken> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(state) = state.as_mut() {
            state.add_fd(fd, waker, events)
        } else {
            Err(Error::InvalidContext)
        }
    })
}

/// Tells the waking system to wake `waker` when `fd` becomes readable.
/// The 'fd' must be fully owned by the future adding the waker, and must not be closed until the
/// next time the future is polled. If the fd is closed, there is a race where another FD can be
/// opened on top of it causing the next poll to access the new target file.
/// Returns a `WakerToken` that can be used to cancel the waker before it completes.
pub(crate) fn add_read_waker(fd: RawFd, waker: Waker) -> Result<WakerToken> {
    notify_fd_ready(fd, waker, WatchingEvents::empty().set_read())
}

/// Tells the waking system to wake `waker` when `fd` becomes writable.
/// The 'fd' must be fully owned by the future adding the waker, and must not be closed until the
/// next time the future is polled. If the fd is closed, there is a race where another FD can be
/// opened on top of it causing the next poll to access the new target file.
/// Returns a `WakerToken` that can be used to cancel the waker before it completes.
pub(crate) fn add_write_waker(fd: RawFd, waker: Waker) -> Result<WakerToken> {
    notify_fd_ready(fd, waker, WatchingEvents::empty().set_write())
}

/// Cancels the waker that returned the given token if the waker hasn't yet fired.
pub(crate) fn cancel_waker(token: &WakerToken) -> Result<()> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(state) = state.as_mut() {
            state.cancel_waker(token)
        } else {
            Err(Error::InvalidContext)
        }
    })
}

pub(crate) fn get_result(token: &WakerToken) -> Option<std::io::Result<u32>> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(state) = state.as_mut() {
            state.get_result(token)
        } else {
            None
        }
    })
}

/// Adds a new top level future to the Executor.
/// These futures must return `()`, indicating they are intended to create side-effects only.
pub(crate) fn add_future(future: Pin<Box<dyn Future<Output = ()>>>) -> Result<()> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(state) = state.as_mut() {
            state.new_futures.push_back(ExecutableFuture::new(future));
            Ok(())
        } else {
            Err(Error::InvalidContext)
        }
    })
}

// Tracks active wakers and associates wakers with the futures that registered them.
struct RingWakerState {
    ctx: URingContext,
    pending_ops: BTreeMap<WakerToken, (File, WatchingEvents, Waker)>,
    next_token: u64, // Next token for adding to the context.
    completed_ops: BTreeMap<WakerToken, std::io::Result<u32>>,
    new_futures: VecDeque<ExecutableFuture<()>>,
}

impl RingWakerState {
    fn new() -> Result<Self> {
        Ok(RingWakerState {
            ctx: URingContext::new(256).map_err(Error::CreatingContext)?,
            pending_ops: BTreeMap::new(),
            next_token: 0,
            completed_ops: BTreeMap::new(),
            new_futures: VecDeque::new(),
        })
    }

    // Adds an fd that, when and event is ready, will trigger the given waker.
    fn add_fd(&mut self, fd: RawFd, waker: Waker, events: WatchingEvents) -> Result<WakerToken> {
        let duped_fd = unsafe {
            // Safe because duplicating an FD doesn't affect memory safety, and the dup'd FD
            // will only be added to the poll loop.
            File::from_raw_fd(dup_fd(fd)?)
        };
        self.ctx
            .add_poll_fd(duped_fd.as_raw_fd(), &events, self.next_token)
            .map_err(Error::SubmittingWaker)?;
        let next_token = WakerToken(self.next_token);
        self.pending_ops
            .insert(next_token.clone(), (duped_fd, events, waker));
        self.next_token += 1;
        Ok(next_token)
    }

    // Remove the waker for the given token if it hasn't fired yet.
    fn cancel_waker(&mut self, token: &WakerToken) -> Result<()> {
        if let Some((file, events, _waker)) = self.pending_ops.remove(token) {
            // TODO - handle canceling other ops
            self.ctx
                .remove_poll_fd(file.as_raw_fd(), &events, token.0)
                .map_err(Error::RemovingWaker)?
        }
        Ok(())
    }

    // Waits until one of the FDs is readable and wakes the associated waker.
    fn wait_wake_event(&mut self) -> Result<()> {
        let events = self.ctx.wait().map_err(Error::URingEnter)?;
        for (raw_token, result) in events {
            let token = WakerToken(raw_token);
            if let Some((_file, _event, waker)) = self.pending_ops.remove(&token) {
                // Store the result so it can be retrieved after the future is woken.
                self.completed_ops.insert(token, result);
                waker.wake_by_ref();
            }
        }
        Ok(())
    }

    fn get_result(&mut self, token: &WakerToken) -> Option<std::io::Result<u32>> {
        if let Some(result) = self.completed_ops.remove(token) {
            Some(result)
        } else {
            None
        }
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
                STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    if let Some(state) = state.as_mut() {
                        state.wait_wake_event()?;
                    } else {
                        unreachable!("Can't get here without a context being created");
                    }
                    Ok(())
                })?;
            }
        }
    }
}

impl<T: FutureList> URingExecutor<T> {
    /// Create a new executor.
    pub fn new(futures: T) -> Result<URingExecutor<T>> {
        STATE.with(|state| {
            if state.borrow().is_some() {
                return Err(Error::AttemptedDuplicateExecutor);
            }
            state.replace(Some(RingWakerState::new()?));
            Ok(())
        })?;
        Ok(URingExecutor { futures })
    }

    // Add any new futures and wakers to the lists.
    fn append_futures(&mut self) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            if let Some(state) = state.as_mut() {
                self.futures.futures_mut().append(&mut state.new_futures);
            } else {
                unreachable!("Can't get here without a context being created");
            }
        });
    }
}

impl<T: FutureList> Drop for URingExecutor<T> {
    fn drop(&mut self) {
        STATE.with(|state| {
            state.replace(None);
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

#[cfg(test)]
mod test {
    use std::future::Future;
    use std::os::unix::io::AsRawFd;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use futures::future::Either;
    use futures::pin_mut;

    use super::*;

    use crate::executor::UnitFutures;

    struct TestFut {
        f: File,
        token: Option<WakerToken>,
    }

    impl TestFut {
        fn new(f: File) -> TestFut {
            TestFut { f, token: None }
        }
    }

    impl Future for TestFut {
        type Output = u64;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            if self.token.is_none() {
                self.token = Some(add_read_waker(self.f.as_raw_fd(), cx.waker().clone()).unwrap());
            }
            Poll::Pending
        }
    }

    impl Drop for TestFut {
        fn drop(&mut self) {
            if let Some(token) = self.token.take() {
                cancel_waker(&token).unwrap();
            }
        }
    }

    #[test]
    fn cancel() {
        async fn do_test() {
            let (r, _w) = sys_util::pipe(true).unwrap();
            let done = async { 5usize };
            let pending = TestFut::new(r);
            pin_mut!(done);
            pin_mut!(pending);
            match futures::future::select(pending, done).await {
                Either::Right((5, _pending)) => (),
                _ => panic!("unexpected select result"),
            }
        }

        let fut = do_test();

        let mut ex = URingExecutor::new(UnitFutures::new()).expect("Failed creating executor");
        add_future(Box::pin(fut)).unwrap();
        ex.run().unwrap();
        STATE.with(|state| {
            let state = state.borrow_mut();
            assert!(state.as_ref().unwrap().pending_ops.is_empty());
        });
    }

    #[test]
    fn run() {
        // Example of starting the framework and running a future:
        async fn my_async(x: Rc<RefCell<u64>>) {
            x.replace(4);
        }

        let mut ex = URingExecutor::new(UnitFutures::new()).expect("Failed creating executor");
        let x = Rc::new(RefCell::new(0));
        crate::uring_executor::add_future(Box::pin(my_async(x.clone()))).unwrap();
        ex.run().unwrap();
        assert_eq!(*x.borrow(), 4);
    }
}
