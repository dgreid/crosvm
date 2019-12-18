// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The executor runs all given futures to completion. Futures register wakers associated with file
//! descriptors. The wakers will be called when the FD becomes readable or writable depending on
//! the situation.
//!
//! `FdExecutor` is meant to be used with the `futures-rs` crate that provides combinators and
//! utility functions to combine futures.
//!
//! # Example of starting the framework and running a future:
//!
//! ```
//! use cros_async::Executor;
//! async fn my_async() {
//!     // Insert async code here.
//! }
//!
//! let mut ex = cros_async::empty_executor();
//! cros_async::add_future(Box::pin(my_async()));
//! ex.run();
//! ```

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::task::{RawWaker, RawWakerVTable, Waker};

use sys_util::{PollContext, WatchingEvents};

// Temporary vectors of new additions to the executor.
// File descriptor wakers that are added during poll calls.
thread_local!(static NEW_FDS: RefCell<Vec<(SavedFd, Waker, WatchingEvents)>> =
              RefCell::new(Vec::new()));
// Top level futures that are added during poll calls.
thread_local!(static NEW_FUTURES: RefCell<VecDeque<ExecutableFuture<()>>> =
              RefCell::new(VecDeque::new()));

/// Tells the waking system to wake `waker` when `fd` becomes readable.
pub fn add_read_waker(fd: &dyn AsRawFd, waker: Waker) {
    NEW_FDS.with(|new_fds| {
        let mut new_fds = new_fds.borrow_mut();
        new_fds.push((
            SavedFd(fd.as_raw_fd()),
            waker,
            WatchingEvents::empty().set_read(),
        ));
    });
}

/// Tells the waking system to wake `waker` when `fd` becomes writable.
pub fn add_write_waker(fd: &dyn AsRawFd, waker: Waker) {
    NEW_FDS.with(|new_fds| {
        let mut new_fds = new_fds.borrow_mut();
        new_fds.push((
            SavedFd(fd.as_raw_fd()),
            waker,
            WatchingEvents::empty().set_write(),
        ));
    });
}

/// Adds a new top level future to the Executor.
/// These futures must return `()`, indicating they are intended to create side-effects only.
pub fn add_future(future: Pin<Box<dyn Future<Output = ()>>>) {
    NEW_FUTURES.with(|new_futures| {
        let mut new_futures = new_futures.borrow_mut();
        new_futures.push_back(ExecutableFuture::new(future, AtomicBool::new(true)));
    });
}

// Couples a future owned by the executor with a flag that indicates the future is ready to be
// polled. Futures will start with the flag set. After blocking by returning `Poll::Pending`, the
// flag will be false until the waker is triggers and sets the flag to true, signalling the future
// can be polled again.
struct ExecutableFuture<T> {
    future: Pin<Box<dyn Future<Output = T>>>,
    needs_poll: AtomicBool,
}

impl<T> ExecutableFuture<T> {
    pub fn new(
        future: Pin<Box<dyn Future<Output = T>>>,
        needs_poll: AtomicBool,
    ) -> ExecutableFuture<T> {
        ExecutableFuture { future, needs_poll }
    }
}

trait FutureList {
    type Output;
    fn push_back(&mut self, future: ExecutableFuture<()>);
    fn append(&mut self, futures: &mut VecDeque<ExecutableFuture<()>>);
    fn results(&mut self) -> Option<Self::Output>;
    fn any_ready(&self) -> bool;

    /// for each future that is ready:
    ///  poll it
    ///  remove it if ready
    fn run_remove_done(&mut self);
}

struct UnitFutures {
    futures: VecDeque<ExecutableFuture<()>>,
}

impl UnitFutures {
    pub fn new() -> UnitFutures {
        UnitFutures {
            futures: VecDeque::new(),
        }
    }
}

impl FutureList for UnitFutures {
    type Output = ();

    fn push_back(&mut self, future: ExecutableFuture<()>) {
        self.futures.push_back(future);
    }
    fn append(&mut self, futures: &mut VecDeque<ExecutableFuture<()>>) {
        self.futures.append(futures);
    }
    fn results(&mut self) -> Option<Self::Output> {
        if self.futures.is_empty() {
            Some(())
        } else {
            None
        }
    }
    fn any_ready(&self) -> bool {
        self.futures
            .iter()
            .any(|fut| fut.needs_poll.load(Ordering::Relaxed))
    }
    fn run_remove_done(&mut self) {
        let to_remove: Vec<usize> = self
            .futures
            .iter_mut()
            .enumerate()
            .filter_map(|(i, fut)| {
                if fut.needs_poll.swap(false, Ordering::Relaxed) {
                    if let Poll::Ready(_) = poll_one(fut) {
                        return Some(i);
                    }
                }
                None
            })
            .collect();
        for i in to_remove.into_iter() {
            self.futures.remove(i);
        }
    }
}

// Polls one future and returns the result.
// Covers setting up the waker and context before calling the future.
fn poll_one<O>(fut: &mut ExecutableFuture<O>) -> Poll<O> {
    // Safe because a valid pointer is passed to `create_waker` and the valid result is
    // passed to `Waker::from_raw`.
    let waker = unsafe {
        let raw_waker = create_waker(&fut.needs_poll as *const _ as *const _);
        Waker::from_raw(raw_waker)
    };
    let mut ctx = Context::from_waker(&waker);
    let f = fut.future.as_mut();
    f.poll(&mut ctx)
}

pub trait Executor {
    type Output;
    /// Run the executor, this will return once the exit crieteria is met. The exit criteria is
    /// specified when the executor is created, for example running until all futures are complete.
    /// exit criteria.
    fn run(&mut self) -> Self::Output;
}

/// Runs futures to completion on a single thread. Futures are allowed to block on file descriptors
/// only. Futures can only block on FDs becoming readable or writable. `FdExecutor` is meant to be
/// used where a poll or select loop would be used otherwise.
struct FdExecutor<T: FutureList> {
    futures: T,
    poll_ctx: PollContext<u64>,
    token_map: BTreeMap<u64, (SavedFd, Waker)>,
    next_token: u64, // Next token for adding to the poll context.
}

impl<T: FutureList> Executor for FdExecutor<T> {
    type Output = T::Output;

    fn run(&mut self) -> Self::Output {
        self.run_all()
    }
}

impl<T: FutureList> FdExecutor<T> {
    /// Create a new executor.
    pub fn new(futures: T) -> FdExecutor<T> {
        FdExecutor {
            futures,
            poll_ctx: PollContext::new().unwrap(),
            token_map: BTreeMap::new(),
            next_token: 0,
        }
    }

    // Run the executor, If 'exit_any' is true, 'run_all' returns after any future completes. If
    // 'exit_any' is false, `run_all` only returns after all futures have completed.
    fn run_all(&mut self) -> T::Output {
        loop {
            self.futures.run_remove_done();

            // Add any new futures and wakers to the lists.
            NEW_FUTURES.with(|new_futures| {
                let mut new_futures = new_futures.borrow_mut();
                self.futures.append(&mut new_futures);
            });

            NEW_FDS.with(|new_fds| {
                let mut new_fds = new_fds.borrow_mut();
                for (saved_fd, waker, events) in new_fds.drain(..) {
                    self.add_waker(saved_fd, waker, events);
                }
            });

            if let Some(output) = self.futures.results() {
                return output;
            }

            // If no futures are ready, sleep until a waker is signaled.
            if !self.futures.any_ready() {
                self.wait_wake_event();
            }
        }
    }

    // Adds an fd that, when signaled, will trigger the given waker.
    fn add_waker(&mut self, fd: SavedFd, waker: Waker, events: WatchingEvents) {
        while self.token_map.contains_key(&self.next_token) {
            self.next_token += 1;
        }
        self.poll_ctx
            .add_fd_with_events(&fd, events, self.next_token)
            .unwrap();
        let next_token = self.next_token;
        self.token_map.insert(next_token, (fd, waker));
    }

    // Waits until one of the FDs is readable and wakes the associated waker.
    fn wait_wake_event(&mut self) {
        let events = self.poll_ctx.wait().unwrap();
        for e in events.iter() {
            if let Some((fd, waker)) = self.token_map.remove(&e.token()) {
                self.poll_ctx.delete(&fd).unwrap();
                waker.wake_by_ref();
            }
        }
    }
}

/// Creates an empty FdExecutor that can have futures returning `()` added via `add_future`.
pub fn empty_executor() -> impl Executor {
    FdExecutor::new(UnitFutures::new())
}

// Saved FD exists becaus RawFd doesn't impl AsRawFd.
struct SavedFd(RawFd);
impl AsRawFd for SavedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// Boiler-plate for creating a waker with funciton pointers.
// This waker sets the atomic bool it is passed to true.
// The bool will be used by the executor to know which futures to poll
unsafe fn waker_drop(_: *const ()) {}
unsafe fn waker_wake(_: *const ()) {}
unsafe fn waker_wake_by_ref(data_ptr: *const ()) {
    let bool_atomic_ptr = data_ptr as *const AtomicBool;
    let bool_atomic_ref = bool_atomic_ptr.as_ref().unwrap();
    bool_atomic_ref.store(true, Ordering::Relaxed);
}
unsafe fn waker_clone(data_ptr: *const ()) -> RawWaker {
    create_waker(data_ptr)
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

unsafe fn create_waker(data_ptr: *const ()) -> RawWaker {
    RawWaker::new(data_ptr, &WAKER_VTABLE)
}
