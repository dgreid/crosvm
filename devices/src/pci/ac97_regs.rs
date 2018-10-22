// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(dead_code)]

// Audio Mixer Registers
// 00h Reset
// 02h Master Volume Mute
// 04h Headphone Volume Mute
// 06h Master Volume Mono Mute
// 08h Master Tone (R & L)
// 0Ah PC_BEEP Volume Mute
// 0Ch Phone Volume Mute
// 0Eh Mic Volume Mute
// 10h Line In Volume Mute
// 12h CD Volume Mute
// 14h Video Volume Mute
// 16h Aux Volume Mute
// 18h PCM Out Volume Mute
// 1Ah Record Select
// 1Ch Record Gain Mute
// 1Eh Record Gain Mic Mute
// 20h General Purpose
// 22h 3D Control
// 24h AC’97 RESERVED
// 26h Powerdown Ctrl/Stat
// 28h Extended Audio
// 2Ah Extended Audio Ctrl/Stat
//
// Bus Master regs from ICH spec:
// 00h PI_BDBAR PCM In Buffer Descriptor list Base Address Register
// 04h PI_CIV PCM In Current Index Value
// 05h PI_LVI PCM In Last Valid Index
// 06h PI_SR PCM In Status Register
// 08h PI_PICB PCM In Position In Current Buffer
// 0Ah PI_PIV PCM In Prefetched Index Value
// 0Bh PI_CR PCM In Control Register
// 10h PO_BDBAR PCM Out Buffer Descriptor list Base Address Register
// 14h PO_CIV PCM Out Current Index Value
// 15h PO_LVI PCM Out Last Valid Index
// 16h PO_SR PCM Out Status Register
// 18h PO_PICB PCM Out Position In Current Buffer
// 1Ah PO_PIV PCM Out Prefetched Index Value
// 1Bh PO_CR PCM Out Control Register
// 20h MC_BDBAR Mic. In Buffer Descriptor list Base Address Register
// 24h PM_CIV Mic. In Current Index Value
// 25h MC_LVI Mic. In Last Valid Index
// 26h MC_SR Mic. In Status Register
// 28h MC_PICB Mic In Position In Current Buffer
// 2Ah MC_PIV Mic. In Prefetched Index Value
// 2Bh MC_CR Mic. In Control Register
// 2Ch GLOB_CNT Global Control
// 30h GLOB_STA Global Status
// 34h ACC_SEMA Codec Write Semaphore Register

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
// Play Out
pub const PO_BASE: u64 = 0x10;
pub const PO_BDBAR: u64 = PO_BASE;
pub const PO_CIV: u64 = PO_BASE + 0x4;
pub const PO_LVI: u64 = PO_BASE + 0x5;
pub const PO_SR: u64 = PO_BASE + 0x6;
pub const PO_PICB: u64 = PO_BASE + 0x8;
pub const PO_PIV: u64 = PO_BASE + 0xa;
pub const PO_CR: u64 = PO_BASE + 0xb;

// Microphone
pub const MC_BASE: u64 = 0x10;
pub const MC_BDBAR: u64 = PO_BASE;
pub const MC_CIV: u64 = PO_BASE + 0x4;
pub const MC_LVI: u64 = PO_BASE + 0x5;
pub const MC_SR: u64 = PO_BASE + 0x6;
pub const MC_PICB: u64 = PO_BASE + 0x8;
pub const MC_PIV: u64 = PO_BASE + 0xa;
pub const MC_CR: u64 = PO_BASE + 0xb;

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
#[derive(Clone, Default)]
pub struct Ac97FunctionRegs {
    pub bdbar: u32,
    pub civ: u8,
    pub lvi: u8,
    pub sr: u16,
    pub picb: u16,
    pub piv: u8,
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
        self.civ = 0;
        self.lvi = 0;
        self.sr = SR_DCH;
        self.picb = 0;
        self.piv = 0;
        self.cr = self.cr & CR_DONT_CLEAR_MASK;
    }

    /// Read register 4, 5, and 6 as one 32 bit word.
    /// According to the ICH spec, reading these three with one 32 bit access is allowed.
    pub fn atomic_status_regs(&self) -> u32 {
        self.civ as u32| (self.lvi as u32) << 8 | (self.sr as u32) << 16
    }
}

#[derive(Copy, Clone)]
pub enum Ac97Function {
    Input,
    Output,
    Microphone,
}

// Buffer descriptors are four bytes of pointer and 4 bytes of control/length.
pub const DESCRIPTOR_LENGTH: usize = 8;
pub const BD_IOC: u32 = 1 << 31;
