// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helper functions for PCI hotplug.

#![deny(missing_docs)]

use std::os::unix::net::UnixStream;

use anyhow::Context;
use anyhow::Result;
use devices::virtio;
use devices::virtio::VhostUserFrontend;
use devices::BlockResourceCarrier;
use devices::HotPluggable;
use devices::IntxParameter;
use devices::NetResourceCarrier;
use devices::PciDevice;
use devices::VhostUserBlockResourceCarrier;
use devices::VirtioPciDevice;
use hypervisor::ProtectionType;
use vm_memory::GuestMemory;

use crate::crosvm::sys::linux::VirtioDeviceBuilder;

/// Builds HotPlugPci from NetResourceCarrier and NetLocalParameters.
pub fn build_hotplug_net_device(
    net_carrier_device: NetResourceCarrier,
    net_local_parameters: NetLocalParameters,
) -> Result<Box<dyn HotPluggable>> {
    let pci_address = net_carrier_device
        .pci_address
        .context("PCI address not allocated")?;
    let virtio_device = net_carrier_device
        .net_param
        .create_virtio_device(net_local_parameters.protection_type)
        .context("create virtio device")?;
    let mut virtio_pci_device = VirtioPciDevice::new(
        net_local_parameters.guest_memory,
        virtio_device,
        net_carrier_device.msi_device_tube,
        true,
        None,
        net_carrier_device.ioevent_vm_memory_client,
        net_carrier_device.vm_control_tube,
    )
    .context("create virtio PCI device")?;
    virtio_pci_device
        .set_pci_address(pci_address)
        .context("set PCI address")?;
    virtio_pci_device
        .configure_io_bars()
        .context("configure IO BAR")?;
    virtio_pci_device
        .configure_device_bars()
        .context("configure device BAR")?;
    let IntxParameter {
        irq_evt,
        irq_num,
        pin,
    } = net_carrier_device
        .intx_parameter
        .context("Missing INTx parameter.")?;
    virtio_pci_device.assign_irq(irq_evt, pin, irq_num);
    Ok(Box::new(virtio_pci_device))
}

/// Builds HotPlugPci from BlockResourceCarrier and BlockLocalParameters.
pub fn build_hotplug_block_device(
    block_carrier: BlockResourceCarrier,
    local_parameters: BlockLocalParameters,
) -> Result<Box<dyn HotPluggable>> {
    let pci_address = block_carrier
        .pci_address
        .context("PCI address not allocated")?;
    let disk_image = block_carrier
        .disk_option
        .open()
        .context("open disk image")?;
    let base_features = virtio::base_features(local_parameters.protection_type);
    let virtio_device = Box::new(
        virtio::BlockAsync::new(
            base_features,
            disk_image,
            &block_carrier.disk_option,
            None, // control_tube
            None, // queue_size
            None, // num_queues
        )
        .context("create block device")?,
    );
    let mut virtio_pci_device = VirtioPciDevice::new(
        local_parameters.guest_memory,
        virtio_device,
        block_carrier.msi_device_tube,
        true,
        None,
        block_carrier.ioevent_vm_memory_client,
        block_carrier.vm_control_tube,
    )
    .context("create virtio PCI device")?;
    virtio_pci_device
        .set_pci_address(pci_address)
        .context("set PCI address")?;
    virtio_pci_device
        .configure_io_bars()
        .context("configure IO BAR")?;
    virtio_pci_device
        .configure_device_bars()
        .context("configure device BAR")?;
    let IntxParameter {
        irq_evt,
        irq_num,
        pin,
    } = block_carrier
        .intx_parameter
        .context("Missing INTx parameter.")?;
    virtio_pci_device.assign_irq(irq_evt, pin, irq_num);
    Ok(Box::new(virtio_pci_device))
}

/// Additional parameters required on the destination process to configure net VirtioPciDevice.
pub struct NetLocalParameters {
    guest_memory: GuestMemory,
    protection_type: ProtectionType,
}

impl NetLocalParameters {
    /// Constructs NetLocalParameters.
    pub fn new(guest_memory: GuestMemory, protection_type: ProtectionType) -> Self {
        Self {
            guest_memory,
            protection_type,
        }
    }
}

/// Additional parameters required on the destination process to configure block VirtioPciDevice.
pub struct BlockLocalParameters {
    guest_memory: GuestMemory,
    protection_type: ProtectionType,
}

impl BlockLocalParameters {
    /// Constructs BlockLocalParameters.
    pub fn new(guest_memory: GuestMemory, protection_type: ProtectionType) -> Self {
        Self {
            guest_memory,
            protection_type,
        }
    }
}

/// Additional parameters required on the destination process to configure vhost-user-block
/// VirtioPciDevice.
pub struct VhostUserBlockLocalParameters {
    guest_memory: GuestMemory,
    protection_type: ProtectionType,
}

impl VhostUserBlockLocalParameters {
    /// Constructs VhostUserBlockLocalParameters.
    pub fn new(guest_memory: GuestMemory, protection_type: ProtectionType) -> Self {
        Self {
            guest_memory,
            protection_type,
        }
    }
}

/// Builds HotPlugPci from VhostUserBlockResourceCarrier and VhostUserBlockLocalParameters.
pub fn build_hotplug_vhost_user_block_device(
    carrier: VhostUserBlockResourceCarrier,
    local_parameters: VhostUserBlockLocalParameters,
) -> Result<Box<dyn HotPluggable>> {
    let pci_address = carrier.pci_address.context("PCI address not allocated")?;
    let connection: vmm_vhost::Connection = UnixStream::connect(&carrier.socket_path)
        .context("connect to vhost-user socket")?
        .try_into()
        .context("failed to construct Connection from UnixStream")?;
    let base_features = virtio::base_features(local_parameters.protection_type);
    let virtio_device = Box::new(
        VhostUserFrontend::new(
            virtio::DeviceType::Block,
            base_features,
            connection,
            carrier.vm_evt_wrtube,
            carrier.max_queue_size,
            Some(pci_address),
        )
        .context("create vhost-user frontend")?,
    );
    let mut virtio_pci_device = VirtioPciDevice::new(
        local_parameters.guest_memory,
        virtio_device,
        carrier.msi_device_tube,
        true,
        None,
        carrier.ioevent_vm_memory_client,
        carrier.vm_control_tube,
    )
    .context("create virtio PCI device")?;
    virtio_pci_device
        .set_pci_address(pci_address)
        .context("set PCI address")?;
    virtio_pci_device
        .configure_io_bars()
        .context("configure IO BAR")?;
    virtio_pci_device
        .configure_device_bars()
        .context("configure device BAR")?;
    let IntxParameter {
        irq_evt,
        irq_num,
        pin,
    } = carrier.intx_parameter.context("Missing INTx parameter.")?;
    virtio_pci_device.assign_irq(irq_evt, pin, irq_num);
    Ok(Box::new(virtio_pci_device))
}
