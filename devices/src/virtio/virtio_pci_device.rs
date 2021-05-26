// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use sync::Mutex;

use base::{warn, AsRawDescriptor, Event, RawDescriptor, Result, Tube};
use data_model::{DataInit, Le32};
use hypervisor::Datamatch;
use libc::ERANGE;
use resources::{Alloc, MmioType, SystemAllocator};
use vm_memory::GuestMemory;

use super::*;
use crate::pci::{
    MsixCap, MsixConfig, PciAddress, PciBarConfiguration, PciBarPrefetchable, PciBarRegionType,
    PciCapability, PciCapabilityID, PciClassCode, PciConfiguration, PciDevice, PciDeviceError,
    PciDisplaySubclass, PciHeaderType, PciInterruptPin, PciSubclass,
};

use self::virtio_pci_common_config::VirtioPciCommonConfig;

pub enum PciCapabilityType {
    CommonConfig = 1,
    NotifyConfig = 2,
    IsrConfig = 3,
    DeviceConfig = 4,
    PciConfig = 5,
    SharedMemoryConfig = 8,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioPciCap {
    // _cap_vndr and _cap_next are autofilled based on id() in pci configuration
    _cap_vndr: u8,    // Generic PCI field: PCI_CAP_ID_VNDR
    _cap_next: u8,    // Generic PCI field: next ptr
    cap_len: u8,      // Generic PCI field: capability length
    cfg_type: u8,     // Identifies the structure.
    bar: u8,          // Where to find it.
    id: u8,           // Multiple capabilities of the same type
    padding: [u8; 2], // Pad to full dword.
    offset: Le32,     // Offset within bar.
    length: Le32,     // Length of the structure, in bytes.
}
// It is safe to implement DataInit; all members are simple numbers and any value is valid.
unsafe impl DataInit for VirtioPciCap {}

impl PciCapability for VirtioPciCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

impl VirtioPciCap {
    pub fn new(cfg_type: PciCapabilityType, bar: u8, offset: u32, length: u32) -> Self {
        VirtioPciCap {
            _cap_vndr: 0,
            _cap_next: 0,
            cap_len: std::mem::size_of::<VirtioPciCap>() as u8,
            cfg_type: cfg_type as u8,
            bar,
            id: 0,
            padding: [0; 2],
            offset: Le32::from(offset),
            length: Le32::from(length),
        }
    }
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtioPciNotifyCap {
    cap: VirtioPciCap,
    notify_off_multiplier: Le32,
}
// It is safe to implement DataInit; all members are simple numbers and any value is valid.
unsafe impl DataInit for VirtioPciNotifyCap {}

impl PciCapability for VirtioPciNotifyCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

impl VirtioPciNotifyCap {
    pub fn new(
        cfg_type: PciCapabilityType,
        bar: u8,
        offset: u32,
        length: u32,
        multiplier: Le32,
    ) -> Self {
        VirtioPciNotifyCap {
            cap: VirtioPciCap {
                _cap_vndr: 0,
                _cap_next: 0,
                cap_len: std::mem::size_of::<VirtioPciNotifyCap>() as u8,
                cfg_type: cfg_type as u8,
                bar,
                id: 0,
                padding: [0; 2],
                offset: Le32::from(offset),
                length: Le32::from(length),
            },
            notify_off_multiplier: multiplier,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtioPciShmCap {
    cap: VirtioPciCap,
    offset_hi: Le32, // Most sig 32 bits of offset
    length_hi: Le32, // Most sig 32 bits of length
}
// It is safe to implement DataInit; all members are simple numbers and any value is valid.
unsafe impl DataInit for VirtioPciShmCap {}

impl PciCapability for VirtioPciShmCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

impl VirtioPciShmCap {
    pub fn new(cfg_type: PciCapabilityType, bar: u8, offset: u64, length: u64, shmid: u8) -> Self {
        VirtioPciShmCap {
            cap: VirtioPciCap {
                _cap_vndr: 0,
                _cap_next: 0,
                cap_len: std::mem::size_of::<VirtioPciShmCap>() as u8,
                cfg_type: cfg_type as u8,
                bar,
                id: shmid,
                padding: [0; 2],
                offset: Le32::from(offset as u32),
                length: Le32::from(length as u32),
            },
            offset_hi: Le32::from((offset >> 32) as u32),
            length_hi: Le32::from((length >> 32) as u32),
        }
    }
}

/// Subclasses for virtio.
#[allow(dead_code)]
#[derive(Copy, Clone)]
pub enum PciVirtioSubclass {
    NonTransitionalBase = 0xff,
}

impl PciSubclass for PciVirtioSubclass {
    fn get_register_value(&self) -> u8 {
        *self as u8
    }
}

// Allocate one bar for the structs pointed to by the capability structures.
const COMMON_CONFIG_BAR_OFFSET: u64 = 0x0000;
const COMMON_CONFIG_SIZE: u64 = 56;
const ISR_CONFIG_BAR_OFFSET: u64 = 0x1000;
const ISR_CONFIG_SIZE: u64 = 1;
const DEVICE_CONFIG_BAR_OFFSET: u64 = 0x2000;
const DEVICE_CONFIG_SIZE: u64 = 0x1000;
const NOTIFICATION_BAR_OFFSET: u64 = 0x3000;
const NOTIFICATION_SIZE: u64 = 0x1000;
const MSIX_TABLE_BAR_OFFSET: u64 = 0x6000;
const MSIX_TABLE_SIZE: u64 = 0x1000;
const MSIX_PBA_BAR_OFFSET: u64 = 0x7000;
const MSIX_PBA_SIZE: u64 = 0x1000;
const CAPABILITY_BAR_SIZE: u64 = 0x8000;

const NOTIFY_OFF_MULTIPLIER: u32 = 4; // A dword per notification address.

const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040; // Add to device type to get device ID.
const VIRTIO_PCI_REVISION_ID: u8 = 1;

/// Implements the
/// [PCI](http://docs.oasis-open.org/virtio/virtio/v1.0/cs04/virtio-v1.0-cs04.html#x1-650001)
/// transport for virtio devices.
pub struct VirtioPciDevice {
    config_regs: PciConfiguration,
    pci_address: Option<PciAddress>,

    device: Box<dyn VirtioDevice>,
    device_activated: bool,

    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: Option<Event>,
    interrupt_resample_evt: Option<Event>,
    queues: Vec<Queue>,
    queue_evts: Vec<Event>,
    mem: Option<GuestMemory>,
    settings_bar: u8,
    msix_config: Arc<Mutex<MsixConfig>>,
    msix_cap_reg_idx: Option<usize>,
    common_config: VirtioPciCommonConfig,
}

impl VirtioPciDevice {
    /// Constructs a new PCI transport for the given virtio device.
    pub fn new(
        mem: GuestMemory,
        device: Box<dyn VirtioDevice>,
        msi_device_tube: Tube,
    ) -> Result<Self> {
        let mut queue_evts = Vec::new();
        for _ in device.queue_max_sizes() {
            queue_evts.push(Event::new()?)
        }
        let queues = device
            .queue_max_sizes()
            .iter()
            .map(|&s| Queue::new(s))
            .collect();

        let pci_device_id = VIRTIO_PCI_DEVICE_ID_BASE + device.device_type() as u16;

        let (pci_device_class, pci_device_subclass) = match device.device_type() {
            TYPE_GPU => (
                PciClassCode::DisplayController,
                &PciDisplaySubclass::Other as &dyn PciSubclass,
            ),
            _ => (
                PciClassCode::Other,
                &PciVirtioSubclass::NonTransitionalBase as &dyn PciSubclass,
            ),
        };

        let num_queues = device.queue_max_sizes().len();

        // One MSI-X vector per queue plus one for configuration changes.
        let msix_num = u16::try_from(num_queues + 1).map_err(|_| base::Error::new(ERANGE))?;
        let msix_config = Arc::new(Mutex::new(MsixConfig::new(msix_num, msi_device_tube)));

        let config_regs = PciConfiguration::new(
            VIRTIO_PCI_VENDOR_ID,
            pci_device_id,
            pci_device_class,
            pci_device_subclass,
            None,
            PciHeaderType::Device,
            VIRTIO_PCI_VENDOR_ID,
            pci_device_id,
            VIRTIO_PCI_REVISION_ID,
        );

        Ok(VirtioPciDevice {
            config_regs,
            pci_address: None,
            device,
            device_activated: false,
            interrupt_status: Arc::new(AtomicUsize::new(0)),
            interrupt_evt: None,
            interrupt_resample_evt: None,
            queues,
            queue_evts,
            mem: Some(mem),
            settings_bar: 0,
            msix_config,
            msix_cap_reg_idx: None,
            common_config: VirtioPciCommonConfig {
                driver_status: 0,
                config_generation: 0,
                device_feature_select: 0,
                driver_feature_select: 0,
                queue_select: 0,
                msix_config: VIRTIO_MSI_NO_VECTOR,
            },
        })
    }

    fn is_driver_ready(&self) -> bool {
        let ready_bits =
            (DEVICE_ACKNOWLEDGE | DEVICE_DRIVER | DEVICE_DRIVER_OK | DEVICE_FEATURES_OK) as u8;
        self.common_config.driver_status == ready_bits
            && self.common_config.driver_status & DEVICE_FAILED as u8 == 0
    }

    /// Determines if the driver has requested the device reset itself
    fn is_reset_requested(&self) -> bool {
        self.common_config.driver_status == DEVICE_RESET as u8
    }

    fn are_queues_valid(&self) -> bool {
        if let Some(mem) = self.mem.as_ref() {
            // All queues marked as ready must be valid.
            self.queues
                .iter()
                .filter(|q| q.ready)
                .all(|q| q.is_valid(mem))
        } else {
            false
        }
    }

    fn add_settings_pci_capabilities(
        &mut self,
        settings_bar: u8,
    ) -> std::result::Result<(), PciDeviceError> {
        // Add pointers to the different configuration structures from the PCI capabilities.
        let common_cap = VirtioPciCap::new(
            PciCapabilityType::CommonConfig,
            settings_bar,
            COMMON_CONFIG_BAR_OFFSET as u32,
            COMMON_CONFIG_SIZE as u32,
        );
        self.config_regs
            .add_capability(&common_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let isr_cap = VirtioPciCap::new(
            PciCapabilityType::IsrConfig,
            settings_bar,
            ISR_CONFIG_BAR_OFFSET as u32,
            ISR_CONFIG_SIZE as u32,
        );
        self.config_regs
            .add_capability(&isr_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        // TODO(dgreid) - set based on device's configuration size?
        let device_cap = VirtioPciCap::new(
            PciCapabilityType::DeviceConfig,
            settings_bar,
            DEVICE_CONFIG_BAR_OFFSET as u32,
            DEVICE_CONFIG_SIZE as u32,
        );
        self.config_regs
            .add_capability(&device_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let notify_cap = VirtioPciNotifyCap::new(
            PciCapabilityType::NotifyConfig,
            settings_bar,
            NOTIFICATION_BAR_OFFSET as u32,
            NOTIFICATION_SIZE as u32,
            Le32::from(NOTIFY_OFF_MULTIPLIER),
        );
        self.config_regs
            .add_capability(&notify_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        //TODO(dgreid) - How will the configuration_cap work?
        let configuration_cap = VirtioPciCap::new(PciCapabilityType::PciConfig, 0, 0, 0);
        self.config_regs
            .add_capability(&configuration_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let msix_cap = MsixCap::new(
            settings_bar,
            self.msix_config.lock().num_vectors(),
            MSIX_TABLE_BAR_OFFSET as u32,
            settings_bar,
            MSIX_PBA_BAR_OFFSET as u32,
        );
        let msix_offset = self
            .config_regs
            .add_capability(&msix_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;
        self.msix_cap_reg_idx = Some(msix_offset / 4);

        self.settings_bar = settings_bar;
        Ok(())
    }

    fn clone_queue_evts(&self) -> Result<Vec<Event>> {
        self.queue_evts.iter().map(|e| e.try_clone()).collect()
    }
}

impl PciDevice for VirtioPciDevice {
    fn debug_label(&self) -> String {
        format!("pci{}", self.device.debug_label())
    }

    fn allocate_address(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<PciAddress, PciDeviceError> {
        if self.pci_address.is_none() {
            self.pci_address = match resources.allocate_pci(self.debug_label()) {
                Some(Alloc::PciBar {
                    bus,
                    dev,
                    func,
                    bar: _,
                }) => Some(PciAddress { bus, dev, func }),
                _ => None,
            }
        }
        self.pci_address.ok_or(PciDeviceError::PciAllocationFailed)
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut rds = self.device.keep_rds();
        if let Some(interrupt_evt) = &self.interrupt_evt {
            rds.push(interrupt_evt.as_raw_descriptor());
        }
        if let Some(interrupt_resample_evt) = &self.interrupt_resample_evt {
            rds.push(interrupt_resample_evt.as_raw_descriptor());
        }
        let descriptor = self.msix_config.lock().get_msi_socket();
        rds.push(descriptor);
        rds
    }

    fn assign_irq(
        &mut self,
        irq_evt: Event,
        irq_resample_evt: Event,
        irq_num: u32,
        irq_pin: PciInterruptPin,
    ) {
        self.config_regs.set_irq(irq_num as u8, irq_pin);
        self.interrupt_evt = Some(irq_evt);
        self.interrupt_resample_evt = Some(irq_resample_evt);
    }

    fn allocate_io_bars(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(u64, u64)>, PciDeviceError> {
        let address = self
            .pci_address
            .expect("allocaten_address must be called prior to allocate_io_bars");
        // Allocate one bar for the structures pointed to by the capability structures.
        let mut ranges = Vec::new();
        let settings_config_addr = resources
            .mmio_allocator(MmioType::Low)
            .allocate_with_align(
                CAPABILITY_BAR_SIZE,
                Alloc::PciBar {
                    bus: address.bus,
                    dev: address.dev,
                    func: address.func,
                    bar: 0,
                },
                format!(
                    "virtio-{}-cap_bar",
                    type_to_str(self.device.device_type()).unwrap_or("?")
                ),
                CAPABILITY_BAR_SIZE,
            )
            .map_err(|e| PciDeviceError::IoAllocationFailed(CAPABILITY_BAR_SIZE, e))?;
        let config = PciBarConfiguration::new(
            0,
            CAPABILITY_BAR_SIZE,
            PciBarRegionType::Memory32BitRegion,
            PciBarPrefetchable::NotPrefetchable,
        )
        .set_address(settings_config_addr);
        let settings_bar = self
            .config_regs
            .add_pci_bar(config)
            .map_err(|e| PciDeviceError::IoRegistrationFailed(settings_config_addr, e))?
            as u8;
        ranges.push((settings_config_addr, CAPABILITY_BAR_SIZE));

        // Once the BARs are allocated, the capabilities can be added to the PCI configuration.
        self.add_settings_pci_capabilities(settings_bar)?;

        Ok(ranges)
    }

    fn allocate_device_bars(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(u64, u64)>, PciDeviceError> {
        let address = self
            .pci_address
            .expect("allocaten_address must be called prior to allocate_device_bars");
        let mut ranges = Vec::new();
        for config in self.device.get_device_bars(address) {
            let device_addr = resources
                .mmio_allocator(MmioType::High)
                .allocate_with_align(
                    config.size(),
                    Alloc::PciBar {
                        bus: address.bus,
                        dev: address.dev,
                        func: address.func,
                        bar: config.bar_index() as u8,
                    },
                    format!(
                        "virtio-{}-custom_bar",
                        type_to_str(self.device.device_type()).unwrap_or("?")
                    ),
                    config.size(),
                )
                .map_err(|e| PciDeviceError::IoAllocationFailed(config.size(), e))?;
            let config = config.set_address(device_addr);
            let _device_bar = self
                .config_regs
                .add_pci_bar(config)
                .map_err(|e| PciDeviceError::IoRegistrationFailed(device_addr, e))?;
            ranges.push((device_addr, config.size()));
        }
        Ok(ranges)
    }

    fn get_bar_configuration(&self, bar_num: usize) -> Option<PciBarConfiguration> {
        self.config_regs.get_bar_configuration(bar_num)
    }

    fn register_device_capabilities(&mut self) -> std::result::Result<(), PciDeviceError> {
        for cap in self.device.get_device_caps() {
            self.config_regs
                .add_capability(&*cap)
                .map_err(PciDeviceError::CapabilitiesSetup)?;
        }

        Ok(())
    }

    fn ioevents(&self) -> Vec<(&Event, u64, Datamatch)> {
        let bar0 = self.config_regs.get_bar_addr(self.settings_bar as usize);
        let notify_base = bar0 + NOTIFICATION_BAR_OFFSET;
        self.queue_evts
            .iter()
            .enumerate()
            .map(|(i, event)| {
                (
                    event,
                    notify_base + i as u64 * NOTIFY_OFF_MULTIPLIER as u64,
                    Datamatch::AnyLength,
                )
            })
            .collect()
    }

    fn read_config_register(&self, reg_idx: usize) -> u32 {
        let mut data: u32 = self.config_regs.read_reg(reg_idx);
        if let Some(msix_cap_reg_idx) = self.msix_cap_reg_idx {
            if msix_cap_reg_idx == reg_idx {
                data = self.msix_config.lock().read_msix_capability(data);
            }
        }

        data
    }

    fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if let Some(msix_cap_reg_idx) = self.msix_cap_reg_idx {
            if msix_cap_reg_idx == reg_idx {
                let behavior = self.msix_config.lock().write_msix_capability(offset, data);
                self.device.control_notify(behavior);
            }
        }

        (&mut self.config_regs).write_reg(reg_idx, offset, data)
    }

    // Clippy: the value of COMMON_CONFIG_BAR_OFFSET happens to be zero so the
    // expression `COMMON_CONFIG_BAR_OFFSET <= o` is always true, but this code
    // is written such that the value of the const may be changed independently.
    #[allow(clippy::absurd_extreme_comparisons)]
    fn read_bar(&mut self, addr: u64, data: &mut [u8]) {
        // The driver is only allowed to do aligned, properly sized access.
        let bar0 = self.config_regs.get_bar_addr(self.settings_bar as usize);
        let offset = addr - bar0;
        match offset {
            o if COMMON_CONFIG_BAR_OFFSET <= o
                && o < COMMON_CONFIG_BAR_OFFSET + COMMON_CONFIG_SIZE =>
            {
                self.common_config.read(
                    o - COMMON_CONFIG_BAR_OFFSET,
                    data,
                    &mut self.queues,
                    self.device.as_mut(),
                )
            }
            o if ISR_CONFIG_BAR_OFFSET <= o && o < ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE => {
                if let Some(v) = data.get_mut(0) {
                    // Reading this register resets it to 0.
                    *v = self.interrupt_status.swap(0, Ordering::SeqCst) as u8;
                }
            }
            o if DEVICE_CONFIG_BAR_OFFSET <= o
                && o < DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE =>
            {
                self.device.read_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if NOTIFICATION_BAR_OFFSET <= o
                && o < NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE =>
            {
                // Handled with ioevents.
            }

            o if MSIX_TABLE_BAR_OFFSET <= o && o < MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE => {
                self.msix_config
                    .lock()
                    .read_msix_table(o - MSIX_TABLE_BAR_OFFSET, data);
            }

            o if MSIX_PBA_BAR_OFFSET <= o && o < MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE => {
                self.msix_config
                    .lock()
                    .read_pba_entries(o - MSIX_PBA_BAR_OFFSET, data);
            }

            _ => (),
        }
    }

    #[allow(clippy::absurd_extreme_comparisons)]
    fn write_bar(&mut self, addr: u64, data: &[u8]) {
        let bar0 = self.config_regs.get_bar_addr(self.settings_bar as usize);
        let offset = addr - bar0;
        match offset {
            o if COMMON_CONFIG_BAR_OFFSET <= o
                && o < COMMON_CONFIG_BAR_OFFSET + COMMON_CONFIG_SIZE =>
            {
                self.common_config.write(
                    o - COMMON_CONFIG_BAR_OFFSET,
                    data,
                    &mut self.queues,
                    self.device.as_mut(),
                )
            }
            o if ISR_CONFIG_BAR_OFFSET <= o && o < ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE => {
                if let Some(v) = data.get(0) {
                    self.interrupt_status
                        .fetch_and(!(*v as usize), Ordering::SeqCst);
                }
            }
            o if DEVICE_CONFIG_BAR_OFFSET <= o
                && o < DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE =>
            {
                self.device.write_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if NOTIFICATION_BAR_OFFSET <= o
                && o < NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE =>
            {
                // Handled with ioevents.
            }
            o if MSIX_TABLE_BAR_OFFSET <= o && o < MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE => {
                let behavior = self
                    .msix_config
                    .lock()
                    .write_msix_table(o - MSIX_TABLE_BAR_OFFSET, data);
                self.device.control_notify(behavior);
            }
            o if MSIX_PBA_BAR_OFFSET <= o && o < MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE => {
                self.msix_config
                    .lock()
                    .write_pba_entries(o - MSIX_PBA_BAR_OFFSET, data);
            }

            _ => (),
        };

        if !self.device_activated && self.is_driver_ready() && self.are_queues_valid() {
            if let Some(interrupt_evt) = self.interrupt_evt.take() {
                self.interrupt_evt = match interrupt_evt.try_clone() {
                    Ok(evt) => Some(evt),
                    Err(e) => {
                        warn!(
                            "{} failed to clone interrupt_evt: {}",
                            self.debug_label(),
                            e
                        );
                        None
                    }
                };

                if let Some(interrupt_resample_evt) = self.interrupt_resample_evt.take() {
                    self.interrupt_resample_evt = match interrupt_resample_evt.try_clone() {
                        Ok(evt) => Some(evt),
                        Err(e) => {
                            warn!(
                                "{} failed to clone interrupt_resample_evt: {}",
                                self.debug_label(),
                                e
                            );
                            None
                        }
                    };
                    if let Some(mem) = self.mem.take() {
                        self.mem = Some(mem.clone());
                        let interrupt = Interrupt::new(
                            self.interrupt_status.clone(),
                            interrupt_evt,
                            interrupt_resample_evt,
                            Some(self.msix_config.clone()),
                            self.common_config.msix_config,
                        );

                        match self.clone_queue_evts() {
                            Ok(queue_evts) => {
                                // Use ready queues and their events.
                                let (queues, queue_evts) = self
                                    .queues
                                    .clone()
                                    .into_iter()
                                    .zip(queue_evts.into_iter())
                                    .filter(|(q, _)| q.ready)
                                    .unzip();

                                self.device.activate(mem, interrupt, queues, queue_evts);
                                self.device_activated = true;
                            }
                            Err(e) => {
                                warn!(
                                    "{} not activate due to failed to clone queue_evts: {}",
                                    self.debug_label(),
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Device has been reset by the driver
        if self.device_activated && self.is_reset_requested() && self.device.reset() {
            self.device_activated = false;
            // reset queues
            self.queues.iter_mut().for_each(Queue::reset);
            // select queue 0 by default
            self.common_config.queue_select = 0;
        }
    }

    fn on_device_sandboxed(&mut self) {
        self.device.on_device_sandboxed();
    }
}
