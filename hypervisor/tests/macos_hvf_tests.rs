// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for the Hypervisor.framework (HVF) backend on macOS.
//!
//! These tests verify that HVF is available and functional on macOS with Apple Silicon.
//! All tests require running on macOS with an aarch64 processor.
//!
//! Note: These tests require proper Hypervisor.framework entitlements to run.
//! If the binary is not signed with the `com.apple.security.hypervisor` entitlement,
//! the tests will be skipped.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::sync::Arc;
use std::sync::Barrier;

use base::pagesize;
use base::MappedRegion;
use base::MemoryMappingBuilder;
use hypervisor::hvf::hv_result;
use hypervisor::hvf::hv_vm_create;
use hypervisor::hvf::hv_vm_destroy;
use hypervisor::hvf::hv_vm_map;
use hypervisor::hvf::Hvf;
use hypervisor::hvf::HvfVm;
use hypervisor::hvf::HV_MEMORY_EXEC;
use hypervisor::hvf::HV_MEMORY_READ;
use hypervisor::hvf::HV_MEMORY_WRITE;
use hypervisor::Hypervisor;
use hypervisor::HypervisorCap;
use hypervisor::MemCacheType::CacheCoherent;
use hypervisor::Vcpu;
use hypervisor::VcpuAArch64;
use hypervisor::VcpuExit;
use hypervisor::VcpuRegAArch64;
use hypervisor::Vm;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

/// Helper function to check if HVF is available.
/// Returns true if HVF can be used, false otherwise.
fn is_hvf_available() -> bool {
    // Try to create and immediately destroy a VM to verify HVF is available
    // SAFETY: We're calling the HVF framework functions with valid parameters
    let create_result = unsafe { hv_vm_create(std::ptr::null_mut()) };

    if hv_result(create_result).is_ok() {
        // SAFETY: We just created the VM successfully
        let _ = unsafe { hv_vm_destroy() };
        true
    } else {
        false
    }
}

/// Macro to skip tests when HVF is not available.
/// This allows tests to pass in environments without HVF access.
macro_rules! skip_if_no_hvf {
    () => {
        if !is_hvf_available() {
            eprintln!(
                "Skipping test: HVF is not available. \
                 This may be due to missing entitlements or running in a VM."
            );
            return;
        }
    };
}

/// Get page-aligned memory size (at least one page).
fn page_aligned_size(min_size: u64) -> u64 {
    let page = pagesize() as u64;
    ((min_size + page - 1) / page) * page
}

fn map_guest_memory(vm: &HvfVm) {
    for region in vm.get_memory().regions() {
        // SAFETY: Each host address and size comes from the GuestMemory owned by `vm`, which
        // remains alive for the duration of the mapping.
        let ret = unsafe {
            hv_vm_map(
                region.host_addr as *mut std::ffi::c_void,
                region.guest_addr.0,
                region.size,
                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
            )
        };
        hv_result(ret).expect("Failed to map guest memory into HVF");
    }
}

/// Test that HVF is available on this macOS system.
///
/// This test verifies that Hypervisor.framework can be accessed and that
/// we can create/destroy a VM at the framework level.
#[test]
fn test_hvf_available() {
    // Try to create and immediately destroy a VM to verify HVF is available
    // SAFETY: We're calling the HVF framework functions with valid parameters
    let create_result = unsafe { hv_vm_create(std::ptr::null_mut()) };

    // If creation succeeded, destroy the VM
    if hv_result(create_result).is_ok() {
        // SAFETY: We just created the VM successfully
        let destroy_result = unsafe { hv_vm_destroy() };
        assert!(
            hv_result(destroy_result).is_ok(),
            "Failed to destroy HVF VM"
        );
        println!("HVF is available on this system.");
    } else {
        // HVF might not be available - this could be due to:
        // 1. Running on Intel Mac (x86_64)
        // 2. Running in a VM without nested virtualization
        // 3. Hypervisor.framework entitlement not available
        // 4. Another VM already exists in this process
        eprintln!(
            "HVF is not available on this system. \
             This test requires macOS on Apple Silicon with Hypervisor.framework entitlements."
        );
        // Don't fail the test - just skip it by returning early
        // This allows CI to pass even without entitlements
    }
}

/// Test creating a basic VM instance via HVF.
///
/// This test verifies that we can create an Hvf hypervisor instance and
/// then create a VM with guest memory.
#[test]
fn test_hvf_create_vm() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    // Verify the hypervisor reports expected capabilities
    assert!(
        hvf.check_capability(HypervisorCap::UserMemory),
        "HVF should support user memory"
    );

    // Create guest memory (page-aligned)
    let mem_size = page_aligned_size(0x10000); // At least 64KB, page-aligned
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");

    // Create the VM
    let vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");

    // Verify we can access the guest memory through the VM
    let memory = vm.get_memory();
    assert!(
        memory.num_regions() > 0,
        "VM should have at least one memory region"
    );

    // Verify guest physical address bits
    let phys_bits = vm.get_guest_phys_addr_bits();
    assert!(
        phys_bits >= 36 && phys_bits <= 52,
        "Guest physical address bits should be reasonable (got {phys_bits})"
    );
}

/// Test creating a VM and adding a vCPU.
///
/// This test verifies that we can create a vCPU within a VM and that
/// the vCPU can be initialized and configured.
#[test]
fn test_hvf_create_vcpu() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    let mem_size = page_aligned_size(0x10000);
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");

    let vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");

    // Create a vCPU
    let vcpu = vm.create_vcpu(0).expect("Failed to create vCPU");

    // Verify the vCPU ID
    assert_eq!(vcpu.id(), 0, "vCPU ID should be 0");

    // Initialize the vCPU (HVF doesn't require explicit features for basic operation)
    vcpu.init(&[]).expect("Failed to initialize vCPU");

    // Test that we can read/write registers
    // Set PC to a known value
    let test_pc = 0x1000u64;
    vcpu.set_one_reg(VcpuRegAArch64::Pc, test_pc)
        .expect("Failed to set PC");

    let read_pc = vcpu
        .get_one_reg(VcpuRegAArch64::Pc)
        .expect("Failed to get PC");
    assert_eq!(read_pc, test_pc, "PC value should match what we set");

    // Test X0 register
    let test_x0 = 0xDEADBEEFu64;
    vcpu.set_one_reg(VcpuRegAArch64::X(0), test_x0)
        .expect("Failed to set X0");

    let read_x0 = vcpu
        .get_one_reg(VcpuRegAArch64::X(0))
        .expect("Failed to get X0");
    assert_eq!(read_x0, test_x0, "X0 value should match what we set");
}

/// Test creating a VM and mapping guest memory regions.
///
/// This test verifies that we can add memory regions to the VM's
/// guest physical address space.
#[test]
fn test_hvf_memory_mapping() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    // Create initial guest memory (page-aligned)
    let initial_mem_size = page_aligned_size(0x10000);
    let guest_mem = GuestMemory::new(&[(GuestAddress(0), initial_mem_size)])
        .expect("Failed to create guest memory");

    let mut vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");

    // Create an additional memory region to map (page-aligned)
    let additional_mem_size = pagesize();
    let mmap = MemoryMappingBuilder::new(additional_mem_size)
        .build()
        .expect("Failed to create memory mapping");

    // Get the pointer before moving to verify later
    let mmap_ptr = mmap.as_ptr();
    let mmap_size = mmap.size();

    // Add the memory region at a guest address that doesn't overlap with initial memory
    // Use a page-aligned address well beyond the initial memory
    let guest_addr = GuestAddress(0x100000); // 1MB offset
    let slot = vm
        .add_memory_region(guest_addr, Box::new(mmap), false, false, CacheCoherent)
        .expect("Failed to add memory region");

    // Verify the slot was assigned
    assert!(slot < u32::MAX, "Memory slot should be valid");

    // Remove the memory region and verify we get it back
    let removed = vm
        .remove_memory_region(slot)
        .expect("Failed to remove memory region");

    assert_eq!(
        removed.as_ptr(),
        mmap_ptr,
        "Should get back the same memory"
    );
    assert_eq!(removed.size(), mmap_size, "Size should match");
}

/// Test running a vCPU with minimal code that halts.
///
/// This test loads a minimal ARM64 instruction sequence that performs a WFI
/// (Wait For Interrupt) which should cause the vCPU to exit with VcpuExit::Hlt.
#[test]
fn test_hvf_vcpu_run() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    // Create guest memory large enough for our code (page-aligned)
    let mem_size = page_aligned_size(0x10000);
    let load_addr = GuestAddress(pagesize() as u64); // Start code at second page
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");

    let vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");
    map_guest_memory(&vm);

    // ARM64 instructions (little-endian):
    // WFI (Wait For Interrupt) - causes HLT exit in HVF
    // Encoding: 0xD503207F
    let wfi_instruction: [u8; 4] = [0x7F, 0x20, 0x03, 0xD5];

    // Write the WFI instruction to guest memory
    vm.get_memory()
        .write_at_addr(&wfi_instruction, load_addr)
        .expect("Failed to write code to guest memory");

    // Create and initialize vCPU
    let mut vcpu = vm.create_vcpu(0).expect("Failed to create vCPU");
    vcpu.init(&[]).expect("Failed to initialize vCPU");

    // Set up the vCPU registers:
    // - PC points to our WFI instruction
    // - PSTATE/CPSR set to EL1h mode with interrupts masked
    vcpu.set_one_reg(VcpuRegAArch64::Pc, load_addr.0)
        .expect("Failed to set PC");

    // Set PSTATE to EL1h (Exception Level 1, using SP_EL1)
    // PSTATE bits: M[4:0] = 0b00101 (EL1h), DAIF = 0b1111 (all interrupts masked)
    // Full value: 0x3C5 = 0b1111000101
    let pstate = 0x3C5u64;
    vcpu.set_one_reg(VcpuRegAArch64::Pstate, pstate)
        .expect("Failed to set PSTATE");

    // Run the vCPU
    let exit = vcpu.run().expect("Failed to run vCPU");

    // We expect a Hlt exit from the WFI instruction
    match exit {
        VcpuExit::Hlt => {
            // Success! WFI caused the expected exit
        }
        VcpuExit::Intr => {
            // Interrupt occurred - this is acceptable, try running again
            let exit2 = vcpu.run().expect("Failed to run vCPU second time");
            match exit2 {
                VcpuExit::Hlt => {
                    // Success on second try
                }
                other => {
                    panic!("Expected Hlt exit on second run, got {other:?}");
                }
            }
        }
        VcpuExit::Mmio => {
            // MMIO exit could happen if memory isn't properly set up
            panic!("Got MMIO exit - memory might not be properly mapped");
        }
        other => {
            panic!("Expected Hlt exit from WFI instruction, got {other:?}");
        }
    }

    // Verify PC is at the WFI instruction (should not have advanced since WFI traps)
    let final_pc = vcpu
        .get_one_reg(VcpuRegAArch64::Pc)
        .expect("Failed to read final PC");
    assert_eq!(
        final_pc, load_addr.0,
        "PC should be at WFI instruction after Hlt exit"
    );
}

/// Test that HVF advances PC past an HVC without changing guest registers.
#[test]
fn test_hvf_hvc_exit_state() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");
    let mem_size = page_aligned_size(0x10000);
    let load_addr = GuestAddress(pagesize() as u64);
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");
    let vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");
    map_guest_memory(&vm);

    // HVC #0 followed by WFI, both encoded little-endian.
    let code = [0x02, 0x00, 0x00, 0xD4, 0x7F, 0x20, 0x03, 0xD5];
    vm.get_memory()
        .write_at_addr(&code, load_addr)
        .expect("Failed to write guest code");

    let mut vcpu = vm.create_vcpu(0).expect("Failed to create vCPU");
    vcpu.init(&[]).expect("Failed to initialize vCPU");
    vcpu.set_one_reg(VcpuRegAArch64::Pc, load_addr.0)
        .expect("Failed to set PC");
    vcpu.set_one_reg(VcpuRegAArch64::Pstate, 0x3C5)
        .expect("Failed to set PSTATE");

    let x4_sentinel = 0x4444_4444_4444_4444;
    let x30_sentinel = 0x3030_3030_3030_3030;
    vcpu.set_one_reg(VcpuRegAArch64::X(4), x4_sentinel)
        .expect("Failed to set X4");
    vcpu.set_one_reg(VcpuRegAArch64::X(30), x30_sentinel)
        .expect("Failed to set X30");

    assert!(matches!(
        vcpu.run().expect("Failed to run HVC instruction"),
        VcpuExit::Hypercall
    ));
    assert_eq!(
        vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap(),
        load_addr.0 + 4,
        "HVF should report PC at the instruction following HVC"
    );
    assert_eq!(vcpu.get_one_reg(VcpuRegAArch64::X(4)).unwrap(), x4_sentinel);
    assert_eq!(
        vcpu.get_one_reg(VcpuRegAArch64::X(30)).unwrap(),
        x30_sentinel
    );

    vcpu.set_one_reg(VcpuRegAArch64::X(0), 0x0001_0001)
        .expect("Failed to set hypercall result");
    assert!(matches!(
        vcpu.run().expect("Failed to resume after HVC"),
        VcpuExit::Hlt
    ));
    assert_eq!(
        vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap(),
        load_addr.0 + 4,
        "WFI should leave PC at the instruction after HVC"
    );
}

/// Test that we can create multiple vCPUs.
#[test]
fn test_hvf_multiple_vcpus() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    let mem_size = page_aligned_size(0x10000);
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");

    let vm = Arc::new(HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM"));
    let barrier = Arc::new(Barrier::new(2));

    let workers: Vec<_> = [(0, 0x1111), (1, 0x2222)]
        .into_iter()
        .map(|(id, value)| {
            let vm = Arc::clone(&vm);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Hypervisor.framework associates each vCPU with the thread that creates it.
                // Keep creation, access, and destruction on this worker thread.
                let vcpu = vm.create_vcpu(id);
                barrier.wait();
                let vcpu = vcpu.unwrap_or_else(|e| panic!("Failed to create vCPU {id}: {e}"));

                assert_eq!(vcpu.id(), id);
                vcpu.init(&[])
                    .unwrap_or_else(|e| panic!("Failed to init vCPU {id}: {e}"));
                vcpu.set_one_reg(VcpuRegAArch64::X(0), value)
                    .unwrap_or_else(|e| panic!("Failed to set X0 on vCPU {id}: {e}"));

                vcpu.get_one_reg(VcpuRegAArch64::X(0))
                    .unwrap_or_else(|e| panic!("Failed to get X0 from vCPU {id}: {e}"))
            })
        })
        .collect();

    let values: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("vCPU worker panicked"))
        .collect();
    assert_eq!(values, [0x1111, 0x2222]);
}

/// Test vector register access (SIMD/FP registers V0-V31).
#[test]
fn test_hvf_vector_registers() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    let mem_size = page_aligned_size(0x10000);
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");

    let vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");
    let vcpu = vm.create_vcpu(0).expect("Failed to create vCPU");
    vcpu.init(&[]).expect("Failed to initialize vCPU");

    // Test setting and getting a 128-bit vector register
    let test_value: u128 = 0xDEADBEEF_CAFEBABE_12345678_9ABCDEF0;
    vcpu.set_vector_reg(0, test_value)
        .expect("Failed to set V0");

    let read_value = vcpu.get_vector_reg(0).expect("Failed to get V0");
    assert_eq!(read_value, test_value, "V0 value should match what we set");

    // Test another register
    let test_value2: u128 = 0xFFFFFFFF_FFFFFFFF_00000000_00000000;
    vcpu.set_vector_reg(31, test_value2)
        .expect("Failed to set V31");

    let read_value2 = vcpu.get_vector_reg(31).expect("Failed to get V31");
    assert_eq!(
        read_value2, test_value2,
        "V31 value should match what we set"
    );
}

/// Test immediate exit functionality.
#[test]
fn test_hvf_immediate_exit() {
    skip_if_no_hvf!();

    let hvf = Hvf::new().expect("Failed to create Hvf instance");

    let mem_size = page_aligned_size(0x10000);
    let guest_mem =
        GuestMemory::new(&[(GuestAddress(0), mem_size)]).expect("Failed to create guest memory");

    let vm = HvfVm::new(&hvf, guest_mem).expect("Failed to create HVF VM");
    let mut vcpu = vm.create_vcpu(0).expect("Failed to create vCPU");
    vcpu.init(&[]).expect("Failed to initialize vCPU");

    // Set up PC to point to valid code (WFI)
    let wfi: [u8; 4] = [0x7F, 0x20, 0x03, 0xD5];
    let load_addr = GuestAddress(pagesize() as u64);
    vm.get_memory()
        .write_at_addr(&wfi, load_addr)
        .expect("Failed to write code");

    vcpu.set_one_reg(VcpuRegAArch64::Pc, load_addr.0)
        .expect("Failed to set PC");
    vcpu.set_one_reg(VcpuRegAArch64::Pstate, 0x3C5)
        .expect("Failed to set PSTATE");

    // Request immediate exit before running
    vcpu.set_immediate_exit(true);

    // Run should return immediately with Intr
    let exit = vcpu.run().expect("Failed to run vCPU");

    match exit {
        VcpuExit::Intr => {
            // Expected - immediate exit was requested
        }
        VcpuExit::Hlt => {
            // Also acceptable - might have executed WFI before exit was processed
        }
        other => {
            panic!("Expected Intr or Hlt exit, got {other:?}");
        }
    }
}
