// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS stubs for vhost-user GPU backend.
//!
//! GPU on macOS would require display integration (Metal or similar),
//! which is not yet implemented. This module provides stub implementations.

use std::path::PathBuf;

use anyhow::bail;
use argh::FromArgs;
use base::RawDescriptor;

use crate::virtio::vhost_user_backend::gpu::GpuBackend;
use crate::virtio::GpuParameters;
use crate::virtio::Interrupt;

fn gpu_parameters_from_str(input: &str) -> Result<GpuParameters, String> {
    serde_json::from_str(input).map_err(|e| e.to_string())
}

fn parse_wayland_sock(s: &str) -> Result<(String, PathBuf), String> {
    // Wayland is not available on macOS
    Err(format!("Wayland is not supported on macOS: {}", s))
}

#[derive(FromArgs)]
/// GPU device (not supported on macOS)
#[argh(subcommand, name = "gpu")]
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

    #[argh(option, from_str_fn(parse_wayland_sock), arg_name = "PATH[,name=NAME]")]
    /// path to Wayland sockets (not supported on macOS)
    wayland_sock: Vec<(String, PathBuf)>,
    #[argh(option, arg_name = "PATH")]
    /// path to bridge sockets (not supported on macOS)
    resource_bridge: Vec<String>,
    #[argh(option, arg_name = "DISPLAY")]
    /// X11 display name (not supported on macOS)
    x_display: Option<String>,
    #[argh(
        option,
        from_str_fn(gpu_parameters_from_str),
        default = "Default::default()",
        arg_name = "JSON"
    )]
    /// a JSON object of virtio-gpu parameters
    params: GpuParameters,
}

impl GpuBackend {
    pub fn start_platform_workers(&mut self, _interrupt: Interrupt) -> anyhow::Result<()> {
        bail!("GPU platform workers not supported on macOS")
    }
}

/// Starts a vhost-user GPU device.
///
/// On macOS, this returns an error since GPU support is not yet implemented.
pub fn run_gpu_device(_opts: Options) -> anyhow::Result<()> {
    bail!("vhost-user GPU backend is not supported on macOS")
}
