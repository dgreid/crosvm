// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{self, Instant};

use audio::{DummyStreamSource, PlaybackBuffer, StreamSource};
use data_model::VolatileMemory;
use pci::ac97_regs::*;
use sys_util::{EventFd, GuestAddress, GuestMemory};

pub enum BusMasterAction {
    /// `NoAction` indicates that no action needs to be taken by the caller.
    NoAction,
    /// `StartAudio` indicates that audio for the given function should be started.
    StartAudio(Ac97Function),
    /// `StopAudio` indicates that audio for the given function should be stopped.
    StopAudio(Ac97Function),
}

// Bus Master registers
struct Ac97BusMasterRegs {
    pi_regs: Ac97FunctionRegs, // Input
    po_regs: Ac97FunctionRegs, // Output
    po_pointer_update_time: Instant, // Time the picb and civ regs were last updated.
    mc_regs: Ac97FunctionRegs, // Microphone
    glob_cnt: u32,
    glob_sta: u32,
}

impl Ac97BusMasterRegs {
    pub fn new() -> Ac97BusMasterRegs {
        Ac97BusMasterRegs {
            pi_regs: Ac97FunctionRegs::new(),
            po_regs: Ac97FunctionRegs::new(),
            po_pointer_update_time: Instant::now(),
            mc_regs: Ac97FunctionRegs::new(),
            glob_cnt: 0,
            glob_sta: GLOB_STA_RESET_VAL, 
        }
    }
 
    fn func_regs(&mut self, func: &Ac97Function) -> &Ac97FunctionRegs {
        match func {
            Ac97Function::Input => &self.pi_regs,
            Ac97Function::Output => &self.po_regs,
            Ac97Function::Microphone => &self.mc_regs,
        }
    }

    fn func_regs_mut(&mut self, func: &Ac97Function) -> &mut Ac97FunctionRegs {
        match func {
            Ac97Function::Input => &mut self.pi_regs,
            Ac97Function::Output => &mut self.po_regs,
            Ac97Function::Microphone => &mut self.mc_regs,
        }
    }
}

pub struct Ac97BusMaster {
    // Keep guest memory as each function will use it for buffer descriptors.
    mem: GuestMemory,
    regs: Arc<Mutex<Ac97BusMasterRegs>>,
    acc_sema: u8,

    // Audio thread book keeping.
    audio_thread_po: Option<thread::JoinHandle<()>>,
    audio_thread_po_run: Arc<AtomicBool>,
    audio_thread_mc: Option<thread::JoinHandle<()>>,
    audio_thread_mc_run: Arc<AtomicBool>,

    // IRQ event
    event_fd: Option<EventFd>,

    // Audio server used to create playback streams.
    audio_server: Box<dyn StreamSource>,
}

impl Ac97BusMaster {
    pub fn new(mem: GuestMemory, audio_server: Box<dyn StreamSource>) -> Self {
        Ac97BusMaster {
            mem,
            regs: Arc::new(Mutex::new(Ac97BusMasterRegs::new())),
            acc_sema: 0,

            audio_thread_po: None,
            audio_thread_po_run: Arc::new(AtomicBool::new(false)),
            audio_thread_mc: None,
            audio_thread_mc_run: Arc::new(AtomicBool::new(false)),

            event_fd: None,

            audio_server,
        }
    }

    pub fn set_event_fd(&mut self, irq_evt: EventFd) {
        self.event_fd = Some(irq_evt);
    }

    fn set_bdbar(&mut self, func: Ac97Function, val: u32) {
        self.regs.lock().unwrap().func_regs_mut(&func).bdbar = val & !0x07;
    }

    fn set_lvi(&mut self, func: Ac97Function, val: u8) {
        // TODO(dgreid) - handle new pointer
        self.regs.lock().unwrap().func_regs_mut(&func).lvi = val % 32; // LVI wraps at 32.
    }

    fn set_sr(&mut self, func: Ac97Function, val: u16) {
        let mut sr = self.regs.lock().unwrap().func_regs(&func).sr;
        if val & SR_FIFOE != 0 {
            sr &= !SR_FIFOE;
        }
        if val & SR_LVBCI != 0 {
            sr &= !SR_LVBCI;
        }
        if val & SR_BCIS != 0 {
            sr &= !SR_BCIS;
        }
        Self::update_sr(&mut self.regs.lock().unwrap(), &func, sr, &self.event_fd);
    }

    fn stop_audio(&mut self, func: &Ac97Function) {
        match func {
            Ac97Function::Input => (), // TODO(dgreid)
            Ac97Function::Output => {
                self.audio_thread_po_run.store(false, Ordering::Relaxed);
                if let Some(thread) = self.audio_thread_po.take() {
                    thread.join().unwrap();
                }
            }
            Ac97Function::Microphone => {
                self.audio_thread_mc_run.store(false, Ordering::Relaxed);
                if let Some(thread) = self.audio_thread_mc.take() {
                    thread.join().unwrap();
                }
            }
        };

    }

    fn start_audio(&mut self, func: &Ac97Function) {
        let mut thread_mem = self.mem.clone();
        let thread_regs = self.regs.clone();
        Self::next_buffer_descriptor(thread_regs.lock().unwrap().func_regs_mut(func), &thread_mem);
        match func {
            Ac97Function::Output => {
                self.audio_thread_po_run.store(true, Ordering::Relaxed);
                let thread_run = self.audio_thread_po_run.clone();
                // TODO(dgreid) - determine the format for otuput_stream.
                let buffer_samples = Self::current_buffer_size(thread_regs.lock().unwrap().func_regs(func), &self.mem);
                println!("start with buffer size {}", buffer_samples);
                let mut output_stream = self.audio_server.new_playback_stream(2, 48000, buffer_samples/2); // TODO - assuming 2 channels.
                self.audio_thread_po = Some(thread::spawn(move || {
                    while thread_run.load(Ordering::Relaxed) {
                        let mut pb_buf = output_stream.next_playback_buffer();
                        Self::play_buffer(&thread_regs, &thread_mem, &mut pb_buf);
                    }
                }));
            }
            Ac97Function::Microphone => {
                self.audio_thread_mc_run.store(true, Ordering::Relaxed);
                let thread_run = self.audio_thread_mc_run.clone();
                self.audio_thread_mc = Some(thread::spawn(move || {
                    let cap_buf = vec![0i16; 480];
                    while thread_run.load(Ordering::Relaxed) {
                        // TODO - actually connect to audio input.
                        Self::record_buffer(&thread_regs, &mut thread_mem, &cap_buf);
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                }));
            }
            Ac97Function::Input => {
                // TODO(dgreid)
            }
        }
    }

    fn set_cr(&mut self, func: Ac97Function, val: u8) {
        if val & CR_RR != 0 {
            self.stop_audio(&func);
            let mut regs = self.regs.lock().unwrap();
            regs.func_regs_mut(&func).do_reset();
        } else {
            let cr = self.regs.lock().unwrap().func_regs(&func).cr;
            if val & CR_RPBM == 0 {
                // Run/Pause set to pause.
                // TODO(dgreid) disable audio.
                self.stop_audio(&func);
                let mut regs = self.regs.lock().unwrap();
                regs.func_regs_mut(&func).sr |= SR_DCH;;
            } else if cr & CR_RPBM == 0 { // Not already running.
                // Run/Pause set to run.
                {
                    let mut regs = self.regs.lock().unwrap();
                    let mut func_regs = regs.func_regs_mut(&func);
                    func_regs.piv = 0;
                    func_regs.civ = 0;
                    //fetch_bd (s, r);
                    func_regs.sr &= !SR_DCH;
                }
                self.start_audio(&func);
            }
            let mut regs = self.regs.lock().unwrap();
            regs.func_regs_mut(&func).cr = val & CR_VALID_MASK;
        }
    }

    fn update_sr(regs: &mut Ac97BusMasterRegs, func: &Ac97Function, val: u16, event_fd: &Option<EventFd>) {
        let int_mask = match func {
            Ac97Function::Input => GS_PIINT,
            Ac97Function::Output => GS_POINT,
            Ac97Function::Microphone => GS_MINT,
        };

        let mut interrupt_high = false;

        {
            let mut func_regs = regs.func_regs_mut(func);
            let initial_sr = func_regs.sr;
            if val & SR_INT_MASK != initial_sr & SR_INT_MASK {
                if (val & SR_LVBCI) != 0 && (func_regs.cr & CR_LVBIE) != 0 {
                    interrupt_high = true;
                }
                if (val & SR_BCIS) != 0 && (func_regs.cr & CR_IOCE) != 0 {
                    interrupt_high = true;
                }
            }

            func_regs.sr = val;
        }

        // TODO - maybe update glob_sta as a combinatino of all audio thread sources in the main context.
        if interrupt_high {
            regs.glob_sta |= int_mask;
            if let Some(irq_evt) = event_fd {
                irq_evt.write(1).unwrap();
            }
        } else {
            regs.glob_sta &= !int_mask;
            // TODO(dgreid) - can we deassert the interrupt?
        }
    }

    fn set_glob_cnt(&mut self, new_glob_cnt: u32) {
        // TODO(dgreid) handle other bits.
        if new_glob_cnt & GLOB_CNT_COLD_RESET == 0 {
            self.stop_audio(&Ac97Function::Input);
            self.stop_audio(&Ac97Function::Output);
            self.stop_audio(&Ac97Function::Microphone);
            let mut regs = self.regs.lock().unwrap();
            regs.pi_regs.do_reset();
            regs.po_regs.do_reset();
            regs.mc_regs.do_reset();

            regs.glob_cnt =  new_glob_cnt & GLOB_CNT_STABLE_BITS;
            self.acc_sema = 0;
            return;
        }
        let mut regs = self.regs.lock().unwrap();
        if new_glob_cnt & GLOB_CNT_WARM_RESET != 0 {
            // TODO(dgreid) - check if running and if so, ignore.
            regs.glob_cnt = new_glob_cnt & !GLOB_CNT_WARM_RESET; // Auto-cleared reset bit.
            return;
        }
        regs.glob_cnt = new_glob_cnt;
    }

    /// Return the number of sample sent to the buffer.
    fn play_buffer(regs: &Arc<Mutex<Ac97BusMasterRegs>>, mem: &GuestMemory, out_buffer: &mut PlaybackBuffer) -> usize {
        let num_channels = 2;

        let mut regs = regs.lock().unwrap();

        let mut samples_written = 0;
        // walk the valid buffers, fill from each, update status regs as we go.
        while samples_written / num_channels < out_buffer.len() {
            {
            let mut func_regs = regs.func_regs_mut(&Ac97Function::Output);
            let civ = func_regs.civ;
            let descriptor_addr = func_regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
            let buffer_addr: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64)).unwrap();
            let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();
            let buffer_len_samples = (control_reg & 0x0000_ffff) as usize;
            let sample_size = 2;

            let mut samples_remaining = func_regs.picb as usize;
            let samples_to_write = std::cmp::min(out_buffer.len()*num_channels - samples_written, samples_remaining);
            let read_pos = (buffer_addr + (buffer_len_samples - samples_remaining as usize) as u32 * sample_size) as u64;
            let buffer_offset = samples_to_write * sample_size as usize;
            mem.get_slice(read_pos, samples_to_write as u64 * sample_size as u64).unwrap().copy_to(&mut out_buffer.buffer[buffer_offset..]);
            samples_remaining -= samples_to_write;
            func_regs.picb = samples_remaining as u16;
            println!("picb {}", func_regs.picb);
            samples_written += samples_to_write;

            // Check if this buffer is finished.
            if samples_remaining == 0 {
                Self::buffer_completed(func_regs, &mem);
            }
            }

            regs.po_pointer_update_time = Instant::now();
        }
        samples_written / num_channels
    }

    /// Return the number of samples read from the buffer.
    fn record_buffer(regs: &Arc<Mutex<Ac97BusMasterRegs>>, mem: &mut GuestMemory, buffer: &[i16]) -> usize {
        // TODO
        480
    }

    fn buffer_completed(regs: &mut Ac97FunctionRegs, mem: &GuestMemory) {
        // Check if the completed descriptor wanted an interrupt on completion.
        let civ = regs.civ;
        let descriptor_addr = regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
        let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();

        if control_reg & BD_IOC != 0 {
            regs.sr |= SR_BCIS;
            // TODO(dgreid) - update_sr();
        }

        Self::next_buffer_descriptor(regs, mem);
    }

    fn next_buffer_descriptor(regs: &mut Ac97FunctionRegs, mem: &GuestMemory) {
        // Move CIV to PIV.
        let mut civ = regs.piv;

        // TODO - handle civ hitting lvi.
        let descriptor_addr = regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
        let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();
        let picb = control_reg as u16; // Truncate droping control bits, leaving buffer length.
        regs.civ = civ;
        civ = (civ + 1) % 32; // Move PIV to the next buffer.
        regs.piv = civ;
        regs.picb = picb;
    }

    fn current_buffer_size(func_regs: &Ac97FunctionRegs, mem: &GuestMemory) -> usize {
        let civ = func_regs.civ;
        let descriptor_addr = func_regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
        let control_reg: u32 = mem.read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4)).unwrap();
        let buffer_len: usize = control_reg as usize & 0x0000_ffff;
        buffer_len
    }

    pub fn is_cold_reset(&self) -> bool {
        self.regs.lock().unwrap().glob_cnt & GLOB_CNT_COLD_RESET == 0
    }

    pub fn readb(&mut self, offset: u64) -> u8 {
        let regs = self.regs.lock().unwrap();
        match offset {
            0x04 => regs.pi_regs.civ,
            0x05 => regs.pi_regs.lvi,
            0x06 => regs.pi_regs.sr as u8,
            0x0a => regs.pi_regs.piv,
            0x0b => regs.pi_regs.cr,
            0x14 => regs.po_regs.civ,
            0x15 => regs.po_regs.lvi,
            0x16 => regs.po_regs.sr as u8,
            0x1a => regs.po_regs.piv,
            0x1b => regs.po_regs.cr,
            0x24 => regs.mc_regs.civ,
            0x25 => regs.mc_regs.lvi,
            0x26 => regs.mc_regs.sr as u8,
            0x2a => regs.mc_regs.piv,
            0x2b => regs.mc_regs.cr,
            0x34 => self.acc_sema,
            _ => 0,
        }
    }

    pub fn readw(&mut self, offset: u64) -> u16 {
        let regs = self.regs.lock().unwrap();
        match offset {
            0x06 => regs.pi_regs.sr,
            0x08 => regs.pi_regs.picb,
            0x16 => regs.po_regs.sr,
            0x18 => { // PO PICB
                if !self.audio_thread_po_run.load(Ordering::Relaxed) {
                    // Not running, no need to estimate what has been consumed.
                    regs.po_regs.picb
                } else {
                    // Estimate how many samples have been played since the last audio callback.
                    // TODO(dgreid) - support other sample rates.
                    let num_channels = 2;
                    let micros = regs.po_pointer_update_time.elapsed().subsec_micros();
                    println!("micros {} picb {}", micros, regs.po_regs.picb);
                    let consumed = micros * 48 / 1000;
                    regs.po_regs.picb.saturating_sub((num_channels * consumed) as u16)
                }
            }
            0x26 => regs.mc_regs.sr,
            0x28 => regs.mc_regs.picb,
            _ => 0,
        }
    }

    pub fn readl(&mut self, offset: u64) -> u32 {
        let regs = self.regs.lock().unwrap();
        match offset {
            0x00 => regs.pi_regs.bdbar,
            0x04 => regs.pi_regs.atomic_status_regs(),
            0x10 => regs.po_regs.bdbar,
            0x14 => regs.po_regs.atomic_status_regs(),
            0x20 => regs.mc_regs.bdbar,
            0x24 => regs.mc_regs.atomic_status_regs(),
            0x2c => regs.glob_cnt,
            0x30 => regs.glob_sta,
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
        let mut ac97 = Ac97BusMaster::new(GuestMemory::new(&[]).unwrap(), Box::new(DummyStreamSource::new()));

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
        let mut ac97 = Ac97BusMaster::new(GuestMemory::new(&[]).unwrap(), Box::new(DummyStreamSource::new()));

        let sr_addrs = [0x06u64, 0x16, 0x26];

        for sr in &sr_addrs {
            assert_eq!(ac97.readw(*sr), 0x0001);
            ac97.writew(*sr, 0xffff);
            assert_eq!(ac97.readw(*sr), 0x0001);
        }
    }

    #[test]
    fn bm_global_control() {
        let mut ac97 = Ac97BusMaster::new(GuestMemory::new(&[]).unwrap(), Box::new(DummyStreamSource::new()));

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
        const BUFFER_SIZE: usize = 32768;
        const FRAGMENT_SIZE: usize = BUFFER_SIZE / 2;

        const GUEST_ADDR_BASE: u32 = 0x100_0000;
        let mem = GuestMemory::new(&[(GuestAddress(GUEST_ADDR_BASE as u64), 1024*1024*1024)]).unwrap();
        let mut ac97 = Ac97BusMaster::new(mem.clone(), Box::new(DummyStreamSource::new()));

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
                mem.write_obj_at_addr(GUEST_ADDR_BASE + FRAGMENT_SIZE as u32, pointer_addr).unwrap();
            };
            mem.write_obj_at_addr(IOC_MASK | (FRAGMENT_SIZE as u32) / 2, control_addr).unwrap();
        }

        ac97.writeb(PO_LVI, LVI_MASK);

        // TODO(dgreid) - clear interrupts.

        // Start.
        ac97.writeb(PO_CR, CR_RPBM);

        let start_time = Instant::now();

        std::thread::sleep(time::Duration::from_millis(50));
        let elapsed = start_time.elapsed().subsec_micros() as usize;
        let picb = ac97.readw(PO_PICB);
        let mut civ = ac97.readb(PO_CIV);
        let pos = (FRAGMENT_SIZE - (picb as usize * 2)) / 4;

        let rate = ((pos*1000) / elapsed) * 1000 + (((pos*1000) % elapsed) * 1000) / elapsed; 

        // Check that frames are consumed at close to 48k.
        // The thread should consume more than a frame in 20ms, limit that it doesn't consume more
        // than two to reduce flakyness from slow machines running this test.
        assert!(rate > 47500 && rate < 48500,
                "Invalid sample rate: played {} in {}us. Rate: {} {}", pos, elapsed, rate, civ);

        assert!(ac97.readw(PO_SR) & 0x01 == 0); // DMA is running.
        assert_ne!(0, ac97.readw(PO_PICB));

        for i in 0..30 {
            if civ != 0 {
                break;
            }
            std::thread::sleep(time::Duration::from_millis(20));
            civ = ac97.readb(PO_CIV);
        }
            
        assert_ne!(0, civ);
        let picb = ac97.readw(PO_PICB);

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
}
