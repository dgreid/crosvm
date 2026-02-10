// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

use crate::virtio::VirtioDevice;
use crate::VirtioDeviceArgs;
use crate::VirtioDeviceModule;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, FromKeyValues)]
#[serde(deny_unknown_fields)]
/// Configuration for a Vsock device.
///
/// Note: vsock is not currently supported on macOS.
pub struct VsockConfig {
    /// CID to be used for this vsock device.
    pub cid: u64,
}

impl VsockConfig {
    /// Create a new vsock configuration.
    pub fn new(cid: u64) -> Self {
        Self { cid }
    }
}

#[derive(Serialize, Deserialize)]
pub struct VirtioVsockModule {
    config: VsockConfig,
}

impl VirtioVsockModule {
    pub fn new(config: VsockConfig) -> Self {
        Self { config }
    }
}

impl VirtioDeviceModule for VirtioVsockModule {
    fn sort_name(&self) -> &'static str {
        "vsock"
    }

    fn create(&self, _args: &mut VirtioDeviceArgs<'_>) -> anyhow::Result<Box<dyn VirtioDevice>> {
        let _ = &self.config;
        anyhow::bail!("virtio-vsock is not supported on macOS")
    }
}
