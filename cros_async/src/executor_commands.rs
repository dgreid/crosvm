// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::{self, Display};
use std::future::Future;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::Waker;

use crate::executor::{Executor, FutureList};
use crate::fd_executor::FdExecutor;
use crate::uring_executor::URingExecutor;
use crate::{fd_executor, uring_executor};

#[derive(Debug)]
pub enum Error {
    /// Error from the FD executor.
    FdExecutor(fd_executor::Error),
    /// An unknown pending waker was passed in.
    UnknownWaker,
    /// Error from the uring executor.
    URingExecutor(uring_executor::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            FdExecutor(e) => write!(f, "Failure in the FD executor: {}", e),
            UnknownWaker => write!(f, "Unknown waker"),
            URingExecutor(e) => write!(f, "Failure in the uring executor: {}", e),
        }
    }
}

/// Inner wrapper around a u64 used a token to uniquely identify a pending waker.
#[derive(Clone, PartialEq, PartialOrd, Eq, Ord)]
pub(crate) struct WakerToken(pub(crate) u64);

/// A token returned from `add_waker` that can be used to cancel the waker before it completes.
/// Used to manage getting the result from the underlying executor for a completed operation.
/// Dropping a `PendingWaker` will get the result from the executor.
pub struct PendingWaker {
    token: Option<WakerToken>,
}

impl PendingWaker {
    pub(crate) fn new(token: WakerToken) -> PendingWaker {
        PendingWaker { token: Some(token) }
    }

    pub fn result(&mut self) -> Option<std::io::Result<u32>> {
        self.token.take().as_ref().and_then(Self::get_result)
    }

    /// Cancels the waker that returned the given token if the waker hasn't yet fired.
    pub fn cancel(&mut self) -> Result<()> {
        if let Some(token) = self.token.take() {
            Self::do_cancel(&token)
        } else {
            Err(Error::UnknownWaker)
        }
    }

    fn do_cancel(token: &WakerToken) -> Result<()> {
        if use_uring() {
            uring_executor::cancel_waker(token).map_err(Error::URingExecutor)
        } else {
            fd_executor::cancel_waker(token).map_err(Error::FdExecutor)
        }
    }

    fn get_result(token: &WakerToken) -> Option<std::io::Result<u32>> {
        if use_uring() {
            uring_executor::get_result(token)
        } else {
            fd_executor::get_result(token)
        }
    }
}

impl Drop for PendingWaker {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // The common case is that the operation has completed and might need the result
            // reclaimed.
            if Self::get_result(&token).is_some() {
                return;
            }
            let _ = Self::do_cancel(&token);
        }
    }
}

// Functions to be used by `Future` implementations

pub(crate) static TEST_DISABLE_URING: AtomicBool = AtomicBool::new(false);

// Checks if the uring executor is available and if it us use it.
// Caches the result so that the check is only run once.
// Useful for falling back to the FD executor on pre-uring kernels.
fn use_uring() -> bool {
    const UNKNOWN: u32 = 0;
    const URING: u32 = 1;
    const FD: u32 = 2;
    static USE_URING: AtomicU32 = AtomicU32::new(UNKNOWN);
    if TEST_DISABLE_URING.load(Ordering::Relaxed) {
        return false;
    }
    match USE_URING.load(Ordering::Relaxed) {
        UNKNOWN => {
            if uring_executor::supported() {
                USE_URING.store(URING, Ordering::Relaxed);
                true
            } else {
                USE_URING.store(FD, Ordering::Relaxed);
                false
            }
        }
        URING => true,
        FD => false,
        _ => unreachable!("invalid use uring state"),
    }
}

// Runs an executor with the given future list.
// Chooses the uring executor if available, otherwise falls back to the FD executor.
pub(crate) fn run_executor<T: FutureList>(future_list: T) -> Result<T::Output> {
    if use_uring() {
        URingExecutor::new(future_list)
            .and_then(|mut ex| ex.run())
            .map_err(Error::URingExecutor)
    } else {
        FdExecutor::new(future_list)
            .and_then(|mut ex| ex.run())
            .map_err(Error::FdExecutor)
    }
}

/// Tells the waking system to wake `waker` when `fd` becomes readable.
/// The 'fd' must be fully owned by the future adding the waker, and must not be closed until the
/// next time the future is polled. If the fd is closed, there is a race where another FD can be
/// opened on top of it causing the next poll to access the new target file.
/// Returns a `PendingWaker` that can be used to cancel the waker before it completes.
pub fn add_read_waker(fd: RawFd, waker: Waker) -> Result<PendingWaker> {
    if use_uring() {
        uring_executor::add_read_waker(fd, waker)
            .map(PendingWaker::new)
            .map_err(Error::URingExecutor)
    } else {
        fd_executor::add_read_waker(fd, waker)
            .map(PendingWaker::new)
            .map_err(Error::FdExecutor)
    }
}

/// Tells the waking system to wake `waker` when `fd` becomes writable.
/// The 'fd' must be fully owned by the future adding the waker, and must not be closed until the
/// next time the future is polled. If the fd is closed, there is a race where another FD can be
/// opened on top of it causing the next poll to access the new target file.
/// Returns a `PendingWaker` that can be used to cancel the waker before it completes.
pub fn add_write_waker(fd: RawFd, waker: Waker) -> Result<PendingWaker> {
    if use_uring() {
        uring_executor::add_write_waker(fd, waker)
            .map(PendingWaker::new)
            .map_err(Error::URingExecutor)
    } else {
        fd_executor::add_write_waker(fd, waker)
            .map(PendingWaker::new)
            .map_err(Error::FdExecutor)
    }
}

/// Adds a new top level future to the Executor.
/// These futures must return `()`, indicating they are intended to create side-effects only.
pub fn add_future(future: Pin<Box<dyn Future<Output = ()>>>) -> Result<()> {
    if use_uring() {
        uring_executor::add_future(future).map_err(Error::URingExecutor)
    } else {
        fd_executor::add_future(future).map_err(Error::FdExecutor)
    }
}

// test function to check the number of pending futures
pub fn pending_ops() -> usize {
    if use_uring() {
        uring_executor::pending_ops()
    } else {
        fd_executor::pending_ops()
    }
}

#[cfg(test)]
mod test {
    use std::cell::RefCell;
    use std::fs::File;
    use std::future::Future;
    use std::os::unix::io::AsRawFd;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll};

    use futures::future::Either;

    use super::*;
    use crate::PendingWaker;

    struct TestFut {
        f: File,
        pending_waker: Option<PendingWaker>,
    }

    impl TestFut {
        fn new(f: File) -> TestFut {
            TestFut {
                f,
                pending_waker: None,
            }
        }
    }

    impl Future for TestFut {
        type Output = u64;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            if self.pending_waker.is_none() {
                println!("pend");
                self.pending_waker =
                    Some(crate::add_read_waker(self.f.as_raw_fd(), cx.waker().clone()).unwrap());
            }
            Poll::Pending
        }
    }

    impl Drop for TestFut {
        fn drop(&mut self) {
            println!("drop test fut");
        }
    }

    // Because of the global uring vs fd setting these all need to be in one test so they don't
    // fight over the configuration.
    #[test]
    fn test_it() {
        loop {
            async fn do_test() {
                let (r, _w) = sys_util::pipe(true).unwrap();
                let done = Box::pin(async { 5usize });
                let pending = Box::pin(TestFut::new(r));
                match futures::future::select(pending, done).await {
                    Either::Right((5, pending)) => std::mem::drop(pending),
                    _ => panic!("unexpected select result"),
                }
                // test that dropping the incomplete future removed the waker.
                assert_eq!(0, pending_ops());
            }

            let fut = do_test();

            crate::run_one(Box::pin(fut)).unwrap();

            // Example of starting the framework and running a future:
            async fn my_async(x: Rc<RefCell<u64>>) {
                x.replace(4);
            }

            let x = Rc::new(RefCell::new(0));
            crate::run_one(Box::pin(my_async(x.clone()))).unwrap();
            assert_eq!(*x.borrow(), 4);

            // If we tested FD then we either already tested both, or uring isn't supported. Either way
            // we've done all we can do.
            if !use_uring() {
                break;
            }
            // otherwise we tested uring,force fd and test again.
            crate::executor_commands::TEST_DISABLE_URING.store(true, Ordering::Relaxed);
        }
    }
}
