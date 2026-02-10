// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS stubs for vhost-user snd backend.
//!
//! Audio on macOS would require CoreAudio integration, which is not yet implemented.
//! This module provides stub implementations that return errors.

use anyhow::bail;
use argh::FromArgs;
use base::RawDescriptor;

use crate::virtio::snd::parameters::Parameters;

fn snd_parameters_from_str(input: &str) -> Result<Parameters, String> {
    serde_keyvalue::from_key_values(input).map_err(|e| e.to_string())
}

#[derive(FromArgs)]
#[argh(subcommand, name = "snd")]
/// Snd device (not supported on macOS)
pub struct Options {
    #[argh(option, arg_name = "PATH", hidden_help)]
    /// deprecated - please use --socket-path instead
    socket: Option<String>,
    #[argh(option, arg_name = "PATH")]
    /// path to the vhost-user socket to bind to.
    socket_path: Option<String>,
    #[argh(option, arg_name = "FD")]
    /// file descriptor of a connected vhost-user socket.
    fd: Option<RawDescriptor>,

    #[argh(
        option,
        arg_name = "CONFIG",
        from_str_fn(snd_parameters_from_str),
        default = "Default::default()",
        long = "config"
    )]
    /// comma separated key=value pairs for setting up snd devices (not supported on macOS).
    params: Parameters,
}

/// Starts a vhost-user snd device.
///
/// On macOS, this returns an error since audio is not yet implemented.
pub fn run_snd_device(_opts: Options) -> anyhow::Result<()> {
    bail!("vhost-user snd backend is not supported on macOS")
}
