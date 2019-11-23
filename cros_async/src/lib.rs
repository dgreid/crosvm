// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An Executor and future combinators based on operations that block on system file desrciptors.
//!
//! This crate is meant to be used with the `futures-rs` crate that provides combinators and
//! utility functions to combine futures. All futures will run until they block on a file descriptor
//! becoming readable or writable. Facilities are provided to register future wakers based on such
//! events.
//!
//! # Running top-level futures.
//!
//! Use helper functions based the desired behavior of your application.
//!
//! ## Completing several futures.
//!
//! If there are several top level that all need to be completed, use the "complete" family of
//! executor constructors. These return an [`Executor`](trait.Executor.html) whose
//! `run` function will retun only once all the futures passed to it have completed. These functions
//! are inspired by the `join` function from futures-rs, but build to be run inside an FD based
//! executor and to poll only when necessary. See the docs for [`complete2`](fn.complete2.html),
//! [`complete3`](fn.complete3.html), [`complete4`](fn.complete4.html), and
//! [`complete5`](fn.complete5.html).
//!
//! ## Many futures all returning `()`
//!
//! It there are futures that produce side effects and return `()`, the
//! [`empty_executor`](fn.empty_executor.html) function provides an Executor that runs futures
//! reutrning `()`. Futures are added using the [`add_future`](fn.add_future.html)
//! function.
//!
//! # Implementing new FD-based futures.
//!
//! When building futures to be run in an `FdExecutor` framework, use the following helper functions
//! to perform common tasks:
//!
//! [`add_read_waker`](fn.add_read_waker.html) - Used to associate a provided FD becoming readable
//! with the future being woken. Used before returning Poll::Pending from a future that waits until
//! an FD is writable.
//!
//! [`add_write_waker`](fn.add_write_waker.html) - Used to associate a provided FD becoming writable
//! with the future being woken. Used before returning Poll::Pending from a future that waits until
//! an FD is readable.
//!
//! [`add_future`](fn.add_future.html) - Used to add a new future to the top-level list of running
//! futures.

mod combinations;
mod executor;
mod fd_executor;
mod select;
mod waker;

pub use executor::Executor;
pub use fd_executor::{add_future, add_read_waker, add_write_waker};
pub use select::SelectResult;

use executor::UnitFutures;
use fd_executor::FdExecutor;
use std::future::Future;

/// Creates an empty FdExecutor that can have futures returning `()` added via
/// [`add_future`](fn.add_future.html).
pub fn empty_executor() -> impl Executor {
    FdExecutor::new(UnitFutures::new())
}

// Select helpers to run until any future completes.

/// Creates an executor that runs the two given futures until one completes, returning a tuple
/// containing the result of the finished future and the still pending future.
///
///  # Example
///
///    ```
///    use cros_async::{select2, SelectResult, Executor};
///    use futures::future::pending;
///
///    let first = async {5};
///    let second = async {let () = pending().await;};
///    match select2(first, second) {
///        (SelectResult::Finished(5), SelectResult::Pending(_second)) => (),
///        _ => panic!("Select didn't return the first future"),
///    };
///    ```
pub fn select2<F1: Future, F2: Future>(f1: F1, f2: F2) -> (SelectResult<F1>, SelectResult<F2>) {
    FdExecutor::new(select::Select2::new(f1, f2)).run()
}

/// Creates an executor that runs the three given futures until one or more completes, returning a
/// tuple containing the result of the finished future(s) and the still pending future(s).
///
///  # Example
///
///    ```
///    use cros_async::{select3, SelectResult, Executor};
///    use futures::future::pending;
///
///    let first = async {4};
///    let second = async {let () = pending().await;};
///    let third = async {5};
///    match select3(first, second, third) {
///        (SelectResult::Finished(4), SelectResult::Pending(_second), SelectResult::Finished(5)) => (),
///        _ => panic!("Select didn't return the futures"),
///    };
///    ```
pub fn select3<F1: Future, F2: Future, F3: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
) -> (SelectResult<F1>, SelectResult<F2>, SelectResult<F3>) {
    FdExecutor::new(select::Select3::new(f1, f2, f3)).run()
}

/// Creates an executor that runs the four given futures until one or more completes, returning a
/// tuple containing the result of the finished future(s) and the still pending future(s).
///
///  # Example
///
///    ```
///    use cros_async::{select4, SelectResult, Executor};
///    use futures::future::pending;
///
///    let first = async {4};
///    let second = async {let () = pending().await;};
///    let third = async {5};
///    let fourth = async {let () = pending().await;};
///    match select4(first, second, third, fourth) {
///        (SelectResult::Finished(4), SelectResult::Pending(_second),
///         SelectResult::Finished(5), SelectResult::Pending(_fourth)) => (),
///        _ => panic!("Select didn't return the futures"),
///    };
///    ```
pub fn select4<F1: Future, F2: Future, F3: Future, F4: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
    f4: F4,
) -> (
    SelectResult<F1>,
    SelectResult<F2>,
    SelectResult<F3>,
    SelectResult<F4>,
) {
    FdExecutor::new(select::Select4::new(f1, f2, f3, f4)).run()
}

/// Creates an executor that runs the five given futures until one or more completes, returning a
/// tuple containing the result of the finished future(s) and the still pending future(s).
///
///  # Example
///
///    ```
///    use cros_async::{select5, SelectResult, Executor};
///    use futures::future::pending;
///
///    let first = async {4};
///    let second = async {let () = pending().await;};
///    let third = async {5};
///    let fourth = async {let () = pending().await;};
///    let fifth = async {6};
///    match select5(first, second, third, fourth, fifth) {
///        (SelectResult::Finished(4), SelectResult::Pending(_second),
///         SelectResult::Finished(5), SelectResult::Pending(_fourth),
///         SelectResult::Finished(6)) => (),
///        _ => panic!("Select didn't return the futures"),
///    };
///    ```
pub fn select5<F1: Future, F2: Future, F3: Future, F4: Future, F5: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
    f4: F4,
    f5: F5,
) -> (
    SelectResult<F1>,
    SelectResult<F2>,
    SelectResult<F3>,
    SelectResult<F4>,
    SelectResult<F5>,
) {
    FdExecutor::new(select::Select5::new(f1, f2, f3, f4, f5)).run()
}

// Combination helpers to run until all futures are complete.

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
///    assert_eq!(complete2(first, second), (5,6));
///    ```
pub fn complete2<F1: Future, F2: Future>(f1: F1, f2: F2) -> (F1::Output, F2::Output) {
    FdExecutor::new(combinations::Complete2::new(f1, f2)).run()
}

/// Creates an executor that runs the three given futures to completion, returning a tuple of the
/// outputs each yields.
///
///  # Example
///
///    ```
///    use cros_async::{complete3, Executor};
///
///    assert_eq!(complete3(async{1}, async{2}, async{3}), (1,2,3));
///    ```
pub fn complete3<F1: Future, F2: Future, F3: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
) -> (F1::Output, F2::Output, F3::Output) {
    FdExecutor::new(combinations::Complete3::new(f1, f2, f3)).run()
}

/// Creates an executor that runs the four given futures to completion, returning a tuple of the
/// outputs each yields.
///
///  # Example
///
///    ```
///    use cros_async::{complete4, Executor};
///
///    assert_eq!(complete4(async{1}, async{2}, async{3}, async{4}), (1,2,3,4));
///    ```
pub fn complete4<F1: Future, F2: Future, F3: Future, F4: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
    f4: F4,
) -> (F1::Output, F2::Output, F3::Output, F4::Output) {
    FdExecutor::new(combinations::Complete4::new(f1, f2, f3, f4)).run()
}

/// Creates an executor that runs the five given futures to completion, returning a tuple of the
/// outputs each yields.
///
///  # Example
///
///    ```
///    use cros_async::{complete5, Executor};
///
///    assert_eq!(complete5(async{1}, async{2}, async{3}, async{4}, async{5}), (1,2,3,4,5));
///    ```
pub fn complete5<F1: Future, F2: Future, F3: Future, F4: Future, F5: Future>(
    f1: F1,
    f2: F2,
    f3: F3,
    f4: F4,
    f5: F5,
) -> (F1::Output, F2::Output, F3::Output, F4::Output, F5::Output) {
    FdExecutor::new(combinations::Complete5::new(f1, f2, f3, f4, f5)).run()
}
