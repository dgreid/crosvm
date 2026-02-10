// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! GICv3 emulation for macOS HVF.
//!
//! This module provides a minimal GICv3 emulation sufficient to allow Linux guests
//! to boot and receive timer interrupts. It emulates:
//! - Distributor (GICD) - manages SPIs (Shared Peripheral Interrupts)
//! - Redistributor (GICR) - per-CPU interface for PPIs and SGIs
//!
// NOTE: Atomic operations throughout use SeqCst ordering. While Acquire/Release
// would be sufficient for the cross-VCPU publish/consume pattern, SeqCst is used
// consistently for simplicity and to avoid subtle ordering bugs. This can be
// relaxed to Acquire/Release in the future if GIC emulation becomes a performance
// bottleneck.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use base::trace;
use sync::Mutex;
use vm_control::DeviceId;
use vm_control::PlatformDeviceId;

use crate::BusAccessInfo;
use crate::BusDevice;
use crate::Suspendable;

/// Number of SPIs supported (aligned to 32)
pub const GIC_NR_SPIS: u32 = 32;

/// Number of SGIs (Software Generated Interrupts)
pub const GIC_NR_SGIS: u32 = 16;

/// Number of PPIs (Private Peripheral Interrupts)
pub const GIC_NR_PPIS: u32 = 16;

/// Total number of interrupts per CPU (SGIs + PPIs)
pub const GIC_NR_PRIVATE_IRQS: u32 = GIC_NR_SGIS + GIC_NR_PPIS;

/// First SPI interrupt number
pub const GIC_SPI_BASE: u32 = GIC_NR_PRIVATE_IRQS;

/// Virtual timer PPI number (PPI 11, INTID 27)
pub const VTIMER_PPI: u32 = 11;
pub const VTIMER_INTID: u32 = GIC_NR_SGIS + VTIMER_PPI; // 16 + 11 = 27

/// GIC Distributor size
pub const GICD_SIZE: u64 = 0x10000;

/// GIC Redistributor size per CPU
pub const GICR_SIZE: u64 = 0x20000;

// GICD (Distributor) register offsets
const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IIDR: u64 = 0x0008;
const GICD_TYPER2: u64 = 0x000C;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_ISPENDR: u64 = 0x0200;
const GICD_ICPENDR: u64 = 0x0280;
const GICD_ISACTIVER: u64 = 0x0300;
const GICD_ICACTIVER: u64 = 0x0380;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ITARGETSR: u64 = 0x0800;
const GICD_ICFGR: u64 = 0x0C00;
const GICD_IGRPMODR: u64 = 0x0D00;
const GICD_PIDR2: u64 = 0xFFE8;
const GICD_IROUTER: u64 = 0x6000;

// GICR (Redistributor) register offsets - RD base (first 64KB)
const GICR_CTLR: u64 = 0x0000;
const GICR_IIDR: u64 = 0x0004;
const GICR_TYPER: u64 = 0x0008;
const GICR_TYPER_HI: u64 = 0x000C; // High 32 bits of TYPER
const GICR_WAKER: u64 = 0x0014;
const GICR_PIDR2: u64 = 0xFFE8;

// GICR SGI base (second 64KB, offset 0x10000)
const GICR_SGI_BASE: u64 = 0x10000;
const GICR_IGROUPR0: u64 = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;
const GICR_ICENABLER0: u64 = GICR_SGI_BASE + 0x0180;
const GICR_ISPENDR0: u64 = GICR_SGI_BASE + 0x0200;
const GICR_ICPENDR0: u64 = GICR_SGI_BASE + 0x0280;
const GICR_ISACTIVER0: u64 = GICR_SGI_BASE + 0x0300;
const GICR_ICACTIVER0: u64 = GICR_SGI_BASE + 0x0380;
const GICR_IPRIORITYR: u64 = GICR_SGI_BASE + 0x0400;
const GICR_ICFGR0: u64 = GICR_SGI_BASE + 0x0C00;
const GICR_ICFGR1: u64 = GICR_SGI_BASE + 0x0C04;
const GICR_IGRPMODR0: u64 = GICR_SGI_BASE + 0x0D00;

// GICD_CTLR bits
const GICD_CTLR_ENABLE_G0: u32 = 1 << 0;
const GICD_CTLR_ENABLE_G1NS: u32 = 1 << 1;
const GICD_CTLR_ENABLE_G1S: u32 = 1 << 2;
const GICD_CTLR_ARE_S: u32 = 1 << 4;
const GICD_CTLR_ARE_NS: u32 = 1 << 5;
const GICD_CTLR_DS: u32 = 1 << 6;
const GICD_CTLR_RWP: u32 = 1 << 31;

// GICR_WAKER bits
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;

// Architecture version in PIDR2
const GIC_PIDR2_ARCH_GICV3: u32 = 0x3 << 4;

/// Spurious interrupt ID (returned when no interrupt is pending)
pub const GIC_SPURIOUS_INTID: u32 = 1023;

/// Priority representing idle state (lowest priority)
const GIC_IDLE_PRIORITY: u8 = 0xff;

/// GIC CPU Interface (ICC registers) - one per CPU.
///
/// This emulates the GICv3 CPU interface accessed via system registers (ICC_*_EL1).
/// Instead of relying on HVF's in-kernel GIC for interrupt delivery, we handle ICC
/// register traps in userspace and use `hv_vcpu_set_pending_interrupt()` to signal
/// IRQs to the VCPU.
#[derive(Debug)]
pub struct GicCpuInterface {
    /// Priority mask register (ICC_PMR_EL1) - interrupts >= this priority are masked
    pub pmr: AtomicU8,
    /// Binary point registers for Groups 0 and 1 (ICC_BPR0/1_EL1)
    pub bpr: [AtomicU8; 2],
    /// Group enable flags (ICC_IGRPEN0/1_EL1)
    pub group_enable: [AtomicBool; 2],
    /// Current running priority (highest priority of active interrupts)
    pub running_priority: AtomicU8,
    /// Currently active interrupt INTID (1023 = none)
    pub active_intid: AtomicU32,
    /// Active priority registers (ICC_AP0R/AP1R_EL1) - 4 registers per group
    pub apr: [[AtomicU32; 4]; 2],
    /// Control register (ICC_CTLR_EL1)
    pub ctlr: AtomicU32,
    /// Flag: IRQ should be signaled to VCPU via hv_vcpu_set_pending_interrupt()
    pub irq_pending: AtomicBool,
}

impl Default for GicCpuInterface {
    fn default() -> Self {
        Self::new()
    }
}

impl GicCpuInterface {
    /// Create a new CPU interface in reset state
    pub fn new() -> Self {
        Self {
            // PMR reset to 0xFF (allow all priorities). The GICv3 spec says reset is 0
            // (mask all), but we use 0xFF because group_enable defaults to false and
            // gates all interrupt delivery. This avoids requiring the guest to set PMR
            // before enabling interrupts, matching typical boot sequence expectations.
            pmr: AtomicU8::new(0xff),
            bpr: [AtomicU8::new(0), AtomicU8::new(0)],
            group_enable: [AtomicBool::new(false), AtomicBool::new(false)],
            running_priority: AtomicU8::new(GIC_IDLE_PRIORITY),
            active_intid: AtomicU32::new(GIC_SPURIOUS_INTID),
            apr: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))),
            ctlr: AtomicU32::new(0),
            irq_pending: AtomicBool::new(false),
        }
    }

    /// Read ICC_SRE_EL1 - always return 0x7 (SRE=1, DFB=1, DIB=1)
    /// This indicates system register access is enabled and required.
    pub fn read_sre(&self) -> u64 {
        0x7
    }

    /// Read ICC_PMR_EL1 - priority mask
    pub fn read_pmr(&self) -> u64 {
        self.pmr.load(Ordering::Acquire) as u64
    }

    /// Write ICC_PMR_EL1 - priority mask
    pub fn write_pmr(&self, value: u64) {
        self.pmr.store(value as u8, Ordering::Release);
        trace!("ICC_PMR_EL1 write: {:#x}", value);
    }

    /// Read ICC_BPR0_EL1 or ICC_BPR1_EL1
    pub fn read_bpr(&self, group: usize) -> u64 {
        if group < 2 {
            self.bpr[group].load(Ordering::Acquire) as u64
        } else {
            0
        }
    }

    /// Write ICC_BPR0_EL1 or ICC_BPR1_EL1
    pub fn write_bpr(&self, group: usize, value: u64) {
        if group < 2 {
            self.bpr[group].store(value as u8, Ordering::Release);
        }
    }

    /// Read ICC_CTLR_EL1
    pub fn read_ctlr(&self) -> u64 {
        self.ctlr.load(Ordering::Acquire) as u64
    }

    /// Write ICC_CTLR_EL1
    pub fn write_ctlr(&self, value: u64) {
        // Only allow writing safe bits
        self.ctlr.store(value as u32, Ordering::Release);
    }

    /// Read ICC_IGRPEN0_EL1 or ICC_IGRPEN1_EL1
    pub fn read_igrpen(&self, group: usize) -> u64 {
        if group < 2 {
            self.group_enable[group].load(Ordering::Acquire) as u64
        } else {
            0
        }
    }

    /// Write ICC_IGRPEN0_EL1 or ICC_IGRPEN1_EL1
    pub fn write_igrpen(&self, group: usize, value: u64) {
        if group < 2 {
            self.group_enable[group].store((value & 1) != 0, Ordering::Release);
        }
    }

    /// Check if an interrupt can preempt the current running priority.
    ///
    /// An interrupt can preempt if:
    /// 1. Its priority is numerically lower (higher priority) than running_priority
    /// 2. Its priority is numerically lower than PMR (priority mask)
    pub fn can_preempt(&self, priority: u8) -> bool {
        let running = self.running_priority.load(Ordering::Acquire);
        let pmr = self.pmr.load(Ordering::Acquire);
        priority < running && priority < pmr
    }

    /// Check if group 1 interrupts are enabled (Group 1 NS is what Linux uses)
    pub fn is_group1_enabled(&self) -> bool {
        self.group_enable[1].load(Ordering::Acquire)
    }

    /// Acknowledge an interrupt (ICC_IAR1_EL1 read).
    ///
    /// This is called when the guest reads ICC_IAR1_EL1 to get the highest
    /// priority pending interrupt. The interrupt is moved from pending to
    /// active state.
    ///
    /// Returns the INTID of the acknowledged interrupt, or 1023 (spurious) if
    /// no interrupt is deliverable.
    // LIMITATION: Only one interrupt can be active at a time per CPU interface.
    // GICv3 supports nested/preempted interrupts via Active Priority Registers
    // (APR), but we only track a single active_intid. This is sufficient for
    // Linux which uses EOImode=0 and does not nest IRQs. If preemption support
    // is needed, the APR tracking must be implemented.
    pub fn acknowledge(&self, pending_intid: Option<u32>, pending_priority: u8) -> u32 {
        // Check if we have a pending interrupt and it can preempt
        let intid = match pending_intid {
            Some(id) if self.can_preempt(pending_priority) => id,
            _ => {
                trace!("ICC_IAR1_EL1 read: spurious (no pending or can't preempt)");
                return GIC_SPURIOUS_INTID;
            }
        };

        // Move interrupt to active state
        self.active_intid.store(intid, Ordering::Release);
        self.running_priority.store(pending_priority, Ordering::Release);

        intid
    }

    /// End of interrupt (ICC_EOIR1_EL1 write).
    ///
    /// This is called when the guest writes to ICC_EOIR1_EL1 to signal
    /// that interrupt handling is complete. The interrupt is deactivated.
    pub fn end_of_interrupt(&self, intid: u32) {
        let active = self.active_intid.load(Ordering::Acquire);
        if active == intid {
            self.active_intid.store(GIC_SPURIOUS_INTID, Ordering::Release);
            self.running_priority.store(GIC_IDLE_PRIORITY, Ordering::Release);
        }
    }

    /// Read ICC_RPR_EL1 - running priority register
    pub fn read_rpr(&self) -> u64 {
        self.running_priority.load(Ordering::Acquire) as u64
    }

    /// Set the IRQ pending flag (called when distributor has a deliverable interrupt)
    pub fn set_irq_pending(&self) {
        self.irq_pending.store(true, Ordering::Release);
    }

    /// Clear and return the IRQ pending flag (called before vcpu.run())
    pub fn take_irq_pending(&self) -> bool {
        self.irq_pending.swap(false, Ordering::AcqRel)
    }

    /// Check if IRQ pending flag is set
    pub fn is_irq_pending(&self) -> bool {
        self.irq_pending.load(Ordering::Acquire)
    }
}

/// Per-CPU redistributor state
#[derive(Debug)]
pub struct RedistributorState {
    /// CPU ID this redistributor is for
    pub cpu_id: usize,
    /// Whether this is the last redistributor
    pub is_last: bool,
    /// GICR_CTLR register
    pub ctlr: AtomicU32,
    /// GICR_WAKER register
    pub waker: AtomicU32,
    /// Group register for SGIs/PPIs (GICR_IGROUPR0)
    pub group0: AtomicU32,
    /// Enable register for SGIs/PPIs (GICR_ISENABLER0)
    pub enable0: AtomicU32,
    /// Pending register for SGIs/PPIs (GICR_ISPENDR0)
    pub pending0: AtomicU32,
    /// Active register for SGIs/PPIs (GICR_ISACTIVER0)
    pub active0: AtomicU32,
    /// Configuration register for SGIs (GICR_ICFGR0)
    pub icfgr0: AtomicU32,
    /// Configuration register for PPIs (GICR_ICFGR1)
    pub icfgr1: AtomicU32,
    /// Group modifier for SGIs/PPIs (GICR_IGRPMODR0)
    pub igrpmodr0: AtomicU32,
    /// Priority registers (8 bits per interrupt, 32 interrupts = 8 registers)
    pub priority: [AtomicU32; 8],
}

impl RedistributorState {
    pub fn new(cpu_id: usize, is_last: bool) -> Self {
        Self {
            cpu_id,
            is_last,
            ctlr: AtomicU32::new(0),
            // Start awake (not sleeping)
            waker: AtomicU32::new(0),
            // All interrupts in Group 1 NS
            group0: AtomicU32::new(0xFFFFFFFF),
            // SGIs/PPIs disabled by default
            enable0: AtomicU32::new(0),
            pending0: AtomicU32::new(0),
            active0: AtomicU32::new(0),
            icfgr0: AtomicU32::new(0),
            icfgr1: AtomicU32::new(0),
            igrpmodr0: AtomicU32::new(0),
            // Default priority 0 (highest). The GICv3 spec says reset priority is
            // IMPLEMENTATION DEFINED. Using 0 ensures all interrupts can be delivered
            // before the guest configures priorities. This differs from hardware GICs
            // which typically default to 0xa0, but works because group_enable gates
            // delivery until the guest enables the GIC.
            priority: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// Handle read from redistributor register space
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            GICR_CTLR => self.ctlr.load(Ordering::SeqCst),
            GICR_IIDR => {
                // Implementer identification (crosvm)
                0x43_00_00_00
            }
            GICR_TYPER => {
                // Low 32 bits of TYPER
                // Bits [23:8] = Processor_Number (CPU ID)
                // Bit 4 = Last (if this is the last redistributor)
                let mut typer: u32 = (self.cpu_id as u32) << 8;
                if self.is_last {
                    typer |= 1 << 4;
                }
                typer
            }
            GICR_TYPER_HI => {
                // High 32 bits of TYPER: Affinity 3 [31:24], 2 [23:16], 1 [15:8]
                // Bits [7:0] = Affinity 0 value, must match MPIDR Aff0 (CPU ID)
                self.cpu_id as u32
            }
            GICR_WAKER => self.waker.load(Ordering::SeqCst),
            GICR_PIDR2 => GIC_PIDR2_ARCH_GICV3,
            GICR_IGROUPR0 => self.group0.load(Ordering::SeqCst),
            GICR_ISENABLER0 | GICR_ICENABLER0 => self.enable0.load(Ordering::SeqCst),
            GICR_ISPENDR0 | GICR_ICPENDR0 => self.pending0.load(Ordering::SeqCst),
            GICR_ISACTIVER0 | GICR_ICACTIVER0 => self.active0.load(Ordering::SeqCst),
            GICR_ICFGR0 => self.icfgr0.load(Ordering::SeqCst),
            GICR_ICFGR1 => self.icfgr1.load(Ordering::SeqCst),
            GICR_IGRPMODR0 => self.igrpmodr0.load(Ordering::SeqCst),
            o if o >= GICR_IPRIORITYR && o < GICR_IPRIORITYR + 32 => {
                let idx = ((o - GICR_IPRIORITYR) / 4) as usize;
                if idx < self.priority.len() {
                    self.priority[idx].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            _ => {
                trace!("GICR read from unhandled offset {:#x}", offset);
                0
            }
        };

        write_le_u32(data, value);
    }

    /// Handle write to redistributor register space
    pub fn write(&self, offset: u64, data: &[u8]) {
        let value = read_le_u32(data);

        match offset {
            GICR_CTLR => {
                self.ctlr.store(value, Ordering::SeqCst);
            }
            GICR_WAKER => {
                // When ProcessorSleep is cleared, clear ChildrenAsleep
                let mut new_value = value;
                if (value & GICR_WAKER_PROCESSOR_SLEEP) == 0 {
                    new_value &= !GICR_WAKER_CHILDREN_ASLEEP;
                }
                self.waker.store(new_value, Ordering::SeqCst);
            }
            GICR_IGROUPR0 => {
                self.group0.store(value, Ordering::SeqCst);
            }
            GICR_ISENABLER0 => {
                // Set enable bits
                self.enable0.fetch_or(value, Ordering::SeqCst);
            }
            GICR_ICENABLER0 => {
                // Clear enable bits
                self.enable0.fetch_and(!value, Ordering::SeqCst);
            }
            GICR_ISPENDR0 => {
                // Set pending bits
                self.pending0.fetch_or(value, Ordering::SeqCst);
            }
            GICR_ICPENDR0 => {
                // Clear pending bits
                self.pending0.fetch_and(!value, Ordering::SeqCst);
            }
            GICR_ISACTIVER0 => {
                // Set active bits
                self.active0.fetch_or(value, Ordering::SeqCst);
            }
            GICR_ICACTIVER0 => {
                // Clear active bits (EOI)
                self.active0.fetch_and(!value, Ordering::SeqCst);
            }
            GICR_ICFGR0 => {
                self.icfgr0.store(value, Ordering::SeqCst);
            }
            GICR_ICFGR1 => {
                self.icfgr1.store(value, Ordering::SeqCst);
            }
            GICR_IGRPMODR0 => {
                self.igrpmodr0.store(value, Ordering::SeqCst);
            }
            o if o >= GICR_IPRIORITYR && o < GICR_IPRIORITYR + 32 => {
                let idx = ((o - GICR_IPRIORITYR) / 4) as usize;
                if idx < self.priority.len() {
                    self.priority[idx].store(value, Ordering::SeqCst);
                }
            }
            _ => {
                trace!("GICR write to unhandled offset {:#x} value {:#x}", offset, value);
            }
        }
    }

    /// Set a PPI as pending
    pub fn set_ppi_pending(&self, ppi: u32) {
        if ppi < GIC_NR_PPIS {
            let intid = GIC_NR_SGIS + ppi;
            self.pending0.fetch_or(1 << intid, Ordering::SeqCst);
        }
    }

    /// Clear a PPI pending state
    pub fn clear_ppi_pending(&self, ppi: u32) {
        if ppi < GIC_NR_PPIS {
            let intid = GIC_NR_SGIS + ppi;
            self.pending0.fetch_and(!(1 << intid), Ordering::SeqCst);
        }
    }

    /// Check if an interrupt is pending and enabled
    pub fn is_irq_deliverable(&self, intid: u32) -> bool {
        if intid >= GIC_NR_PRIVATE_IRQS {
            return false;
        }
        let mask = 1 << intid;
        let pending = self.pending0.load(Ordering::SeqCst);
        let enabled = self.enable0.load(Ordering::SeqCst);
        (pending & enabled & mask) != 0
    }

    /// Get the priority of a private interrupt (SGI/PPI)
    pub fn get_irq_priority(&self, intid: u32) -> u8 {
        if intid >= GIC_NR_PRIVATE_IRQS {
            return 0xff;
        }
        // Each priority register holds 4 priorities (8 bits each)
        let reg_idx = (intid / 4) as usize;
        let byte_idx = (intid % 4) as usize;
        if reg_idx >= self.priority.len() {
            return 0xff;
        }
        let reg = self.priority[reg_idx].load(Ordering::SeqCst);
        ((reg >> (byte_idx * 8)) & 0xff) as u8
    }

    /// Find the highest priority pending and enabled private interrupt.
    /// Returns (INTID, priority) or None if no interrupt is pending.
    pub fn get_highest_priority_pending(&self) -> Option<(u32, u8)> {
        let mut best: Option<(u32, u8)> = None;

        for intid in 0..GIC_NR_PRIVATE_IRQS {
            if self.is_irq_deliverable(intid) {
                let priority = self.get_irq_priority(intid);
                match best {
                    None => best = Some((intid, priority)),
                    Some((_, best_prio)) if priority < best_prio => {
                        best = Some((intid, priority));
                    }
                    _ => {}
                }
            }
        }

        best
    }
}

/// GIC Distributor state
pub struct DistributorState {
    /// GICD_CTLR register
    pub ctlr: AtomicU32,
    /// Number of CPUs
    pub num_cpus: usize,
    /// Group registers for SPIs
    pub group: [AtomicU32; 1],
    /// Enable registers for SPIs
    pub enable: [AtomicU32; 1],
    /// Pending registers for SPIs
    pub pending: [AtomicU32; 1],
    /// Active registers for SPIs
    pub active: [AtomicU32; 1],
    /// Configuration registers for SPIs
    pub icfgr: [AtomicU32; 2],
    /// Group modifier registers for SPIs
    pub igrpmodr: [AtomicU32; 1],
    /// Priority registers for SPIs (4 per register)
    pub priority: [AtomicU32; 8],
    /// Router registers for SPIs (64-bit each)
    pub router: [AtomicU64; 32],
}

impl DistributorState {
    pub fn new(num_cpus: usize) -> Self {
        Self {
            ctlr: AtomicU32::new(GICD_CTLR_DS), // Disable Security
            num_cpus,
            group: std::array::from_fn(|_| AtomicU32::new(0xFFFFFFFF)),
            enable: std::array::from_fn(|_| AtomicU32::new(0)),
            pending: std::array::from_fn(|_| AtomicU32::new(0)),
            active: std::array::from_fn(|_| AtomicU32::new(0)),
            icfgr: std::array::from_fn(|_| AtomicU32::new(0)),
            igrpmodr: std::array::from_fn(|_| AtomicU32::new(0)),
            priority: std::array::from_fn(|_| AtomicU32::new(0)),
            router: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Handle read from distributor register space
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            GICD_CTLR => self.ctlr.load(Ordering::SeqCst),
            GICD_TYPER => {
                // ITLinesNumber indicates max interrupt lines as ((ITLinesNumber+1)*32)
                // For 32 SPIs (INTIDs 32-63), we need total lines = 64, so ITLinesNumber = 1
                // Formula: ITLinesNumber = (total_interrupts / 32) - 1 = ((32 private + N SPIs) / 32) - 1
                // Simplified: ITLinesNumber = NR_SPIS / 32 (since 32 private IRQs = 1 group)
                // CPUNumber = num_cpus - 1
                // SecurityExtn = 0 (no security)
                // MBIS = 0, LPIS = 0
                let it_lines = GIC_NR_SPIS / 32;
                let cpu_num = (self.num_cpus as u32).saturating_sub(1);
                it_lines | (cpu_num << 5)
            }
            GICD_TYPER2 => 0,
            GICD_IIDR => {
                // Implementer identification (crosvm)
                0x43_00_00_00
            }
            GICD_PIDR2 => GIC_PIDR2_ARCH_GICV3,
            o if o >= GICD_IGROUPR && o < GICD_IGROUPR + 0x80 => {
                let idx = ((o - GICD_IGROUPR) / 4) as usize;
                if idx == 0 {
                    // SGI/PPI group is read from redistributor
                    0
                } else if idx <= self.group.len() {
                    self.group[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ISENABLER && o < GICD_ISENABLER + 0x80 => {
                let idx = ((o - GICD_ISENABLER) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.enable.len() {
                    self.enable[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ICENABLER && o < GICD_ICENABLER + 0x80 => {
                let idx = ((o - GICD_ICENABLER) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.enable.len() {
                    self.enable[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ISPENDR && o < GICD_ISPENDR + 0x80 => {
                let idx = ((o - GICD_ISPENDR) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.pending.len() {
                    self.pending[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ICPENDR && o < GICD_ICPENDR + 0x80 => {
                let idx = ((o - GICD_ICPENDR) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.pending.len() {
                    self.pending[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ISACTIVER && o < GICD_ISACTIVER + 0x80 => {
                let idx = ((o - GICD_ISACTIVER) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.active.len() {
                    self.active[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ICACTIVER && o < GICD_ICACTIVER + 0x80 => {
                let idx = ((o - GICD_ICACTIVER) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.active.len() {
                    self.active[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_IPRIORITYR && o < GICD_IPRIORITYR + 0x400 => {
                let idx = ((o - GICD_IPRIORITYR) / 4) as usize;
                // First 8 registers (32 interrupts) are SGI/PPI, handled by redistributor
                if idx >= 8 && idx < 8 + self.priority.len() {
                    self.priority[idx - 8].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_ITARGETSR && o < GICD_ITARGETSR + 0x400 => {
                // GICv3 doesn't use ITARGETSR (uses IROUTER instead)
                // Return 0 for compatibility
                0
            }
            o if o >= GICD_ICFGR && o < GICD_ICFGR + 0x100 => {
                let idx = ((o - GICD_ICFGR) / 4) as usize;
                // First 2 registers are SGI/PPI
                if idx >= 2 && idx < 2 + self.icfgr.len() {
                    self.icfgr[idx - 2].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_IGRPMODR && o < GICD_IGRPMODR + 0x80 => {
                let idx = ((o - GICD_IGRPMODR) / 4) as usize;
                if idx == 0 {
                    0
                } else if idx <= self.igrpmodr.len() {
                    self.igrpmodr[idx - 1].load(Ordering::SeqCst)
                } else {
                    0
                }
            }
            o if o >= GICD_IROUTER && o < GICD_IROUTER + 0x2000 => {
                let irq = ((o - GICD_IROUTER) / 8) as usize;
                let is_high = ((o - GICD_IROUTER) % 8) >= 4;
                if irq >= GIC_SPI_BASE as usize && irq < GIC_SPI_BASE as usize + self.router.len() {
                    let router = self.router[irq - GIC_SPI_BASE as usize].load(Ordering::SeqCst);
                    if is_high {
                        (router >> 32) as u32
                    } else {
                        router as u32
                    }
                } else {
                    0
                }
            }
            _ => {
                trace!("GICD read from unhandled offset {:#x}", offset);
                0
            }
        };

        write_le_u32(data, value);
    }

    /// Handle write to distributor register space
    pub fn write(&self, offset: u64, data: &[u8]) {
        let value = read_le_u32(data);

        match offset {
            GICD_CTLR => {
                // Allow enabling/disabling groups, but keep DS bit set
                let new_value = (value & (GICD_CTLR_ENABLE_G0 | GICD_CTLR_ENABLE_G1NS |
                    GICD_CTLR_ENABLE_G1S | GICD_CTLR_ARE_S | GICD_CTLR_ARE_NS)) | GICD_CTLR_DS;
                self.ctlr.store(new_value, Ordering::SeqCst);
            }
            o if o >= GICD_IGROUPR && o < GICD_IGROUPR + 0x80 => {
                let idx = ((o - GICD_IGROUPR) / 4) as usize;
                if idx > 0 && idx <= self.group.len() {
                    self.group[idx - 1].store(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ISENABLER && o < GICD_ISENABLER + 0x80 => {
                let idx = ((o - GICD_ISENABLER) / 4) as usize;
                if idx > 0 && idx <= self.enable.len() {
                    self.enable[idx - 1].fetch_or(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ICENABLER && o < GICD_ICENABLER + 0x80 => {
                let idx = ((o - GICD_ICENABLER) / 4) as usize;
                if idx > 0 && idx <= self.enable.len() {
                    self.enable[idx - 1].fetch_and(!value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ISPENDR && o < GICD_ISPENDR + 0x80 => {
                let idx = ((o - GICD_ISPENDR) / 4) as usize;
                if idx > 0 && idx <= self.pending.len() {
                    self.pending[idx - 1].fetch_or(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ICPENDR && o < GICD_ICPENDR + 0x80 => {
                let idx = ((o - GICD_ICPENDR) / 4) as usize;
                if idx > 0 && idx <= self.pending.len() {
                    self.pending[idx - 1].fetch_and(!value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ISACTIVER && o < GICD_ISACTIVER + 0x80 => {
                let idx = ((o - GICD_ISACTIVER) / 4) as usize;
                if idx > 0 && idx <= self.active.len() {
                    self.active[idx - 1].fetch_or(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ICACTIVER && o < GICD_ICACTIVER + 0x80 => {
                let idx = ((o - GICD_ICACTIVER) / 4) as usize;
                if idx > 0 && idx <= self.active.len() {
                    self.active[idx - 1].fetch_and(!value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_IPRIORITYR && o < GICD_IPRIORITYR + 0x400 => {
                let idx = ((o - GICD_IPRIORITYR) / 4) as usize;
                if idx >= 8 && idx < 8 + self.priority.len() {
                    self.priority[idx - 8].store(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_ICFGR && o < GICD_ICFGR + 0x100 => {
                let idx = ((o - GICD_ICFGR) / 4) as usize;
                if idx >= 2 && idx < 2 + self.icfgr.len() {
                    self.icfgr[idx - 2].store(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_IGRPMODR && o < GICD_IGRPMODR + 0x80 => {
                let idx = ((o - GICD_IGRPMODR) / 4) as usize;
                if idx > 0 && idx <= self.igrpmodr.len() {
                    self.igrpmodr[idx - 1].store(value, Ordering::SeqCst);
                }
            }
            o if o >= GICD_IROUTER && o < GICD_IROUTER + 0x2000 => {
                let irq = ((o - GICD_IROUTER) / 8) as usize;
                let is_high = ((o - GICD_IROUTER) % 8) >= 4;
                if irq >= GIC_SPI_BASE as usize && irq < GIC_SPI_BASE as usize + self.router.len() {
                    let idx = irq - GIC_SPI_BASE as usize;
                    // NOTE: This read-modify-write is not atomic. Concurrent writes to the
                    // high and low halves of the same IROUTER could lose one write. This is
                    // acceptable because routing changes only happen during GIC initialization
                    // when a single CPU configures the distributor.
                    if is_high {
                        let mask = 0x0000_0000_FFFF_FFFFu64;
                        let old = self.router[idx].load(Ordering::SeqCst);
                        self.router[idx].store((old & mask) | ((value as u64) << 32), Ordering::SeqCst);
                    } else {
                        let mask = 0xFFFF_FFFF_0000_0000u64;
                        let old = self.router[idx].load(Ordering::SeqCst);
                        self.router[idx].store((old & mask) | (value as u64), Ordering::SeqCst);
                    }
                }
            }
            _ => {
                trace!("GICD write to unhandled offset {:#x} value {:#x}", offset, value);
            }
        }
    }

    /// Set an SPI as pending
    pub fn set_spi_pending(&self, spi: u32) {
        if spi < GIC_NR_SPIS {
            let reg_idx = (spi / 32) as usize;
            let bit = spi % 32;
            if reg_idx < self.pending.len() {
                self.pending[reg_idx].fetch_or(1 << bit, Ordering::SeqCst);
            }
        }
    }

    /// Clear an SPI pending state
    pub fn clear_spi_pending(&self, spi: u32) {
        if spi < GIC_NR_SPIS {
            let reg_idx = (spi / 32) as usize;
            let bit = spi % 32;
            if reg_idx < self.pending.len() {
                self.pending[reg_idx].fetch_and(!(1 << bit), Ordering::SeqCst);
            }
        }
    }

    /// Check if an SPI is pending and enabled.
    pub fn is_spi_deliverable(&self, spi: u32) -> bool {
        if spi >= GIC_NR_SPIS {
            return false;
        }
        let reg_idx = (spi / 32) as usize;
        let bit = spi % 32;
        if reg_idx >= self.enable.len() {
            return false;
        }
        let pending = self.pending[reg_idx].load(Ordering::SeqCst);
        let enabled = self.enable[reg_idx].load(Ordering::SeqCst);
        (pending & enabled & (1 << bit)) != 0
    }

    /// Get the priority of an SPI (0-255, lower is higher priority).
    pub fn get_spi_priority(&self, spi: u32) -> u8 {
        if spi >= GIC_NR_SPIS {
            return 0xff;
        }
        // Each priority register holds 4 priorities (8 bits each)
        let reg_idx = (spi / 4) as usize;
        let byte_idx = (spi % 4) as usize;
        if reg_idx >= self.priority.len() {
            return 0xff;
        }
        let reg = self.priority[reg_idx].load(Ordering::SeqCst);
        ((reg >> (byte_idx * 8)) & 0xff) as u8
    }

    /// Find the highest priority pending and enabled SPI.
    /// Returns (INTID, priority) or None if no SPI is pending.
    pub fn get_highest_priority_pending_spi(&self) -> Option<(u32, u8)> {
        let mut best: Option<(u32, u8)> = None;

        for spi in 0..GIC_NR_SPIS {
            if self.is_spi_deliverable(spi) {
                let priority = self.get_spi_priority(spi);
                match best {
                    None => best = Some((GIC_SPI_BASE + spi, priority)),
                    Some((_, best_prio)) if priority < best_prio => {
                        best = Some((GIC_SPI_BASE + spi, priority));
                    }
                    _ => {}
                }
            }
        }

        best
    }

    /// Check if the distributor has any group 1 SPIs enabled
    pub fn is_group1_enabled(&self) -> bool {
        let ctlr = self.ctlr.load(Ordering::SeqCst);
        (ctlr & GICD_CTLR_ENABLE_G1NS) != 0
    }
}

/// GIC Distributor MMIO device
pub struct GicDistributor {
    state: DistributorState,
}

impl GicDistributor {
    pub fn new(num_cpus: usize) -> Self {
        Self {
            state: DistributorState::new(num_cpus),
        }
    }

    pub fn state(&self) -> &DistributorState {
        &self.state
    }
}

impl BusDevice for GicDistributor {
    fn debug_label(&self) -> String {
        "GICv3 Distributor".to_string()
    }

    fn device_id(&self) -> DeviceId {
        PlatformDeviceId::UserspaceIrqChip.into()
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        self.state.read(info.offset, data);
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        self.state.write(info.offset, data);
    }
}

impl Suspendable for GicDistributor {
    fn snapshot(&mut self) -> anyhow::Result<snapshot::AnySnapshot> {
        Err(anyhow::anyhow!("GicDistributor snapshot not implemented"))
    }

    fn restore(&mut self, _data: snapshot::AnySnapshot) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("GicDistributor restore not implemented"))
    }

    fn sleep(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn wake(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// GIC Redistributor MMIO device (one per CPU)
pub struct GicRedistributor {
    state: RedistributorState,
}

impl GicRedistributor {
    pub fn new(cpu_id: usize, is_last: bool) -> Self {
        Self {
            state: RedistributorState::new(cpu_id, is_last),
        }
    }

    pub fn state(&self) -> &RedistributorState {
        &self.state
    }
}

impl BusDevice for GicRedistributor {
    fn debug_label(&self) -> String {
        format!("GICv3 Redistributor CPU{}", self.state.cpu_id)
    }

    fn device_id(&self) -> DeviceId {
        PlatformDeviceId::UserspaceIrqChip.into()
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        self.state.read(info.offset, data);
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        self.state.write(info.offset, data);
    }
}

impl Suspendable for GicRedistributor {
    fn snapshot(&mut self) -> anyhow::Result<snapshot::AnySnapshot> {
        Err(anyhow::anyhow!("GicRedistributor snapshot not implemented"))
    }

    fn restore(&mut self, _data: snapshot::AnySnapshot) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("GicRedistributor restore not implemented"))
    }

    fn sleep(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn wake(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Helper to read a little-endian u32 from a byte slice
fn read_le_u32(data: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    let len = std::cmp::min(data.len(), 4);
    bytes[..len].copy_from_slice(&data[..len]);
    u32::from_le_bytes(bytes)
}

/// Helper to write a little-endian u32 to a byte slice
fn write_le_u32(data: &mut [u8], value: u32) {
    let bytes = value.to_le_bytes();
    let len = std::cmp::min(data.len(), 4);
    data[..len].copy_from_slice(&bytes[..len]);
}
