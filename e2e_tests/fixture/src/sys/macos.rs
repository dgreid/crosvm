// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS-specific VM test fixture implementation.
//!
//! This is a minimal stub implementation. Full e2e tests with the delegate
//! communication pattern are not yet supported on macOS. See
//! `e2e_tests/tests/macos_boot_tests.rs` for self-contained macOS boot tests.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::bail;
use anyhow::Result;
use delegate::wire_format::DelegateMessage;
use serde_json::StreamDeserializer;

use crate::vm::Config;

pub(crate) type SerialArgs = Path;

/// Returns the name of crosvm binary.
pub fn binary_name() -> &'static str {
    "crosvm"
}

pub struct TestVmSys {
    #[allow(dead_code)]
    pub from_guest_reader: Arc<
        Mutex<
            StreamDeserializer<
                'static,
                serde_json::de::IoRead<BufReader<std::fs::File>>,
                DelegateMessage,
            >,
        >,
    >,
    #[allow(dead_code)]
    pub to_guest: Arc<Mutex<File>>,
    #[allow(dead_code)]
    pub control_socket_path: PathBuf,
    pub process: Option<Child>,
}

impl TestVmSys {
    pub fn check_rootfs_file(_rootfs_path: &Path) {
        // On macOS, we don't have O_DIRECT, so skip this check
    }

    pub fn new_generic<F>(_f: F, _cfg: Config, _sudo: bool) -> Result<TestVmSys>
    where
        F: FnOnce(&mut Command, &Path, &Config) -> Result<()>,
    {
        bail!("Full e2e test fixture is not yet implemented for macOS. Use macos_boot_tests.rs instead.")
    }

    pub fn append_config_args(
        _command: &mut Command,
        _test_dir: &Path,
        _cfg: &Config,
    ) -> Result<()> {
        bail!("Full e2e test fixture is not yet implemented for macOS. Use macos_boot_tests.rs instead.")
    }

    pub fn crosvm_command(
        &self,
        _command: &str,
        _args: Vec<String>,
        _sudo: bool,
    ) -> Result<Vec<u8>> {
        bail!("Full e2e test fixture is not yet implemented for macOS. Use macos_boot_tests.rs instead.")
    }
}
