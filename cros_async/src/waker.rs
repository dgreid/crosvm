// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{RawWaker, RawWakerVTable};

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

pub unsafe fn create_waker(data_ptr: *const ()) -> RawWaker {
    RawWaker::new(data_ptr, &WAKER_VTABLE)
}
