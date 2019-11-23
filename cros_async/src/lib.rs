// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An Executor and base futures based on operation that block on system file desrciptors.
//!
//! This crate is meant to be used with the `futures-rs` crate that provides combinators and
//! utility functions to combine futures.
//!
//! The [`FdExecutor`](crate::fd_executor::FdExecutor) runs provided futures to completion, using
//! FDs to wake the individual tasks.
//!
//! # Implementing new FD-based futures.
//!
//! When building futures to be run in an `FdExecutor` framework, use the following helper functions
//! to perform common tasks:
//!
//! `add_read_waker` - Used to associate a provided FD becoming readable with the future being
//! woken.
//!
//! `add_write_waker` - Used to associate a provided FD becoming writable with the future being
//! woken.
//!
//! `add_future` - Used to add a new future to the top-level list of running futures.
//!

mod fd_executor;

pub use fd_executor::{add_future, add_read_waker, add_write_waker, FdExecutor};
