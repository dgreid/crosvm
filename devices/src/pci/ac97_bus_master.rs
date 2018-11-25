// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use audio::{PlaybackBuffer, StreamControl, StreamSource};
use data_model::VolatileMemory;
use pci::ac97_mixer::Ac97Mixer;
use pci::ac97_regs::*;
use sys_util::{EventFd, GuestAddress, GuestMemory};

const DEVICE_SAMPLE_RATE: usize = 48000;

// Bus Master registers. Keeps the state of the bus master register values. Used to share the state
// between the main and audio threads.
struct Ac97BusMasterRegs {
    pi_regs: Ac97FunctionRegs,       // Input
    po_regs: Ac97FunctionRegs,       // Output
    po_pointer_update_time: Instant, // Time the picb and civ regs were last updated.
    mc_regs: Ac97FunctionRegs,       // Microphone
    glob_cnt: u32,
    glob_sta: u32,

    // IRQ event - driven by the glob_sta register.
    irq_evt: Option<EventFd>,
}

impl Ac97BusMasterRegs {
    fn new() -> Ac97BusMasterRegs {
        Ac97BusMasterRegs {
            pi_regs: Ac97FunctionRegs::new(),
            po_regs: Ac97FunctionRegs::new(),
            po_pointer_update_time: Instant::now(),
            mc_regs: Ac97FunctionRegs::new(),
            glob_cnt: 0,
            glob_sta: GLOB_STA_RESET_VAL,
            irq_evt: None,
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

// Internal error type used for reporting errors from the audio playback thread.
enum PlaybackError {
    HitEnd, // Out of audio buffers to play.
}
type PlaybackResult<T> = std::result::Result<T, PlaybackError>;

/// `Ac97BusMaster` emulated the bus master portion of AC97. It exposes a register read/write
/// interface compliant with the the ICH bus master.
pub struct Ac97BusMaster {
    // Keep guest memory as each function will use it for buffer descriptors.
    mem: GuestMemory,
    regs: Arc<Mutex<Ac97BusMasterRegs>>,
    acc_sema: u8,

    // Audio thread book keeping.
    audio_thread_po: Option<thread::JoinHandle<()>>,
    audio_thread_po_run: Arc<AtomicBool>,
    po_stream_control: Option<Box<dyn StreamControl>>,

    // Audio server used to create playback streams.
    audio_server: Box<dyn StreamSource>,

    // Thread for hadlind IRQ resample events from the guest.
    irq_resample_thread: Option<thread::JoinHandle<()>>,
}

impl Ac97BusMaster {
    /// Creates an Ac97BusMaster` object that plays audio from `mem` to streams provided by
    /// `audio_server`.
    pub fn new(mem: GuestMemory, audio_server: Box<dyn StreamSource>) -> Self {
        Ac97BusMaster {
            mem,
            regs: Arc::new(Mutex::new(Ac97BusMasterRegs::new())),
            acc_sema: 0,

            // TODO - group this state in a struct.
            audio_thread_po: None,
            audio_thread_po_run: Arc::new(AtomicBool::new(false)),
            po_stream_control: None,

            audio_server,

            irq_resample_thread: None,
        }
    }

    /// Provides the events needed to raise interrupts in the guest.
    pub fn set_irq_event_fd(&mut self, irq_evt: EventFd, irq_resample_evt: EventFd) {
        let thread_regs = self.regs.clone();
        self.regs.lock().unwrap().irq_evt = Some(irq_evt);
        self.irq_resample_thread = Some(thread::spawn(move || {
            loop {
                irq_resample_evt.read().unwrap(); // TODO Unwrap.
                { // Scope for the lock on thread_regs.
                    let mut regs = thread_regs.lock().unwrap();
                    if regs.func_regs(&Ac97Function::Output).sr & (SR_LVBCI | SR_BCIS) != 0 {
                        if let Some(irq_evt) = regs.irq_evt.as_ref() {
                            irq_evt.write(1).unwrap();
                        }
                    }
                }
            }
        }));
    }

    /// Called when `mixer` has been changed and the new values should be applied to currently
    /// active streams.
    pub fn update_mixer_settings(&mut self, mixer: &Ac97Mixer) {
        if let Some(control) = self.po_stream_control.as_mut() {
            // The audio server only supports one volume, not separate left and right.
            let (muted, left_volume, _right_volume) = mixer.get_master_volume();
            control.set_volume(left_volume);
            control.set_mute(muted);
        }
    }

    /// Checks if the bus master is in the cold reset state.
    pub fn is_cold_reset(&self) -> bool {
        self.regs.lock().unwrap().glob_cnt & GLOB_CNT_COLD_RESET == 0
    }

    /// Reads a byte from the given `offset`.
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

    /// Reads a word from the given `offset`.
    pub fn readw(&mut self, offset: u64) -> u16 {
        let regs = self.regs.lock().unwrap();
        match offset {
            0x06 => regs.pi_regs.sr,
            0x08 => regs.pi_regs.picb,
            0x16 => regs.po_regs.sr,
            0x18 => {
                // PO PICB
                if !self.audio_thread_po_run.load(Ordering::Relaxed) {
                    // Not running, no need to estimate what has been consumed.
                    regs.po_regs.picb
                } else {
                    // Estimate how many samples have been played since the last audio callback.
                    let num_channels = 2;
                    let micros = regs.po_pointer_update_time.elapsed().subsec_micros();
                    let consumed = micros * (DEVICE_SAMPLE_RATE as u32 / 1000) / 1000;
                    regs.po_regs
                        .picb
                        .saturating_sub((num_channels * consumed) as u16)
                }
            }
            0x26 => regs.mc_regs.sr,
            0x28 => regs.mc_regs.picb,
            _ => 0,
        }
    }

    /// Reads a 32-bit word from the given `offset`.
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

    /// Writes the byte `val` to the register specified by `offset`.
    pub fn writeb(&mut self, offset: u64, val: u8, mixer: &Ac97Mixer) {
        // Only process writes to the control register when cold reset is set.
        if self.is_cold_reset() {
            return;
        }

        match offset {
            0x04 => (), // RO
            0x05 => self.set_lvi(Ac97Function::Input, val),
            0x0a => (), // RO
            0x0b => self.set_cr(Ac97Function::Input, val, mixer),
            0x14 => (), // RO
            0x15 => self.set_lvi(Ac97Function::Output, val),
            0x1a => (), // RO
            0x1b => self.set_cr(Ac97Function::Output, val, mixer),
            0x24 => (), // RO
            0x25 => self.set_lvi(Ac97Function::Microphone, val),
            0x2a => (), // RO
            0x2b => self.set_cr(Ac97Function::Microphone, val, mixer),
            0x34 => self.acc_sema = val,
            o => println!("wtf write byte to 0x{:x}", o),
        }
    }

    /// Writes the word `val` to the register specified by `offset`.
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

    /// Writes the 32-bit `val` to the register specified by `offset`.
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
            0x2c => self.set_glob_cnt(val), // TODO - return StopAudio if needed.
            0x30 => (), // RO
            o => println!("wtf write long to 0x{:x}", o),
        }
    }
    fn set_bdbar(&mut self, func: Ac97Function, val: u32) {
        self.regs.lock().unwrap().func_regs_mut(&func).bdbar = val & !0x07;
    }

    fn set_lvi(&mut self, func: Ac97Function, val: u8) {
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
        Self::update_sr(&mut self.regs.lock().unwrap(), &func, sr);
    }

    fn stop_audio(&mut self, func: &Ac97Function) {
        match func {
            Ac97Function::Input => (),
            Ac97Function::Output => {
                self.audio_thread_po_run.store(false, Ordering::Relaxed);
                if let Some(thread) = self.audio_thread_po.take() {
                    thread.join().unwrap();
                }
            }
            Ac97Function::Microphone => (),
        };
    }

    fn start_audio(&mut self, func: &Ac97Function, mixer: &Ac97Mixer) {
        let thread_mem = self.mem.clone();
        let thread_regs = self.regs.clone();
        match func {
            Ac97Function::Input => (),
            Ac97Function::Output => {
                self.audio_thread_po_run.store(true, Ordering::Relaxed);
                let thread_run = self.audio_thread_po_run.clone();
                let buffer_samples = Self::current_buffer_size(
                    thread_regs.lock().unwrap().func_regs(func),
                    &self.mem,
                );
                thread_regs.lock().unwrap().func_regs_mut(func).picb = buffer_samples as u16;
                println!("start with buffer size {}", buffer_samples);
                let (mut stream_control, mut output_stream) = self.audio_server.new_playback_stream(
                    2,
                    DEVICE_SAMPLE_RATE,
                    buffer_samples / 2,
                );
                self.po_stream_control = Some(stream_control);
                self.update_mixer_settings(mixer);
                self.audio_thread_po = Some(thread::spawn(move || {
                    while thread_run.load(Ordering::Relaxed) {
                        let mut pb_buf = output_stream.next_playback_buffer();
                        match Self::play_buffer(&thread_regs, &thread_mem, &mut pb_buf) {
                            Err(PlaybackError::HitEnd) => {
                                thread_run.store(false, Ordering::Relaxed);
                            }
                            Ok(_) => (),
                        }
                    }
                }));
            }
            Ac97Function::Microphone => (),
        }
    }

    fn set_cr(&mut self, func: Ac97Function, val: u8, mixer: &Ac97Mixer) {
        if val & CR_RR != 0 {
            self.stop_audio(&func);
            let mut regs = self.regs.lock().unwrap();
            regs.func_regs_mut(&func).do_reset();
        } else {
            let cr = self.regs.lock().unwrap().func_regs(&func).cr;
            if val & CR_RPBM == 0 {
                // Run/Pause set to pause.
                self.stop_audio(&func);
                let mut regs = self.regs.lock().unwrap();
                regs.func_regs_mut(&func).sr |= SR_DCH;
            } else if cr & CR_RPBM == 0 {
                // Not already running.
                // Run/Pause set to run.
                {
                    let mut regs = self.regs.lock().unwrap();
                    let func_regs = regs.func_regs_mut(&func);
                    func_regs.piv = 0;
                    func_regs.civ = 0;
                    //fetch_bd (s, r);
                    func_regs.sr &= !SR_DCH;
                }
                self.start_audio(&func, mixer);
            }
            let mut regs = self.regs.lock().unwrap();
            regs.func_regs_mut(&func).cr = val & CR_VALID_MASK;
        }
    }

    fn update_sr(regs: &mut Ac97BusMasterRegs, func: &Ac97Function, val: u16) {
        let int_mask = match func {
            Ac97Function::Input => GS_PIINT,
            Ac97Function::Output => GS_POINT,
            Ac97Function::Microphone => GS_MINT,
        };

        let mut interrupt_high = false;

        {
            let func_regs = regs.func_regs_mut(func);
            func_regs.sr = val;
            if val & SR_INT_MASK != 0 {
                if (val & SR_LVBCI) != 0 && (func_regs.cr & CR_LVBIE) != 0 {
                    interrupt_high = true;
                }
                if (val & SR_BCIS) != 0 && (func_regs.cr & CR_IOCE) != 0 {
                    interrupt_high = true;
                }
            }
        }

        if interrupt_high {
            regs.glob_sta |= int_mask;
            regs.irq_evt.as_ref().unwrap().write(1).unwrap();
        } else {
            regs.glob_sta &= !int_mask;
            if regs.glob_sta & GS_PIINT | GS_POINT | GS_MINT == 0 {
                regs.irq_evt.as_ref().unwrap().write(0).unwrap();
            }
        }
    }

    fn stop_all_audio(&mut self) {
        self.stop_audio(&Ac97Function::Input);
        self.stop_audio(&Ac97Function::Output);
        self.stop_audio(&Ac97Function::Microphone);
    }

    fn reset_audio_regs(&mut self) {
        self.stop_all_audio();
        let mut regs = self.regs.lock().unwrap();
        regs.pi_regs.do_reset();
        regs.po_regs.do_reset();
        regs.mc_regs.do_reset();
    }

    fn set_glob_cnt(&mut self, new_glob_cnt: u32) {
        // Only the reset bits are emulated, the GPI and PCM formatting are not supported.
        if new_glob_cnt & GLOB_CNT_COLD_RESET == 0 {
            self.reset_audio_regs();

            let mut regs = self.regs.lock().unwrap();
            regs.glob_cnt = new_glob_cnt & GLOB_CNT_STABLE_BITS;
            self.acc_sema = 0;
            return;
        }
        if new_glob_cnt & GLOB_CNT_WARM_RESET != 0 {
            // Check if running and if so, ignore. Warm reset is specified to no-op when the device
            // is playing or recording audio.
            if !self.audio_thread_po_run.load(Ordering::Relaxed) {
                self.stop_all_audio();
                let mut regs = self.regs.lock().unwrap();
                regs.glob_cnt = new_glob_cnt & !GLOB_CNT_WARM_RESET; // Auto-cleared reset bit.
                return;
            }
        }
        self.regs.lock().unwrap().glob_cnt = new_glob_cnt;
    }

    /// Return the number of sample sent to the buffer.
    fn play_buffer(
        regs: &Arc<Mutex<Ac97BusMasterRegs>>,
        mem: &GuestMemory,
        out_buffer: &mut PlaybackBuffer,
    ) -> PlaybackResult<usize> {
        let num_channels = 2;

        let mut regs = regs.lock().unwrap();

        // TODO - handle server buffers smaller or larger than the guest buffers.
        // maybe:
        // while have buffers
        //   while space in out_buffer and guest_buffer
        //     write
        //   }
        //   buffer_complete
        // }
        let mut samples_written = 0;
        while samples_written / num_channels < out_buffer.len() {
            let func_regs = regs.func_regs_mut(&Ac97Function::Output);
            let next_buffer = func_regs.civ;
            let descriptor_addr = func_regs.bdbar + next_buffer as u32 * DESCRIPTOR_LENGTH as u32;
            let buffer_addr: u32 = mem
                .read_obj_from_addr(GuestAddress(descriptor_addr as u64))
                .unwrap();
            let sample_size = 2;

            let samples_remaining = func_regs.picb as usize - samples_written;
            if samples_remaining == 0 {
                break;
            }
            let samples_to_write = std::cmp::min(
                out_buffer.len() * num_channels - samples_written,
                samples_remaining,
            );
            let read_pos = (buffer_addr + samples_written as u32 * sample_size) as u64;
            let buffer_offset = samples_to_write * sample_size as usize;
            mem.get_slice(read_pos, samples_to_write as u64 * sample_size as u64)
                .unwrap()
                .copy_to(&mut out_buffer.buffer[buffer_offset..]);
            samples_written += samples_to_write;
        }
        regs.po_pointer_update_time = Instant::now();
        if Self::buffer_completed(&mut regs, &mem, &Ac97Function::Output) {
            Err(PlaybackError::HitEnd)
        } else {
            Ok(samples_written / num_channels)
        }
    }

    // Return true if out of buffers.
    fn buffer_completed(
        regs: &mut Ac97BusMasterRegs,
        mem: &GuestMemory,
        func: &Ac97Function,
    ) -> bool {
        // Check if the completed descriptor wanted an interrupt on completion.
        let civ = regs.func_regs(func).civ;
        let descriptor_addr = regs.func_regs(func).bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
        let control_reg: u32 = mem
            .read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4))
            .unwrap();

        if control_reg & BD_IOC != 0 {
            let new_sr = regs.func_regs(func).sr | SR_BCIS;
            Self::update_sr(regs, func, new_sr);
        }

        Self::next_buffer_descriptor(regs, func)
    }

    fn next_buffer_descriptor(regs: &mut Ac97BusMasterRegs, func: &Ac97Function) -> bool {
        let civ = regs.func_regs(func).civ;
        let lvi = regs.func_regs(func).lvi;
        // If the current buffer was the last valid buffer, then update the status register to
        // indicate that the end of audio was hit and possibly raise an interrupt.
        if civ == lvi {
            let new_sr = regs.func_regs(func).sr | SR_LVBCI | SR_DCH | SR_CELV;
            Self::update_sr(regs, func, new_sr);
            true
        } else {
            let func_regs = regs.func_regs_mut(func);
            func_regs.civ = func_regs.piv;
            func_regs.piv = (func_regs.piv + 1) % 32; // Move PIV to the next buffer.
            false
        }
    }

    fn current_buffer_size(func_regs: &Ac97FunctionRegs, mem: &GuestMemory) -> usize {
        let civ = func_regs.civ;
        let descriptor_addr = func_regs.bdbar + civ as u32 * DESCRIPTOR_LENGTH as u32;
        let control_reg: u32 = mem
            .read_obj_from_addr(GuestAddress(descriptor_addr as u64 + 4))
            .unwrap();
        let buffer_len: usize = control_reg as usize & 0x0000_ffff;
        buffer_len
    }

}

#[cfg(test)]
mod test {
    use super::*;

    use std::time;

    use audio::DummyStreamSource;

    const GLOB_CNT: u64 = 0x2c;

    #[test]
    fn bm_bdbar() {
        let mut bm = Ac97BusMaster::new(
            GuestMemory::new(&[]).unwrap(),
            Box::new(DummyStreamSource::new()),
        );

        let bdbars = [0x00u64, 0x10, 0x20];

        // Make sure writes have no affect during cold reset.
        bm.writel(0x00, 0x5555_555f);
        assert_eq!(bm.readl(0x00), 0x0000_0000);

        // Relesase cold reset.
        bm.writel(GLOB_CNT, 0x0000_0002);

        // Tests that the base address is writable and that the bottom three bits are read only.
        for bdbar in &bdbars {
            assert_eq!(bm.readl(*bdbar), 0x0000_0000);
            bm.writel(*bdbar, 0x5555_555f);
            assert_eq!(bm.readl(*bdbar), 0x5555_5558);
        }
    }

    #[test]
    fn bm_status_reg() {
        let mut bm = Ac97BusMaster::new(
            GuestMemory::new(&[]).unwrap(),
            Box::new(DummyStreamSource::new()),
        );

        let sr_addrs = [0x06u64, 0x16, 0x26];

        for sr in &sr_addrs {
            assert_eq!(bm.readw(*sr), 0x0001);
            bm.writew(*sr, 0xffff);
            assert_eq!(bm.readw(*sr), 0x0001);
        }
    }

    #[test]
    fn bm_global_control() {
        let mut bm = Ac97BusMaster::new(
            GuestMemory::new(&[]).unwrap(),
            Box::new(DummyStreamSource::new()),
        );

        assert_eq!(bm.readl(GLOB_CNT), 0x0000_0000);

        // Relesase cold reset.
        bm.writel(GLOB_CNT, 0x0000_0002);

        // Check interrupt enable bits are writable.
        bm.writel(GLOB_CNT, 0x0000_0072);
        assert_eq!(bm.readl(GLOB_CNT), 0x0000_0072);

        // A Warm reset should doesn't affect register state and is auto cleared.
        bm.writel(0x00, 0x5555_5558);
        bm.writel(GLOB_CNT, 0x0000_0076);
        assert_eq!(bm.readl(GLOB_CNT), 0x0000_0072);
        assert_eq!(bm.readl(0x00), 0x5555_5558);
        // Check that a cold reset works, but setting bdbar and checking it is zeroed.
        bm.writel(0x00, 0x5555_555f);
        bm.writel(GLOB_CNT, 0x000_0070);
        assert_eq!(bm.readl(GLOB_CNT), 0x0000_0070);
        assert_eq!(bm.readl(0x00), 0x0000_0000);
    }

    #[test]
    fn start_playback() {
        const LVI_MASK: u8 = 0x1f; // Five bits for 32 total entries.
        const IOC_MASK: u32 = 0x8000_0000; // Interrupt on completion.
        let num_buffers = LVI_MASK as usize + 1;
        const BUFFER_SIZE: usize = 32768;
        const FRAGMENT_SIZE: usize = BUFFER_SIZE / 2;

        const GUEST_ADDR_BASE: u32 = 0x100_0000;
        let mem = GuestMemory::new(&[(GuestAddress(GUEST_ADDR_BASE as u64), 1024 * 1024 * 1024)])
            .unwrap();
        let mut bm = Ac97BusMaster::new(mem.clone(), Box::new(DummyStreamSource::new()));
        let mixer = Ac97Mixer::new();

        // Release cold reset.
        bm.writel(GLOB_CNT, 0x0000_0002);

        // Setup ping-pong buffers. A and B repeating for every possible index.
        bm.writel(PO_BDBAR, GUEST_ADDR_BASE);
        for i in 0..num_buffers {
            let pointer_addr = GuestAddress(GUEST_ADDR_BASE as u64 + i as u64 * 8);
            let control_addr = GuestAddress(GUEST_ADDR_BASE as u64 + i as u64 * 8 + 4);
            if i % 2 == 0 {
                mem.write_obj_at_addr(GUEST_ADDR_BASE, pointer_addr)
                    .unwrap();
            } else {
                mem.write_obj_at_addr(GUEST_ADDR_BASE + FRAGMENT_SIZE as u32, pointer_addr)
                    .unwrap();
            };
            mem.write_obj_at_addr(IOC_MASK | (FRAGMENT_SIZE as u32) / 2, control_addr)
                .unwrap();
        }

        bm.writeb(PO_LVI, LVI_MASK, &mixer);

        // Start.
        bm.writeb(PO_CR, CR_RPBM, &mixer);

        let start_time = Instant::now();

        std::thread::sleep(time::Duration::from_millis(50));
        let elapsed = start_time.elapsed().subsec_micros() as usize;
        let picb = bm.readw(PO_PICB);
        let mut civ = bm.readb(PO_CIV);
        assert_eq!(civ, 0);
        let pos = (FRAGMENT_SIZE - (picb as usize * 2)) / 4;

        let rate = ((pos * 1000) / elapsed) * 1000 + (((pos * 1000) % elapsed) * 1000) / elapsed;

        // Check that frames are consumed at close to 48k.
        // This wont be exact as during unit tests the thread scheduling is highly variable.
        assert!(
            rate > 24000 && rate < 80000, // Big range but UT thread scheduling is unstable.
            "Invalid sample rate: played {} in {}us. Rate: {} {}",
            pos,
            elapsed,
            rate,
            civ
        );

        assert!(bm.readw(PO_SR) & 0x01 == 0); // DMA is running.
        assert_ne!(0, bm.readw(PO_PICB));

        // civ should move eventually.
        for _i in 0..30 {
            if civ != 0 {
                break;
            }
            std::thread::sleep(time::Duration::from_millis(20));
            civ = bm.readb(PO_CIV);
        }

        assert_ne!(0, civ);

        // Buffer complete should be set as the IOC bit was set in the descriptor.
        assert!(bm.readw(MC_SR) & SR_BCIS != 0);
        // Clear the BCIS bit
        bm.writew(MC_SR, SR_BCIS);
        assert!(bm.readw(MC_SR) & SR_BCIS == 0);

        // Set last valid to the two buffers from now and wait until it is hit.
        bm.writeb(PO_LVI, civ + 2, &mixer);
        std::thread::sleep(time::Duration::from_millis(1000));
        assert!(bm.readw(MC_SR) & SR_LVBCI != 0);
        assert_eq!(bm.readb(PO_LVI), bm.readb(PO_CIV));

        // Stop.
        bm.writeb(PO_CR, 0, &mixer);
        assert!(bm.readw(PO_SR) & 0x01 != 0); // DMA is not running.
    }
}
