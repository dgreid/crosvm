// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Trait definitions and implementations for PCI hotplug.

#![deny(missing_docs)]

use std::path::PathBuf;

use base::AsRawDescriptor;
use base::AsRawDescriptors;
use base::RawDescriptor;
use base::SendTube;
use base::Tube;
use serde::Deserialize;
use serde::Serialize;
use vm_control::api::VmMemoryClient;

use crate::virtio::block::DiskOption;
use crate::virtio::NetParameters;
use crate::IrqLevelEvent;
use crate::PciAddress;
use crate::PciDevice;
use crate::PciDeviceError;
use crate::PciInterruptPin;

pub type Result<T> = std::result::Result<T, PciDeviceError>;

/// Delegates a method call to the inner carrier variant.
macro_rules! carrier_delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self {
            ResourceCarrier::VirtioNet(c) => c.$method($($arg),*),
            ResourceCarrier::VirtioBlock(c) => c.$method($($arg),*),
            ResourceCarrier::VhostUserBlock(c) => c.$method($($arg),*),
        }
    }
}

/// A ResourceCarrier moves resources for PCI device across process boundary.
///
/// ResourceCarrier can be sent across processes using De/Serialize. All the variants shall be able
/// to convert into a HotPlugPluggable device.
#[derive(Serialize, Deserialize)]
pub enum ResourceCarrier {
    /// virtio-net device.
    VirtioNet(NetResourceCarrier),
    /// virtio-block device.
    VirtioBlock(BlockResourceCarrier),
    /// vhost-user-block device.
    VhostUserBlock(VhostUserBlockResourceCarrier),
}

impl ResourceCarrier {
    /// Returns debug label for the target device.
    pub fn debug_label(&self) -> String {
        carrier_delegate!(self, debug_label)
    }

    /// A vector of device-specific file descriptors that must be kept open
    /// after jailing. Must be called before the process is jailed.
    pub fn keep_rds(&self) -> Vec<RawDescriptor> {
        carrier_delegate!(self, keep_rds)
    }
    /// Allocate the preferred address to the device.
    pub fn allocate_address(
        &mut self,
        preferred_address: PciAddress,
        resources: &mut resources::SystemAllocator,
    ) -> Result<()> {
        carrier_delegate!(self, allocate_address, preferred_address, resources)
    }
    /// Assign a legacy PCI IRQ to this device.
    /// The device may write to `irq_evt` to trigger an interrupt.
    /// When `irq_resample_evt` is signaled, the device should re-assert `irq_evt` if necessary.
    pub fn assign_irq(&mut self, irq_evt: IrqLevelEvent, pin: PciInterruptPin, irq_num: u32) {
        carrier_delegate!(self, assign_irq, irq_evt, pin, irq_num)
    }
}

/// Additional requirements for a PciDevice to support hotplug.
/// A hotplug device can be configured without access to the SystemAllocator.
pub trait HotPluggable: PciDevice {
    /// Sets PciAddress to pci_addr. Replaces allocate_address.
    fn set_pci_address(&mut self, pci_addr: PciAddress) -> Result<()>;

    /// Configures IO BAR layout without memory alloc. Replaces allocate_io_bars.
    fn configure_io_bars(&mut self) -> Result<()>;

    /// Configure device BAR layout without memory alloc. Replaces allocate_device_bars.
    fn configure_device_bars(&mut self) -> Result<()>;
}

impl<T: HotPluggable + ?Sized> HotPluggable for Box<T> {
    fn set_pci_address(&mut self, pci_addr: PciAddress) -> Result<()> {
        (**self).set_pci_address(pci_addr)
    }

    fn configure_io_bars(&mut self) -> Result<()> {
        (**self).configure_io_bars()
    }

    fn configure_device_bars(&mut self) -> Result<()> {
        (**self).configure_device_bars()
    }
}

/// A NetResourceCarrier is a ResourceCarrier specialization for virtio-net devices.
///
/// TODO(b/289155315): make members private.
#[derive(Serialize, Deserialize)]
pub struct NetResourceCarrier {
    /// NetParameters for constructing tap device
    pub net_param: NetParameters,
    /// msi_device_tube for VirtioPciDevice constructor
    pub msi_device_tube: Tube,
    /// ioevent_vm_memory_client for VirtioPciDevice constructor
    pub ioevent_vm_memory_client: VmMemoryClient,
    /// pci_address for the hotplugged device
    pub pci_address: Option<PciAddress>,
    /// intx_parameter for assign_irq
    pub intx_parameter: Option<IntxParameter>,
    /// vm_control_tube for VirtioPciDevice constructor
    pub vm_control_tube: Tube,
}

impl NetResourceCarrier {
    ///Constructs NetResourceCarrier.
    pub fn new(
        net_param: NetParameters,
        msi_device_tube: Tube,
        ioevent_vm_memory_client: VmMemoryClient,
        vm_control_tube: Tube,
    ) -> Self {
        Self {
            net_param,
            msi_device_tube,
            ioevent_vm_memory_client,
            pci_address: None,
            intx_parameter: None,
            vm_control_tube,
        }
    }

    fn debug_label(&self) -> String {
        "virtio-net".to_owned()
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut keep_rds = vec![
            self.msi_device_tube.as_raw_descriptor(),
            self.ioevent_vm_memory_client.as_raw_descriptor(),
        ];
        if let Some(intx_parameter) = &self.intx_parameter {
            keep_rds.extend(intx_parameter.irq_evt.as_raw_descriptors());
        }
        keep_rds
    }

    fn allocate_address(
        &mut self,
        preferred_address: PciAddress,
        resources: &mut resources::SystemAllocator,
    ) -> Result<()> {
        match self.pci_address {
            None => {
                if resources.reserve_pci(preferred_address, self.debug_label()) {
                    self.pci_address = Some(preferred_address);
                } else {
                    return Err(PciDeviceError::PciAllocationFailed);
                }
            }
            Some(pci_address) => {
                if pci_address != preferred_address {
                    return Err(PciDeviceError::PciAllocationFailed);
                }
            }
        }
        Ok(())
    }

    fn assign_irq(&mut self, irq_evt: IrqLevelEvent, pin: PciInterruptPin, irq_num: u32) {
        self.intx_parameter = Some(IntxParameter {
            irq_evt,
            pin,
            irq_num,
        });
    }
}

/// Parameters for legacy INTx interrrupt.
#[derive(Serialize, Deserialize)]
pub struct IntxParameter {
    /// interrupt level event
    pub irq_evt: IrqLevelEvent,
    /// INTx interrupt pin
    pub pin: PciInterruptPin,
    /// irq num
    pub irq_num: u32,
}

/// A BlockResourceCarrier is a ResourceCarrier specialization for virtio-block devices.
#[derive(Serialize, Deserialize)]
pub struct BlockResourceCarrier {
    /// DiskOption for constructing block device
    pub disk_option: DiskOption,
    /// msi_device_tube for VirtioPciDevice constructor
    pub msi_device_tube: Tube,
    /// ioevent_vm_memory_client for VirtioPciDevice constructor
    pub ioevent_vm_memory_client: VmMemoryClient,
    /// pci_address for the hotplugged device
    pub pci_address: Option<PciAddress>,
    /// intx_parameter for assign_irq
    pub intx_parameter: Option<IntxParameter>,
    /// vm_control_tube for VirtioPciDevice constructor
    pub vm_control_tube: Tube,
}

impl BlockResourceCarrier {
    /// Constructs BlockResourceCarrier.
    pub fn new(
        disk_option: DiskOption,
        msi_device_tube: Tube,
        ioevent_vm_memory_client: VmMemoryClient,
        vm_control_tube: Tube,
    ) -> Self {
        Self {
            disk_option,
            msi_device_tube,
            ioevent_vm_memory_client,
            pci_address: None,
            intx_parameter: None,
            vm_control_tube,
        }
    }

    fn debug_label(&self) -> String {
        "virtio-block".to_owned()
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut keep_rds = vec![
            self.msi_device_tube.as_raw_descriptor(),
            self.ioevent_vm_memory_client.as_raw_descriptor(),
        ];
        if let Some(intx_parameter) = &self.intx_parameter {
            keep_rds.extend(intx_parameter.irq_evt.as_raw_descriptors());
        }
        keep_rds
    }

    fn allocate_address(
        &mut self,
        preferred_address: PciAddress,
        resources: &mut resources::SystemAllocator,
    ) -> Result<()> {
        match self.pci_address {
            None => {
                if resources.reserve_pci(preferred_address, self.debug_label()) {
                    self.pci_address = Some(preferred_address);
                } else {
                    return Err(PciDeviceError::PciAllocationFailed);
                }
            }
            Some(pci_address) => {
                if pci_address != preferred_address {
                    return Err(PciDeviceError::PciAllocationFailed);
                }
            }
        }
        Ok(())
    }

    fn assign_irq(&mut self, irq_evt: IrqLevelEvent, pin: PciInterruptPin, irq_num: u32) {
        self.intx_parameter = Some(IntxParameter {
            irq_evt,
            pin,
            irq_num,
        });
    }
}

/// A VhostUserBlockResourceCarrier is a ResourceCarrier specialization for vhost-user-block
/// devices.
#[derive(Serialize, Deserialize)]
pub struct VhostUserBlockResourceCarrier {
    /// Socket path for connecting to the vhost-user backend
    pub socket_path: PathBuf,
    /// Optional maximum queue size
    pub max_queue_size: Option<u16>,
    /// msi_device_tube for VirtioPciDevice constructor
    pub msi_device_tube: Tube,
    /// ioevent_vm_memory_client for VirtioPciDevice constructor
    pub ioevent_vm_memory_client: VmMemoryClient,
    /// pci_address for the hotplugged device
    pub pci_address: Option<PciAddress>,
    /// intx_parameter for assign_irq
    pub intx_parameter: Option<IntxParameter>,
    /// vm_control_tube for VirtioPciDevice constructor
    pub vm_control_tube: Tube,
    /// vm_evt_wrtube for signaling backend crashes to the main loop
    pub vm_evt_wrtube: SendTube,
}

impl VhostUserBlockResourceCarrier {
    /// Constructs VhostUserBlockResourceCarrier.
    pub fn new(
        socket_path: PathBuf,
        max_queue_size: Option<u16>,
        msi_device_tube: Tube,
        ioevent_vm_memory_client: VmMemoryClient,
        vm_control_tube: Tube,
        vm_evt_wrtube: SendTube,
    ) -> Self {
        Self {
            socket_path,
            max_queue_size,
            msi_device_tube,
            ioevent_vm_memory_client,
            pci_address: None,
            intx_parameter: None,
            vm_control_tube,
            vm_evt_wrtube,
        }
    }

    fn debug_label(&self) -> String {
        "vhost-user-block".to_owned()
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut keep_rds = vec![
            self.msi_device_tube.as_raw_descriptor(),
            self.ioevent_vm_memory_client.as_raw_descriptor(),
            self.vm_evt_wrtube.as_raw_descriptor(),
        ];
        if let Some(intx_parameter) = &self.intx_parameter {
            keep_rds.extend(intx_parameter.irq_evt.as_raw_descriptors());
        }
        keep_rds
    }

    fn allocate_address(
        &mut self,
        preferred_address: PciAddress,
        resources: &mut resources::SystemAllocator,
    ) -> Result<()> {
        match self.pci_address {
            None => {
                if resources.reserve_pci(preferred_address, self.debug_label()) {
                    self.pci_address = Some(preferred_address);
                } else {
                    return Err(PciDeviceError::PciAllocationFailed);
                }
            }
            Some(pci_address) => {
                if pci_address != preferred_address {
                    return Err(PciDeviceError::PciAllocationFailed);
                }
            }
        }
        Ok(())
    }

    fn assign_irq(&mut self, irq_evt: IrqLevelEvent, pin: PciInterruptPin, irq_num: u32) {
        self.intx_parameter = Some(IntxParameter {
            irq_evt,
            pin,
            irq_num,
        });
    }
}

#[cfg(test)]
mod tests {
    use resources::AddressRange;
    use resources::SystemAllocator;
    use resources::SystemAllocatorConfig;

    use super::*;
    use crate::virtio::NetParametersMode;

    fn test_allocator() -> SystemAllocator {
        SystemAllocator::new(
            SystemAllocatorConfig {
                io: Some(AddressRange {
                    start: 0x1000,
                    end: 0xffff,
                }),
                low_mmio: AddressRange {
                    start: 0x3000_0000,
                    end: 0x3000_ffff,
                },
                high_mmio: AddressRange {
                    start: 0x1_0000_0000,
                    end: 0x1_ffff_ffff,
                },
                platform_mmio: None,
                first_irq: 5,
            },
            None,
            &[],
        )
        .unwrap()
    }

    fn test_addr() -> PciAddress {
        PciAddress {
            bus: 1,
            dev: 0,
            func: 0,
        }
    }

    fn net_params() -> NetParameters {
        NetParameters {
            mode: NetParametersMode::TapName {
                tap_name: "t".to_owned(),
                mac: None,
            },
            vq_pairs: None,
            vhost_net: None,
            packed_queue: false,
            pci_address: None,
            mrg_rxbuf: false,
        }
    }

    fn new_net_carrier() -> NetResourceCarrier {
        let (_msi_h, msi_d) = Tube::pair().unwrap();
        let (_io_h, io_d) = Tube::pair().unwrap();
        let (_vmc_h, vmc_d) = Tube::pair().unwrap();
        NetResourceCarrier::new(net_params(), msi_d, VmMemoryClient::new(io_d), vmc_d)
    }

    fn new_block_carrier() -> BlockResourceCarrier {
        let (_msi_h, msi_d) = Tube::pair().unwrap();
        let (_io_h, io_d) = Tube::pair().unwrap();
        let (_vmc_h, vmc_d) = Tube::pair().unwrap();
        BlockResourceCarrier::new(
            DiskOption::default(),
            msi_d,
            VmMemoryClient::new(io_d),
            vmc_d,
        )
    }

    fn new_vhost_user_block_carrier() -> VhostUserBlockResourceCarrier {
        let (_msi_h, msi_d) = Tube::pair().unwrap();
        let (_io_h, io_d) = Tube::pair().unwrap();
        let (_vmc_h, vmc_d) = Tube::pair().unwrap();
        let (_evt_h, evt_d) = Tube::pair().unwrap();
        let evt_send = evt_d.try_clone_send_tube().unwrap();
        VhostUserBlockResourceCarrier::new(
            PathBuf::from("/tmp/test.sock"),
            None,
            msi_d,
            VmMemoryClient::new(io_d),
            vmc_d,
            evt_send,
        )
    }

    #[test]
    fn debug_labels() {
        assert_eq!(
            ResourceCarrier::VirtioNet(new_net_carrier()).debug_label(),
            "virtio-net"
        );
        assert_eq!(
            ResourceCarrier::VirtioBlock(new_block_carrier()).debug_label(),
            "virtio-block"
        );
        assert_eq!(
            ResourceCarrier::VhostUserBlock(new_vhost_user_block_carrier()).debug_label(),
            "vhost-user-block"
        );
    }

    #[test]
    fn net_keep_rds_without_irq() {
        let c = ResourceCarrier::VirtioNet(new_net_carrier());
        assert_eq!(c.keep_rds().len(), 2);
    }

    #[test]
    fn block_keep_rds_without_irq() {
        let c = ResourceCarrier::VirtioBlock(new_block_carrier());
        assert_eq!(c.keep_rds().len(), 2);
    }

    // vhost-user-block uniquely threads vm_evt_wrtube through keep_rds.
    #[test]
    fn vhost_user_block_keep_rds_includes_vm_evt_wrtube() {
        let c = ResourceCarrier::VhostUserBlock(new_vhost_user_block_carrier());
        assert_eq!(c.keep_rds().len(), 3);
    }

    #[test]
    fn keep_rds_includes_irq_after_assign() {
        let mut c = ResourceCarrier::VhostUserBlock(new_vhost_user_block_carrier());
        let pre = c.keep_rds().len();
        c.assign_irq(IrqLevelEvent::new().unwrap(), PciInterruptPin::IntA, 5);
        assert_eq!(c.keep_rds().len(), pre + 2);
    }

    #[test]
    fn allocate_address_reserves_exactly_once() {
        let mut allocator = test_allocator();
        let addr = test_addr();
        let mut c = ResourceCarrier::VirtioBlock(new_block_carrier());

        c.allocate_address(addr, &mut allocator).unwrap();
        c.allocate_address(addr, &mut allocator).unwrap();
        assert!(!allocator.reserve_pci(addr, "other".to_owned()));
    }

    #[test]
    fn allocate_address_rejects_different_address_after_first() {
        let mut allocator = test_allocator();
        let first = test_addr();
        let second = PciAddress {
            bus: 1,
            dev: 1,
            func: 0,
        };
        let mut c = ResourceCarrier::VirtioNet(new_net_carrier());

        c.allocate_address(first, &mut allocator).unwrap();
        assert!(matches!(
            c.allocate_address(second, &mut allocator),
            Err(PciDeviceError::PciAllocationFailed)
        ));
    }

    #[test]
    fn allocate_address_fails_if_slot_taken() {
        let mut allocator = test_allocator();
        let addr = test_addr();
        assert!(allocator.reserve_pci(addr, "sitter".to_owned()));

        let mut c = ResourceCarrier::VhostUserBlock(new_vhost_user_block_carrier());
        assert!(matches!(
            c.allocate_address(addr, &mut allocator),
            Err(PciDeviceError::PciAllocationFailed)
        ));
    }

    // Regression for the PCI-address-leak-on-failure path: after a carrier reserves an address and
    // that address is released (as cleanup_hotplug_resources would do on a failed hotplug), a
    // fresh carrier must be able to reserve the same slot.
    #[test]
    fn released_address_is_reusable_by_next_carrier() {
        let mut allocator = test_allocator();
        let addr = test_addr();

        let mut first = ResourceCarrier::VirtioNet(new_net_carrier());
        first.allocate_address(addr, &mut allocator).unwrap();
        assert!(!allocator.reserve_pci(addr, "other".to_owned()));

        assert!(allocator.release_pci(addr));

        let mut second = ResourceCarrier::VirtioBlock(new_block_carrier());
        second.allocate_address(addr, &mut allocator).unwrap();
    }
}
