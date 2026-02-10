// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS stubs for vhost-user FS backend.
//!
//! The FS backend requires minijail for sandboxing, which is not available
//! on macOS. This module provides stub implementations.

use anyhow::bail;

use crate::virtio::vhost_user_backend::fs::Options;

/// Starts a vhost-user FS device.
///
/// On macOS, this returns an error since the FS backend requires minijail.
pub fn start_device(_opts: Options) -> anyhow::Result<()> {
    bail!("vhost-user FS backend is not supported on macOS (requires minijail)")
}
