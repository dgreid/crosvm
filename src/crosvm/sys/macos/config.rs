// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::bail;
use anyhow::Context;
use devices::SerialParameters;
use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::from_key_values;
use serde_keyvalue::FromKeyValues;

use crate::crosvm::config::Config;

/// Hypervisor backends available on macOS
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromKeyValues)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum HypervisorKind {
    /// Apple Hypervisor.framework (arm64 only)
    Hvf,
}

/// A directory to be shared with the VM via virtiofs.
pub struct SharedDir {
    pub src: PathBuf,
    pub tag: String,
    pub fs_cfg: devices::virtio::fs::Config,
}

impl FromStr for SharedDir {
    type Err = anyhow::Error;

    fn from_str(param: &str) -> Result<Self, Self::Err> {
        // Format: src:tag[:key=value:...]
        // Supported keys: timeout, cache, writeback
        let mut components = param.split(':');
        let src = PathBuf::from(
            components
                .next()
                .context("missing source path for `shared-dir`")?,
        );
        let tag = components
            .next()
            .context("missing tag for `shared-dir`")?
            .to_owned();

        if !src.is_dir() {
            bail!("source path for `shared-dir` must be a directory");
        }

        let mut shared_dir = SharedDir {
            src,
            tag,
            fs_cfg: Default::default(),
        };

        let type_opts: Vec<&str> = components.collect();
        if !type_opts.is_empty() {
            shared_dir.fs_cfg = from_key_values(&type_opts.join(","))
                .map_err(|e| anyhow::anyhow!("failed to parse fs config: {e}"))?;
        }

        Ok(shared_dir)
    }
}

// Doesn't do anything on macOS.
pub fn check_serial_params(_serial_params: &SerialParameters) -> Result<(), String> {
    Ok(())
}

pub fn validate_config(_cfg: &mut Config) -> std::result::Result<(), String> {
    Ok(())
}
