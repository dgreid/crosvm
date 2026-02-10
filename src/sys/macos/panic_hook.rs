// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::env;
use std::panic;
use std::panic::PanicHookInfo;
use std::process::abort;

use base::error;

/// The intent of our panic hook is to get panic info into the syslog.
/// It will always abort on panic to ensure proper termination.
pub fn set_panic_hook() {
    let default_panic = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Log panic information
        env::set_var("RUST_BACKTRACE", "1");

        if let Some(location) = info.location() {
            error!(
                "panic occurred at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
        } else {
            error!("panic occurred at unknown location");
        }

        if let Some(msg) = info.payload().downcast_ref::<&str>() {
            error!("panic message: {}", msg);
        } else if let Some(msg) = info.payload().downcast_ref::<String>() {
            error!("panic message: {}", msg);
        }

        // Call default panic handler for backtrace
        default_panic(info);

        // Abort to ensure clean termination
        abort();
    }));
}
