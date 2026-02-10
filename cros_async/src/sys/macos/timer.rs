// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::TimerTrait;

use crate::AsyncResult;
use crate::IntoAsync;
use crate::TimerAsync;

impl<T: TimerTrait + IntoAsync> TimerAsync<T> {
    /// Wait for the timer to fire on MacOS.
    ///
    /// On macOS, the timer is backed by a kqueue with EVFILT_TIMER, which cannot be polled
    /// using read() like Linux's timerfd. We wait for the FD to become readable, which
    /// indicates the timer has fired. For oneshot timers, the event is automatically removed
    /// from the kqueue after being delivered, so no explicit consumption is needed.
    pub async fn wait_sys(&self) -> AsyncResult<()> {
        // Wait for the kqueue FD to be readable. This will wake when the timer fires.
        // For kqueue-backed timers, readability indicates the timer event is available.
        // The kqueue executor will poll the timer's kqueue FD for readability.
        self.io_source.wait_readable().await?;
        Ok(())
    }
}
