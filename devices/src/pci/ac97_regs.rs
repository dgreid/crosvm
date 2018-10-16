// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(dead_code)]
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// Size of IO register regions
pub const MIXER_REGS_SIZE: u64 = 0x100;
pub const MASTER_REGS_SIZE: u64 = 0x400;

// Global Control
pub const GLOB_CNT_COLD_RESET: u32 = 0x0000_0002;
pub const GLOB_CNT_WARM_RESET: u32 = 0x0000_0004;
pub const GLOB_CNT_STABLE_BITS: u32 = 0x0000_007f; // Bits not affected by reset.

// Global status
pub const GLOB_STA_RESET_VAL: u32 = 0x0000_0100; // primary codec ready set.
// glob_sta bits
pub const GS_MD3: u32 = 1 << 17;
pub const GS_AD3: u32 = 1 << 16;
pub const GS_RCS: u32 = 1 << 15;
pub const GS_B3S12: u32 = 1 << 14;
pub const GS_B2S12: u32 = 1 << 13;
pub const GS_B1S12: u32 = 1 << 12;
pub const GS_S1R1: u32 = 1 << 11;
pub const GS_S0R1: u32 = 1 << 10;
pub const GS_S1CR: u32 = 1 << 9;
pub const GS_S0CR: u32 = 1 << 8;
pub const GS_MINT: u32 = 1 << 7;
pub const GS_POINT: u32 = 1 << 6;
pub const GS_PIINT: u32 = 1 << 5;
pub const GS_RSRVD: u32 = 1 << 4 | 1 << 3;
pub const GS_MOINT: u32 = 1 << 2;
pub const GS_MIINT: u32 = 1 << 1;
pub const GS_GSCI: u32 = 1;
pub const GS_RO_MASK: u32 = GS_B3S12
    | GS_B2S12
    | GS_B1S12
    | GS_S1CR
    | GS_S0CR
    | GS_MINT
    | GS_POINT
    | GS_PIINT
    | GS_RSRVD
    | GS_MOINT
    | GS_MIINT;
pub const GS_VALID_MASK: u32 = 0x0003_ffff;
pub const GS_WCLEAR_MASK: u32 = GS_RCS | GS_S1R1 | GS_S0R1 | GS_GSCI;

// Audio funciton registers.

// Status Register Bits.
pub const SR_DCH: u16 = 0x01;
pub const SR_CELV: u16 = 0x02;
pub const SR_LVBCI: u16 = 0x04;
pub const SR_BCIS: u16 = 0x08;
pub const SR_FIFOE: u16 = 0x10;
pub const SR_VALID_MASK: u16 = 0x1f;
pub const SR_WCLEAR_MASK: u16 = SR_FIFOE | SR_BCIS | SR_LVBCI;
pub const SR_RO_MASK: u16 = SR_DCH | SR_CELV;
pub const SR_INT_MASK: u16 = SR_BCIS | SR_LVBCI;

// Control Register Bits.
pub const CR_RPBM: u8 = 0x01;
pub const CR_RR: u8 = 0x02;
pub const CR_LVBIE: u8 = 0x04;
pub const CR_FEIE: u8 = 0x08;
pub const CR_IOCE: u8 = 0x10;
pub const CR_VALID_MASK: u8 = 0x1f;
pub const CR_DONT_CLEAR_MASK: u8 = CR_IOCE | CR_FEIE | CR_LVBIE;

// Mixer register bits
pub const MUTE_REG_BIT: u16 = 0x8000;
pub const VOL_REG_MASK: u16 = 0x003f;
pub const MIXER_VOL_MASK: u16 = 0x001f;
pub const MIXER_VOL_LEFT_SHIFT: usize = 8;
pub const MIXER_MIC_20DB: u16 = 0x0040;
// Powerdown reg
pub const PD_REG_STATUS_MASK: u16 = 0x000f;
pub const PD_REG_OUTPUT_MUTE_MASK: u16 = 0xb200;
pub const PD_REG_INPUT_MUTE_MASK: u16 = 0x0d00;

// Registers for individual audio functions.
// Some are atomic as they need to be updated from the audio thread.
#[derive(Clone, Default)]
pub struct Ac97FunctionRegs {
    pub bdbar: u32,
    pub civ: Arc<AtomicUsize>, // Actually u8
    pub lvi: u8,
    pub sr: u16,
    pub picb: Arc<AtomicUsize>, // Actually u16
    pub piv: Arc<AtomicUsize>, // Actually u8
    pub cr: u8,
}

impl Ac97FunctionRegs {
    pub fn new() -> Self {
        Ac97FunctionRegs {
            sr: SR_DCH,
            ..Default::default()
        }
    }

    pub fn do_reset(&mut self) {
        self.bdbar = 0;
        self.civ.store(0, Ordering::Relaxed);
        self.lvi = 0;
        self.sr = SR_DCH;
        self.picb.store(0, Ordering::Relaxed);
        self.piv.store(0, Ordering::Relaxed);
        self.cr = self.cr & CR_DONT_CLEAR_MASK;
    }

    /// Read register 4, 5, and 6 as one 32 bit word.
    /// According to the ICH spec, reading these three with one 32 bit access is allowed.
    pub fn atomic_status_regs(&self) -> u32 {
        self.civ.load(Ordering::Relaxed) as u32 | (self.lvi as u32) << 8 | (self.sr as u32) << 16
    }
}

pub enum Ac97Function {
    Input,
    Output,
    Microphone,
}
