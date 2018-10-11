// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern crate devices;
extern crate sys_util;

use std::time;

use devices::Ac97;

const GLOB_CNT: u64 = 0x2c;

#[test]
fn bm_bdbar() {
    let mut ac97 = Ac97::new();

    let bdbars = [0x00u64, 0x10, 0x20];

    // Make sure writes have no affect during cold reset.
    ac97.bm_writel(0x00, 0x5555_555f);
    assert_eq!(ac97.bm_readl(0x00), 0x0000_0000);
 
    // Relesase cold reset.
    ac97.bm_writel(GLOB_CNT, 0x0000_0002);

    // Tests that the base address is writable and that the bottom three bits are read only.
    for bdbar in &bdbars {
        assert_eq!(ac97.bm_readl(*bdbar), 0x0000_0000);
        ac97.bm_writel(*bdbar, 0x5555_555f);
        assert_eq!(ac97.bm_readl(*bdbar), 0x5555_5558);
    }
}

#[test]
fn bm_status_reg() {
    let mut ac97 = Ac97::new();

    let sr_addrs = [0x06u64, 0x16, 0x26];

    for sr in &sr_addrs {
        assert_eq!(ac97.bm_readw(*sr), 0x0001);
        ac97.bm_writew(*sr, 0xffff);
        assert_eq!(ac97.bm_readw(*sr), 0x0001);
    }
}

#[test]
fn bm_global_control() {
    let mut ac97 = Ac97::new();

    assert_eq!(ac97.bm_readl(GLOB_CNT), 0x0000_0000);

    // Relesase cold reset.
    ac97.bm_writel(GLOB_CNT, 0x0000_0002);

    // Check interrupt enable bits are writable.
    ac97.bm_writel(GLOB_CNT, 0x0000_0072);
    assert_eq!(ac97.bm_readl(GLOB_CNT), 0x0000_0072);

    // A Warm reset should doesn't affect register state and is auto cleared.
    ac97.bm_writel(0x00, 0x5555_5558);
    ac97.bm_writel(GLOB_CNT, 0x0000_0076);
    assert_eq!(ac97.bm_readl(GLOB_CNT), 0x0000_0072);
    assert_eq!(ac97.bm_readl(0x00), 0x5555_5558);
    // Check that a cold reset works, but setting bdbar and checking it is zeroed.
    ac97.bm_writel(0x00, 0x5555_555f);
    ac97.bm_writel(GLOB_CNT, 0x000_0070);
    assert_eq!(ac97.bm_readl(GLOB_CNT), 0x0000_0070);
    assert_eq!(ac97.bm_readl(0x00), 0x0000_0000);
}

#[test]
fn test_measure_clock() {
    const LVI_MASK: u8 = 0x1f; // Five bits for 32 total entries.
    const IOC_MASK: u32 = 0x8000_0000; // Interrupt on completion.
    let num_buffers = LVI_MASK as usize + 1;
    let mut bdbar: Vec<u32> = vec![0; num_buffers * 2]; // Each entry is a pointer and a length.
    const buffer_size: usize = 1024 * 2;

    // Initialize PO registers.
    const PO_BASE: u64 = 0x10;
    const PO_BDBAR: u64 = PO_BASE;
    const PO_CIV: u64 = PO_BASE + 0x4;
    const PO_LVI: u64 = PO_BASE + 0x5;
    const PO_SR: u64 = PO_BASE + 0x6;
    const PO_PICB: u64 = PO_BASE + 0x8;
    const PO_PIV: u64 = PO_BASE + 0xa;
    const PO_CR: u64 = PO_BASE + 0xb;
    const CR_RPBM: u8 = 0x01;

    let mut ac97 = Ac97::new();

    // Release cold reset.
    ac97.bm_writel(GLOB_CNT, 0x0000_0002);

    // Setup ping-pong buffers. A and B repeating for every possible index.
    let buffer: Vec<i16> = vec![0; buffer_size];
    ac97.bm_writel(PO_BDBAR, &buffer[0] as * const i16 as u32);
    let (buffer_a, buffer_b) = buffer.split_at(buffer_size/2);
    for i in 0..num_buffers {
        bdbar[i*2] = if i % 2 == 0 {
            &buffer_a[0] as * const i16 as u32
        } else {
            &buffer_b[0] as * const i16 as u32
        };
        bdbar[i*2 + 1] = IOC_MASK | (buffer_size as u32 / 2);
    }

    ac97.bm_writeb(PO_CIV, 0);
    ac97.bm_writeb(PO_LVI, LVI_MASK);

    // TODO(dgreid) - clear interrupts.
    
    // Start.
    ac97.bm_writeb(PO_CR, CR_RPBM);

    std::thread::sleep(time::Duration::from_millis(10));

    assert_ne!(0, ac97.bm_readw(PO_PICB));
    assert_ne!(0, ac97.bm_readw(PO_CIV));
 
    // Stop.
    ac97.bm_writeb(PO_CR, 0);
}
