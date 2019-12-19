// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Need non-snake case so the macro can re-use type names for variables.
#![allow(non_snake_case)]

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

use futures::future::{maybe_done, MaybeDone};

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

    // Polls the future if needed and returns the result.
    // Covers setting up the waker and context before calling the future.
    fn poll(&mut self) -> Poll<T> {
        // Safe because a valid pointer is passed to `create_waker` and the valid result is
        // passed to `Waker::from_raw`.
        let waker = unsafe {
            let raw_waker = create_waker(&self.needs_poll as *const _ as *const _);
            Waker::from_raw(raw_waker)
        };
        let mut ctx = Context::from_waker(&waker);
        let f = self.future.as_mut();
        f.poll(&mut ctx)
    }
}

// Private trait used to allow one executor to behave differently depending on the implementor of
// this trait used by the executor.  Using FutureList allows the executor code to be common across
// different collections of crates and different termination behavior. For example, one List can
// decide to exit after the first trait completes, others can wait until all are complete.
trait FutureList {
    type Output;

    // Return a mutable reference to the list of futures that can be added or removed from this
    // List.
    fn futures_mut(&mut self) -> &mut UnitFutures;
    // polls all futures that are ready.
    fn poll_results(&mut self) -> Option<Self::Output>;
    fn any_ready(&self) -> bool;
}

// `UnitFutures` is the simplest implementor of `FutureList` it runs all futures added to it until
// there are none left to poll. The futures must all return `()`.
struct UnitFutures {
    futures: VecDeque<ExecutableFuture<()>>,
}

impl UnitFutures {
    pub fn new() -> UnitFutures {
        UnitFutures {
            futures: VecDeque::new(),
        }
    }
    fn append(&mut self, futures: &mut VecDeque<ExecutableFuture<()>>) {
        self.futures.append(futures);
    }
    fn poll_all(&mut self) {
        let to_remove: Vec<usize> = self
            .futures
            .iter_mut()
            .enumerate()
            .filter_map(|(i, fut)| {
                if fut.needs_poll.swap(false, Ordering::Relaxed) {
                    if let Poll::Ready(_) = fut.poll() {
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

impl FutureList for UnitFutures {
    type Output = ();

    fn futures_mut(&mut self) -> &mut UnitFutures {
        self
    }
    fn poll_results(&mut self) -> Option<Self::Output> {
        self.poll_all();
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
            // Add any new futures and wakers to the lists.
            NEW_FUTURES.with(|new_futures| {
                let mut new_futures = new_futures.borrow_mut();
                self.futures.futures_mut().append(&mut new_futures);
            });

            NEW_FDS.with(|new_fds| {
                let mut new_fds = new_fds.borrow_mut();
                for (saved_fd, waker, events) in new_fds.drain(..) {
                    self.add_waker(saved_fd, waker, events);
                }
            });

            if let Some(output) = self.futures.poll_results() {
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

/// Creates an executor that runs the two given futures to completion, returning a tuple of the
/// outputs each yields.
///
///  # Example
///
///    ```
///    use cros_async::{complete2, Executor};
///
///    let first = async {5};
///    let second = async {6};
///    let mut ex = complete2(first, second);
///    assert_eq!(ex.run(), (5,6));
///    ```
pub fn complete2<F1: Future, F2: Future>(
    f1: F1,
    f2: F2,
) -> impl Executor<Output = (F1::Output, F2::Output)> {
    FdExecutor::new(Complete2::new(f1, f2))
}
pub fn complete3<F1: Future, F2: Future, F3: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
) -> impl Executor<Output = (F1::Output, F2::Output, F3::Output)> {
    FdExecutor::new(Complete3::new(f1, f2, f3))
}
pub fn complete4<F1: Future, F2: Future, F3: Future, F4: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
    f4: F4,
) -> impl Executor<Output = (F1::Output, F2::Output, F3::Output, F4::Output)> {
    FdExecutor::new(Complete4::new(f1, f2, f3, f4))
}
pub fn complete5<F1: Future, F2: Future, F3: Future, F4: Future, F5: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
    f4: F4,
    f5: F5,
) -> impl Executor<Output = (F1::Output, F2::Output, F3::Output, F4::Output, F5::Output)> {
    FdExecutor::new(Complete5::new(f1, f2, f3, f4, f5))
}

// Saved FD exists because RawFd doesn't impl AsRawFd.
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

// Macro-generate future combinators to allow for running different numbers of top-level futures in
// an executor.
macro_rules! generate {
    ($(
        $(#[$doc:meta])*
        ($Complete:ident, <$($Fut:ident),*>),
    )*) => ($(
        $(#[$doc])*
        #[must_use = "Combinations of futures don't do anything unless run in an executor."]
        paste::item! {
            pub struct $Complete<$($Fut: Future),*> {
                added_futures: UnitFutures,
                $($Fut: MaybeDone<$Fut>,)*
                $([<$Fut _ready>]: AtomicBool,)*
            }
        }

        impl<$($Fut: Future),*> $Complete<$($Fut),*> {
            paste::item! {
                fn new($($Fut: $Fut),*) -> $Complete<$($Fut),*> {
                    $Complete {
                        added_futures: UnitFutures::new(),
                        $($Fut: maybe_done($Fut),)*
                        $([<$Fut _ready>]: AtomicBool::new(true),)*
                    }
                }
            }
        }

        impl<$($Fut: Future),*> FutureList for $Complete<$($Fut),*> {
            type Output = ($($Fut::Output),*);

            fn futures_mut(&mut self) -> &mut UnitFutures {
                &mut self.added_futures
            }

            paste::item! {
                fn poll_results(&mut self) -> Option<Self::Output> {
                    let _ = self.added_futures.poll_results();

                    let mut complete = true;
                    $(
                        let $Fut = unsafe {
                            // Safe because no future will be moved before the structure is dropped and
                            // no future can run after the structure is dropped.
                            Pin::new_unchecked(&mut self.$Fut)
                        };
                        if self.[<$Fut _ready>].swap(false, Ordering::Relaxed) {
                            let waker = unsafe {
                                let raw_waker =
                                    create_waker(&self.[<$Fut _ready>] as *const _ as *const _);
                                Waker::from_raw(raw_waker)
                            };
                            let mut ctx = Context::from_waker(&waker);
                            complete &= $Fut.poll(&mut ctx).is_ready();
                        }
                    )*

                        if complete {
                            $(
                                let $Fut = unsafe {
                                    // Safe because no future will be moved before the structure is dropped and
                                    // no future can run after the structure is dropped.
                                    Pin::new_unchecked(&mut self.$Fut)
                                };
                            )*
                            Some(($($Fut.take_output().unwrap()), *))
                        } else {
                            None
                        }
                }

    fn any_ready(&self) -> bool {
        let mut ready = self.added_futures.any_ready();
        $(
            ready |= self.[<$Fut _ready>].load(Ordering::Relaxed);
        )*
        ready
    }
            }
        }
    )*)
}

generate! {
    /// Future for the [`complete`](complete()) function.
    (Complete2, <Fut1, Fut2>),

    /// Future for the [`complete3`] function.
    (Complete3, <Fut1, Fut2, Fut3>),

    /// Future for the [`complete4`] function.
    (Complete4, <Fut1, Fut2, Fut3, Fut4>),

    /// Future for the [`complete5`] function.
    (Complete5, <Fut1, Fut2, Fut3, Fut4, Fut5>),
}
