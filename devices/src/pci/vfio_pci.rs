// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::thread;
use std::u32;

use libc::EINVAL;

use kvm::Datamatch;
use msg_socket::{MsgReceiver, MsgSender};
use resources::{AddressAllocator, Alloc, Error as ResErr, SystemAllocator};
use sys_util::{error, Error as SysError, EventFd, PollContext, PollToken};

use vfio_sys::*;
use vm_control::{
    MaybeOwnedFd, VfioDeviceRequestSocket, VfioDriverRequest, VfioDriverResponse,
    VfioServerRequest, VfioServerResponse, VfioServerResponseSocket,
};

use crate::pci::pci_device::{Error as PciDeviceError, PciDevice};
use crate::pci::PciInterruptPin;

use crate::vfio::VfioDevice;

struct VfioPciConfig {
    device: Arc<VfioDevice>,
}

impl VfioPciConfig {
    fn new(device: Arc<VfioDevice>) -> Self {
        VfioPciConfig { device }
    }

    #[allow(dead_code)]
    fn read_config_byte(&self, offset: u32) -> u8 {
        let mut data: [u8; 1] = [0];
        self.device
            .region_read(VFIO_PCI_CONFIG_REGION_INDEX, data.as_mut(), offset.into());

        data[0]
    }

    #[allow(dead_code)]
    fn read_config_word(&self, offset: u32) -> u16 {
        let mut data: [u8; 2] = [0, 0];
        self.device
            .region_read(VFIO_PCI_CONFIG_REGION_INDEX, data.as_mut(), offset.into());

        u16::from_le_bytes(data)
    }

    #[allow(dead_code)]
    fn read_config_dword(&self, offset: u32) -> u32 {
        let mut data: [u8; 4] = [0, 0, 0, 0];
        self.device
            .region_read(VFIO_PCI_CONFIG_REGION_INDEX, data.as_mut(), offset.into());

        u32::from_le_bytes(data)
    }

    #[allow(dead_code)]
    fn write_config_byte(&self, buf: u8, offset: u32) {
        self.device.region_write(
            VFIO_PCI_CONFIG_REGION_INDEX,
            ::std::slice::from_ref(&buf),
            offset.into(),
        )
    }

    #[allow(dead_code)]
    fn write_config_word(&self, buf: u16, offset: u32) {
        let data: [u8; 2] = buf.to_le_bytes();
        self.device
            .region_write(VFIO_PCI_CONFIG_REGION_INDEX, &data, offset.into())
    }

    #[allow(dead_code)]
    fn write_config_dword(&self, buf: u32, offset: u32) {
        let data: [u8; 4] = buf.to_le_bytes();
        self.device
            .region_write(VFIO_PCI_CONFIG_REGION_INDEX, &data, offset.into())
    }
}

const PCI_CAPABILITY_LIST: u32 = 0x34;
const PCI_CAP_ID_MSI: u8 = 0x05;

// MSI registers
const PCI_MSI_NEXT_POINTER: u32 = 0x1; // Next cap pointer
const PCI_MSI_FLAGS: u32 = 0x2; // Message Control
const PCI_MSI_FLAGS_ENABLE: u16 = 0x0001; // MSI feature enabled
const PCI_MSI_FLAGS_64BIT: u16 = 0x0080; // 64-bit addresses allowed
const PCI_MSI_FLAGS_MASKBIT: u16 = 0x0100; // Per-vector masking capable
const PCI_MSI_ADDRESS_LO: u32 = 0x4; // MSI address lower 32 bits
const PCI_MSI_ADDRESS_HI: u32 = 0x8; // MSI address upper 32 bits (if 64 bit allowed)
const PCI_MSI_DATA_32: u32 = 0x8; // 16 bits of data for 32-bit message address
const PCI_MSI_DATA_64: u32 = 0xC; // 16 bits of date for 64-bit message address

// MSI length
const MSI_LENGTH_32BIT: u32 = 0xA;
const MSI_LENGTH_64BIT_WITHOUT_MASK: u32 = 0xE;
const MSI_LENGTH_64BIT_WITH_MASK: u32 = 0x18;

enum VfioMsiChange {
    Disable,
    Enable,
}

struct VfioMsiCap {
    offset: u32,
    size: u32,
    ctl: u16,
    address: u64,
    data: u16,
}

impl VfioMsiCap {
    fn new(config: &VfioPciConfig) -> Self {
        // msi minimum size is 0xa
        let mut msi_len: u32 = MSI_LENGTH_32BIT;
        let mut cap_next: u32 = config.read_config_byte(PCI_CAPABILITY_LIST).into();
        while cap_next != 0 {
            let cap_id = config.read_config_byte(cap_next.into());
            // find msi cap
            if cap_id == PCI_CAP_ID_MSI {
                let msi_ctl = config.read_config_word(cap_next + PCI_MSI_FLAGS);
                if msi_ctl & PCI_MSI_FLAGS_64BIT != 0 {
                    msi_len = MSI_LENGTH_64BIT_WITHOUT_MASK;
                }
                if msi_ctl & PCI_MSI_FLAGS_MASKBIT != 0 {
                    msi_len = MSI_LENGTH_64BIT_WITH_MASK;
                }
                break;
            }
            let offset = cap_next + PCI_MSI_NEXT_POINTER;
            cap_next = config.read_config_byte(offset).into();
        }

        VfioMsiCap {
            offset: cap_next,
            size: msi_len,
            ctl: 0,
            address: 0,
            data: 0,
        }
    }

    fn is_msi_reg(&self, index: u64, len: usize) -> bool {
        if index >= self.offset as u64
            && index + len as u64 <= (self.offset + self.size) as u64
            && len as u32 <= self.size
        {
            true
        } else {
            false
        }
    }

    fn write_msi_reg(&mut self, index: u64, data: &[u8]) -> Option<VfioMsiChange> {
        let len = data.len();
        let offset = index as u32 - self.offset;
        let mut ret: Option<VfioMsiChange> = None;

        // write msi ctl
        if len == 2 && offset == PCI_MSI_FLAGS {
            let was_enabled = self.is_msi_enabled();
            let value: [u8; 2] = [data[0], data[1]];
            self.ctl = u16::from_le_bytes(value);
            let is_enabled = self.is_msi_enabled();
            if !was_enabled && is_enabled {
                ret = Some(VfioMsiChange::Enable);
            } else if was_enabled && !is_enabled {
                ret = Some(VfioMsiChange::Disable)
            }
        } else if len == 4 && offset == PCI_MSI_ADDRESS_LO && self.size == MSI_LENGTH_32BIT {
            //write 32 bit message address
            let value: [u8; 8] = [data[0], data[1], data[2], data[3], 0, 0, 0, 0];
            self.address = u64::from_le_bytes(value);
        } else if len == 4 && offset == PCI_MSI_ADDRESS_LO && self.size != MSI_LENGTH_32BIT {
            // write 64 bit message address low part
            let value: [u8; 8] = [data[0], data[1], data[2], data[3], 0, 0, 0, 0];
            self.address &= !0xffffffff;
            self.address |= u64::from_le_bytes(value);
        } else if len == 4 && offset == PCI_MSI_ADDRESS_HI && self.size != MSI_LENGTH_32BIT {
            //write 64 bit message address high part
            let value: [u8; 8] = [0, 0, 0, 0, data[0], data[1], data[2], data[3]];
            self.address &= 0xffffffff;
            self.address |= u64::from_le_bytes(value);
        } else if len == 8 && offset == PCI_MSI_ADDRESS_LO && self.size != MSI_LENGTH_32BIT {
            // write 64 bit message address
            let value: [u8; 8] = [
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ];
            self.address = u64::from_le_bytes(value);
        } else if len == 2 && offset == PCI_MSI_DATA_32 && self.size == MSI_LENGTH_32BIT {
            // write message data
            let value: [u8; 2] = [data[0], data[1]];
            self.data = u16::from_le_bytes(value);
        } else if len == 2
            && offset == PCI_MSI_DATA_64
            && self.size == MSI_LENGTH_64BIT_WITHOUT_MASK
        {
            // write message data
            let value: [u8; 2] = [data[0], data[1]];
            self.data = u16::from_le_bytes(value);
        } else if len == 2 && offset == PCI_MSI_DATA_64 && self.size == MSI_LENGTH_64BIT_WITH_MASK {
            // write message data
            let value: [u8; 2] = [data[0], data[1]];
            self.data = u16::from_le_bytes(value);
        }

        ret
    }

    fn is_msi_enabled(&self) -> bool {
        self.ctl & PCI_MSI_FLAGS_ENABLE == PCI_MSI_FLAGS_ENABLE
    }

    fn get_msi_address(&self) -> u64 {
        self.address
    }

    fn get_msi_data(&self) -> u16 {
        self.data
    }
}

struct MmioInfo {
    bar_index: u32,
    start: u64,
    length: u64,
}

struct IoInfo {
    bar_index: u32,
}

/// Implements the Vfio Pci device, then a pci device is added into vm
pub struct VfioPciDevice {
    device: Arc<VfioDevice>,
    config: VfioPciConfig,
    pci_bus_dev: Option<(u8, u8)>,
    interrupt_evt: Option<EventFd>,
    interrupt_resample_evt: Option<EventFd>,
    mmio_regions: Vec<MmioInfo>,
    io_regions: Vec<IoInfo>,
    msi_cap: VfioMsiCap,
    virq: u32,
    vm_socket: VfioDeviceRequestSocket,
    active: bool,
    server_socket: Option<VfioServerResponseSocket>,
}

impl VfioPciDevice {
    /// Constructs a new Vfio Pci device for the give Vfio device
    pub fn new(
        device: Box<VfioDevice>,
        vfio_device_socket: VfioDeviceRequestSocket,
        vfio_server_socket: Option<VfioServerResponseSocket>,
    ) -> Self {
        let dev = Arc::new(*device);
        let config = VfioPciConfig::new(Arc::clone(&dev));
        let msi_cap = VfioMsiCap::new(&config);

        VfioPciDevice {
            device: dev,
            config,
            pci_bus_dev: None,
            interrupt_evt: None,
            interrupt_resample_evt: None,
            mmio_regions: Vec::new(),
            io_regions: Vec::new(),
            msi_cap,
            virq: 0,
            vm_socket: vfio_device_socket,
            active: false,
            server_socket: vfio_server_socket,
        }
    }

    fn find_region(&self, addr: u64) -> Option<MmioInfo> {
        for mmio_info in self.mmio_regions.iter() {
            if addr >= mmio_info.start && addr < mmio_info.start + mmio_info.length {
                return Some(MmioInfo {
                    bar_index: mmio_info.bar_index,
                    start: mmio_info.start,
                    length: mmio_info.length,
                });
            }
        }

        None
    }

    fn add_msi_routing(&self, address: u64, data: u32) {
        if let Err(e) = self
            .vm_socket
            .send(&VfioDriverRequest::AddMsiRoute(self.virq, address, data))
        {
            error!("failed to send AddMsiRoute request at {:?}", e);
        }
        match self.vm_socket.recv() {
            Ok(VfioDriverResponse::Err(e)) => error!("failed to call AddMsiRoute request {:?}", e),
            Ok(_) => return,
            Err(e) => error!("failed to receive AddMsiRoute response {:?}", e),
        }
    }

    fn allocate_bar_addr(
        &self,
        resources: &mut SystemAllocator,
        size: u64,
        bar: u32,
        is_64bit: bool,
    ) -> Result<u64, PciDeviceError> {
        let (bus, dev) = self
            .pci_bus_dev
            .expect("assign_bus_dev must be called prior to allocate_io_bars");
        let pci_bar = Alloc::PciBar {
            bus,
            dev,
            bar: bar as u8,
        };
        let mut allocator: &mut AddressAllocator;
        if self.device.get_region_flags(bar) & VFIO_REGION_INFO_FLAG_MMAP != 0 {
            allocator = resources.device_allocator();
            match allocator.try_allocate_with_align(size, pci_bar.clone(), size) {
                Ok(bar_addr) => {
                    if bar_addr >= 1 << 32 && !is_64bit {
                        allocator = resources.mmio_allocator();
                    }
                }
                Err(ResErr::OutOfSpace) => allocator = resources.mmio_allocator(),
                Err(e) => return Err(PciDeviceError::IoAllocationFailed(size, e)),
            }
        } else {
            allocator = resources.mmio_allocator();
            match allocator.try_allocate_with_align(size, pci_bar.clone(), size) {
                Ok(bar_addr) => {
                    if bar_addr >= 1 << 32 && !is_64bit {
                        allocator = resources.device_allocator();
                    }
                }
                Err(ResErr::OutOfSpace) => allocator = resources.device_allocator(),
                Err(e) => return Err(PciDeviceError::IoAllocationFailed(size, e)),
            }
        }
        let bar_addr = allocator
            .allocate_with_align(size, pci_bar, "vfio_bar".to_string(), size)
            .map_err(|e| PciDeviceError::IoAllocationFailed(size, e))?;

        Ok(bar_addr)
    }

    fn add_bar_mmap(&self, index: u32, bar_addr: u64) -> Option<u32> {
        if self.device.get_region_flags(index) & VFIO_REGION_INFO_FLAG_MMAP != 0 {
            let (mmap_offset, mmap_size) = self.device.get_region_mmap(index);
            if mmap_size == 0 {
                return None;
            }

            let guest_map_start = bar_addr + mmap_offset;
            let region_offset = self.device.get_region_offset(index);
            if self
                .vm_socket
                .send(&VfioDriverRequest::RegisterMmapMemory(
                    MaybeOwnedFd::Borrowed(self.device.as_raw_fd()),
                    mmap_size as usize,
                    (region_offset + mmap_offset) as usize,
                    guest_map_start,
                ))
                .is_err()
            {
                return None;
            }
            let response = match self.vm_socket.recv() {
                Ok(res) => res,
                Err(_) => return None,
            };
            match response {
                VfioDriverResponse::RegisterMmapMemory { slot, host } => {
                    // Safe because the given guest_map_start is valid guest bar address. and
                    // the host pointer is correct and valid guaranteed by MemoryMapping interface.
                    match unsafe { self.device.vfio_dma_map(guest_map_start, mmap_size, host) } {
                        Ok(_) => Some(slot),
                        Err(e) => {
                            error!("{}", e);
                            return None;
                        }
                    }
                }
                _ => return None,
            }
        } else {
            None
        }
    }

    fn active_thread(&mut self, device: Arc<VfioDevice>) {
        let server_socket: VfioServerResponseSocket;
        match self.server_socket.take() {
            Some(socket) => server_socket = socket,
            None => return,
        }

        let thread_result = thread::Builder::new()
            .name("vfio".to_string())
            .spawn(move || {
                #[derive(PollToken)]
                enum Token {
                    VfioRequest,
                }

                let poll_ctx: PollContext<Token> = match PollContext::new()
                    .and_then(|pc| pc.add(&server_socket, Token::VfioRequest).and(Ok(pc)))
                {
                    Ok(pc) => pc,
                    Err(e) => {
                        error!("failed creating PollContext: {}", e);
                        return;
                    }
                };

                'poll: loop {
                    let events = match poll_ctx.wait() {
                        Ok(v) => v,
                        Err(e) => {
                            error!("failed polling for events: {}", e);
                            break;
                        }
                    };

                    for event in events.iter_readable() {
                        match event.token() {
                            Token::VfioRequest => {
                                let req = match server_socket.recv() {
                                    Ok(req) => req,
                                    Err(e) => {
                                        error!("server socket recv failure: {:?}", e);
                                        if poll_ctx.delete(&server_socket).is_err() {
                                            error!("failed to delete vfio server_socket from poll_context");
                                        }
                                        break;
                                    }
                                };

                                let resp = match req {
                                    VfioServerRequest::DmaMap(iova, size, host_addr) => {
                                        // Safe because the given iova is valid guest address, and
                                        // if it is overlap with others, this function return error
                                        // the host pointer is correct and valid guaranteed by MemoryMapping
                                        // interface.
                                        match unsafe { device.vfio_dma_map(iova, size, host_addr) } {
                                            Ok(()) => VfioServerResponse::Ok,
                                            _ => VfioServerResponse::Err(SysError::new(EINVAL)),
                                        }
                                    }
                                    VfioServerRequest::DmaUnmap(iova, size) => {
                                        match device.vfio_dma_unmap(iova, size) {
                                            Ok(()) => VfioServerResponse::Ok,
                                            _ => VfioServerResponse::Err(SysError::new(EINVAL)),
                                        }
                                    }
                                };

                                if let Err(e) = server_socket.send(&resp) {
                                    error!("server socket failed send: {:?}", e);
                                    if poll_ctx.delete(&server_socket).is_err() {
                                         error!("failed to delete vfio server_socket from poll_context");
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        if let Err(e) = thread_result {
            error!("failed to spawn vfio thread: {}", e);
            return;
        }
    }

    fn activate(&mut self) {
        for mmio_info in self.mmio_regions.iter() {
            self.add_bar_mmap(mmio_info.bar_index, mmio_info.start);
        }

        let device = self.device.clone();
        self.active_thread(device);

        self.active = true;
    }
}

impl PciDevice for VfioPciDevice {
    fn debug_label(&self) -> String {
        format!("vfio pci device")
    }

    fn assign_bus_dev(&mut self, bus: u8, device: u8) {
        self.pci_bus_dev = Some((bus, device));
    }

    fn keep_fds(&self) -> Vec<RawFd> {
        let mut fds = self.device.keep_fds();
        if let Some(ref interrupt_evt) = self.interrupt_evt {
            fds.push(interrupt_evt.as_raw_fd());
        }
        if let Some(ref interrupt_resample_evt) = self.interrupt_resample_evt {
            fds.push(interrupt_resample_evt.as_raw_fd());
        }
        fds.push(self.vm_socket.as_raw_fd());
        if let Some(ref server_socket) = self.server_socket {
            fds.push(server_socket.as_raw_fd());
        }
        fds
    }

    fn assign_irq(
        &mut self,
        irq_evt: EventFd,
        irq_resample_evt: Option<EventFd>,
        irq_num: u32,
        irq_pin: PciInterruptPin,
    ) {
        self.config.write_config_byte(irq_num as u8, 0x3C);
        self.config.write_config_byte(irq_pin as u8 + 1, 0x3D);
        self.interrupt_evt = Some(irq_evt);
        self.interrupt_resample_evt = irq_resample_evt;
        self.virq = irq_num;
    }

    fn need_resample_evt(&self) -> bool {
        false
    }

    fn allocate_io_bars(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> Result<Vec<(u64, u64)>, PciDeviceError> {
        let mut ranges = Vec::new();
        let mut i = VFIO_PCI_BAR0_REGION_INDEX;

        while i <= VFIO_PCI_ROM_REGION_INDEX {
            let mut low: u32 = 0xffffffff;
            let offset: u32;
            if i == VFIO_PCI_ROM_REGION_INDEX {
                offset = 0x30;
            } else {
                offset = 0x10 + i * 4;
            }
            self.config.write_config_dword(low, offset);
            low = self.config.read_config_dword(offset);

            let low_flag = low & 0xf;
            let is_64bit = match low_flag & 0x4 {
                0x4 => true,
                _ => false,
            };
            if (low_flag & 0x1 == 0 || i == VFIO_PCI_ROM_REGION_INDEX) && low != 0 {
                let mut upper: u32 = 0xffffffff;
                if is_64bit {
                    self.config.write_config_dword(upper, offset + 4);
                    upper = self.config.read_config_dword(offset + 4);
                }

                low &= 0xffff_fff0;
                let mut size: u64 = u64::from(upper);
                size <<= 32;
                size |= u64::from(low);
                size = !size + 1;

                let bar_addr = self.allocate_bar_addr(resources, size, i, is_64bit)?;

                ranges.push((bar_addr, size));
                self.mmio_regions.push(MmioInfo {
                    bar_index: i,
                    start: bar_addr,
                    length: size,
                });

                low = bar_addr as u32;
                low |= low_flag;
                self.config.write_config_dword(low, offset);
                if is_64bit {
                    upper = (bar_addr >> 32) as u32;
                    self.config.write_config_dword(upper, offset + 4);
                }
            } else if low_flag & 0x1 == 0x1 {
                self.io_regions.push(IoInfo { bar_index: i });
            }

            if is_64bit {
                i += 2;
            } else {
                i += 1;
            }
        }

        if let Err(e) = self.device.setup_dma_map() {
            error!(
                "failed to add all guest memory regions into iommu table: {}",
                e
            );
        }

        Ok(ranges)
    }

    fn allocate_device_bars(
        &mut self,
        _resources: &mut SystemAllocator,
    ) -> Result<Vec<(u64, u64)>, PciDeviceError> {
        Ok(Vec::new())
    }

    fn register_device_capabilities(&mut self) -> Result<(), PciDeviceError> {
        Ok(())
    }

    fn ioeventfds(&self) -> Vec<(&EventFd, u64, Datamatch)> {
        Vec::new()
    }

    fn read_config_register(&self, reg_idx: usize) -> u32 {
        let reg: u32 = (reg_idx * 4) as u32;

        let mut config = self.config.read_config_dword(reg);

        // Ignore IO bar
        if reg >= 0x10 && reg <= 0x24 {
            for io_info in self.io_regions.iter() {
                if io_info.bar_index * 4 + 0x10 == reg {
                    config = 0;
                }
            }
        }

        config
    }

    fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if !self.active {
            self.activate();
        }

        let start = (reg_idx * 4) as u64 + offset;

        if self.msi_cap.is_msi_reg(start, data.len()) {
            if let Some(ref interrupt_evt) = self.interrupt_evt {
                match self.msi_cap.write_msi_reg(start, data) {
                    Some(VfioMsiChange::Enable) => {
                        if let Err(e) = self.device.msi_enable(interrupt_evt) {
                            error!("{}", e);
                        }
                        let address = self.msi_cap.get_msi_address();
                        let data = self.msi_cap.get_msi_data();
                        self.add_msi_routing(address, data.into());
                    }
                    Some(VfioMsiChange::Disable) => {
                        if let Err(e) = self.device.msi_disable() {
                            error!("{}", e);
                        }
                    }
                    None => (),
                }
            }
        }

        self.device
            .region_write(VFIO_PCI_CONFIG_REGION_INDEX, data, start);
    }

    fn read_bar(&mut self, addr: u64, data: &mut [u8]) {
        if let Some(mmio_info) = self.find_region(addr) {
            let offset = addr - mmio_info.start;
            self.device.region_read(mmio_info.bar_index, data, offset);
        }
    }

    fn write_bar(&mut self, addr: u64, data: &[u8]) {
        if let Some(mmio_info) = self.find_region(addr) {
            let offset = addr - mmio_info.start;
            self.device.region_write(mmio_info.bar_index, data, offset);
        }
    }
}
