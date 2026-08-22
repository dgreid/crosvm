# macOS ARM64 Port Status

This document tracks the current status of the macOS ARM64 (Apple Silicon) port of crosvm.

## Overview

The macOS port uses Apple's Hypervisor.framework to run ARM64 virtual machines on Apple Silicon
Macs. This is an experimental port with VM boot, block devices, shared filesystems, GPU, SMP, and
audio working.

## Current Status

### Working Features

| Feature            | Status     | Notes                                                        |
| ------------------ | ---------- | ------------------------------------------------------------ |
| HVF VM Creation    | ✅ Working | Creates VM using Hypervisor.framework                        |
| Memory Mapping     | ✅ Working | Split low/high banks to avoid GIC region                     |
| Kernel Loading     | ✅ Working | Loads ARM64 Linux kernel at 0x80000000                       |
| Initrd Loading     | ✅ Working | Loads initramfs for module-based kernels                     |
| FDT Generation     | ✅ Working | Generates device tree with memory, devices, initrd           |
| VCPU Execution     | ✅ Working | Runs guest code in EL1                                       |
| PSCI v1.1          | ✅ Working | VERSION, FEATURES, MIGRATE_INFO_TYPE, CPU_ON                 |
| Serial Output      | ✅ Working | Earlycon via MMIO UART at 0x3f8                              |
| HVC Handling       | ✅ Working | Returns PSCI results through the standard SMCCC register ABI |
| GIC Emulation      | ✅ Working | Userspace GICv3 with HVF IRQ injection                       |
| Virtual Timer      | ✅ Working | PPI 11 via GIC interrupt delivery                            |
| Block Devices      | ✅ Working | virtio-blk via MMIO transport, async I/O                     |
| Filesystem Sharing | ✅ Working | virtiofs via --shared-dir, host-guest file sharing           |
| MMIO Bus           | ✅ Working | Full virtio MMIO v2 device support                           |
| IRQ Delivery       | ✅ Working | Edge-triggered SPI injection via IRQ handler thread          |
| GPU (virtio-gpu)   | ✅ Working | 2D framebuffer via MMIO, DRM/fb0 device in guest             |
| Audio (virtio-snd) | ✅ Working | CoreAudio backend; guest kernel needs CONFIG_SND_VIRTIO      |
| SMP Boot           | ✅ Working | PSCI CPU_ON, tested with up to 4 CPUs                        |

### Partially Working

| Feature      | Status        | Notes                                               |
| ------------ | ------------- | --------------------------------------------------- |
| Serial Input | ⚠️ Workaround | Polling-based input, kqueue doesn't work with stdin |

### Not Yet Implemented

| Feature | Status     | Notes             |
| ------- | ---------- | ----------------- |
| Network | ❌ Missing | No virtio-net yet |

## Boot Progress

The Linux kernel (tested with Debian 6.1.0-49-arm64) boots fully:

1. ✅ Early boot and earlycon initialization
1. ✅ PSCI probe - detects PSCIv1.1
1. ✅ CPU feature detection
1. ✅ Memory zone initialization (split low/high banks)
1. ✅ GIC initialization (userspace GICv3 emulation)
1. ✅ Timer initialization (vtimer PPI 11)
1. ✅ virtio-mmio device detection
1. ✅ virtio-blk driver loads, partition table scanned
1. ✅ ext4 root filesystem mounted
1. ✅ virtiofs shared directories accessible
1. ✅ Init process runs, shell prompt reached

## Key Bug Fixes

### XZR Register in MMIO Data Abort (Critical)

The `get_reg(31)` and `set_reg(31, value)` methods in HVF VCPU incorrectly mapped register 31 to the
PC instead of the zero register (XZR). In ARM64 data abort syndrome encoding, SRT=31 always means
XZR. This caused every `str wzr, [addr]` instruction (write zero) during MMIO to write the PC value
instead, corrupting virtio device status registers and preventing device activation.

### SMCCC Return Handling

Hypervisor.framework reports the guest PC at the instruction following `hvc`. The PSCI handler only
updates X0 with the return value, leaving the PC, stack, and other registers untouched so each Linux
kernel version can complete its own SMCCC wrapper correctly.

### GICR_TYPER 64-bit Access

The GIC redistributor's GICR_TYPER register is 64-bit but was only handling 32-bit reads. Added
8-byte read handling for kernels that read it as a single 64-bit load.

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

The PSCI implementation handles the HVC trap, writes the result to X0, and resumes at the PC
reported by Hypervisor.framework. Guest SMCCC wrapper instructions are not skipped or emulated,
which keeps the ABI compatible across Linux kernel versions.

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
1. IRQ handler thread polls events with 10ms timeout
1. Marks the SPI pending in the userspace GIC model
1. Kicks boot VCPU via `hv_vcpus_exit()` to process the interrupt
1. Calls `hv_vcpu_set_pending_interrupt()` before the VCPU re-enters the guest

## References

- [Apple Hypervisor.framework Documentation](https://developer.apple.com/documentation/hypervisor)
- [ARM PSCI Specification](https://developer.arm.com/documentation/den0022)
- [ARM SMCCC Specification](https://developer.arm.com/documentation/den0028)
