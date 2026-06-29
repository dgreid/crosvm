# macOS ARM64 Port Status

This document tracks the current status of the macOS ARM64 (Apple Silicon) port of crosvm.

## Overview

The macOS port uses Apple's Hypervisor.framework to run ARM64 virtual machines on Apple Silicon Macs. This is an experimental port with basic VM execution working.

## Current Status

### Working Features

| Feature | Status | Notes |
|---------|--------|-------|
| HVF VM Creation | ✅ Working | Creates VM using Hypervisor.framework |
| Memory Mapping | ✅ Working | Maps guest memory with correct permissions |
| Kernel Loading | ✅ Working | Loads ARM64 Linux kernel at 0x80000000 |
| FDT Generation | ✅ Working | Generates device tree for guest |
| VCPU Execution | ✅ Working | Runs guest code in EL1 |
| PSCI v1.1 | ✅ Working | VERSION, FEATURES, MIGRATE_INFO_TYPE |
| Serial Output | ✅ Working | Earlycon via MMIO UART at 0x3f8 |
| HVC Handling | ✅ Working | Hypercall trap and emulation |

### Partially Working

| Feature | Status | Notes |
|---------|--------|-------|
| Virtual Timer | ⚠️ Partial | HVF vtimer APIs implemented, but no GIC to deliver interrupts |
| MMIO Bus | ⚠️ Partial | Serial works, some accesses fail |
| Serial Input | ⚠️ Disabled | kqueue doesn't work with stdin on macOS |

### Not Yet Implemented

| Feature | Status | Notes |
|---------|--------|-------|
| GIC Emulation | ❌ Missing | Required for proper interrupt delivery |
| Block Devices | ❌ Missing | No disk support yet |
| Network | ❌ Missing | No virtio-net yet |
| Multiple VCPUs | ❌ Untested | Code exists but needs testing |

## Boot Progress

The Linux kernel boots through:

1. ✅ Early boot and earlycon initialization
2. ✅ PSCI probe - detects PSCIv1.1
3. ✅ CPU feature detection (BTI, PAC, LSE, etc.)
4. ✅ Memory zone initialization
5. ✅ RCU and scheduler setup
6. ✅ SMP initialization (single CPU)
7. ⚠️ Timer initialization fails ("No interrupt available")
8. ✅ Uses jiffies clocksource as fallback
9. ✅ devtmpfs, pinctrl, netlink initialized
10. ⚠️ Continues with MMIO errors for unhandled devices

## Known Issues

### Timer Interrupts

The kernel reports:
```
arch_timer: No interrupt available, giving up
Failed to initialize '/timer': -22
```

This is because:
- HVF provides vtimer support via `hv_vcpu_set_vtimer_mask()` and exit reason `HV_EXIT_REASON_VTIMER_ACTIVATED`
- We inject IRQs via `hv_vcpu_set_pending_interrupt()`
- But without GIC emulation, the kernel can't identify which interrupt (PPI 11) fired

### Serial Input

Serial input is disabled because macOS kqueue doesn't work properly with stdin when it's a TTY. Serial output (kernel messages) works fine.

### MMIO Errors

Some MMIO accesses fail with "Invalid argument" for addresses not handled by the MMIO bus. These are typically for devices not yet emulated.

## Building and Running

### Prerequisites

- macOS on Apple Silicon (M1/M2/M3)
- Rust toolchain
- ARM64 Linux kernel image

### Build

```bash
cargo build -p crosvm
```

### Sign with HVF Entitlement

```bash
cat > crosvm.entitlements << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
EOF

codesign --sign - --entitlements crosvm.entitlements --force target/debug/crosvm
```

### Run

```bash
./target/debug/crosvm run path/to/arm64_Image
```

With options:
```bash
./target/debug/crosvm run --mem 1024 --cpus 1 path/to/arm64_Image
```

## Architecture Notes

### HVF PSCI Handling

The PSCI implementation includes a workaround for HVF's handling of the ARM SMCCC calling convention:

1. When HVC traps, HVF advances PC past the HVC instruction
2. The next instruction is typically `ldr x4, [sp]` to load the result buffer address
3. HVF doesn't properly handle this load from SP_EL1 (kernel stack)
4. We read SP_EL1 ourselves, extract the result buffer address from guest memory
5. Set X4 to this address and advance PC by 4 to skip the ldr
6. The guest's stp instruction then stores results correctly

### Virtual Timer

HVF provides virtual timer support:
- `hv_vcpu_set_vtimer_offset()` - set timer epoch
- `hv_vcpu_set_vtimer_mask()` - enable/disable timer exits
- `HV_EXIT_REASON_VTIMER_ACTIVATED` - timer fired
- `hv_vcpu_set_pending_interrupt()` - inject IRQ

However, proper timer support requires GIC emulation to route the timer interrupt (PPI 11) to the guest.

## Next Steps

1. **GIC Emulation** - Implement minimal GICv3 distributor/redistributor to deliver timer interrupts
2. **Block Device** - Add virtio-blk support for disk images
3. **Network** - Add virtio-net support
4. **Serial Input** - Implement polling-based input as alternative to kqueue

## References

- [Apple Hypervisor.framework Documentation](https://developer.apple.com/documentation/hypervisor)
- [ARM PSCI Specification](https://developer.arm.com/documentation/den0022)
- [ARM SMCCC Specification](https://developer.arm.com/documentation/den0028)
