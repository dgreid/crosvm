// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io;

use base::Tube;
use base::TubeError;
use base::TubeResult;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Executor;
use crate::IoSource;

/// Async wrapper around a Tube for sending/receiving messages.
pub struct AsyncTube {
    inner: IoSource<Tube>,
}

impl AsyncTube {
    /// Create a new AsyncTube from a Tube.
    pub fn new(ex: &Executor, tube: Tube) -> io::Result<AsyncTube> {
        Ok(AsyncTube {
            inner: ex.async_from(tube)?,
        })
    }

    /// Receive the next message from the tube.
    pub async fn next<T: DeserializeOwned>(&self) -> TubeResult<T> {
        self.inner
            .wait_readable()
            .await
            .map_err(|e| TubeError::Recv(e.into()))?;
        self.inner.as_source().recv()
    }

    /// Send a message through the tube.
    pub async fn send<T: 'static + Serialize + Send + Sync>(&self, msg: T) -> TubeResult<()> {
        self.inner.as_source().send(&msg)
    }
}

impl From<AsyncTube> for Tube {
    fn from(at: AsyncTube) -> Tube {
        at.inner.into_source()
    }
}
