// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use devices::SerialParameters;
use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

use crate::crosvm::config::Config;

/// Hypervisor backends available on macOS
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromKeyValues)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum HypervisorKind {
    /// Apple Hypervisor.framework (arm64 only)
    Hvf,
}

// Doesn't do anything on macOS.
pub fn check_serial_params(_serial_params: &SerialParameters) -> Result<(), String> {
    Ok(())
}

pub fn validate_config(_cfg: &mut Config) -> std::result::Result<(), String> {
    Ok(())
}
