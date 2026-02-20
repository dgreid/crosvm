// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::str::FromStr;
use std::sync::Arc;

use base::error;
use base::warn;
use base::Event;
use resources::SystemAllocator;
use sync::Mutex;

use crate::pci::pci_configuration::PciCapConfig;
use crate::pci::pci_configuration::PciCapConfigWriteResult;
use crate::pci::pci_configuration::PciCapMapping;
use crate::pci::pci_configuration::PciCapability;
use crate::pci::pcie::pci_bridge::PciBridgeBusRange;
use crate::pci::pcie::pcie_device::PcieCap;
use crate::pci::pcie::pcie_device::PcieDevice;
use crate::pci::pcie::pcie_host::PcieHostPort;
use crate::pci::pcie::*;
use crate::pci::pm::PciDevicePower;
use crate::pci::pm::PciPmCap;
use crate::pci::pm::PmConfig;
use crate::pci::pm::PmStatusChange;
use crate::pci::MsiConfig;
use crate::pci::PciAddress;
use crate::pci::PciDeviceError;

// reserve 8MB memory window
const PCIE_BR_MEM_SIZE: u64 = 0x80_0000;
// reserve 64MB prefetch window
const PCIE_BR_PREF_MEM_SIZE: u64 = 0x400_0000;

fn trigger_interrupt(msi: &Option<Arc<Mutex<MsiConfig>>>) {
    if let Some(msi_config) = msi {
        let msi_config = msi_config.lock();
        if msi_config.is_msi_enabled() {
            msi_config.trigger()
        }
    }
}

struct PcieRootCap {
    secondary_bus_num: u8,
    subordinate_bus_num: u8,

    control: u16,
    status: u32,
    pme_pending_requester_id: Option<u16>,

    msi_config: Option<Arc<Mutex<MsiConfig>>>,
}

impl PcieRootCap {
    fn new(secondary_bus_num: u8, subordinate_bus_num: u8) -> Self {
        PcieRootCap {
            secondary_bus_num,
            subordinate_bus_num,
            control: 0,
            status: 0,
            pme_pending_requester_id: None,
            msi_config: None,
        }
    }

    fn clone_interrupt(&mut self, msi_config: Arc<Mutex<MsiConfig>>) {
        self.msi_config = Some(msi_config);
    }

    fn trigger_pme_interrupt(&self) {
        if (self.control & PCIE_ROOTCTL_PME_ENABLE) != 0
            && (self.status & PCIE_ROOTSTA_PME_STATUS) != 0
        {
            trigger_interrupt(&self.msi_config)
        }
    }
}

static PCIE_ROOTS_CAP: Mutex<Vec<Arc<Mutex<PcieRootCap>>>> = Mutex::new(Vec::new());

fn push_pcie_root_cap(root_cap: Arc<Mutex<PcieRootCap>>) {
    PCIE_ROOTS_CAP.lock().push(root_cap);
}

fn get_pcie_root_cap(bus_num: u8) -> Option<Arc<Mutex<PcieRootCap>>> {
    for root_cap in PCIE_ROOTS_CAP.lock().iter() {
        let root_cap_lock = root_cap.lock();
        if root_cap_lock.secondary_bus_num <= bus_num
            && root_cap_lock.subordinate_bus_num >= bus_num
        {
            return Some(root_cap.clone());
        }
    }

    None
}

pub struct PciePort {
    device_id: u16,
    debug_label: String,
    preferred_address: Option<PciAddress>,
    pci_address: Option<PciAddress>,
    bus_range: PciBridgeBusRange,
    pcie_host: Option<PcieHostPort>,
    pcie_config: Arc<Mutex<PcieConfig>>,
    pm_config: Arc<Mutex<PmConfig>>,

    msi_config: Option<Arc<Mutex<MsiConfig>>>,

    // For PcieRootPort, root_cap point to itself
    // For PcieDownstreamPort or PciDownstreamPort, root_cap point to PcieRootPort its behind.
    root_cap: Arc<Mutex<PcieRootCap>>,
    port_type: PcieDevicePortType,

    prepare_hotplug: bool,
}

impl PciePort {
    /// Constructs a new PCIE port
    pub fn new(
        device_id: u16,
        debug_label: String,
        primary_bus_num: u8,
        secondary_bus_num: u8,
        slot_implemented: bool,
        port_type: PcieDevicePortType,
    ) -> Self {
        let bus_range = PciBridgeBusRange {
            primary: primary_bus_num,
            secondary: secondary_bus_num,
            subordinate: secondary_bus_num,
        };

        let root_cap = if port_type == PcieDevicePortType::RootPort {
            let cap = Arc::new(Mutex::new(PcieRootCap::new(
                secondary_bus_num,
                secondary_bus_num,
            )));
            push_pcie_root_cap(cap.clone());
            cap
        } else {
            get_pcie_root_cap(primary_bus_num).expect("Pcie root port should be created at first")
        };

        PciePort {
            device_id,
            debug_label,
            preferred_address: None,
            pci_address: None,
            bus_range,
            pcie_host: None,
            msi_config: None,
            pcie_config: Arc::new(Mutex::new(PcieConfig::new(
                root_cap.clone(),
                slot_implemented,
                port_type,
            ))),
            pm_config: Arc::new(Mutex::new(PmConfig::new(false))),

            root_cap,
            port_type,

            prepare_hotplug: false,
        }
    }

    pub fn new_from_host(
        pcie_host: PcieHostPort,
        slot_implemented: bool,
        port_type: PcieDevicePortType,
    ) -> std::result::Result<Self, PciDeviceError> {
        let bus_range = pcie_host.get_bus_range();
        let host_address = PciAddress::from_str(&pcie_host.host_name())
            .map_err(|e| PciDeviceError::PciAddressParseFailure(pcie_host.host_name(), e))?;
        let root_cap = if port_type == PcieDevicePortType::RootPort {
            let cap = Arc::new(Mutex::new(PcieRootCap::new(
                bus_range.secondary,
                bus_range.subordinate,
            )));
            push_pcie_root_cap(cap.clone());
            cap
        } else {
            get_pcie_root_cap(bus_range.primary).expect("Pcie root port should be created at first")
        };

        Ok(PciePort {
            device_id: pcie_host.read_device_id(),
            debug_label: pcie_host.host_name(),
            preferred_address: Some(host_address),
            pci_address: None,
            bus_range,
            pcie_host: Some(pcie_host),
            msi_config: None,
            pcie_config: Arc::new(Mutex::new(PcieConfig::new(
                root_cap.clone(),
                slot_implemented,
                port_type,
            ))),
            pm_config: Arc::new(Mutex::new(PmConfig::new(false))),

            root_cap,
            port_type,

            prepare_hotplug: false,
        })
    }

    pub fn get_device_id(&self) -> u16 {
        self.device_id
    }

    pub fn get_address(&self) -> Option<PciAddress> {
        self.pci_address
    }

    pub fn debug_label(&self) -> String {
        self.debug_label.clone()
    }

    pub fn preferred_address(&self) -> Option<PciAddress> {
        self.preferred_address
    }

    pub fn allocate_address(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<PciAddress, PciDeviceError> {
        if self.pci_address.is_none() {
            if let Some(address) = self.preferred_address {
                if resources.reserve_pci(address, self.debug_label()) {
                    self.pci_address = Some(address);
                } else {
                    self.pci_address = None;
                }
            } else {
                self.pci_address =
                    resources.allocate_pci(self.bus_range.primary, self.debug_label());
            }
        }
        self.pci_address.ok_or(PciDeviceError::PciAllocationFailed)
    }

    pub fn read_config(&self, reg_idx: usize, data: &mut u32) {
        if let Some(host) = &self.pcie_host {
            host.read_config(reg_idx, data);
        }
    }

    pub fn write_config(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if let Some(host) = self.pcie_host.as_mut() {
            host.write_config(reg_idx, offset, data);
        }
    }

    pub fn handle_cap_write_result(&mut self, res: Box<dyn PciCapConfigWriteResult>) {
        if let Some(status) = res.downcast_ref::<PmStatusChange>() {
            if status.from == PciDevicePower::D3
                && status.to == PciDevicePower::D0
                && self.prepare_hotplug
            {
                if let Some(host) = self.pcie_host.as_mut() {
                    host.hotplug_probe();
                    self.prepare_hotplug = false;
                }
            }
        }
    }

    pub fn get_caps(&self) -> Vec<(Box<dyn PciCapability>, Option<Box<dyn PciCapConfig>>)> {
        vec![
            (
                Box::new(PcieCap::new(self.port_type, self.hotplug_implemented(), 0)),
                Some(Box::new(self.pcie_config.clone())),
            ),
            (
                Box::new(PciPmCap::new()),
                Some(Box::new(self.pm_config.clone())),
            ),
        ]
    }

    pub fn get_bus_range(&self) -> Option<PciBridgeBusRange> {
        Some(self.bus_range)
    }

    pub fn get_bridge_window_size(&self) -> (u64, u64) {
        if let Some(host) = &self.pcie_host {
            host.get_bridge_window_size()
        } else {
            (PCIE_BR_MEM_SIZE, PCIE_BR_PREF_MEM_SIZE)
        }
    }

    pub fn get_slot_control(&self) -> u16 {
        self.pcie_config.lock().get_slot_control()
    }

    pub fn clone_interrupt(&mut self, msi_config: Arc<Mutex<MsiConfig>>) {
        if self.port_type == PcieDevicePortType::RootPort {
            self.root_cap.lock().clone_interrupt(msi_config.clone());
        }
        self.pcie_config.lock().msi_config = Some(msi_config.clone());
        self.msi_config = Some(msi_config);
    }

    pub fn hotplug_implemented(&self) -> bool {
        self.pcie_config.lock().slot_control.is_some()
    }

    pub fn inject_pme(&mut self, requester_id: u16) {
        let mut r = self.root_cap.lock();
        if (r.status & PCIE_ROOTSTA_PME_STATUS) != 0 {
            r.status |= PCIE_ROOTSTA_PME_PENDING;
            r.pme_pending_requester_id = Some(requester_id);
        } else {
            r.status &= !PCIE_ROOTSTA_PME_REQ_ID_MASK;
            r.status |= requester_id as u32;
            r.pme_pending_requester_id = None;
            r.status |= PCIE_ROOTSTA_PME_STATUS;
            r.trigger_pme_interrupt();
        }
    }

    /// Has command completion pending.
    pub fn is_hpc_pending(&self) -> bool {
        self.pcie_config.lock().hpc_sender.is_some()
    }

    /// Sets a sender for hot plug or unplug complete.
    pub fn set_hpc_sender(&mut self, event: Event) {
        self.pcie_config
            .lock()
            .hpc_sender
            .replace(HotPlugCompleteSender::new(event));
    }

    pub fn trigger_hp_or_pme_interrupt(&mut self) {
        if self.pm_config.lock().should_trigger_pme() {
            self.pcie_config.lock().hp_interrupt_pending = true;
            self.inject_pme(self.pci_address.unwrap().pme_requester_id());
        } else {
            self.pcie_config.lock().trigger_hp_interrupt();
        }
    }

    pub fn is_host(&self) -> bool {
        self.pcie_host.is_some()
    }

    /// Checks if the slot is enabled by guest and ready for hotplug events.
    pub fn is_hotplug_ready(&self) -> bool {
        self.pcie_config.lock().is_hotplug_ready()
    }

    /// Gets a notification when the port is ready for hotplug.  If the port is already ready, then
    /// the notification event is triggerred immediately.
    pub fn get_ready_notification(&mut self) -> std::result::Result<Event, PciDeviceError> {
        self.pcie_config.lock().get_ready_notification()
    }

    pub fn hot_unplug(&mut self) {
        if let Some(host) = self.pcie_host.as_mut() {
            host.hot_unplug()
        }
    }

    pub fn is_match(&self, host_addr: PciAddress) -> Option<u8> {
        if host_addr.bus == self.bus_range.secondary || self.pcie_host.is_none() {
            Some(self.bus_range.secondary)
        } else {
            None
        }
    }

    pub fn removed_downstream_valid(&self) -> bool {
        self.pcie_config.lock().removed_downstream_valid
    }

    pub fn mask_slot_status(&mut self, mask: u16) {
        self.pcie_config.lock().mask_slot_status(mask);
    }

    pub fn set_slot_status(&mut self, flag: u16) {
        self.pcie_config.lock().set_slot_status(flag);
    }

    pub fn should_trigger_pme(&mut self) -> bool {
        self.pm_config.lock().should_trigger_pme()
    }

    pub fn prepare_hotplug(&mut self) {
        self.prepare_hotplug = true;
    }
}

#[cfg(test)]
impl PciePort {
    /// Test helper: simulate guest enabling the hotplug slot by writing the required
    /// SLTCTL bits directly to the PcieConfig.
    pub fn enable_slot_for_test(&mut self) {
        let mut config = self.pcie_config.lock();
        let enable_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        config.write_pcie_cap(PCIE_SLTCTL_OFFSET, &enable_value.to_le_bytes());
    }

    /// Test helper: set the power indicator to ON via SLTCTL.
    pub fn set_pic_on_for_test(&mut self) {
        let mut config = self.pcie_config.lock();
        let value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        config.write_pcie_cap(PCIE_SLTCTL_OFFSET, &value.to_le_bytes());
    }

    /// Test helper: set the power indicator to BLINK via SLTCTL.
    pub fn set_pic_blink_for_test(&mut self) {
        let mut config = self.pcie_config.lock();
        let value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_BLINK
            | PCIE_SLTCTL_AIC_OFF;
        config.write_pcie_cap(PCIE_SLTCTL_OFFSET, &value.to_le_bytes());
    }
}

struct HotPlugCompleteSender {
    sender: Event,
    armed: bool,
}

impl HotPlugCompleteSender {
    fn new(sender: Event) -> Self {
        Self {
            sender,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn armed(&self) -> bool {
        self.armed
    }

    fn signal(&self) -> base::Result<()> {
        self.sender.signal()
    }
}

pub struct PcieConfig {
    msi_config: Option<Arc<Mutex<MsiConfig>>>,

    slot_control: Option<u16>,
    slot_status: u16,

    // For PcieRootPort, root_cap point to itself
    // For PcieDownstreamPort or PciDownstreamPort, root_cap point to PcieRootPort its behind.
    root_cap: Arc<Mutex<PcieRootCap>>,
    port_type: PcieDevicePortType,

    hpc_sender: Option<HotPlugCompleteSender>,
    hp_interrupt_pending: bool,
    removed_downstream_valid: bool,

    enabled: bool,
    hot_plug_ready_notifications: Vec<Event>,
    cap_mapping: Option<PciCapMapping>,
}

impl PcieConfig {
    fn new(
        root_cap: Arc<Mutex<PcieRootCap>>,
        slot_implemented: bool,
        port_type: PcieDevicePortType,
    ) -> Self {
        PcieConfig {
            msi_config: None,

            slot_control: if slot_implemented {
                Some(PCIE_SLTCTL_PIC_OFF | PCIE_SLTCTL_AIC_OFF)
            } else {
                None
            },
            slot_status: 0,

            root_cap,
            port_type,

            hpc_sender: None,
            hp_interrupt_pending: false,
            removed_downstream_valid: false,

            enabled: false,
            hot_plug_ready_notifications: Vec::new(),
            cap_mapping: None,
        }
    }

    fn read_pcie_cap(&self, offset: usize, data: &mut u32) {
        if offset == PCIE_SLTCTL_OFFSET {
            *data = ((self.slot_status as u32) << 16) | (self.get_slot_control() as u32);
        } else if offset == PCIE_ROOTCTL_OFFSET {
            *data = match self.port_type {
                PcieDevicePortType::RootPort => self.root_cap.lock().control as u32,
                _ => 0,
            };
        } else if offset == PCIE_ROOTSTA_OFFSET {
            *data = match self.port_type {
                PcieDevicePortType::RootPort => self.root_cap.lock().status,
                _ => 0,
            };
        }
    }

    // Checks if the slot is enabled by guest and ready for hotplug events.
    fn is_hotplug_ready(&self) -> bool {
        // The hotplug capability flags are set when the guest enables the device. Checks all flags
        // required by the hotplug mechanism.
        let slot_control = self.get_slot_control();
        (slot_control & (PCIE_SLTCTL_PDCE | PCIE_SLTCTL_ABPE)) != 0
            && (slot_control & PCIE_SLTCTL_CCIE) != 0
            && (slot_control & PCIE_SLTCTL_HPIE) != 0
    }

    /// Gets a notification when the port is ready for hotplug. If the port is already ready, then
    /// the notification event is triggerred immediately.
    fn get_ready_notification(&mut self) -> std::result::Result<Event, PciDeviceError> {
        let event = Event::new().map_err(|e| PciDeviceError::EventCreationFailed(e.errno()))?;
        if self.is_hotplug_ready() {
            event
                .signal()
                .map_err(|e| PciDeviceError::EventSignalFailed(e.errno()))?;
        } else {
            self.hot_plug_ready_notifications.push(
                event
                    .try_clone()
                    .map_err(|e| PciDeviceError::EventCloneFailed(e.errno()))?,
            );
        }
        Ok(event)
    }

    fn write_pcie_cap(&mut self, offset: usize, data: &[u8]) {
        self.removed_downstream_valid = false;
        match offset {
            PCIE_SLTCTL_OFFSET => {
                let Ok(value) = data.try_into().map(u16::from_le_bytes) else {
                    warn!("write SLTCTL isn't word, len: {}", data.len());
                    return;
                };
                if !self.enabled
                    && (value & (PCIE_SLTCTL_PDCE | PCIE_SLTCTL_ABPE)) != 0
                    && (value & PCIE_SLTCTL_CCIE) != 0
                    && (value & PCIE_SLTCTL_HPIE) != 0
                {
                    // Device is getting enabled by the guest.
                    for notf_event in self.hot_plug_ready_notifications.drain(..) {
                        if let Err(e) = notf_event.signal() {
                            error!("Failed to signal hot plug ready: {}", e);
                        }
                    }
                    self.enabled = true;
                }

                // if slot is populated, power indicator is off,
                // it will detach devices
                let old_control = self.get_slot_control();
                match self.slot_control.as_mut() {
                    Some(v) => *v = value,
                    None => return,
                }
                if (self.slot_status & PCIE_SLTSTA_PDS != 0)
                    && (value & PCIE_SLTCTL_PIC == PCIE_SLTCTL_PIC_OFF)
                    && (old_control & PCIE_SLTCTL_PIC != PCIE_SLTCTL_PIC_OFF)
                {
                    self.removed_downstream_valid = true;
                    self.slot_status &= !PCIE_SLTSTA_PDS;
                    self.trigger_hp_interrupt();
                }

                // Guest enable hotplug interrupt and has hotplug interrupt
                // pending, inject it right row.
                if (old_control & PCIE_SLTCTL_HPIE == 0)
                    && (value & PCIE_SLTCTL_HPIE == PCIE_SLTCTL_HPIE)
                    && self.hp_interrupt_pending
                {
                    self.hp_interrupt_pending = false;
                    self.trigger_hp_interrupt();
                }

                if old_control != value {
                    let old_pic_state = old_control & PCIE_SLTCTL_PIC;
                    let pic_state = value & PCIE_SLTCTL_PIC;
                    if old_pic_state == PCIE_SLTCTL_PIC_BLINK && old_pic_state != pic_state {
                        // The power indicator (PIC) is controled by the guest to indicate the power
                        // state of the slot.
                        // For successful hotplug: OFF => BLINK => (board enabled) => ON
                        // For failed hotplug: OFF => BLINK => (board enable failed) => OFF
                        // For hot unplug: ON => BLINK => (board disabled) => OFF
                        // hot (un)plug is completed at next slot status write after it changed to
                        // ON or OFF state.

                        if let Some(sender) = self.hpc_sender.as_mut() {
                            sender.arm();
                        }
                    }
                    self.slot_status |= PCIE_SLTSTA_CC;
                    self.trigger_cc_interrupt();
                }
            }
            PCIE_SLTSTA_OFFSET => {
                if self.slot_control.is_none() {
                    return;
                }
                if let Some(hpc_sender) = self.hpc_sender.as_mut() {
                    if hpc_sender.armed() {
                        if let Err(e) = hpc_sender.signal() {
                            error!("Failed to send hot un/plug complete signal: {}", e);
                        }
                        self.hpc_sender = None;
                    }
                }
                let Ok(value) = data.try_into().map(u16::from_le_bytes) else {
                    warn!("write SLTSTA isn't word, len: {}", data.len());
                    return;
                };
                if value & PCIE_SLTSTA_ABP != 0 {
                    self.slot_status &= !PCIE_SLTSTA_ABP;
                }
                if value & PCIE_SLTSTA_PFD != 0 {
                    self.slot_status &= !PCIE_SLTSTA_PFD;
                }
                if value & PCIE_SLTSTA_PDC != 0 {
                    self.slot_status &= !PCIE_SLTSTA_PDC;
                }
                if value & PCIE_SLTSTA_CC != 0 {
                    self.slot_status &= !PCIE_SLTSTA_CC;
                }
                if value & PCIE_SLTSTA_DLLSC != 0 {
                    self.slot_status &= !PCIE_SLTSTA_DLLSC;
                }
            }
            PCIE_ROOTCTL_OFFSET => {
                let Ok(v) = data.try_into().map(u16::from_le_bytes) else {
                    warn!("write root control isn't word, len: {}", data.len());
                    return;
                };
                if self.port_type == PcieDevicePortType::RootPort {
                    self.root_cap.lock().control = v;
                } else {
                    warn!("write root control register while device isn't root port");
                }
            }
            PCIE_ROOTSTA_OFFSET => {
                let Ok(v) = data.try_into().map(u32::from_le_bytes) else {
                    warn!("write root status isn't dword, len: {}", data.len());
                    return;
                };
                if self.port_type == PcieDevicePortType::RootPort {
                    if v & PCIE_ROOTSTA_PME_STATUS != 0 {
                        let mut r = self.root_cap.lock();
                        if let Some(requester_id) = r.pme_pending_requester_id {
                            r.status &= !PCIE_ROOTSTA_PME_PENDING;
                            r.status &= !PCIE_ROOTSTA_PME_REQ_ID_MASK;
                            r.status |= requester_id as u32;
                            r.status |= PCIE_ROOTSTA_PME_STATUS;
                            r.pme_pending_requester_id = None;
                            r.trigger_pme_interrupt();
                        } else {
                            r.status &= !PCIE_ROOTSTA_PME_STATUS;
                        }
                    }
                } else {
                    warn!("write root status register while device isn't root port");
                }
            }
            _ => (),
        }
    }

    fn get_slot_control(&self) -> u16 {
        if let Some(slot_control) = self.slot_control {
            return slot_control;
        }
        0
    }

    fn trigger_cc_interrupt(&self) {
        if (self.get_slot_control() & PCIE_SLTCTL_CCIE) != 0
            && (self.slot_status & PCIE_SLTSTA_CC) != 0
        {
            trigger_interrupt(&self.msi_config)
        }
    }

    fn trigger_hp_interrupt(&mut self) {
        let slot_control = self.get_slot_control();
        if (slot_control & PCIE_SLTCTL_HPIE) != 0 {
            self.set_slot_status(PCIE_SLTSTA_PDC);
            if (self.slot_status & slot_control & (PCIE_SLTCTL_ABPE | PCIE_SLTCTL_PDCE)) != 0 {
                trigger_interrupt(&self.msi_config)
            }
        }
    }

    fn mask_slot_status(&mut self, mask: u16) {
        self.slot_status &= mask;
        if let Some(mapping) = self.cap_mapping.as_mut() {
            mapping.set_reg(
                PCIE_SLTCTL_OFFSET / 4,
                (self.slot_status as u32) << 16,
                0xffff0000,
            );
        }
    }

    fn set_slot_status(&mut self, flag: u16) {
        self.slot_status |= flag;
        if let Some(mapping) = self.cap_mapping.as_mut() {
            mapping.set_reg(
                PCIE_SLTCTL_OFFSET / 4,
                (self.slot_status as u32) << 16,
                0xffff0000,
            );
        }
    }
}

const PCIE_CONFIG_READ_MASK: [u32; PCIE_CAP_LEN / 4] = {
    let mut arr: [u32; PCIE_CAP_LEN / 4] = [0; PCIE_CAP_LEN / 4];
    arr[PCIE_SLTCTL_OFFSET / 4] = 0xffffffff;
    arr[PCIE_ROOTCTL_OFFSET / 4] = 0xffffffff;
    arr[PCIE_ROOTSTA_OFFSET / 4] = 0xffffffff;
    arr
};

impl PciCapConfig for PcieConfig {
    fn read_mask(&self) -> &'static [u32] {
        &PCIE_CONFIG_READ_MASK
    }

    fn read_reg(&self, reg_idx: usize) -> u32 {
        let mut data = 0;
        self.read_pcie_cap(reg_idx * 4, &mut data);
        data
    }

    fn write_reg(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Box<dyn PciCapConfigWriteResult>> {
        self.write_pcie_cap(reg_idx * 4 + offset as usize, data);
        None
    }

    fn set_cap_mapping(&mut self, mapping: PciCapMapping) {
        self.cap_mapping = Some(mapping);
    }
}

/// Helper trait for implementing PcieDevice where most functions
/// are proxied directly to a PciePort instance.
#[cfg(test)]
impl PcieConfig {
    /// Test helper: get current slot_status.
    fn get_slot_status(&self) -> u16 {
        self.slot_status
    }

    /// Test helper: check if hpc_sender is present.
    fn has_hpc_sender(&self) -> bool {
        self.hpc_sender.is_some()
    }

    /// Test helper: check if hpc_sender is armed.
    fn is_hpc_armed(&self) -> bool {
        self.hpc_sender.as_ref().is_some_and(|s| s.armed())
    }
}

pub trait PciePortVariant: Send {
    fn get_pcie_port(&self) -> &PciePort;
    fn get_pcie_port_mut(&mut self) -> &mut PciePort;

    /// Called via PcieDevice.get_removed_devices
    fn get_removed_devices_impl(&self) -> Vec<PciAddress>;

    /// Called via PcieDevice.hotplug_implemented
    fn hotplug_implemented_impl(&self) -> bool;

    /// Called via PcieDevice.hotplug
    fn hotplugged_impl(&self) -> bool;
}

impl<T: PciePortVariant> PcieDevice for T {
    fn get_device_id(&self) -> u16 {
        self.get_pcie_port().get_device_id()
    }

    fn debug_label(&self) -> String {
        self.get_pcie_port().debug_label()
    }

    fn preferred_address(&self) -> Option<PciAddress> {
        self.get_pcie_port().preferred_address()
    }

    fn allocate_address(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<PciAddress, PciDeviceError> {
        self.get_pcie_port_mut().allocate_address(resources)
    }

    fn clone_interrupt(&mut self, msi_config: Arc<Mutex<MsiConfig>>) {
        self.get_pcie_port_mut().clone_interrupt(msi_config);
    }

    fn read_config(&self, reg_idx: usize, data: &mut u32) {
        self.get_pcie_port().read_config(reg_idx, data);
    }

    fn write_config(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        self.get_pcie_port_mut().write_config(reg_idx, offset, data);
    }

    fn get_caps(&self) -> Vec<(Box<dyn PciCapability>, Option<Box<dyn PciCapConfig>>)> {
        self.get_pcie_port().get_caps()
    }

    fn handle_cap_write_result(&mut self, res: Box<dyn PciCapConfigWriteResult>) {
        self.get_pcie_port_mut().handle_cap_write_result(res)
    }

    fn get_bus_range(&self) -> Option<PciBridgeBusRange> {
        self.get_pcie_port().get_bus_range()
    }

    fn get_removed_devices(&self) -> Vec<PciAddress> {
        self.get_removed_devices_impl()
    }

    fn hotplug_implemented(&self) -> bool {
        self.hotplug_implemented_impl()
    }

    fn hotplugged(&self) -> bool {
        self.hotplugged_impl()
    }

    fn get_bridge_window_size(&self) -> (u64, u64) {
        self.get_pcie_port().get_bridge_window_size()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a PcieConfig for testing with slot_implemented=true and RootPort type.
    fn new_test_config() -> PcieConfig {
        let root_cap = Arc::new(Mutex::new(PcieRootCap::new(1, 1)));
        PcieConfig::new(root_cap, true, PcieDevicePortType::RootPort)
    }

    /// Creates a PcieConfig for testing with slot_implemented=false.
    fn new_test_config_no_slot() -> PcieConfig {
        let root_cap = Arc::new(Mutex::new(PcieRootCap::new(1, 1)));
        PcieConfig::new(root_cap, false, PcieDevicePortType::RootPort)
    }

    /// Helper: write a u16 value to SLTCTL offset.
    fn write_sltctl(config: &mut PcieConfig, value: u16) {
        config.write_pcie_cap(PCIE_SLTCTL_OFFSET, &value.to_le_bytes());
    }

    /// Helper: write a u16 value to SLTSTA offset.
    fn write_sltsta(config: &mut PcieConfig, value: u16) {
        config.write_pcie_cap(PCIE_SLTSTA_OFFSET, &value.to_le_bytes());
    }

    #[test]
    fn sltctl_enable_drains_ready_notifications() {
        let mut config = new_test_config();
        // Get a ready notification event before enabling.
        let notf_event = config.get_ready_notification().unwrap();
        assert!(!config.is_hotplug_ready());

        // Write SLTCTL with PDCE + ABPE + CCIE + HPIE to enable the slot.
        let enable_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, enable_value);

        // Notification should be signaled (readable without blocking).
        assert!(config.enabled);
        // The event should have been signaled.
        notf_event
            .wait_timeout(std::time::Duration::from_millis(10))
            .unwrap();

        // Verify is_hotplug_ready returns true.
        assert!(config.is_hotplug_ready());
    }

    #[test]
    fn sltctl_enable_already_ready_signals_immediately() {
        let mut config = new_test_config();

        // Enable the slot first.
        let enable_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, enable_value);

        // Now get a ready notification - should be signaled immediately since already enabled.
        let notf_event = config.get_ready_notification().unwrap();
        notf_event
            .wait_timeout(std::time::Duration::from_millis(10))
            .unwrap();
    }

    #[test]
    fn power_indicator_off_to_blink_to_on_plug_success() {
        let mut config = new_test_config();

        // Enable the slot.
        let enable_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, enable_value);

        // Simulate PDS being set (device present).
        config.set_slot_status(PCIE_SLTSTA_PDS);

        // Transition PIC OFF -> BLINK (guest starts plug sequence).
        let blink_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_BLINK
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, blink_value);

        // CC bit should be set since slot control changed.
        assert!(config.get_slot_status() & PCIE_SLTSTA_CC != 0);

        // Set up HPC sender before the BLINK->ON transition.
        let hpc_event = Event::new().unwrap();
        let hpc_recvr = hpc_event.try_clone().unwrap();
        config.hpc_sender = Some(HotPlugCompleteSender::new(hpc_event));

        // Transition PIC BLINK -> ON (plug success).
        let on_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, on_value);

        // The hpc_sender should now be armed.
        assert!(config.is_hpc_armed());

        // Next SLTSTA write should signal the hpc_sender.
        write_sltsta(&mut config, PCIE_SLTSTA_CC);

        // Sender should be consumed.
        assert!(!config.has_hpc_sender());
        // Receiver should be signaled.
        hpc_recvr
            .wait_timeout(std::time::Duration::from_millis(10))
            .unwrap();
    }

    #[test]
    fn power_indicator_off_to_blink_to_off_plug_fail() {
        let mut config = new_test_config();

        // Enable the slot.
        let enable_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, enable_value);

        // Simulate PDS being set.
        config.set_slot_status(PCIE_SLTSTA_PDS);

        // Transition PIC OFF -> BLINK.
        let blink_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_BLINK
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, blink_value);

        // Set up HPC sender.
        let hpc_event = Event::new().unwrap();
        let hpc_recvr = hpc_event.try_clone().unwrap();
        config.hpc_sender = Some(HotPlugCompleteSender::new(hpc_event));

        // Transition PIC BLINK -> OFF (plug failure).
        let off_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, off_value);

        // HPC sender should be armed (BLINK -> OFF triggers arming).
        assert!(config.is_hpc_armed());

        // When PIC goes to OFF and PDS was set, removed_downstream_valid should be set
        // and PDS should be cleared.
        assert!(config.removed_downstream_valid);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_PDS, 0);

        // Next SLTSTA write should signal the hpc_sender.
        write_sltsta(&mut config, PCIE_SLTSTA_CC);
        assert!(!config.has_hpc_sender());
        hpc_recvr
            .wait_timeout(std::time::Duration::from_millis(10))
            .unwrap();
    }

    #[test]
    fn power_indicator_on_to_blink_to_off_unplug() {
        let mut config = new_test_config();

        // Enable the slot with PIC_ON.
        let on_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, on_value);

        // Simulate PDS being set.
        config.set_slot_status(PCIE_SLTSTA_PDS);
        config.enabled = true;

        // Transition PIC ON -> BLINK.
        let blink_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_BLINK
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, blink_value);

        // Set up HPC sender.
        let hpc_event = Event::new().unwrap();
        let hpc_recvr = hpc_event.try_clone().unwrap();
        config.hpc_sender = Some(HotPlugCompleteSender::new(hpc_event));

        // Transition PIC BLINK -> OFF (unplug).
        let off_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, off_value);

        // HPC sender should be armed.
        assert!(config.is_hpc_armed());

        // PDS was set and PIC went to OFF, so removed_downstream_valid should be set.
        assert!(config.removed_downstream_valid);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_PDS, 0);

        // Signal completion via SLTSTA write.
        write_sltsta(&mut config, PCIE_SLTSTA_CC);
        assert!(!config.has_hpc_sender());
        hpc_recvr
            .wait_timeout(std::time::Duration::from_millis(10))
            .unwrap();
    }

    #[test]
    fn removed_downstream_valid_set_when_pds_cleared_by_pic_off() {
        let mut config = new_test_config();

        // Enable with PIC_ON.
        let on_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, on_value);
        config.enabled = true;

        // Set PDS.
        config.set_slot_status(PCIE_SLTSTA_PDS);
        assert!(!config.removed_downstream_valid);

        // Transition PIC ON -> OFF.
        let off_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, off_value);

        // removed_downstream_valid should be set.
        assert!(config.removed_downstream_valid);
        // PDS should be cleared.
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_PDS, 0);
    }

    #[test]
    fn removed_downstream_valid_not_set_when_pds_not_set() {
        let mut config = new_test_config();

        // Enable with PIC_ON.
        let on_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, on_value);
        config.enabled = true;

        // Don't set PDS. Transition PIC ON -> OFF.
        let off_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, off_value);

        // removed_downstream_valid should NOT be set since PDS was not set.
        assert!(!config.removed_downstream_valid);
    }

    #[test]
    fn sltsta_w1c_behavior() {
        let mut config = new_test_config();

        // Set various slot status bits.
        config.set_slot_status(
            PCIE_SLTSTA_ABP
                | PCIE_SLTSTA_PFD
                | PCIE_SLTSTA_PDC
                | PCIE_SLTSTA_CC
                | PCIE_SLTSTA_DLLSC,
        );
        assert_ne!(config.get_slot_status(), 0);

        // Write 1 to ABP to clear it.
        write_sltsta(&mut config, PCIE_SLTSTA_ABP);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_ABP, 0);
        // Others should still be set.
        assert_ne!(config.get_slot_status() & PCIE_SLTSTA_PFD, 0);
        assert_ne!(config.get_slot_status() & PCIE_SLTSTA_PDC, 0);
        assert_ne!(config.get_slot_status() & PCIE_SLTSTA_CC, 0);
        assert_ne!(config.get_slot_status() & PCIE_SLTSTA_DLLSC, 0);

        // Write 1 to PFD to clear it.
        write_sltsta(&mut config, PCIE_SLTSTA_PFD);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_PFD, 0);

        // Write 1 to PDC to clear it.
        write_sltsta(&mut config, PCIE_SLTSTA_PDC);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_PDC, 0);

        // Write 1 to CC to clear it.
        write_sltsta(&mut config, PCIE_SLTSTA_CC);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_CC, 0);

        // Write 1 to DLLSC to clear it.
        write_sltsta(&mut config, PCIE_SLTSTA_DLLSC);
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_DLLSC, 0);

        // All should be clear now.
        assert_eq!(config.get_slot_status(), 0);
    }

    #[test]
    fn sltsta_w1c_clears_multiple_bits_at_once() {
        let mut config = new_test_config();

        // Set all W1C bits.
        config.set_slot_status(
            PCIE_SLTSTA_ABP
                | PCIE_SLTSTA_PFD
                | PCIE_SLTSTA_PDC
                | PCIE_SLTSTA_CC
                | PCIE_SLTSTA_DLLSC,
        );

        // Clear all at once.
        write_sltsta(
            &mut config,
            PCIE_SLTSTA_ABP
                | PCIE_SLTSTA_PFD
                | PCIE_SLTSTA_PDC
                | PCIE_SLTSTA_CC
                | PCIE_SLTSTA_DLLSC,
        );
        // PDS should remain since it's not W1C via direct SLTSTA write in this path.
        assert_eq!(
            config.get_slot_status()
                & (PCIE_SLTSTA_ABP
                    | PCIE_SLTSTA_PFD
                    | PCIE_SLTSTA_PDC
                    | PCIE_SLTSTA_CC
                    | PCIE_SLTSTA_DLLSC),
            0
        );
    }

    #[test]
    fn deferred_hp_interrupt_fires_when_hpie_enabled() {
        let mut config = new_test_config();

        // Start with HPIE disabled but set hp_interrupt_pending.
        config.hp_interrupt_pending = true;

        // Write SLTCTL with all enables including HPIE.
        let enable_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, enable_value);

        // hp_interrupt_pending should be cleared (consumed).
        assert!(!config.hp_interrupt_pending);
    }

    #[test]
    fn no_slot_impl_sltctl_write_is_noop() {
        let mut config = new_test_config_no_slot();

        // Writing SLTCTL on a port without slot should be a no-op.
        let value = PCIE_SLTCTL_PDCE | PCIE_SLTCTL_ABPE | PCIE_SLTCTL_CCIE | PCIE_SLTCTL_HPIE;
        write_sltctl(&mut config, value);

        // Slot control should remain None (get_slot_control returns 0).
        assert_eq!(config.get_slot_control(), 0);
        assert!(!config.enabled);
    }

    #[test]
    fn no_slot_impl_sltsta_write_is_noop() {
        let mut config = new_test_config_no_slot();

        // Writing SLTSTA on a port without slot should be a no-op.
        write_sltsta(&mut config, PCIE_SLTSTA_ABP);
        // Nothing should crash, slot_status should remain 0.
        assert_eq!(config.get_slot_status(), 0);
    }

    #[test]
    fn hpc_sender_not_armed_if_pic_not_transitioning_from_blink() {
        let mut config = new_test_config();

        // Enable with PIC_OFF.
        let off_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, off_value);
        config.enabled = true;

        // Set up HPC sender.
        let hpc_event = Event::new().unwrap();
        config.hpc_sender = Some(HotPlugCompleteSender::new(hpc_event));

        // Transition PIC OFF -> ON (not from BLINK).
        let on_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, on_value);

        // HPC sender should NOT be armed (transition was not from BLINK).
        assert!(!config.is_hpc_armed());
    }

    #[test]
    fn sltsta_write_does_not_signal_unarmed_hpc_sender() {
        let mut config = new_test_config();

        // Set up an unarmed HPC sender.
        let hpc_event = Event::new().unwrap();
        config.hpc_sender = Some(HotPlugCompleteSender::new(hpc_event));

        // Write to SLTSTA.
        write_sltsta(&mut config, PCIE_SLTSTA_CC);

        // HPC sender should still be present (not consumed) since it wasn't armed.
        assert!(config.has_hpc_sender());
    }

    #[test]
    fn read_sltctl_returns_correct_values() {
        let mut config = new_test_config();

        // Set specific slot control and status.
        let ctl_value = PCIE_SLTCTL_PDCE | PCIE_SLTCTL_HPIE | PCIE_SLTCTL_PIC_ON;
        write_sltctl(&mut config, ctl_value);

        config.set_slot_status(PCIE_SLTSTA_PDS | PCIE_SLTSTA_ABP);

        let mut data = 0u32;
        config.read_pcie_cap(PCIE_SLTCTL_OFFSET, &mut data);

        // Lower 16 bits should be slot control, upper 16 bits should be slot status.
        let read_ctl = (data & 0xFFFF) as u16;
        let read_sta = ((data >> 16) & 0xFFFF) as u16;
        assert_eq!(read_ctl, ctl_value);
        assert_eq!(
            read_sta & (PCIE_SLTSTA_PDS | PCIE_SLTSTA_ABP),
            PCIE_SLTSTA_PDS | PCIE_SLTSTA_ABP
        );
    }

    #[test]
    fn removed_downstream_valid_cleared_on_any_write() {
        let mut config = new_test_config();

        // Enable with PIC_ON and PDS set.
        let on_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_ON
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, on_value);
        config.enabled = true;
        config.set_slot_status(PCIE_SLTSTA_PDS);

        // Transition PIC ON -> OFF to set removed_downstream_valid.
        let off_value = PCIE_SLTCTL_PDCE
            | PCIE_SLTCTL_ABPE
            | PCIE_SLTCTL_CCIE
            | PCIE_SLTCTL_HPIE
            | PCIE_SLTCTL_PIC_OFF
            | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, off_value);
        assert!(config.removed_downstream_valid);

        // Any subsequent write to pcie_cap should clear removed_downstream_valid.
        write_sltctl(&mut config, off_value);
        assert!(!config.removed_downstream_valid);
    }

    #[test]
    fn cc_bit_set_on_slot_control_change() {
        let mut config = new_test_config();

        // Initial slot control is PIC_OFF | AIC_OFF.
        // Write a different value.
        let new_value = PCIE_SLTCTL_PDCE | PCIE_SLTCTL_PIC_OFF | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, new_value);

        // CC bit should be set.
        assert_ne!(config.get_slot_status() & PCIE_SLTSTA_CC, 0);
    }

    #[test]
    fn cc_bit_not_set_on_same_slot_control_write() {
        let mut config = new_test_config();

        // The default slot_control is PIC_OFF | AIC_OFF.
        let default_value = PCIE_SLTCTL_PIC_OFF | PCIE_SLTCTL_AIC_OFF;
        write_sltctl(&mut config, default_value);

        // CC bit should NOT be set since value didn't change.
        assert_eq!(config.get_slot_status() & PCIE_SLTSTA_CC, 0);
    }
}
