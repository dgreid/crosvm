// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FFI bindings for Apple's Hypervisor.framework on aarch64.

#![allow(non_camel_case_types)]
// FFI bindings - many functions are defined for completeness but not all are used yet
#![allow(dead_code)]

use std::ffi::c_void;

/// Return type for Hypervisor.framework functions
pub type hv_return_t = i32;

/// VCPU instance identifier
pub type hv_vcpu_t = u64;

/// Intermediate Physical Address (guest physical address)
pub type hv_ipa_t = u64;

/// Memory flags for mapping guest memory
pub type hv_memory_flags_t = u64;

/// Success return value
pub const HV_SUCCESS: hv_return_t = 0;
pub const HV_ERROR: hv_return_t = 0xfae94001_u32 as i32;
pub const HV_BUSY: hv_return_t = 0xfae94002_u32 as i32;
pub const HV_BAD_ARGUMENT: hv_return_t = 0xfae94003_u32 as i32;
pub const HV_NO_RESOURCES: hv_return_t = 0xfae94005_u32 as i32;
pub const HV_NO_DEVICE: hv_return_t = 0xfae94006_u32 as i32;
pub const HV_DENIED: hv_return_t = 0xfae94007_u32 as i32;
pub const HV_UNSUPPORTED: hv_return_t = 0xfae9400f_u32 as i32;

/// Memory permission flags
pub const HV_MEMORY_READ: hv_memory_flags_t = 1 << 0;
pub const HV_MEMORY_WRITE: hv_memory_flags_t = 1 << 1;
pub const HV_MEMORY_EXEC: hv_memory_flags_t = 1 << 2;

/// General purpose registers
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum hv_reg_t {
    HV_REG_X0 = 0,
    HV_REG_X1 = 1,
    HV_REG_X2 = 2,
    HV_REG_X3 = 3,
    HV_REG_X4 = 4,
    HV_REG_X5 = 5,
    HV_REG_X6 = 6,
    HV_REG_X7 = 7,
    HV_REG_X8 = 8,
    HV_REG_X9 = 9,
    HV_REG_X10 = 10,
    HV_REG_X11 = 11,
    HV_REG_X12 = 12,
    HV_REG_X13 = 13,
    HV_REG_X14 = 14,
    HV_REG_X15 = 15,
    HV_REG_X16 = 16,
    HV_REG_X17 = 17,
    HV_REG_X18 = 18,
    HV_REG_X19 = 19,
    HV_REG_X20 = 20,
    HV_REG_X21 = 21,
    HV_REG_X22 = 22,
    HV_REG_X23 = 23,
    HV_REG_X24 = 24,
    HV_REG_X25 = 25,
    HV_REG_X26 = 26,
    HV_REG_X27 = 27,
    HV_REG_X28 = 28,
    HV_REG_X29 = 29,
    HV_REG_X30 = 30,
    HV_REG_PC = 31,
    HV_REG_FPCR = 32,
    HV_REG_FPSR = 33,
    HV_REG_CPSR = 34,
}

/// System registers
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum hv_sys_reg_t {
    HV_SYS_REG_DBGBVR0_EL1 = 0x8004,
    HV_SYS_REG_DBGBCR0_EL1 = 0x8005,
    HV_SYS_REG_DBGWVR0_EL1 = 0x8006,
    HV_SYS_REG_DBGWCR0_EL1 = 0x8007,
    HV_SYS_REG_DBGBVR1_EL1 = 0x800c,
    HV_SYS_REG_DBGBCR1_EL1 = 0x800d,
    HV_SYS_REG_DBGWVR1_EL1 = 0x800e,
    HV_SYS_REG_DBGWCR1_EL1 = 0x800f,
    HV_SYS_REG_MDCCINT_EL1 = 0x8010,
    HV_SYS_REG_MDSCR_EL1 = 0x8012,
    HV_SYS_REG_DBGBVR2_EL1 = 0x8014,
    HV_SYS_REG_DBGBCR2_EL1 = 0x8015,
    HV_SYS_REG_DBGWVR2_EL1 = 0x8016,
    HV_SYS_REG_DBGWCR2_EL1 = 0x8017,
    HV_SYS_REG_DBGBVR3_EL1 = 0x801c,
    HV_SYS_REG_DBGBCR3_EL1 = 0x801d,
    HV_SYS_REG_DBGWVR3_EL1 = 0x801e,
    HV_SYS_REG_DBGWCR3_EL1 = 0x801f,
    HV_SYS_REG_DBGBVR4_EL1 = 0x8024,
    HV_SYS_REG_DBGBCR4_EL1 = 0x8025,
    HV_SYS_REG_DBGWVR4_EL1 = 0x8026,
    HV_SYS_REG_DBGWCR4_EL1 = 0x8027,
    HV_SYS_REG_DBGBVR5_EL1 = 0x802c,
    HV_SYS_REG_DBGBCR5_EL1 = 0x802d,
    HV_SYS_REG_DBGWVR5_EL1 = 0x802e,
    HV_SYS_REG_DBGWCR5_EL1 = 0x802f,
    HV_SYS_REG_DBGBVR6_EL1 = 0x8034,
    HV_SYS_REG_DBGBCR6_EL1 = 0x8035,
    HV_SYS_REG_DBGWVR6_EL1 = 0x8036,
    HV_SYS_REG_DBGWCR6_EL1 = 0x8037,
    HV_SYS_REG_DBGBVR7_EL1 = 0x803c,
    HV_SYS_REG_DBGBCR7_EL1 = 0x803d,
    HV_SYS_REG_DBGWVR7_EL1 = 0x803e,
    HV_SYS_REG_DBGWCR7_EL1 = 0x803f,
    HV_SYS_REG_DBGBVR8_EL1 = 0x8044,
    HV_SYS_REG_DBGBCR8_EL1 = 0x8045,
    HV_SYS_REG_DBGWVR8_EL1 = 0x8046,
    HV_SYS_REG_DBGWCR8_EL1 = 0x8047,
    HV_SYS_REG_DBGBVR9_EL1 = 0x804c,
    HV_SYS_REG_DBGBCR9_EL1 = 0x804d,
    HV_SYS_REG_DBGWVR9_EL1 = 0x804e,
    HV_SYS_REG_DBGWCR9_EL1 = 0x804f,
    HV_SYS_REG_DBGBVR10_EL1 = 0x8054,
    HV_SYS_REG_DBGBCR10_EL1 = 0x8055,
    HV_SYS_REG_DBGWVR10_EL1 = 0x8056,
    HV_SYS_REG_DBGWCR10_EL1 = 0x8057,
    HV_SYS_REG_DBGBVR11_EL1 = 0x805c,
    HV_SYS_REG_DBGBCR11_EL1 = 0x805d,
    HV_SYS_REG_DBGWVR11_EL1 = 0x805e,
    HV_SYS_REG_DBGWCR11_EL1 = 0x805f,
    HV_SYS_REG_DBGBVR12_EL1 = 0x8064,
    HV_SYS_REG_DBGBCR12_EL1 = 0x8065,
    HV_SYS_REG_DBGWVR12_EL1 = 0x8066,
    HV_SYS_REG_DBGWCR12_EL1 = 0x8067,
    HV_SYS_REG_DBGBVR13_EL1 = 0x806c,
    HV_SYS_REG_DBGBCR13_EL1 = 0x806d,
    HV_SYS_REG_DBGWVR13_EL1 = 0x806e,
    HV_SYS_REG_DBGWCR13_EL1 = 0x806f,
    HV_SYS_REG_DBGBVR14_EL1 = 0x8074,
    HV_SYS_REG_DBGBCR14_EL1 = 0x8075,
    HV_SYS_REG_DBGWVR14_EL1 = 0x8076,
    HV_SYS_REG_DBGWCR14_EL1 = 0x8077,
    HV_SYS_REG_DBGBVR15_EL1 = 0x807c,
    HV_SYS_REG_DBGBCR15_EL1 = 0x807d,
    HV_SYS_REG_DBGWVR15_EL1 = 0x807e,
    HV_SYS_REG_DBGWCR15_EL1 = 0x807f,
    HV_SYS_REG_MIDR_EL1 = 0xc000,
    HV_SYS_REG_MPIDR_EL1 = 0xc005,
    HV_SYS_REG_ID_AA64PFR0_EL1 = 0xc020,
    HV_SYS_REG_ID_AA64PFR1_EL1 = 0xc021,
    HV_SYS_REG_ID_AA64DFR0_EL1 = 0xc028,
    HV_SYS_REG_ID_AA64DFR1_EL1 = 0xc029,
    HV_SYS_REG_ID_AA64ISAR0_EL1 = 0xc030,
    HV_SYS_REG_ID_AA64ISAR1_EL1 = 0xc031,
    HV_SYS_REG_ID_AA64MMFR0_EL1 = 0xc038,
    HV_SYS_REG_ID_AA64MMFR1_EL1 = 0xc039,
    HV_SYS_REG_ID_AA64MMFR2_EL1 = 0xc03a,
    HV_SYS_REG_SCTLR_EL1 = 0xc080,
    HV_SYS_REG_CPACR_EL1 = 0xc082,
    HV_SYS_REG_TTBR0_EL1 = 0xc100,
    HV_SYS_REG_TTBR1_EL1 = 0xc101,
    HV_SYS_REG_TCR_EL1 = 0xc102,
    HV_SYS_REG_APIAKEYLO_EL1 = 0xc108,
    HV_SYS_REG_APIAKEYHI_EL1 = 0xc109,
    HV_SYS_REG_APIBKEYLO_EL1 = 0xc10a,
    HV_SYS_REG_APIBKEYHI_EL1 = 0xc10b,
    HV_SYS_REG_APDAKEYLO_EL1 = 0xc110,
    HV_SYS_REG_APDAKEYHI_EL1 = 0xc111,
    HV_SYS_REG_APDBKEYLO_EL1 = 0xc112,
    HV_SYS_REG_APDBKEYHI_EL1 = 0xc113,
    HV_SYS_REG_APGAKEYLO_EL1 = 0xc118,
    HV_SYS_REG_APGAKEYHI_EL1 = 0xc119,
    HV_SYS_REG_SPSR_EL1 = 0xc200,
    HV_SYS_REG_ELR_EL1 = 0xc201,
    HV_SYS_REG_SP_EL0 = 0xc208,
    HV_SYS_REG_AFSR0_EL1 = 0xc288,
    HV_SYS_REG_AFSR1_EL1 = 0xc289,
    HV_SYS_REG_ESR_EL1 = 0xc290,
    HV_SYS_REG_FAR_EL1 = 0xc300,
    HV_SYS_REG_PAR_EL1 = 0xc3a0,
    HV_SYS_REG_MAIR_EL1 = 0xc510,
    HV_SYS_REG_AMAIR_EL1 = 0xc518,
    HV_SYS_REG_VBAR_EL1 = 0xc600,
    HV_SYS_REG_CONTEXTIDR_EL1 = 0xc681,
    HV_SYS_REG_TPIDR_EL1 = 0xc684,
    HV_SYS_REG_CNTKCTL_EL1 = 0xc708,
    HV_SYS_REG_CSSELR_EL1 = 0xd000,
    HV_SYS_REG_TPIDR_EL0 = 0xde82,
    HV_SYS_REG_TPIDRRO_EL0 = 0xde83,
    HV_SYS_REG_CNTV_CTL_EL0 = 0xdf19,
    HV_SYS_REG_CNTV_CVAL_EL0 = 0xdf1a,
    HV_SYS_REG_SP_EL1 = 0xe208,
}

/// SIMD/FP registers
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum hv_simd_fp_reg_t {
    HV_SIMD_FP_REG_Q0 = 0,
    HV_SIMD_FP_REG_Q1 = 1,
    HV_SIMD_FP_REG_Q2 = 2,
    HV_SIMD_FP_REG_Q3 = 3,
    HV_SIMD_FP_REG_Q4 = 4,
    HV_SIMD_FP_REG_Q5 = 5,
    HV_SIMD_FP_REG_Q6 = 6,
    HV_SIMD_FP_REG_Q7 = 7,
    HV_SIMD_FP_REG_Q8 = 8,
    HV_SIMD_FP_REG_Q9 = 9,
    HV_SIMD_FP_REG_Q10 = 10,
    HV_SIMD_FP_REG_Q11 = 11,
    HV_SIMD_FP_REG_Q12 = 12,
    HV_SIMD_FP_REG_Q13 = 13,
    HV_SIMD_FP_REG_Q14 = 14,
    HV_SIMD_FP_REG_Q15 = 15,
    HV_SIMD_FP_REG_Q16 = 16,
    HV_SIMD_FP_REG_Q17 = 17,
    HV_SIMD_FP_REG_Q18 = 18,
    HV_SIMD_FP_REG_Q19 = 19,
    HV_SIMD_FP_REG_Q20 = 20,
    HV_SIMD_FP_REG_Q21 = 21,
    HV_SIMD_FP_REG_Q22 = 22,
    HV_SIMD_FP_REG_Q23 = 23,
    HV_SIMD_FP_REG_Q24 = 24,
    HV_SIMD_FP_REG_Q25 = 25,
    HV_SIMD_FP_REG_Q26 = 26,
    HV_SIMD_FP_REG_Q27 = 27,
    HV_SIMD_FP_REG_Q28 = 28,
    HV_SIMD_FP_REG_Q29 = 29,
    HV_SIMD_FP_REG_Q30 = 30,
    HV_SIMD_FP_REG_Q31 = 31,
}

/// 128-bit SIMD register value type
pub type hv_simd_fp_uchar16_t = [u8; 16];

/// VCPU exit reason
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum hv_exit_reason_t {
    HV_EXIT_REASON_CANCELED = 0,
    HV_EXIT_REASON_EXCEPTION = 1,
    HV_EXIT_REASON_VTIMER_ACTIVATED = 2,
    HV_EXIT_REASON_UNKNOWN = 3,
}

/// VCPU exit information structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct hv_vcpu_exit_t {
    pub reason: hv_exit_reason_t,
    pub exception: hv_vcpu_exit_exception_t,
}

/// Exception exit information
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

/// Opaque VM configuration type
pub type hv_vm_config_t = *mut c_void;

/// Opaque VCPU configuration type
pub type hv_vcpu_config_t = *mut c_void;

/// Interrupt type
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum hv_interrupt_type_t {
    HV_INTERRUPT_TYPE_IRQ = 0,
    HV_INTERRUPT_TYPE_FIQ = 1,
}

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    /// Create a VM instance for the current process
    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;

    /// Destroy the VM instance of the current process
    pub fn hv_vm_destroy() -> hv_return_t;

    /// Map a region of memory in the guest physical address space
    pub fn hv_vm_map(
        addr: *mut c_void,
        ipa: hv_ipa_t,
        size: usize,
        flags: hv_memory_flags_t,
    ) -> hv_return_t;

    /// Unmap a region of memory in the guest physical address space
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;

    /// Protect a region of memory in the guest physical address space
    pub fn hv_vm_protect(ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;

    /// Create a VCPU instance
    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *const hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;

    /// Destroy a VCPU instance
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;

    /// Run a VCPU
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;

    /// Force an immediate exit of a VCPU
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, vcpu_count: u32) -> hv_return_t;

    /// Get the value of a VCPU register
    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t;

    /// Set the value of a VCPU register
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t;

    /// Get the value of a VCPU system register
    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: *mut u64) -> hv_return_t;

    /// Set the value of a VCPU system register
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: u64) -> hv_return_t;

    /// Get the value of a SIMD/FP register
    pub fn hv_vcpu_get_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: hv_simd_fp_reg_t,
        value: *mut hv_simd_fp_uchar16_t,
    ) -> hv_return_t;

    /// Set the value of a SIMD/FP register
    pub fn hv_vcpu_set_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: hv_simd_fp_reg_t,
        value: hv_simd_fp_uchar16_t,
    ) -> hv_return_t;

    /// Set pending interrupt for a VCPU
    pub fn hv_vcpu_set_pending_interrupt(
        vcpu: hv_vcpu_t,
        int_type: hv_interrupt_type_t,
        pending: bool,
    ) -> hv_return_t;

    /// Get pending interrupt state for a VCPU
    pub fn hv_vcpu_get_pending_interrupt(
        vcpu: hv_vcpu_t,
        int_type: hv_interrupt_type_t,
        pending: *mut bool,
    ) -> hv_return_t;

    /// Set the virtual timer mask for a VCPU
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, vtimer_is_masked: bool) -> hv_return_t;

    /// Get the virtual timer offset
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, vtimer_offset: *mut u64) -> hv_return_t;

    /// Set the virtual timer offset
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, vtimer_offset: u64) -> hv_return_t;
}

/// Convert an hv_return_t to a Result
pub fn hv_result(ret: hv_return_t) -> std::result::Result<(), base::Error> {
    if ret == HV_SUCCESS {
        Ok(())
    } else {
        // Map HV errors to appropriate errno values
        let errno = match ret {
            HV_ERROR => libc::EIO,
            HV_BUSY => libc::EBUSY,
            HV_BAD_ARGUMENT => libc::EINVAL,
            HV_NO_RESOURCES => libc::ENOMEM,
            HV_NO_DEVICE => libc::ENODEV,
            HV_DENIED => libc::EPERM,
            HV_UNSUPPORTED => libc::ENOTSUP,
            _ => libc::EIO,
        };
        Err(base::Error::new(errno))
    }
}

// ============================================================================
// GICv3 support (macOS 15.0+)
// ============================================================================

/// Opaque GIC configuration type
pub type hv_gic_config_t = *mut c_void;

/// GIC interrupt IDs for reserved interrupts
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum hv_gic_intid_t {
    HV_GIC_INT_PERFORMANCE_MONITOR = 23,
    HV_GIC_INT_MAINTENANCE = 25,
    HV_GIC_INT_EL2_PHYSICAL_TIMER = 26,
    HV_GIC_INT_EL1_VIRTUAL_TIMER = 27,
    HV_GIC_INT_EL1_PHYSICAL_TIMER = 30,
}

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    // GIC Configuration APIs (macOS 15.0+)

    /// Create a GIC configuration object
    pub fn hv_gic_config_create() -> hv_gic_config_t;

    /// Set the GIC distributor region base address
    pub fn hv_gic_config_set_distributor_base(
        config: hv_gic_config_t,
        distributor_base_address: hv_ipa_t,
    ) -> hv_return_t;

    /// Set the GIC redistributor region base address
    pub fn hv_gic_config_set_redistributor_base(
        config: hv_gic_config_t,
        redistributor_base_address: hv_ipa_t,
    ) -> hv_return_t;

    /// Set the GIC MSI region base address
    pub fn hv_gic_config_set_msi_region_base(
        config: hv_gic_config_t,
        msi_region_base_address: hv_ipa_t,
    ) -> hv_return_t;

    /// Set the range of MSIs supported
    pub fn hv_gic_config_set_msi_interrupt_range(
        config: hv_gic_config_t,
        msi_intid_base: u32,
        msi_intid_count: u32,
    ) -> hv_return_t;

    // GIC Device APIs (macOS 15.0+)

    /// Create a GIC v3 device for a VM
    pub fn hv_gic_create(gic_config: hv_gic_config_t) -> hv_return_t;

    /// Trigger a Shared Peripheral Interrupt (SPI)
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;

    /// Send a Message Signaled Interrupt (MSI)
    pub fn hv_gic_send_msi(address: hv_ipa_t, intid: u32) -> hv_return_t;

    /// Reset the GIC device
    pub fn hv_gic_reset() -> hv_return_t;

    // GIC Parameter APIs (macOS 15.0+)

    /// Get the size of the GIC distributor region
    pub fn hv_gic_get_distributor_size(distributor_size: *mut usize) -> hv_return_t;

    /// Get the alignment for the distributor base address
    pub fn hv_gic_get_distributor_base_alignment(
        distributor_base_alignment: *mut usize,
    ) -> hv_return_t;

    /// Get the total size of the GIC redistributor region
    pub fn hv_gic_get_redistributor_region_size(
        redistributor_region_size: *mut usize,
    ) -> hv_return_t;

    /// Get the size of a single GIC redistributor
    pub fn hv_gic_get_redistributor_size(redistributor_size: *mut usize) -> hv_return_t;

    /// Get the alignment for the redistributor base address
    pub fn hv_gic_get_redistributor_base_alignment(
        redistributor_base_alignment: *mut usize,
    ) -> hv_return_t;

    /// Get the range of SPIs supported
    pub fn hv_gic_get_spi_interrupt_range(
        spi_intid_base: *mut u32,
        spi_intid_count: *mut u32,
    ) -> hv_return_t;

    /// Get the interrupt ID for reserved interrupts
    pub fn hv_gic_get_intid(interrupt: hv_gic_intid_t, intid: *mut u32) -> hv_return_t;

    /// Get the redistributor base guest physical address for a vcpu
    pub fn hv_gic_get_redistributor_base(
        vcpu: hv_vcpu_t,
        redistributor_base_address: *mut hv_ipa_t,
    ) -> hv_return_t;
}
