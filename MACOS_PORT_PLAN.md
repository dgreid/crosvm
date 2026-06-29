# macOS ARM64 Port Status

## Current Status: Linux Kernel Boots Successfully ✅

The macOS ARM64 port of crosvm is now functional. The Linux kernel boots
completely through initialization and runs until it looks for /init (rootfs).

### Boot Evidence

```
[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x610f0000]
[    0.000000] Linux version 6.18.0-dirty
[    0.000000] Machine model: crosvm
[    0.000000] psci: PSCIv1.1 detected in firmware.
[    0.000000] GICv3: 988 SPIs implemented
[    0.000000] GICv3: CPU0: found redistributor 0 region 0:0x00000000c0000000
[    0.000000] arch_timer: cp15 timer running at 24.00MHz (virt).
[    0.000436] Console: colour dummy device 80x25
[    0.004330] smp: Brought up 1 node, 1 CPU
[    0.012583] clocksource: jiffies
[    0.015212] NET: Registered PF_NETLINK/PF_ROUTE protocol family
```

---

## Feature Status

| Feature | Status | Notes |
|---------|--------|-------|
| HVF VM/VCPU | ✅ Complete | Full Hypervisor.framework integration |
| Kernel Loading | ✅ Complete | ARM64 Image format |
| FDT Generation | ✅ Complete | Memory, PSCI, GIC, timer, serial |
| Serial Output | ✅ Complete | MMIO at 0x3f8 (earlycon) |
| Serial Input | ✅ Complete | Polling-based (20ms interval) |
| PSCI v1.1 | ✅ Complete | VERSION, FEATURES, CPU_ON, SYSTEM_OFF |
| GICv3 | ✅ Complete | In-kernel (macOS 15.0+) at 0xC0000000 |
| Virtual Timer | ✅ Complete | 24MHz, interrupt injection working |
| Block Devices | ⚠️ Wired | VirtIO-MMIO ready, needs rootfs test |
| Network | ❌ Missing | Requires TapT for macOS |
| Multi-VCPU | ⚠️ Ready | Infrastructure done, needs testing |

---

## Completed Work

### 1. HVF GIC Integration
- Added FFI bindings for macOS 15.0+ GIC APIs
- GIC distributor at 0xC2000000 (64KB)
- GIC redistributor at 0xC0000000 (32MB region)
- Uses `hv_gic_set_spi()` for device interrupt injection

### 2. VCPU Threading Fix
- VCPUs now created on worker threads (HVF requirement)
- Each thread creates and initializes its own VCPU
- HvfVm wrapped in Arc for thread-safe sharing

### 3. PSCI Implementation
- PSCI v1.1 implementation with all required functions
- CPU_ON support for secondary VCPU startup
- VcpuCoordinator for cross-thread VCPU management

### 4. Serial I/O
- Output: MMIO writes to 0x3f8 work correctly
- Input: Polling-based input (kqueue doesn't work with stdin)

### 5. Timer Support
- Virtual timer with vtimer_offset and vtimer_mask
- HV_EXIT_REASON_VTIMER_ACTIVATED handling
- 24MHz timer frequency

---

## Remaining Work

### Priority 1: Test Block Devices with Rootfs

The virtio-blk infrastructure is in place but needs testing:

```bash
# Create a minimal rootfs
# Then run:
./target/release/crosvm run \
    --rwdisk rootfs.img \
    path/to/arm64_Image
```

**Files:** `src/crosvm/sys/macos.rs` (block device setup)

### Priority 2: Network Support

Network requires implementing TapT for macOS. Options:

1. **vmnet.framework** - Apple's virtual network framework
   - Requires additional entitlements
   - Provides NAT and bridged networking

2. **utun** - macOS user-space tunneling
   - Lower-level, more work to implement
   - No special entitlements needed

**Files to create:**
- `net_util/src/sys/macos/` - TapT implementation
- `devices/src/virtio/net/` - May need macOS adaptations

### Priority 3: Multi-VCPU Testing

The infrastructure is ready:
- VcpuCoordinator handles CPU startup requests
- PSCI CPU_ON sets entry point and context ID
- Secondary VCPUs wait for startup signal

Need to test with `--cpus 2` or higher and verify:
- Secondary CPUs start correctly
- IPI (Inter-Processor Interrupt) works
- SMP scheduling works

### Priority 4: Unhandled MMIO Cleanup

Many kernel probes generate "Failed to handle MMIO" errors for devices
we don't emulate. These are harmless but noisy. Options:

1. Add stub devices that return safe defaults
2. Suppress repeated errors for same addresses
3. Add more devices (RTC, etc.)

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        crosvm                                │
├─────────────────────────────────────────────────────────────┤
│  src/crosvm/sys/macos.rs                                    │
│  ├── run_vm() - Main entry point                            │
│  ├── VcpuCoordinator - Multi-VCPU management                │
│  ├── run_vcpu_loop() - Boot CPU execution                   │
│  ├── run_secondary_vcpu() - Secondary CPU execution         │
│  └── handle_hypercall() - PSCI handling                     │
├─────────────────────────────────────────────────────────────┤
│  hypervisor/src/hvf/                                        │
│  ├── mod.rs - HvfVm, HvfVcpu implementations                │
│  ├── hvf_sys.rs - FFI bindings                              │
│  └── GIC creation, timer control                            │
├─────────────────────────────────────────────────────────────┤
│  devices/src/irqchip/hvf/                                   │
│  ├── mod.rs - HvfIrqChip implementing IrqChip trait         │
│  └── gic.rs - GIC distributor/redistributor emulation       │
├─────────────────────────────────────────────────────────────┤
│  devices/src/serial.rs                                      │
│  └── Polling-based input for macOS                          │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│              Apple Hypervisor.framework                      │
│  ├── hv_vm_create() - VM creation                           │
│  ├── hv_vcpu_create() - VCPU creation (thread-bound)        │
│  ├── hv_gic_create() - In-kernel GICv3 (macOS 15.0+)        │
│  ├── hv_gic_set_spi() - Interrupt injection                 │
│  └── hv_vcpu_run() - VCPU execution                         │
└─────────────────────────────────────────────────────────────┘
```

---

## Quick Start

```bash
# Build
cargo build -p crosvm

# Sign with HVF entitlements
codesign --sign - --entitlements crosvm.entitlements --force target/debug/crosvm

# Run
./target/debug/crosvm run -m 512 -c 1 arm64_Image

# With debug logging
RUST_LOG=info ./target/debug/crosvm run -m 512 -c 1 arm64_Image
```

---

## Key Commits

1. `hypervisor: add HVF GICv3 FFI bindings for macOS 15.0+`
2. `hypervisor: add GIC creation and query methods for HVF`
3. `devices: add userspace GIC emulation for macOS HVF`
4. `devices: add macOS polling-based serial input`
5. `macos: integrate in-kernel GIC and fix VCPU threading`

---

## Testing Checklist

- [x] crosvm builds without errors on macOS ARM64
- [x] crosvm can be code-signed with HVF entitlements
- [x] Kernel boots and shows "Booting Linux" message
- [x] PSCI VERSION call succeeds
- [x] GICv3 detected by kernel
- [x] Timer interrupts work (kernel time advances)
- [x] Serial output works throughout boot
- [x] Serial input works (polling-based)
- [ ] Block devices work with rootfs
- [ ] Network devices work
- [ ] Multi-VCPU boot works
- [ ] E2E tests pass on macOS
- [ ] CI runs tests on every PR
