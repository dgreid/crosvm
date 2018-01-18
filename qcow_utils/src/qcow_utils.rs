// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Exported interface to basic qcow functionality to be used from C.

extern crate libc;
extern crate qcow;

use std::ffi::CStr;
use std::fs::OpenOptions;
use std::os::raw::{c_char, c_int, c_ulong};
use libc::EINVAL;

use qcow::QcowHeader;

#[no_mangle]
pub extern fn create_qcow_with_size(path: *const c_char,
                                    virtual_size: c_ulong) -> c_int {
    let c_str = unsafe {
        // NULL pointers are checked, but this will access any other invalid pointer passed from C
        // code. It's the caller's responsibility to pass a valid pointer.
        if path.is_null() {
            return -EINVAL;
        }
        CStr::from_ptr(path)
    };
    let file_path = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return -EINVAL,
    };

    let h = QcowHeader::create_for_size(virtual_size as u64);
    let mut file = match OpenOptions::new()
                            .read(true)
                            .create(true)
                            .open(file_path) {
        Ok(f) => f,
        Err(_) => return -1,
    };

    match h.write_to(&mut file) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
