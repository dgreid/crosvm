// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::Event;

use crate::AsyncResult;
use crate::EventAsync;
use crate::Executor;

impl EventAsync {
    /// Create a new EventAsync from an Event.
    pub fn new(event: Event, ex: &Executor) -> AsyncResult<EventAsync> {
        ex.async_from(event)
            .map(|io_source| EventAsync { io_source })
    }

    /// Gets the next value from the event.
    ///
    /// On macOS, Events are backed by pipes that write 1 byte per signal.
    /// This drains the pipe and returns the count of signals received.
    pub async fn next_val(&self) -> AsyncResult<u64> {
        // Wait for the pipe to be readable
        self.io_source.wait_readable().await?;

        // Read whatever is available (each signal writes 1 byte)
        let (n, _) = self
            .io_source
            .read_to_vec(None, vec![0u8; 64])
            .await?;

        // Return the number of signals (bytes read)
        Ok(n as u64)
    }
}
