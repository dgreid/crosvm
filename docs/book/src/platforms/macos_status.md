# macOS ARM64 Port Status

This document tracks the current status of the macOS ARM64 (Apple Silicon) port of crosvm.

## Overview

The macOS port uses Apple's Hypervisor.framework to run ARM64 virtual machines on Apple Silicon Macs. This is an experimental port with VM boot, block devices, shared filesystems, GPU, SMP, and audio working.

## Current Status

### Working Features

| Feature | Status | Notes |
|---------|--------|-------|
| HVF VM Creation | ✅ Working | Creates VM using Hypervisor.framework |
| Memory Mapping | ✅ Working | Split low/high banks to avoid GIC region |
| Kernel Loading | ✅ Working | Loads ARM64 Linux kernel at 0x80000000 |
| Initrd Loading | ✅ Working | Loads initramfs for module-based kernels |
| FDT Generation | ✅ Working | Generates device tree with memory, devices, initrd |
| VCPU Execution | ✅ Working | Runs guest code in EL1 |
| PSCI v1.1 | ✅ Working | VERSION, FEATURES, MIGRATE_INFO_TYPE, CPU_ON |
| Serial Output | ✅ Working | Earlycon via MMIO UART at 0x3f8 |
| HVC Handling | ✅ Working | Hypercall trap with page-table-based VA translation |
| GIC Emulation | ✅ Working | HVF in-kernel GICv3 with userspace fallback |
| Virtual Timer | ✅ Working | PPI 11 via GIC interrupt delivery |
| Block Devices | ✅ Working | virtio-blk via MMIO transport, async I/O |
| Filesystem Sharing | ✅ Working | virtiofs via --shared-dir, host-guest file sharing |
| MMIO Bus | ✅ Working | Full virtio MMIO v2 device support |
| IRQ Delivery | ✅ Working | Edge-triggered SPI injection via IRQ handler thread |
| GPU (virtio-gpu) | ✅ Working | 2D framebuffer via MMIO, DRM/fb0 device in guest |
| SMP Boot | ✅ Working | PSCI CPU_ON, tested with up to 4 CPUs |

### Partially Working

| Feature | Status | Notes |
|---------|--------|-------|
| Serial Input | ⚠️ Workaround | Polling-based input, kqueue doesn't work with stdin |
| Audio (virtio-snd) | ⚠️ No driver | CoreAudio backend wired up, needs kernel with CONFIG_SND_VIRTIO |

### Not Yet Implemented

| Feature | Status | Notes |
|---------|--------|-------|
| Network | ❌ Missing | No virtio-net yet |

## Boot Progress

The Linux kernel (tested with Debian 6.1.0-49-arm64) boots fully:

1. ✅ Early boot and earlycon initialization
2. ✅ PSCI probe - detects PSCIv1.1
3. ✅ CPU feature detection
4. ✅ Memory zone initialization (split low/high banks)
5. ✅ GIC initialization (in-kernel HVF GIC)
6. ✅ Timer initialization (vtimer PPI 11)
7. ✅ virtio-mmio device detection
8. ✅ virtio-blk driver loads, partition table scanned
9. ✅ ext4 root filesystem mounted
10. ✅ virtiofs shared directories accessible
11. ✅ Init process runs, shell prompt reached

## Key Bug Fixes

### XZR Register in MMIO Data Abort (Critical)

The `get_reg(31)` and `set_reg(31, value)` methods in HVF VCPU incorrectly mapped register 31 to the PC instead of the zero register (XZR). In ARM64 data abort syndrome encoding, SRT=31 always means XZR. This caused every `str wzr, [addr]` instruction (write zero) during MMIO to write the PC value instead, corrupting virtio device status registers and preventing device activation.

### Page Table Walking for KASLR

The HVC/PSCI workaround for `__arm_smccc_hvc` result buffer access now uses proper ARM64 4-level page table walking (TTBR1_EL1 + TCR_EL1) instead of hardcoded VA ranges, supporting any kernel with KASLR.

### GICR_TYPER 64-bit Access

The GIC redistributor's GICR_TYPER register is 64-bit but was only handling 32-bit reads. Added 8-byte read handling for kernels that read it as a single 64-bit load.

## Building and Running

### Prerequisites

- macOS on Apple Silicon (M1/M2/M3)
- Rust toolchain
- ARM64 Linux kernel image (uncompressed Image format)

### Build

```bash
cargo build --release
codesign --sign - --entitlements crosvm.entitlements --force target/release/crosvm
```

### Run

```bash
# Basic run with kernel
./target/release/crosvm run -m 512 arm64_Image

# With disk and initrd (e.g., Debian)
./target/release/crosvm run -m 4096 \
    --rwdisk disk.raw \
    --initrd initrd.cpio \
    -p "root=/dev/vda1 rw console=ttyS0 earlycon" \
    vmlinuz

# With shared directory
./target/release/crosvm run -m 4096 \
    --rwdisk disk.raw \
    --initrd initrd.cpio \
    --shared-dir "/path/on/host:tagname" \
    -p "root=/dev/vda1 rw console=ttyS0 earlycon" \
    vmlinuz
```

Guest can mount the shared directory:
```bash
mount -t virtiofs tagname /mnt
```

## Architecture Notes

### HVF PSCI Handling

The PSCI implementation includes a workaround for HVF's handling of the ARM SMCCC calling convention. When HVC traps, the next instruction (`ldr x4, [sp]`) loads the result buffer address from the kernel stack, but HVF doesn't execute it. We walk the kernel's page tables to translate SP_EL1 to a physical address, read the result buffer pointer, set X4, and skip the ldr.

### Memory Layout

```
0x00000000 - 0x7FFFFFFF  Device/unmapped
0x80000000 - 0xBFFFFFFF  Low RAM bank (up to 1 GiB)
0xC0000000 - 0xC00FFFFF  GIC Distributor + Redistributors
0xC0100000 - 0xFFFFFFFF  Unmapped
0x100000000+             High RAM bank (remainder if >1 GiB)
```

### IRQ Delivery

Device interrupts flow through a dedicated IRQ handler thread:
1. Device worker completes I/O → signals IrqEdgeEvent (pipe-based)
2. IRQ handler thread polls events with 10ms timeout
3. Calls `hv_gic_set_spi()` to inject SPI into HVF's in-kernel GIC
4. Kicks boot VCPU via `hv_vcpus_exit()` to process the interrupt

## References

- [Apple Hypervisor.framework Documentation](https://developer.apple.com/documentation/hypervisor)
- [ARM PSCI Specification](https://developer.arm.com/documentation/den0022)
- [ARM SMCCC Specification](https://developer.arm.com/documentation/den0028)
