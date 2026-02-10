// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use anyhow::Context;
use base::syslog;
use base::syslog::LogArgs;
use base::syslog::LogConfig;
use base::warn;

use crate::crosvm::sys::cmdline::Commands;
use crate::crosvm::sys::cmdline::DeviceSubcommand;
use crate::CommandStatus;
use crate::Config;

pub(crate) fn start_device(_command: DeviceSubcommand) -> anyhow::Result<()> {
    // MacOS does not support jailed device processes currently
    // This would need to be implemented if device sandboxing is added
    anyhow::bail!("Device subcommands not supported on macOS")
}

pub(crate) fn cleanup() {
    // On macOS, without process jailing, there are no child device processes to clean up.
    // If jailing is added in the future, this should wait for child processes similar to Linux.
    warn!("cleanup called on macOS - no child processes to clean up");
}

pub(crate) fn run_command(_command: Commands, _log_args: LogArgs) -> anyhow::Result<()> {
    // MacOS does not have Commands enum variants currently
    // This would be extended if platform-specific commands are added
    anyhow::bail!("Platform-specific commands not supported on macOS")
}

pub(crate) fn init_log(log_config: LogConfig, _cfg: &Config) -> anyhow::Result<()> {
    if let Err(e) = syslog::init_with(log_config) {
        eprintln!("failed to initialize syslog: {e}");
        return Err(anyhow!("failed to initialize syslog: {}", e));
    }
    Ok(())
}

pub(crate) fn error_to_exit_code(_res: &std::result::Result<CommandStatus, anyhow::Error>) -> i32 {
    1
}
