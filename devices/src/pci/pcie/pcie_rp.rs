// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::Event;
use vm_control::GpeNotify;
use vm_control::PmeNotify;

use crate::bus::HotPlugBus;
use crate::bus::HotPlugKey;
use crate::pci::pcie::pcie_host::PcieHostPort;
use crate::pci::pcie::pcie_port::PciePort;
use crate::pci::pcie::pcie_port::PciePortVariant;
use crate::pci::pcie::*;
use crate::pci::PciAddress;

const PCIE_RP_DID: u16 = 0x3420;
pub struct PcieRootPort {
    pcie_port: PciePort,
    downstream_devices: BTreeMap<PciAddress, HotPlugKey>,
    hotplug_out_begin: bool,
    removed_downstream: Vec<PciAddress>,
}

impl PcieRootPort {
    /// Constructs a new PCIE root port
    pub fn new(secondary_bus_num: u8, slot_implemented: bool) -> Self {
        PcieRootPort {
            pcie_port: PciePort::new(
                PCIE_RP_DID,
                "PcieRootPort".to_string(),
                0,
                secondary_bus_num,
                slot_implemented,
                PcieDevicePortType::RootPort,
            ),
            downstream_devices: BTreeMap::new(),
            hotplug_out_begin: false,
            removed_downstream: Vec::new(),
        }
    }

    /// Constructs a new PCIE root port which associated with the host physical pcie RP
    pub fn new_from_host(pcie_host: PcieHostPort, slot_implemented: bool) -> Result<Self> {
        Ok(PcieRootPort {
            pcie_port: PciePort::new_from_host(
                pcie_host,
                slot_implemented,
                PcieDevicePortType::RootPort,
            )
            .context("PciePort::new_from_host failed")?,
            downstream_devices: BTreeMap::new(),
            hotplug_out_begin: false,
            removed_downstream: Vec::new(),
        })
    }
}

impl PciePortVariant for PcieRootPort {
    fn get_pcie_port(&self) -> &PciePort {
        &self.pcie_port
    }

    fn get_pcie_port_mut(&mut self) -> &mut PciePort {
        &mut self.pcie_port
    }

    fn get_removed_devices_impl(&self) -> Vec<PciAddress> {
        if self.pcie_port.removed_downstream_valid() {
            self.removed_downstream.clone()
        } else {
            Vec::new()
        }
    }

    fn hotplug_implemented_impl(&self) -> bool {
        self.pcie_port.hotplug_implemented()
    }

    fn hotplugged_impl(&self) -> bool {
        false
    }
}

impl HotPlugBus for PcieRootPort {
    fn hot_plug(&mut self, addr: PciAddress) -> Result<Option<Event>> {
        if self.pcie_port.is_hpc_pending() {
            bail!("Hot plug fail: previous slot event is pending.");
        }
        if !self.pcie_port.is_hotplug_ready() {
            bail!("Hot unplug fail: slot is not enabled by the guest yet.");
        }
        self.downstream_devices
            .get(&addr)
            .context("No downstream devices.")?;

        let hpc_sender = Event::new()?;
        let hpc_recvr = hpc_sender.try_clone()?;
        self.pcie_port.set_hpc_sender(hpc_sender);
        self.pcie_port
            .set_slot_status(PCIE_SLTSTA_PDS | PCIE_SLTSTA_ABP);
        self.pcie_port.trigger_hp_or_pme_interrupt();
        Ok(Some(hpc_recvr))
    }

    fn hot_unplug(&mut self, addr: PciAddress) -> Result<Option<Event>> {
        if self.pcie_port.is_hpc_pending() {
            bail!("Hot unplug fail: previous slot event is pending.");
        }
        if !self.pcie_port.is_hotplug_ready() {
            bail!("Hot unplug fail: slot is not enabled by the guest yet.");
        }
        self.downstream_devices
            .remove(&addr)
            .context("No downstream devices.")?;
        if self.hotplug_out_begin {
            bail!("Hot unplug is pending.")
        }
        self.hotplug_out_begin = true;

        self.removed_downstream.clear();
        self.removed_downstream.push(addr);
        // All the remaine devices will be removed also in this hotplug out interrupt
        for (guest_pci_addr, _) in self.downstream_devices.iter() {
            self.removed_downstream.push(*guest_pci_addr);
        }

        let hpc_sender = Event::new()?;
        let hpc_recvr = hpc_sender.try_clone()?;
        let slot_control = self.pcie_port.get_slot_control();
        match slot_control & PCIE_SLTCTL_PIC {
            PCIE_SLTCTL_PIC_ON => {
                self.pcie_port.set_hpc_sender(hpc_sender);
                self.pcie_port.set_slot_status(PCIE_SLTSTA_ABP);
                self.pcie_port.trigger_hp_or_pme_interrupt();
            }
            PCIE_SLTCTL_PIC_OFF => {
                // Do not press attention button, as the slot is already off. Likely caused by
                // previous hot plug failed.
                self.pcie_port.mask_slot_status(!PCIE_SLTSTA_PDS);
                hpc_sender.signal()?;
            }
            _ => {
                // Power indicator in blinking state.
                // Should not be possible, since the previous slot event is pending.
                bail!("Hot unplug fail: Power indicator is blinking.");
            }
        }

        if self.pcie_port.is_host() {
            self.pcie_port.hot_unplug()
        }
        Ok(Some(hpc_recvr))
    }

    fn get_ready_notification(&mut self) -> anyhow::Result<Event> {
        Ok(self.pcie_port.get_ready_notification()?)
    }

    fn get_address(&self) -> Option<PciAddress> {
        self.pcie_port.get_address()
    }

    fn get_secondary_bus_number(&self) -> Option<u8> {
        Some(self.pcie_port.get_bus_range()?.secondary)
    }

    fn is_match(&self, host_addr: PciAddress) -> Option<u8> {
        self.pcie_port.is_match(host_addr)
    }

    fn add_hotplug_device(&mut self, hotplug_key: HotPlugKey, guest_addr: PciAddress) {
        if !self.pcie_port.hotplug_implemented() {
            return;
        }

        // Begin the next round hotplug in process
        if self.hotplug_out_begin {
            self.hotplug_out_begin = false;
            self.downstream_devices.clear();
            self.removed_downstream.clear();
        }

        self.downstream_devices.insert(guest_addr, hotplug_key);
    }

    fn get_hotplug_device(&self, hotplug_key: HotPlugKey) -> Option<PciAddress> {
        for (guest_address, host_info) in self.downstream_devices.iter() {
            if hotplug_key == *host_info {
                return Some(*guest_address);
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.downstream_devices.is_empty()
    }

    fn get_hotplug_key(&self) -> Option<HotPlugKey> {
        None
    }
}

impl GpeNotify for PcieRootPort {
    fn notify(&mut self) {
        if !self.pcie_port.hotplug_implemented() {
            return;
        }

        if self.pcie_port.is_host() {
            self.pcie_port.prepare_hotplug();
        }

        if self.pcie_port.should_trigger_pme() {
            self.pcie_port
                .inject_pme(self.pcie_port.get_address().unwrap().pme_requester_id());
        }
    }
}

impl PmeNotify for PcieRootPort {
    fn notify(&mut self, requester_id: u16) {
        self.pcie_port.inject_pme(requester_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::HotPlugBus;

    /// Creates a PcieRootPort with hotplug slot and enables it for testing.
    /// Uses a unique secondary bus number to avoid global state collisions between tests.
    fn new_enabled_root_port(bus: u8) -> PcieRootPort {
        let mut rp = PcieRootPort::new(bus, true);
        rp.pcie_port.enable_slot_for_test();
        rp
    }

    fn test_device_addr(bus: u8) -> PciAddress {
        PciAddress {
            bus,
            dev: 0,
            func: 0,
        }
    }

    fn test_hotplug_key(bus: u8) -> HotPlugKey {
        HotPlugKey::GuestDevice {
            guest_addr: test_device_addr(bus),
        }
    }

    #[test]
    fn hot_plug_succeeds_with_registered_device() {
        let bus = 10;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        // Register the device first.
        rp.add_hotplug_device(key, addr);

        // hot_plug should succeed and return a completion event.
        let result = rp.hot_plug(addr);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn hot_plug_fails_when_hpc_pending() {
        let bus = 11;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key, addr);

        // First hot_plug succeeds and sets HPC sender.
        let _ = rp.hot_plug(addr).unwrap();

        // Second hot_plug should fail because HPC is pending.
        let addr2 = PciAddress {
            bus,
            dev: 0,
            func: 1,
        };
        let key2 = HotPlugKey::GuestDevice { guest_addr: addr2 };
        rp.add_hotplug_device(key2, addr2);
        let result = rp.hot_plug(addr2);
        assert!(result.is_err());
    }

    #[test]
    fn hot_plug_fails_when_not_hotplug_ready() {
        let bus = 12;
        // Create port without enabling the slot.
        let mut rp = PcieRootPort::new(bus, true);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key, addr);

        let result = rp.hot_plug(addr);
        assert!(result.is_err());
    }

    #[test]
    fn hot_plug_fails_without_registered_device() {
        let bus = 13;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        // Don't register a device.

        let result = rp.hot_plug(addr);
        assert!(result.is_err());
    }

    #[test]
    fn hot_unplug_with_pic_on_sets_abp() {
        let bus = 14;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key, addr);

        // Set PIC to ON to simulate an active device.
        rp.pcie_port.set_pic_on_for_test();

        let result = rp.hot_unplug(addr);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(event.is_some());
    }

    #[test]
    fn hot_unplug_with_pic_off_signals_immediately() {
        let bus = 15;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key, addr);
        // PIC is OFF (default), so unplug should signal immediately.

        let result = rp.hot_unplug(addr);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(event.is_some());
        // Event should be immediately signaled.
        event
            .unwrap()
            .wait_timeout(std::time::Duration::from_millis(10))
            .unwrap();
    }

    #[test]
    fn hot_unplug_with_pic_blink_returns_error() {
        let bus = 16;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key, addr);

        // Set PIC to BLINK.
        rp.pcie_port.set_pic_blink_for_test();

        let result = rp.hot_unplug(addr);
        assert!(result.is_err());
    }

    #[test]
    fn hot_unplug_fails_when_hotplug_out_begin() {
        let bus = 17;
        let mut rp = new_enabled_root_port(bus);
        let addr1 = test_device_addr(bus);
        let key1 = test_hotplug_key(bus);
        let addr2 = PciAddress {
            bus,
            dev: 0,
            func: 1,
        };
        let key2 = HotPlugKey::GuestDevice { guest_addr: addr2 };

        rp.add_hotplug_device(key1, addr1);
        rp.add_hotplug_device(key2, addr2);

        // First unplug succeeds.
        let _ = rp.hot_unplug(addr1).unwrap();

        // Second unplug should fail because hotplug_out_begin is set.
        let result = rp.hot_unplug(addr2);
        assert!(result.is_err());
    }

    #[test]
    fn hot_unplug_fails_when_not_hotplug_ready() {
        let bus = 18;
        let mut rp = PcieRootPort::new(bus, true);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key, addr);

        let result = rp.hot_unplug(addr);
        assert!(result.is_err());
    }

    #[test]
    fn add_hotplug_device_clears_hotplug_out_begin() {
        let bus = 19;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key.clone(), addr);

        // Trigger unplug to set hotplug_out_begin.
        let _ = rp.hot_unplug(addr).unwrap();
        assert!(rp.hotplug_out_begin);

        // Adding a new device should clear hotplug_out_begin.
        let new_addr = PciAddress {
            bus,
            dev: 0,
            func: 1,
        };
        let new_key = HotPlugKey::GuestDevice {
            guest_addr: new_addr,
        };
        rp.add_hotplug_device(new_key, new_addr);
        assert!(!rp.hotplug_out_begin);
        // Previous downstream devices should be cleared too.
        assert!(rp.removed_downstream.is_empty());
    }

    #[test]
    fn add_hotplug_device_no_op_without_slot() {
        let bus = 20;
        let mut rp = PcieRootPort::new(bus, false);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key.clone(), addr);
        // Without slot implemented, add_hotplug_device should be a no-op.
        assert!(rp.is_empty());
    }

    #[test]
    fn is_empty_reflects_device_state() {
        let bus = 21;
        let mut rp = new_enabled_root_port(bus);
        assert!(rp.is_empty());

        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);
        rp.add_hotplug_device(key, addr);
        assert!(!rp.is_empty());
    }

    #[test]
    fn get_hotplug_device_returns_correct_address() {
        let bus = 22;
        let mut rp = new_enabled_root_port(bus);
        let addr = test_device_addr(bus);
        let key = test_hotplug_key(bus);

        rp.add_hotplug_device(key.clone(), addr);
        assert_eq!(rp.get_hotplug_device(key), Some(addr));
    }

    #[test]
    fn get_hotplug_device_returns_none_for_unknown_key() {
        let bus = 23;
        let rp = new_enabled_root_port(bus);
        let key = test_hotplug_key(bus);

        assert_eq!(rp.get_hotplug_device(key), None);
    }

    #[test]
    fn get_secondary_bus_number() {
        let bus = 24;
        let rp = new_enabled_root_port(bus);
        assert_eq!(rp.get_secondary_bus_number(), Some(bus));
    }

    #[test]
    fn get_hotplug_key_returns_none() {
        let bus = 25;
        let rp = new_enabled_root_port(bus);
        assert_eq!(rp.get_hotplug_key(), None);
    }
}
