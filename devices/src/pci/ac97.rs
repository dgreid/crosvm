// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use audio::DummyStreamSource;
use pci::ac97_bus_master::Ac97BusMaster;
use pci::ac97_mixer::Ac97Mixer;
use pci::ac97_regs::*;
use pci::pci_configuration::{
    PciClassCode, PciConfiguration, PciHeaderType, PciMultimediaSubclass,
};
use pci::pci_device::{self, PciDevice, Result};
use pci::PciInterruptPin;
use resources::SystemAllocator;
use sys_util::{EventFd, GuestMemory};

// Use 82801AA because it's what qemu does.
const PCI_DEVICE_ID_INTEL_82801AA_5: u16 = 0x2415;

/// AC97 audio device emulation.
pub struct Ac97Dev {
    config_regs: PciConfiguration,
    ac97: Ac97,
}

impl Ac97Dev {
    pub fn new(mem: GuestMemory) -> Self {
        let config_regs = PciConfiguration::new(
            0x8086,
            PCI_DEVICE_ID_INTEL_82801AA_5,
            PciClassCode::MultimediaController,
            &PciMultimediaSubclass::AudioDevice,
            None, // No Programming interface.
            PciHeaderType::Device,
            0x8086, // Subsystem Vendor ID
            0x1,    // Subsystem ID.
        );

        Ac97Dev {
            config_regs,
            ac97: Ac97::new(mem),
        }
    }
}

impl PciDevice for Ac97Dev {
    fn assign_irq(
        &mut self,
        irq_evt: EventFd,
        irq_resample_evt: EventFd,
        irq_num: u32,
        irq_pin: PciInterruptPin,
    ) {
        self.config_regs.set_irq(irq_num as u8, irq_pin);
        self.ac97.set_event_fd(irq_evt);
    }

    fn allocate_io_bars(&mut self, resources: &mut SystemAllocator) -> Result<Vec<(u64, u64)>> {
        let mut ranges = Vec::new();
        let mixer_regs_addr = resources
            .allocate_mmio_addresses(MIXER_REGS_SIZE)
            .ok_or(pci_device::Error::IoAllocationFailed(MIXER_REGS_SIZE))?;
        self.config_regs
            .add_memory_region(mixer_regs_addr, MIXER_REGS_SIZE)
            .ok_or(pci_device::Error::IoRegistrationFailed(mixer_regs_addr))?;
        ranges.push((mixer_regs_addr, MIXER_REGS_SIZE));
        let master_regs_addr = resources
            .allocate_mmio_addresses(MASTER_REGS_SIZE)
            .ok_or(pci_device::Error::IoAllocationFailed(MASTER_REGS_SIZE))?;
        self.config_regs
            .add_memory_region(master_regs_addr, MASTER_REGS_SIZE)
            .ok_or(pci_device::Error::IoRegistrationFailed(master_regs_addr))?;
        ranges.push((master_regs_addr, MASTER_REGS_SIZE));
        Ok(ranges)
    }

    fn config_registers(&self) -> &PciConfiguration {
        &self.config_regs
    }

    fn config_registers_mut(&mut self) -> &mut PciConfiguration {
        &mut self.config_regs
    }

    fn keep_fds(&self) -> Vec<RawFd> {
        vec![0, 1, 2]
        //        Vec::new()
    }

    fn read_bar(&mut self, addr: u64, data: &mut [u8]) {
        let bar0 = self.config_regs.get_bar_addr(0) as u64;
        let bar1 = self.config_regs.get_bar_addr(1) as u64;
        match addr {
            a if a >= bar0 && a < bar0 + MIXER_REGS_SIZE => self.ac97.read_mixer(addr - bar0, data),
            a if a >= bar1 && a < bar1 + MASTER_REGS_SIZE => {
                self.ac97.read_bus_master(addr - bar1, data)
            }
            _ => (),
        }
    }

    fn write_bar(&mut self, addr: u64, data: &[u8]) {
        let bar0 = self.config_regs.get_bar_addr(0) as u64;
        let bar1 = self.config_regs.get_bar_addr(1) as u64;
        match addr {
            a if a >= bar0 && a < bar0 + MIXER_REGS_SIZE => {
                self.ac97.write_mixer(addr - bar0, data)
            }
            a if a >= bar1 && a < bar1 + MASTER_REGS_SIZE => {
                self.ac97.write_bus_master(addr - bar1, data)
            }
            _ => (),
        }
    }
}

struct Ac97BusDevice {
    audio_function: Arc<Mutex<Ac97>>,
}

impl Ac97BusDevice {
    pub fn new(audio_function: Arc<Mutex<Ac97>>) -> Self {
        Ac97BusDevice { audio_function }
    }
}

// Audio driver controlled by the above registers.
pub struct Ac97 {
    bus_master: Ac97BusMaster,
    mixer: Ac97Mixer,
}

impl Ac97 {
    pub fn new(mem: GuestMemory) -> Self {
        Ac97 {
            bus_master: Ac97BusMaster::new(mem, Box::new(DummyStreamSource::new())),
            mixer: Ac97Mixer::new(),
        }
    }

    pub fn set_event_fd(&mut self, irq_evt: EventFd) {
        self.bus_master.set_event_fd(irq_evt);
    }

    fn read_mixer(&mut self, offset: u64, data: &mut [u8]) {
        //        println!("read from mixer 0x{:x} {}", offset, data.len());
        match data.len() {
            2 => {
                let val: u16 = self.mixer.readw(offset);
                data[0] = val as u8;
                data[1] = (val >> 8) as u8;
            }
            l => println!("wtf mixer read length of {}", l),
        }
    }

    fn write_mixer(&mut self, offset: u64, data: &[u8]) {
        match data.len() {
            2 => self
                .mixer
                .writew(offset, data[0] as u16 | (data[1] as u16) << 8),
            l => println!("wtf mixer write length of {}", l),
        }
    }

    fn read_bus_master(&mut self, offset: u64, data: &mut [u8]) {
        //        println!("read from BM 0x{:x} {}", offset, data.len());
        match data.len() {
            1 => data[0] = self.bus_master.readb(offset),
            2 => {
                let val: u16 = self.bus_master.readw(offset);
                data[0] = val as u8;
                data[1] = (val >> 8) as u8;
            }
            4 => {
                let val: u32 = self.bus_master.readl(offset);
                data[0] = val as u8;
                data[1] = (val >> 8) as u8;
                data[2] = (val >> 16) as u8;
                data[3] = (val >> 24) as u8;
            }
            l => println!("wtf read length of {}", l),
        }
    }

    // TODO(dgreid) make bus master return an action and act on it.
    fn write_bus_master(&mut self, offset: u64, data: &[u8]) {
        //        println!("write to BM 0x{:x} {}", offset, data.len());
        match data.len() {
            1 => self.bus_master.writeb(offset, data[0]),
            2 => self
                .bus_master
                .writew(offset, data[0] as u16 | (data[1] as u16) << 8),
            4 => self.bus_master.writel(
                offset,
                (data[0] as u32)
                    | ((data[1] as u32) << 8)
                    | ((data[2] as u32) << 16)
                    | ((data[3] as u32) << 24),
            ),
            l => println!("wtf write length of {}", l),
        }
    }
}
