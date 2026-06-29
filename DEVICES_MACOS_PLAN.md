# MacOS Devices Enablement Plan

## Overview

This document outlines an incremental plan for enabling device support in crosvm on macOS. The devices are organized by complexity and dependencies, allowing gradual enablement without breaking the build.

## Current Status (Updated 2025-01-29)

### ✅ COMPLETED

#### Phase 1: Foundation
- **devices/src/sys/macos.rs** - Core platform module created
- **devices/src/lib.rs** - compile_error removed, macOS imports added
- **Serial device platform support** - `devices/src/sys/macos/serial_device.rs` created

#### Phase 3: Core VirtIO Devices
- **VirtIO Block** - `devices/src/virtio/block/sys/macos.rs` exists
- **VirtIO Console** - `devices/src/virtio/console/sys/macos.rs` exists
- **VirtIO RNG** - Platform-agnostic, uses OsRng
- **VirtIO Balloon** - Platform-agnostic with fallback paths

#### Phase 4: Networking
- **VirtIO Net** - `devices/src/virtio/net/sys/macos.rs` created with stubs
- **net_util macOS** - TapT trait updated with FileReadWriteVolatile, FakeTap implemented
- **vhost_user_backend/net** - macOS stub created (returns unsupported error)

#### Phase 5: Advanced VirtIO Devices
- **VirtIO Sound** - `devices/src/virtio/snd/sys/macos.rs` created (null backend stub)
- **vhost_user_backend/snd** - macOS stub created (returns unsupported error)
- **vhost_user_backend/gpu** - macOS stub created (returns unsupported error)
- **vhost_user_backend/fs** - macOS stub created (returns unsupported error)

#### arch crate Support
- **arch/src/pstore/sys/macos.rs** - Created (trivial no-op)
- **arch/src/serial/sys/macos.rs** - Created (skip jailing)
- **arch/src/lib.rs** - Added macOS cfg guards for device setup
- **arch/src/serial.rs** - Updated Minijail imports for macOS

### 🔄 IN PROGRESS

#### aarch64 crate
- Partial cfg guards added for vmwdt and Minijail imports
- Significant work remains (see ARCH_MACOS_PLAN.md)

### ❌ NOT STARTED / BLOCKED

- **Slirp networking** - slirp.rs is Windows-only; needs macOS port or alternative
- **vmnet.framework** - Native macOS networking (requires entitlements)
- **CoreAudio backend** - Real audio support (currently using null backend)
- **GPU backend** - Requires Metal/OpenGL integration
- **Full crosvm binary** - Blocked on arch crate completion

---

## Build Status

```bash
# ✅ These commands succeed:
cargo build -p devices                           # Basic devices
cargo build -p devices --features net            # With networking stubs
cargo build -p devices --features audio          # With audio stubs
cargo build -p devices --features net,audio      # Combined

# ❌ These commands fail (arch crate issues):
cargo build -p crosvm                            # Main binary - blocked on arch crate
```

---

## Strategy

1. **Create Unix-common abstractions** where Linux and macOS can share code
2. **Stub out Linux-only features** that don't have macOS equivalents
3. **Enable devices incrementally** from simplest (platform-agnostic) to most complex
4. **Test each device** independently before moving to the next

---

## Phase 1: Foundation (Enable Compilation) ✅ COMPLETE

**Goal**: Get devices crate to compile for macOS target without runtime functionality.

### 1.1 Core Platform Module ✅

**Files created:**
- `devices/src/sys/macos.rs` - macOS-specific implementations
- `devices/src/sys/macos/serial_device.rs` - Serial device support

**Files modified:**
- `devices/src/sys.rs` - Added macOS branch to cfg_if
- `devices/src/lib.rs` - Replaced compile_error with macOS module imports

### 1.2 Serial Device Platform Support ✅

**Files created:**
- `devices/src/serial/sys/macos.rs` (if exists)
- `devices/src/sys/macos/serial_device.rs`

---

## Phase 2: Platform-Agnostic Devices ✅ VERIFIED

**Goal**: Enable devices that don't require platform-specific code.

### 2.1 Simple Emulated Devices ✅

These devices are pure emulation with no OS integration:

| Device | File | Status | Notes |
|--------|------|--------|-------|
| CMOS/RTC | `src/cmos.rs` | ✅ | x86_64 only, platform-agnostic |
| i8042 | `src/i8042.rs` | ✅ | PS/2 keyboard, platform-agnostic |
| PL030 | `src/pl030.rs` | ✅ | ARM RTC, platform-agnostic |
| Battery | `src/bat.rs` | ✅ | ACPI battery, platform-agnostic |
| FW_CFG | `src/fw_cfg.rs` | ✅ | Firmware config, platform-agnostic |

### 2.2 Serial Port Core ✅

Serial device support is implemented and compiles on macOS.

---

## Phase 3: Core VirtIO Devices ✅ COMPLETE

**Goal**: Enable essential virtio devices for basic VM functionality.

### 3.1 VirtIO Block (Disk) ✅

**Files:** `devices/src/virtio/block/sys/macos.rs`

**Status:** Compiles on macOS. Uses disk crate which has macOS support.

### 3.2 VirtIO Console ✅

**Files:** `devices/src/virtio/console/sys/macos.rs`

**Status:** Compiles on macOS.

### 3.3 VirtIO RNG ✅

**Status:** Platform-agnostic, uses OsRng which works on macOS.

### 3.4 VirtIO Balloon ✅

**Status:** Platform-agnostic with fallback paths for missing kernel features.

---

## Phase 4: Networking 🔄 PARTIAL

**Goal**: Enable network connectivity for VMs.

### 4.1 VirtIO Net ✅ (Compilation)

**Files created:**
- `devices/src/virtio/net/sys/macos.rs` - Packet processing stubs
- `devices/src/virtio/vhost_user_backend/net/sys/macos.rs` - Returns unsupported error

**Files modified:**
- `devices/src/virtio/net.rs` - Added macOS to error variant cfg guards
- `net_util/src/sys/macos.rs` - Added FileReadWriteVolatile to TapT, implemented for FakeTap

**Status:** Compiles. Runtime networking requires slirp or vmnet.

### 4.2 Slirp Backend ❌ NOT STARTED

**Blocker:** `net_util/src/slirp.rs` is `#![cfg(windows)]` only.

**Required work:**
- Port slirp.rs to support macOS
- Or implement vmnet.framework backend

### 4.3 vmnet.framework Backend ❌ NOT STARTED

**Priority:** LOW (requires app entitlements)

---

## Phase 5: Advanced VirtIO Devices 🔄 PARTIAL

### 5.1 VirtIO Sound ✅ (Compilation with null backend)

**Files created:**
- `devices/src/virtio/snd/sys/macos.rs` - Null audio backend stub
- `devices/src/virtio/vhost_user_backend/snd/sys/macos.rs` - Returns unsupported error

**Files modified:**
- `devices/src/virtio/snd/common_backend/async_funcs.rs` - Added macOS to cfg guard

**Status:** Compiles with null backend. Real audio requires CoreAudio integration.

### 5.2 VirtIO GPU ✅ (Compilation with stub)

**Files created:**
- `devices/src/virtio/vhost_user_backend/gpu/sys/macos.rs` - Returns unsupported error

**Status:** Stub only. Real GPU requires Metal/OpenGL backend.

### 5.3 VirtIO FS ✅ (Compilation with stub)

**Files created:**
- `devices/src/virtio/vhost_user_backend/fs/sys/macos.rs` - Returns unsupported error

**Status:** Stub only. FS backend requires minijail which is not available on macOS.

---

## Phase 6: IPC and Advanced Features

### 6.1 VirtIO Vhost-User Frontend

**Status:** Partially working via connection/sys/macos modules.

### 6.2 VirtIO Vsock ❌ NOT STARTED

**Blocker:** Requires full software emulation (no kernel vsock on macOS).

---

## Phase 7: Linux-Only Features ✅ GUARDED

These features are conditionally compiled out on macOS:

| Feature | Status | Notes |
|---------|--------|-------|
| VFIO | ✅ Guarded | `#[cfg(any(target_os = "android", target_os = "linux"))]` |
| vmwdt | ✅ Guarded | Linux-only watchdog timer |
| VirtCpufreq | ✅ Guarded | Linux-only CPU frequency control |
| ProxyDevice | ✅ Skipped | Uses FakeMinijailStub on macOS |

---

## Phase 8: IRQ Chip Support

**Status:** Depends on hypervisor backend (HVF). Partially implemented.

---

## Dependencies Summary

### ✅ Completed:
- base crate macOS support (Tube, Event, kqueue)
- cros_async kqueue executor
- hypervisor HVF backend
- vm_memory macOS support
- disk crate macOS support
- net_util macOS support (with FakeTap)
- jail crate FakeMinijailStub

### ❌ Needed for full functionality:
- Slirp macOS port or vmnet.framework backend
- CoreAudio backend for real audio
- Metal/OpenGL backend for GPU

---

## Next Steps

**Immediate priority:** Complete arch crate macOS support (see ARCH_MACOS_PLAN.md)

Once arch is complete:
1. Test booting a minimal Linux kernel
2. Add slirp or vmnet networking
3. Add CoreAudio backend for sound
4. Add Metal backend for GPU (optional)

---

## Files Changed Summary

### New files created:
```
devices/src/virtio/net/sys/macos.rs
devices/src/virtio/snd/sys/macos.rs
devices/src/virtio/vhost_user_backend/net/sys/macos.rs
devices/src/virtio/vhost_user_backend/snd/sys/macos.rs
devices/src/virtio/vhost_user_backend/gpu/sys/macos.rs
devices/src/virtio/vhost_user_backend/fs/sys/macos.rs
arch/src/pstore/sys/macos.rs
arch/src/serial/sys/macos.rs
```

### Files modified:
```
devices/src/virtio/net.rs
devices/src/virtio/net/sys.rs
devices/src/virtio/snd/sys/mod.rs
devices/src/virtio/snd/common_backend/async_funcs.rs
devices/src/virtio/vhost_user_backend/net/sys.rs
devices/src/virtio/vhost_user_backend/snd/sys.rs
devices/src/virtio/vhost_user_backend/gpu/sys.rs
devices/src/virtio/vhost_user_backend/fs/sys.rs
net_util/src/sys/macos.rs
arch/src/lib.rs
arch/src/serial.rs
arch/src/pstore/sys.rs
arch/src/serial/sys.rs
aarch64/src/lib.rs
```
