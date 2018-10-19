// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{thread, time};

use data_model::VolatileMemory;
use pci::ac97_regs::*;
use sys_util::{GuestAddress, GuestMemory};

pub enum BusMasterAction {
    /// `NoAction` indicates that no action needs to be taken by the caller.
    NoAction,
    /// `StartAudio` indicates that audio for the given function should be started.
    StartAudio(Ac97Function),
    /// `StopAudio` indicates that audio for the given function should be stopped.
    StopAudio(Ac97Function),
}

pub struct Ac97BusMaster {
    // Keep guest memory as each function will use it for buffer descriptors.
    mem: GuestMemory,
    // Bus Master registers
    pi_regs: Ac97FunctionRegs, // Input
    po_regs: Ac97FunctionRegs, // Output
    mc_regs: Ac97FunctionRegs, // Microphone
    glob_cnt: u32,
    glob_sta: u32,
    acc_sema: u8,

    // Audio thread book keeping.
    audio_thread_po: Option<thread::JoinHandle<()>>,
    audio_thread_po_run: Arc<AtomicBool>,
}

impl Ac97BusMaster {
    pub fn new(mem: GuestMemory) -> Self {
        Ac97BusMaster {
            mem,
            pi_regs: Ac97FunctionRegs::new(),
            po_regs: Ac97FunctionRegs::new(),
            mc_regs: Ac97FunctionRegs::new(),
            glob_cnt: 0,
            glob_sta: GLOB_STA_RESET_VAL, 
            acc_sema: 0,

            audio_thread_po: None,
            audio_thread_po_run: Arc::new(AtomicBool::new(false)),
        }
    }
 
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
                    match func {
                        Ac97Function::Output => {
                            while thread_run.load(Ordering::Relaxed) {
                                // TODO - actually connect to audio output.
                                Self::play_buffer(&mut thread_regs, &thread_mem, &mut pb_buf);
                                thread::sleep(std::time::Duration::from_millis(5));
                            }
                        }
                        Ac97Function::Microphone |
                        Ac97Function::Input => {
                            while thread_run.load(Ordering::Relaxed) {
                                // TODO - actually connect to audio output.
                                Self::record_buffer(&mut thread_regs, &mut thread_mem, &pb_buf);
                                thread::sleep(std::time::Duration::from_millis(5));
                            }
                        }
                    };
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

            *self = Ac97BusMaster::new(self.mem.clone());
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

    /// Return the number of sample sent to the buffer.
    fn play_buffer(regs: &mut Ac97FunctionRegs, mem: &GuestMemory, out_buffer: &mut [i16]) -> usize {
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

    /// Return the number of samples read from the buffer.
    fn record_buffer(regs: &mut Ac97FunctionRegs, mem: &mut GuestMemory, buffer: &[i16]) -> usize {
        let mut written = 0;

        println!("record_buffer");

        // walk the valid buffers, fill each, update status regs as we go.
        while written < buffer.len() {
            let civ = regs.civ.load(Ordering::Relaxed) as u8;
            let descriptor_addr = regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
            let buffer_addr: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64)).unwrap();
            let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();
            let buffer_len: u32 = control_reg & 0x0000_ffff;

            let mut picb = regs.picb.load(Ordering::Relaxed) as u16;
            let nread = std::cmp::min(buffer.len() - written, picb as usize);
            let read_pos = (buffer_addr + (buffer_len - picb as u32)) as u64;
            mem.get_slice(read_pos, nread as u64 * 2).unwrap().copy_from(&buffer[..nread]);
            picb -= nread as u16;
            regs.picb.store(picb as usize, Ordering::Relaxed);
            written += nread;

            // Check if this buffer is finished.
            if picb == 0 {
                Self::next_buffer_descriptor(regs, &mem);
            }
        }
        println!("record_buffer wrote {}", written);
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

    pub fn is_cold_reset(&self) -> bool {
        self.glob_cnt & GLOB_CNT_COLD_RESET == 0
    }

    pub fn readb(&mut self, offset: u64) -> u8 {
        match offset {
            0x04 => self.pi_regs.civ.load(Ordering::Relaxed) as u8,
            0x05 => self.pi_regs.lvi,
            0x06 => self.pi_regs.sr as u8,
            0x0a => self.pi_regs.piv.load(Ordering::Relaxed) as u8,
            0x0b => self.pi_regs.cr,
            0x14 => self.po_regs.civ.load(Ordering::Relaxed) as u8,
            0x15 => self.po_regs.lvi,
            0x16 => self.po_regs.sr as u8,
            0x1a => self.po_regs.piv.load(Ordering::Relaxed) as u8,
            0x1b => self.po_regs.cr,
            0x24 => self.mc_regs.civ.load(Ordering::Relaxed) as u8,
            0x25 => self.mc_regs.lvi,
            0x26 => self.mc_regs.sr as u8,
            0x2a => self.mc_regs.piv.load(Ordering::Relaxed) as u8,
            0x2b => self.mc_regs.cr,
            0x34 => self.acc_sema,
            _ => 0,
        }
    }

    pub fn readw(&mut self, offset: u64) -> u16 {
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

    pub fn readl(&mut self, offset: u64) -> u32 {
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

    pub fn writeb(&mut self, offset: u64, val: u8) {
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
            0x16 => (), //TODO(dgreid, write-clear LVI int status,
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

    pub fn writew(&mut self, offset: u64, val: u16) {
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

    pub fn writel(&mut self, offset: u64, val: u32) {
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
}

#[cfg(test)]
mod test {
    use super::*;

    const GLOB_CNT: u64 = 0x2c;

    #[test]
    fn bm_bdbar() {
        let mut ac97 = Ac97BusMaster::new(GuestMemory::new(&[]).unwrap());

        let bdbars = [0x00u64, 0x10, 0x20];

        // Make sure writes have no affect during cold reset.
        ac97.writel(0x00, 0x5555_555f);
        assert_eq!(ac97.readl(0x00), 0x0000_0000);

        // Relesase cold reset.
        ac97.writel(GLOB_CNT, 0x0000_0002);

        // Tests that the base address is writable and that the bottom three bits are read only.
        for bdbar in &bdbars {
            assert_eq!(ac97.readl(*bdbar), 0x0000_0000);
            ac97.writel(*bdbar, 0x5555_555f);
            assert_eq!(ac97.readl(*bdbar), 0x5555_5558);
        }
    }

    #[test]
    fn bm_status_reg() {
        let mut ac97 = Ac97BusMaster::new(GuestMemory::new(&[]).unwrap());

        let sr_addrs = [0x06u64, 0x16, 0x26];

        for sr in &sr_addrs {
            assert_eq!(ac97.readw(*sr), 0x0001);
            ac97.writew(*sr, 0xffff);
            assert_eq!(ac97.readw(*sr), 0x0001);
        }
    }

    #[test]
    fn bm_global_control() {
        let mut ac97 = Ac97BusMaster::new(GuestMemory::new(&[]).unwrap());

        assert_eq!(ac97.readl(GLOB_CNT), 0x0000_0000);

        // Relesase cold reset.
        ac97.writel(GLOB_CNT, 0x0000_0002);

        // Check interrupt enable bits are writable.
        ac97.writel(GLOB_CNT, 0x0000_0072);
        assert_eq!(ac97.readl(GLOB_CNT), 0x0000_0072);

        // A Warm reset should doesn't affect register state and is auto cleared.
        ac97.writel(0x00, 0x5555_5558);
        ac97.writel(GLOB_CNT, 0x0000_0076);
        assert_eq!(ac97.readl(GLOB_CNT), 0x0000_0072);
        assert_eq!(ac97.readl(0x00), 0x5555_5558);
        // Check that a cold reset works, but setting bdbar and checking it is zeroed.
        ac97.writel(0x00, 0x5555_555f);
        ac97.writel(GLOB_CNT, 0x000_0070);
        assert_eq!(ac97.readl(GLOB_CNT), 0x0000_0070);
        assert_eq!(ac97.readl(0x00), 0x0000_0000);
    }

    #[test]
    fn start_playback() {
        const LVI_MASK: u8 = 0x1f; // Five bits for 32 total entries.
        const IOC_MASK: u32 = 0x8000_0000; // Interrupt on completion.
        let num_buffers = LVI_MASK as usize + 1;
        const BUFFER_SIZE: usize = 960;

        const GUEST_ADDR_BASE: u32 = 0x100_0000;
        let mem = GuestMemory::new(&[(GuestAddress(GUEST_ADDR_BASE as u64), 1024*1024*1024)]).unwrap();
        let mut ac97 = Ac97BusMaster::new(mem.clone());

        // Release cold reset.
        ac97.writel(GLOB_CNT, 0x0000_0002);

        // Setup ping-pong buffers. A and B repeating for every possible index.
        ac97.writel(PO_BDBAR, GUEST_ADDR_BASE);
        for i in 0..num_buffers {
            let pointer_addr = GuestAddress(GUEST_ADDR_BASE as u64 + i as u64 * 8);
            let control_addr = GuestAddress(GUEST_ADDR_BASE as u64 + i as u64 * 8 + 4);
            if i % 2 == 0 {
                mem.write_obj_at_addr(GUEST_ADDR_BASE, pointer_addr).unwrap();
            } else {
                mem.write_obj_at_addr(GUEST_ADDR_BASE + BUFFER_SIZE as u32 / 2, pointer_addr).unwrap();
            };
            mem.write_obj_at_addr(IOC_MASK | (BUFFER_SIZE as u32 / 2), control_addr).unwrap();
        }

        ac97.writeb(PO_LVI, LVI_MASK);

        // TODO(dgreid) - clear interrupts.

        // Start.
        ac97.writeb(PO_CR, CR_RPBM);

        std::thread::sleep(time::Duration::from_millis(20));

        assert!(ac97.readw(PO_SR) & 0x01 == 0); // DMA is running.
        assert_ne!(0, ac97.readw(PO_PICB));
        assert_ne!(0, ac97.readb(PO_CIV));

        // TODO(dgreid) - check interrupts were set.

        // Buffer complete should be set as the IOC bit was set in the descriptor.
        assert!(ac97.readw(MC_SR) & SR_BCIS != 0);
        // Clear the BCIS bit
        ac97.writew(MC_SR, SR_BCIS);
        assert!(ac97.readw(MC_SR) & SR_BCIS == 0);

        // Stop.
        ac97.writeb(PO_CR, 0);
        assert!(ac97.readw(PO_SR) & 0x01 != 0); // DMA is not running.
    }

    #[test]
    fn start_record() {
        const LVI_MASK: u8 = 0x1f; // Five bits for 32 total entries.
        const IOC_MASK: u32 = 0x8000_0000; // Interrupt on completion.
        let num_buffers = LVI_MASK as usize + 1;
        const BUFFER_SIZE: usize = 960;

        const GUEST_ADDR_BASE: u32 = 0x100_0000;
        let mem = GuestMemory::new(&[(GuestAddress(GUEST_ADDR_BASE as u64), 1024*1024*1024)]).unwrap();
        let mut ac97 = Ac97BusMaster::new(mem.clone());

        // Release cold reset.
        ac97.writel(GLOB_CNT, 0x0000_0002);

        // Setup ping-pong buffers. A and B repeating for every possible index.
        ac97.writel(MC_BDBAR, GUEST_ADDR_BASE);
        for i in 0..num_buffers {
            let pointer_addr = GuestAddress(GUEST_ADDR_BASE as u64 + i as u64 * 8);
            let control_addr = GuestAddress(GUEST_ADDR_BASE as u64 + i as u64 * 8 + 4);
            if i % 2 == 0 {
                mem.write_obj_at_addr(GUEST_ADDR_BASE, pointer_addr).unwrap();
            } else {
                mem.write_obj_at_addr(GUEST_ADDR_BASE + BUFFER_SIZE as u32 / 2, pointer_addr).unwrap();
            };
            mem.write_obj_at_addr(IOC_MASK | (BUFFER_SIZE as u32 / 2), control_addr).unwrap();
        }

        ac97.writeb(MC_LVI, LVI_MASK);

        // TODO(dgreid) - clear interrupts.

        // Start.
        ac97.writeb(MC_CR, CR_RPBM);

        std::thread::sleep(time::Duration::from_millis(20));

        assert!(ac97.readw(MC_SR) & 0x01 == 0); // DMA is running.
        assert_ne!(0, ac97.readw(MC_PICB));
        assert_ne!(0, ac97.readb(MC_CIV));

        // TODO(dgreid) - check interrupts were set.

        // Stop.
        ac97.writeb(MC_CR, 0);
        assert!(ac97.readw(MC_SR) & 0x01 != 0); // DMA is not running.
    }
}
