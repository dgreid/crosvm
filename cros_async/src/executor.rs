// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Waker;
use std::task::{Context, Poll};

use crate::waker::create_waker;

/// Represents a future executor that can be run. Implementers of the trait will take a list of
/// futures and poll them until completed.
pub trait Executor {
    type Output;
    /// Run the executor, this will return once the exit crieteria is met. The exit criteria is
    /// specified when the executor is created, for example running until all futures are complete.
    fn run(&mut self) -> Self::Output;
}

// Couples a future owned by the executor with a flag that indicates the future is ready to be
// polled. Futures will start with the flag set. After blocking by returning `Poll::Pending`, the
// flag will be false until the waker is triggered and sets the flag to true, signalling the
// executor to poll the future again.
pub(crate) struct ExecutableFuture<T> {
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
// different collections of crates and different termination behavior. For example, one list can
// decide to exit after the first trait completes, others can wait until all are complete.
pub(crate) trait FutureList {
    type Output;

    // Return a mutable reference to the list of futures that can be added or removed from this
    // List.
    fn futures_mut(&mut self) -> &mut UnitFutures;
    // Polls all futures that are ready.
    fn poll_results(&mut self) -> Option<Self::Output>;
    // Returns true if any future in the list is ready to be polled.
    fn any_ready(&self) -> bool;
}

// `UnitFutures` is the simplest implementor of `FutureList`. It runs all futures added to it until
// there are none left to poll. The futures must all return `()`.
pub(crate) struct UnitFutures {
    futures: VecDeque<ExecutableFuture<()>>,
}

impl UnitFutures {
    // Creates a new, empty list of futures.
    pub fn new() -> UnitFutures {
        UnitFutures {
            futures: VecDeque::new(),
        }
    }

    // Adds a future to the list of futures to be polled.
    pub fn append(&mut self, futures: &mut VecDeque<ExecutableFuture<()>>) {
        self.futures.append(futures);
    }

    // Polls all futures that are ready to be polled. Removes any futures thatIndicate they are
    // completed.
    pub fn poll_all(&mut self) {
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
