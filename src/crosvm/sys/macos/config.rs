// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::bail;
use anyhow::Context;
#[cfg(feature = "net")]
use devices::virtio::NetParametersMode;
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

pub fn validate_config(
    #[allow(unused_variables)] cfg: &mut Config,
) -> std::result::Result<(), String> {
    #[cfg(feature = "net")]
    for (index, net) in cfg.net.iter().enumerate() {
        if !matches!(&net.mode, NetParametersMode::SocketVmnet { .. }) {
            return Err(format!(
                "net device {index}: macOS only supports socket-vmnet networking"
            ));
        }
        if !matches!(net.vq_pairs, None | Some(1)) {
            return Err(format!(
                "net device {index}: macOS socket-vmnet supports only one queue pair"
            ));
        }
        if net.packed_queue {
            return Err(format!(
                "net device {index}: packed queues are not supported on macOS"
            ));
        }
        if net.mrg_rxbuf {
            return Err(format!(
                "net device {index}: mergeable receive buffers are not supported on macOS"
            ));
        }
        if net.pci_address.is_some() {
            return Err(format!(
                "net device {index}: pci-address is not supported for virtio-mmio devices on macOS"
            ));
        }
    }

    Ok(())
}

#[cfg(all(test, feature = "net"))]
mod tests {
    use devices::virtio::NetParameters;

    use super::*;

    fn socket_vmnet_parameters() -> NetParameters {
        NetParameters {
            mode: NetParametersMode::SocketVmnet {
                socket_vmnet: PathBuf::from("/var/run/socket_vmnet"),
                mac: None,
            },
            vq_pairs: None,
            packed_queue: false,
            pci_address: None,
            mrg_rxbuf: false,
        }
    }

    fn config_with_net(net: NetParameters) -> Config {
        let mut cfg = Config::default();
        cfg.net.push(net);
        cfg
    }

    #[test]
    fn accepts_supported_socket_vmnet_config() {
        let mut cfg = config_with_net(socket_vmnet_parameters());

        assert_eq!(validate_config(&mut cfg), Ok(()));

        let mut net = socket_vmnet_parameters();
        net.vq_pairs = Some(1);
        assert_eq!(validate_config(&mut config_with_net(net)), Ok(()));
    }

    #[test]
    fn rejects_unsupported_socket_vmnet_options() {
        let mut net = socket_vmnet_parameters();
        net.vq_pairs = Some(2);
        assert!(validate_config(&mut config_with_net(net)).is_err());

        let mut net = socket_vmnet_parameters();
        net.packed_queue = true;
        assert!(validate_config(&mut config_with_net(net)).is_err());

        let mut net = socket_vmnet_parameters();
        net.mrg_rxbuf = true;
        assert!(validate_config(&mut config_with_net(net)).is_err());

        let mut net = socket_vmnet_parameters();
        net.pci_address = Some(Default::default());
        assert!(validate_config(&mut config_with_net(net)).is_err());
    }

    #[test]
    fn rejects_non_socket_vmnet_mode() {
        let mut cfg = config_with_net(NetParameters {
            mode: NetParametersMode::TapName {
                tap_name: "tap0".to_owned(),
                mac: None,
            },
            vq_pairs: None,
            packed_queue: false,
            pci_address: None,
            mrg_rxbuf: false,
        });

        assert!(validate_config(&mut cfg).is_err());
    }
}
