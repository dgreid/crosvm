// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MacOS GPU stubs.
//!
//! GPU rendering is not currently supported on macOS. These are stub
//! implementations to allow the codebase to compile.

use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

use crate::gpu::DisplayModeTrait;

/// MacOS mouse mode stub
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, FromKeyValues)]
#[serde(rename_all = "snake_case")]
pub enum MacMouseMode {
    /// Sends multi-touch events to the guest.
    Touchscreen,
}

/// MacOS display mode stub
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacDisplayMode {
    Windowed(u32, u32),
}

impl DisplayModeTrait for MacDisplayMode {
    fn get_window_size(&self) -> (u32, u32) {
        match self {
            Self::Windowed(width, height) => (*width, *height),
        }
    }

    fn get_virtual_display_size(&self) -> (u32, u32) {
        self.get_window_size()
    }

    fn get_virtual_display_size_4k_uhd(&self, _is_4k_uhd_enabled: bool) -> (u32, u32) {
        self.get_virtual_display_size()
    }
}
