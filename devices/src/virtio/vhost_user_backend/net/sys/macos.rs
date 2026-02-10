// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS stubs for vhost-user net backend.
//!
//! macOS doesn't have Linux-style TAP devices, so the vhost-user net backend
//! is not fully functional. This module provides stub implementations that
//! return errors when trying to use TAP-based networking.

use anyhow::bail;
use argh::FromArgs;
use cros_async::IntoAsync;
use net_util::TapT;
use vm_memory::GuestMemory;

use crate::virtio;
use crate::virtio::vhost_user_backend::net::NetBackend;

/// Platform specific impl of VhostUserDevice::start_queue.
///
/// On macOS, this is a stub that returns an error since TAP devices
/// are not available.
pub(in crate::virtio::vhost_user_backend::net) fn start_queue<T: 'static + TapT + IntoAsync>(
    _backend: &mut NetBackend<T>,
    _idx: usize,
    _queue: virtio::Queue,
    _mem: GuestMemory,
) -> anyhow::Result<()> {
    bail!("vhost-user net backend is not supported on macOS (no TAP device support)")
}

#[derive(FromArgs)]
#[argh(subcommand, name = "net")]
/// Net device (not supported on macOS)
pub struct Options {
    #[argh(option, arg_name = "SOCKET_PATH,IP_ADDR,NET_MASK,MAC_ADDR")]
    /// TAP device config (not supported on macOS)
    device: Vec<String>,
    #[argh(option, arg_name = "SOCKET_PATH,TAP_FD")]
    /// TAP FD with a socket path (not supported on macOS)
    tap_fd: Vec<String>,
    #[argh(option, arg_name = "SOCKET_PATH,TAP_NAME")]
    /// TAP NAME with a socket path (not supported on macOS)
    tap_name: Vec<String>,
    #[argh(switch, arg_name = "MRG_RXBUF")]
    /// whether enable MRG_RXBUF feature
    mrg_rxbuf: bool,
}

/// Starts a vhost-user net device.
///
/// On macOS, this returns an error since TAP devices are not available.
pub fn start_device(_opts: Options) -> anyhow::Result<()> {
    bail!("vhost-user net backend is not supported on macOS (no TAP device support)")
}
