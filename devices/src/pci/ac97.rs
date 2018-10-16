// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use data_model::VolatileMemory;
use pci::ac97_regs::*;
use pci::ac97_mixer::Ac97Mixer;
use pci::pci_configuration::{
    PciClassCode, PciConfiguration, PciHeaderType, PciMultimediaSubclass,
};
use pci::pci_device::{self, PciDevice, Result};
use pci::PciInterruptPin;
use resources::SystemAllocator;
use sys_util::{EventFd, GuestAddress, GuestMemory};

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
            0x1, // Subsystem ID.
        );

        Ac97Dev {
            config_regs,
            ac97: Ac97::new(mem),
        }
    }
}

impl PciDevice for Ac97Dev {
    fn assign_irq(&mut self, irq_evt: EventFd, irq_num: u32, irq_pin: PciInterruptPin) {
        self.config_regs.set_irq(irq_num as u8, irq_pin);
    }

    fn allocate_io_bars(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> Result<Vec<(u64, u64)>> {
        let mut ranges = Vec::new();
        let mixer_regs_addr = resources.allocate_mmio_addresses(MIXER_REGS_SIZE)
            .ok_or(pci_device::Error::IoAllocationFailed(MIXER_REGS_SIZE))?;
        self.config_regs
            .add_memory_region(mixer_regs_addr, MIXER_REGS_SIZE)
            .ok_or(pci_device::Error::IoRegistrationFailed(mixer_regs_addr))?;
        ranges.push((mixer_regs_addr, MIXER_REGS_SIZE));
        let master_regs_addr = resources.allocate_mmio_addresses(MASTER_REGS_SIZE)
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
        Vec::new()
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

// Buffer descriptors
const DESCRIPTOR_LENGTH: usize = 8;

// Audio driver controlled by the above registers.
pub struct Ac97 {
    mem: GuestMemory, // For playback and record buffers.

    // Bus Master registers
    pi_regs: Ac97FunctionRegs, // Input
    po_regs: Ac97FunctionRegs, // Output
    mc_regs: Ac97FunctionRegs, // Microphone
    glob_cnt: u32,
    glob_sta: u32,
    acc_sema: u8,

    mixer: Ac97Mixer,

    audio_thread_po: Option<thread::JoinHandle<()>>,
    audio_thread_po_run: Arc<AtomicBool>,
}

impl Ac97 {
    pub fn new(mem: GuestMemory) -> Self {
        Ac97 {
            mem,

            pi_regs: Ac97FunctionRegs::new(),
            po_regs: Ac97FunctionRegs::new(),
            mc_regs: Ac97FunctionRegs::new(),
            glob_cnt: 0,
            glob_sta: GLOB_STA_RESET_VAL, 
            acc_sema: 0,

            mixer: Ac97Mixer::new(),
            audio_thread_po: None,
            audio_thread_po_run: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return the number of sample sent ts the buffer.
    fn play_buffer(regs: &mut Ac97FunctionRegs, mem: &mut GuestMemory, out_buffer: &mut [i16]) -> usize {
        let mut written = 0;

        println!("play_buffer");

        // walk the valid buffers, fill from each, update status regs as we go.
        while written < out_buffer.len() {
            let civ = regs.civ.load(Ordering::Relaxed) as u8;
            let descriptor_addr = regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
            let buffer_addr: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64)).unwrap();
            let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();
            let buffer_len: u32 = control_reg & 0x0000_ffff;

            let mut picb = regs.picb.load(Ordering::Relaxed) as u16;
            let nread = std::cmp::min(out_buffer.len() - written, picb as usize);
            let read_pos = (buffer_addr + (buffer_len - picb as u32)) as u64;
            mem.get_slice(read_pos, nread as u64 * 2).unwrap().copy_to(&mut out_buffer[..nread]);
            picb -= nread as u16;
            regs.picb.store(picb as usize, Ordering::Relaxed);
            written += nread;

            // Check if this buffer is finished.
            if picb == 0 {
                Self::next_buffer_descriptor(regs, &mem);
            }
        }
        println!("play_buffer wrote {}", written);
        written
    }

    fn next_buffer_descriptor(regs: &mut Ac97FunctionRegs, mem: &GuestMemory) {
        let mut civ = regs.civ.load(Ordering::Relaxed) as u8;

        civ = (civ + 1) % 32;
        // TODO - handle civ hitting lvi.
        let descriptor_addr = regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
        let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();
        let picb = control_reg as u16; // Truncate droping control bits, leaving buffer length.
        regs.civ.store(civ as usize, Ordering::Relaxed);
        regs.piv.store(civ as usize, Ordering::Relaxed);
        regs.picb.store(picb as usize, Ordering::Relaxed);
    }

    // Bus master handling
    fn bm_regs(&mut self, func: &Ac97Function) -> &Ac97FunctionRegs {
        match func {
            Ac97Function::Input => &self.pi_regs,
            Ac97Function::Output => &self.po_regs,
            Ac97Function::Microphone => &self.mc_regs,
        }
    }

    fn bm_regs_mut(&mut self, func: &Ac97Function) -> &mut Ac97FunctionRegs {
        match func {
            Ac97Function::Input => &mut self.pi_regs,
            Ac97Function::Output => &mut self.po_regs,
            Ac97Function::Microphone => &mut self.mc_regs,
        }
    }

    fn set_bdbar(&mut self, func: Ac97Function, val: u32) {
        self.bm_regs_mut(&func).bdbar = val & !0x07;
    }

    fn set_lvi(&mut self, func: Ac97Function, val: u8) {
        // TODO(dgreid) - handle new pointer
        self.bm_regs_mut(&func).lvi = val % 32; // LVI wraps at 32.
    }

    fn set_sr(&mut self, func: Ac97Function, val: u16) {
        let mut sr = self.bm_regs(&func).sr;
        if val & SR_FIFOE != 0 {
            sr &= !SR_FIFOE;
        }
        if val & SR_LVBCI != 0 {
            sr &= !SR_LVBCI;
        }
        if val & SR_BCIS != 0 {
            sr &= !SR_BCIS;
        }
        self.update_sr(&func, sr);
    }

    fn set_cr(&mut self, func: Ac97Function, val: u8) {
        let mut regs = match func {
            Ac97Function::Input => &mut self.pi_regs,
            Ac97Function::Output => &mut self.po_regs,
            Ac97Function::Microphone => &mut self.mc_regs,
        };
        if val & CR_RR != 0 {
            regs.do_reset();
            // TODO(dgreid) stop audio
        } else {
            if val & CR_RPBM == 0 {
                // Run/Pause set to pause.
                // TODO(dgreid) disable audio.
                self.audio_thread_po_run.store(false, Ordering::Relaxed);
                if let Some(thread) = self.audio_thread_po.take() {
                    thread.join().unwrap();
                }
                regs.sr |= SR_DCH;
            } else if regs.cr & CR_RPBM == 0 { // Not already running.
                // Run/Pause set to run.
                regs.piv.store(0x1f, Ordering::Relaxed); // Set to last buffer.
                regs.civ.store(0x1f, Ordering::Relaxed);
                //fetch_bd (s, r);
                regs.sr &= !SR_DCH;
                Self::next_buffer_descriptor(&mut regs, &self.mem);
                // TODO(dgreid) start audio.
                let mut thread_regs = regs.clone();
                let mut thread_mem = self.mem.clone();
                self.audio_thread_po_run.store(true, Ordering::Relaxed);
                let thread_run = self.audio_thread_po_run.clone();
                self.audio_thread_po = Some(thread::spawn(move || {
                    println!("in thread");
                    let mut pb_buf = vec![0i16; 480];
                    while thread_run.load(Ordering::Relaxed) {
                        // TODO - actually connect to audio output.
                        Self::play_buffer(&mut thread_regs, &mut thread_mem, &mut pb_buf);
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                }));
            }
            regs.cr = val & CR_VALID_MASK;
        }
    }

    fn update_sr(&mut self, func: &Ac97Function, val: u16) {
        let (regs, int_mask) = match func {
            Ac97Function::Input => (&mut self.pi_regs, GS_PIINT),
            Ac97Function::Output => (&mut self.po_regs, GS_POINT),
            Ac97Function::Microphone => (&mut self.mc_regs, GS_MINT),
        };

        let mut interrupt_high = false;

        if val & SR_INT_MASK != regs.sr & SR_INT_MASK {
            if (val & SR_LVBCI) != 0 && (regs.cr & CR_LVBIE) != 0 {
                interrupt_high = true;
            }
            if (val & SR_BCIS) != 0 && (regs.cr & CR_IOCE) != 0 {
                interrupt_high = true;
            }
        }

        regs.sr = val;

        if interrupt_high {
            self.glob_sta |= int_mask;
        //pci_irq_assert(&s->dev);
        } else {
            self.glob_sta &= !int_mask;
            //pci_irq_deassert(&s->dev);
        }
    }

    fn set_glob_cnt(&mut self, new_glob_cnt: u32) {
        // TODO(dgreid) handle other bits.
        if new_glob_cnt & GLOB_CNT_COLD_RESET == 0 {
            self.pi_regs.do_reset();
            self.po_regs.do_reset();
            self.mc_regs.do_reset();

            *self = Ac97::new(self.mem.clone());
            self.glob_cnt =  new_glob_cnt & GLOB_CNT_STABLE_BITS;
            return;
        }
        if new_glob_cnt & GLOB_CNT_WARM_RESET != 0 {
            // TODO(dgreid) - check if running and if so, ignore.
            self.glob_cnt = new_glob_cnt & !GLOB_CNT_WARM_RESET; // Auto-cleared reset bit.
            return;
        }
        self.glob_cnt = new_glob_cnt;
    }

    fn is_cold_reset(&self) -> bool {
        self.glob_cnt & GLOB_CNT_COLD_RESET == 0
    }

    pub fn bm_readb(&mut self, offset: u64) -> u8 {
        match offset {
            0x04 => self.pi_regs.civ.load(Ordering::Relaxed) as u8,
            0x05 => self.pi_regs.lvi,
            0x0a => self.pi_regs.piv.load(Ordering::Relaxed) as u8,
            0x0b => self.pi_regs.cr,
            0x14 => self.po_regs.civ.load(Ordering::Relaxed) as u8,
            0x15 => self.po_regs.lvi,
            0x1a => self.po_regs.piv.load(Ordering::Relaxed) as u8,
            0x1b => self.po_regs.cr,
            0x24 => self.mc_regs.civ.load(Ordering::Relaxed) as u8,
            0x25 => self.mc_regs.lvi,
            0x2a => self.mc_regs.piv.load(Ordering::Relaxed) as u8,
            0x2b => self.mc_regs.cr,
            0x34 => self.acc_sema,
            _ => 0,
        }
    }

    pub fn bm_readw(&mut self, offset: u64) -> u16 {
        match offset {
            0x06 => self.pi_regs.sr,
            0x08 => self.pi_regs.picb.load(Ordering::Relaxed) as u16,
            0x16 => self.po_regs.sr,
            0x18 => self.po_regs.picb.load(Ordering::Relaxed) as u16,
            0x26 => self.mc_regs.sr,
            0x28 => self.mc_regs.picb.load(Ordering::Relaxed) as u16,
            _ => 0,
        }
    }

    pub fn bm_readl(&mut self, offset: u64) -> u32 {
        match offset {
            0x00 => self.pi_regs.bdbar,
            0x04 => self.pi_regs.atomic_status_regs(),
            0x10 => self.po_regs.bdbar,
            0x14 => self.po_regs.atomic_status_regs(),
            0x20 => self.mc_regs.bdbar,
            0x24 => self.mc_regs.atomic_status_regs(),
            0x2c => self.glob_cnt,
            0x30 => self.glob_sta,
            _ => 0,
        }
    }

    pub fn bm_writeb(&mut self, offset: u64, val: u8) {
        // Only process writes to the control register when cold reset is set.
        if self.is_cold_reset() {
            return;
        }

        match offset {
            0x04 => (), // RO
            0x05 => self.set_lvi(Ac97Function::Input, val),
            0x0a => (), // RO
            0x0b => self.set_cr(Ac97Function::Input, val),
            0x14 => (), // RO
            0x15 => self.set_lvi(Ac97Function::Output, val),
            0x1a => (), // RO
            0x1b => self.set_cr(Ac97Function::Output, val),
            0x24 => (), // RO
            0x25 => self.set_lvi(Ac97Function::Microphone, val),
            0x2a => (), // RO
            0x2b => self.set_cr(Ac97Function::Microphone, val),
            0x34 => self.acc_sema = val,
            o => println!("wtf write byte to 0x{:x}", o),
        }
    }

    pub fn bm_writew(&mut self, offset: u64, val: u16) {
        // Only process writes to the control register when cold reset is set.
        if self.is_cold_reset() {
            return;
        }
        match offset {
            0x06 => self.set_sr(Ac97Function::Input, val),
            0x08 => (), // RO
            0x16 => self.set_sr(Ac97Function::Output, val),
            0x18 => (), // RO
            0x26 => self.set_sr(Ac97Function::Microphone, val),
            0x28 => (), // RO
            o => println!("wtf write word to 0x{:x}", o),
        }
    }

    pub fn bm_writel(&mut self, offset: u64, val: u32) {
        // Only process writes to the control register when cold reset is set.
        if self.is_cold_reset() {
            if offset != 0x2c {
                return;
            }
        }
        match offset {
            0x00 => self.set_bdbar(Ac97Function::Input, val),
            0x10 => self.set_bdbar(Ac97Function::Output, val),
            0x20 => self.set_bdbar(Ac97Function::Microphone, val),
            0x2c => self.set_glob_cnt(val),
            0x30 => (), // RO
            o => println!("wtf write long to 0x{:x}", o),
        }
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
            2 => self.mixer.writew(offset, data[0] as u16 | (data[1] as u16) << 8),
            l => println!("wtf mixer write length of {}", l),
        }
    }

    fn read_bus_master(&mut self, offset: u64, data: &mut [u8]) {
        //        println!("read from BM 0x{:x} {}", offset, data.len());
        match data.len() {
            1 => data[0] = self.bm_readb(offset),
            2 => {
                let val: u16 = self.bm_readw(offset);
                data[0] = val as u8;
                data[1] = (val >> 8) as u8;
            }
            4 => {
                let val: u32 = self.bm_readl(offset);
                data[0] = val as u8;
                data[1] = (val >> 8) as u8;
                data[2] = (val >> 16) as u8;
                data[3] = (val >> 24) as u8;
            }
            l => println!("wtf read length of {}", l),
        }
    }

    fn write_bus_master(&mut self, offset: u64, data: &[u8]) {
        //        println!("write to BM 0x{:x} {}", offset, data.len());
        match data.len() {
            1 => self.bm_writeb(offset, data[0]),
            2 => self.bm_writew(offset, data[0] as u16 | (data[1] as u16) << 8),
            4 => self.bm_writel(
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
