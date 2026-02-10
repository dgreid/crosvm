// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io;

use remain::sorted;
use thiserror::Error as ThisError;

#[sorted]
#[derive(ThisError, Debug)]
pub enum AsyncErrorSys {
    #[error("Kqueue executor error: {0}")]
    KqueueExecutor(#[from] super::kqueue_executor::Error),
    #[error("Kqueue source error: {0}")]
    KqueueSource(#[from] super::kqueue_source::Error),
}

impl From<AsyncErrorSys> for io::Error {
    fn from(err: AsyncErrorSys) -> Self {
        match err {
            AsyncErrorSys::KqueueExecutor(e) => e.into(),
            AsyncErrorSys::KqueueSource(e) => e.into(),
        }
    }
}
