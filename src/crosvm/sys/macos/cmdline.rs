// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::FromArgs;

/// MacOS device subcommands (placeholder)
#[derive(FromArgs)]
#[argh(subcommand)]
pub enum DeviceSubcommand {
    // MacOS does not support device subprocesses currently
    // Add device types here if jailed device processes are implemented
}

/// MacOS-specific commands (placeholder)
#[derive(FromArgs)]
#[argh(subcommand)]
pub enum Commands {
    // MacOS does not have platform-specific commands currently
    // Add commands here as needed
}
