# macOS Architecture Support Plan

## Overview

This document outlines the remaining work needed to complete macOS support in the architecture crates (`arch`, `aarch64`, `x86_64`) so that crosvm can boot a Linux guest on macOS.

## Current Blockers

Running `cargo build -p crosvm` fails with errors in:
1. **aarch64 crate** - Missing platform-specific modules and Linux-only features
2. **arch crate** - Missing `sys::linux` equivalents for macOS
3. **Main crosvm binary** - Depends on above crates

---

## Parallelizable Work Streams

The work is organized into independent streams that different agents can work on simultaneously.

---

## Stream A: arch::sys macOS Module

**Owner:** Agent A
**Priority:** HIGH (blocks other work)
**Estimated complexity:** Medium

### Goal
Create `arch/src/sys/macos.rs` module with macOS implementations of platform-specific functions.

### Current State
- `arch/src/sys.rs` only has a Linux branch
- Functions like `generate_platform_bus` and `add_goldfish_battery` are Linux-only

### Tasks

#### A.1: Create arch/src/sys/macos.rs

**File to create:** `arch/src/sys/macos.rs`

**Required exports:**
```rust
// Stub or implement these based on what aarch64/x86_64 crates need:
pub fn generate_platform_bus(...) -> Result<...> { ... }
pub fn add_goldfish_battery(...) -> Result<...> { ... }
// Check Linux implementation for full signature
```

**Approach:**
1. Read `arch/src/sys/linux.rs` to understand the interface
2. Create stubs that return errors or empty results
3. For battery: Return `None` or error (not critical for basic boot)
4. For platform bus: May need real implementation or careful stubbing

#### A.2: Update arch/src/sys.rs

**File to modify:** `arch/src/sys.rs`

**Change:**
```rust
cfg_if::cfg_if! {
    if #[cfg(any(target_os = "android", target_os = "linux"))] {
        pub mod linux;
    } else if #[cfg(target_os = "macos")] {
        pub mod macos;
        pub use macos::*;  // Re-export for compatibility
    }
}
```

### Testing
```bash
cargo build -p arch
```

---

## Stream B: aarch64 Linux-Only Features

**Owner:** Agent B
**Priority:** HIGH
**Estimated complexity:** High

### Goal
Add cfg guards and stubs for Linux-only features in the aarch64 crate.

### Current Errors
```
error[E0433]: failed to resolve: could not find `linux` in `sys`
   --> aarch64/src/lib.rs:775:24 (arch::sys::linux::generate_platform_bus)
   --> aarch64/src/lib.rs:982:60 (arch::sys::linux::add_goldfish_battery)

error[E0433]: failed to resolve: could not find `vmwdt` in `devices`
    --> aarch64/src/lib.rs:1406:31

error[E0425]: cannot find value `AARCH64_GIC_NR_SPIS` in crate `devices`
   --> aarch64/src/lib.rs:755:27

error[E0425]: cannot find value `VMWDT_DEFAULT_*` in this scope
error[E0425]: cannot find value `platform_dev_resources` in this scope
error[E0425]: cannot find value `logical_core_*` in crate `base`
```

### Tasks

#### B.1: Guard arch::sys::linux references

**File:** `aarch64/src/lib.rs`

**Find all occurrences of:**
- `arch::sys::linux::`
- `sys::linux::`

**Wrap with cfg guards or create macOS alternatives:**
```rust
#[cfg(any(target_os = "android", target_os = "linux"))]
let platform_resources = arch::sys::linux::generate_platform_bus(...)?;
#[cfg(target_os = "macos")]
let platform_resources = arch::sys::macos::generate_platform_bus(...)?;
```

#### B.2: Guard vmwdt usage

**File:** `aarch64/src/lib.rs`

**Search for:** `vmwdt`, `VMWDT_DEFAULT_`

**Wrap all usage with:**
```rust
#[cfg(any(target_os = "android", target_os = "linux"))]
{
    // vmwdt code
}
```

#### B.3: Handle GIC constants

**File:** `aarch64/src/lib.rs`

**Error:** `AARCH64_GIC_NR_SPIS` not found

**Investigation needed:**
- Check if this is exported from devices crate on macOS
- May need to add to devices crate exports or define locally

#### B.4: Guard platform_dev_resources

**Files:** `aarch64/src/lib.rs`, `aarch64/src/fdt.rs`

**Wrap platform device resource handling:**
```rust
#[cfg(any(target_os = "android", target_os = "linux"))]
let platform_dev_resources = ...;
#[cfg(target_os = "macos")]
let platform_dev_resources = Vec::new();  // Empty on macOS
```

#### B.5: Guard CPU frequency functions

**File:** `aarch64/src/lib.rs`

**Functions from base crate:**
- `logical_core_max_freq_khz`
- `logical_core_frequencies_khz`
- `logical_core_capacity`
- `logical_core_cluster_id`

**These are Linux-only. Wrap usage:**
```rust
#[cfg(any(target_os = "android", target_os = "linux"))]
{
    // CPU frequency code
}
#[cfg(target_os = "macos")]
{
    // Return default/dummy values or skip
}
```

### Testing
```bash
cargo build -p aarch64
```

---

## Stream C: VmComponents Field Guards

**Owner:** Agent C
**Priority:** HIGH
**Estimated complexity:** Medium

### Goal
Handle Linux-only fields in VmComponents and related structs.

### Current Errors
```
error[E0609]: no field `vfio_platform_pm` on type `VmComponents`
error[E0609]: no field `cpu_frequencies` on type `VmComponents`
error[E0609]: no field `virt_cpufreq_v2` on type `VmComponents`
```

### Tasks

#### C.1: Find VmComponents definition

**File:** Likely `arch/src/lib.rs` or similar

**Search for:** `struct VmComponents`

#### C.2: Add cfg guards to Linux-only fields

**Example:**
```rust
pub struct VmComponents {
    // Common fields
    pub memory: GuestMemory,

    // Linux-only fields
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub vfio_platform_pm: Option<...>,

    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub cpu_frequencies: Option<...>,

    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub virt_cpufreq_v2: bool,
}
```

#### C.3: Guard all usage sites

**Search all files for:**
- `.vfio_platform_pm`
- `.cpu_frequencies`
- `.virt_cpufreq_v2`

**Wrap each with appropriate cfg guards.**

### Testing
```bash
cargo build -p arch
cargo build -p aarch64
```

---

## Stream D: Method Signature Mismatches

**Owner:** Agent D
**Priority:** MEDIUM
**Estimated complexity:** Medium

### Goal
Fix method signature mismatches between trait definitions and implementations.

### Current Error
```
error[E0050]: method `register_pci_device` has 5 parameters but the declaration in trait has 4
```

### Tasks

#### D.1: Find trait definition

**Search for:** `fn register_pci_device`

**Likely locations:**
- `arch/src/lib.rs`
- Architecture-specific trait implementations

#### D.2: Compare Linux vs macOS signatures

The trait may have platform-specific parameters (like swap_controller).

#### D.3: Align signatures

Either:
- Add the missing parameter to the trait
- Remove the extra parameter from the impl
- Use cfg attributes to have different signatures per platform

### Testing
```bash
cargo build -p arch
cargo build -p aarch64
```

---

## Stream E: BusDeviceObj Methods

**Owner:** Agent E
**Priority:** MEDIUM
**Estimated complexity:** Low

### Goal
Handle missing methods on BusDeviceObj trait.

### Current Errors
```
error[E0599]: no method named `as_platform_device` found for reference `&Box<dyn BusDeviceObj>`
error[E0599]: no method named `into_platform_device` found for reference `Box<dyn BusDeviceObj>`
```

### Tasks

#### E.1: Find BusDeviceObj trait definition

**File:** Likely `devices/src/lib.rs` or `devices/src/bus.rs`

#### E.2: Check if methods are cfg-guarded

These methods may be Linux-only. If so, guard their usage in aarch64.

#### E.3: Add stubs or guards

**Option 1:** Add macOS stubs to trait:
```rust
#[cfg(target_os = "macos")]
fn as_platform_device(&self) -> Option<&dyn PlatformDevice> { None }
```

**Option 2:** Guard usage sites in aarch64.

### Testing
```bash
cargo build -p aarch64
```

---

## Stream F: pKVM and IOMMU

**Owner:** Agent F
**Priority:** LOW (can be stubbed)
**Estimated complexity:** Low

### Goal
Handle pKVM-specific code that's Linux-only.

### Current Error
```
error[E0425]: cannot find function `get_pkvm_pviommu_ids` in this scope
```

### Tasks

#### F.1: Find and guard pKVM code

**Search for:** `pkvm`, `pviommu`

**Wrap with:**
```rust
#[cfg(any(target_os = "android", target_os = "linux"))]
```

### Testing
```bash
cargo build -p aarch64
```

---

## Dependency Order

```
Stream A (arch::sys::macos)
    ↓
Stream B (aarch64 Linux-only features) ←── depends on A
    ↓
Stream C (VmComponents fields) ←── can run in parallel with B
    ↓
Stream D (Method signatures) ←── can run in parallel with B, C
    ↓
Stream E (BusDeviceObj methods) ←── can run in parallel with B, C, D
    ↓
Stream F (pKVM/IOMMU) ←── can run in parallel with B, C, D, E
    ↓
Final integration and testing
```

**Parallel execution:**
- Stream A must complete first (others depend on it)
- Streams B, C, D, E, F can run in parallel after A completes

---

## Quick Reference: Files to Modify

### New files to create:
```
arch/src/sys/macos.rs
```

### Files to modify:
```
arch/src/sys.rs                  # Add macos module
arch/src/lib.rs                  # VmComponents field guards
aarch64/src/lib.rs              # Most changes here
aarch64/src/fdt.rs              # platform_dev_resources
devices/src/bus.rs              # BusDeviceObj methods (if needed)
```

---

## Testing Strategy

### Per-stream testing:
```bash
# After Stream A:
cargo build -p arch

# After Streams B-F:
cargo build -p aarch64

# Final test:
cargo build -p crosvm
```

### Integration test:
Once crosvm builds, test with a minimal Linux kernel:
```bash
./target/debug/crosvm run \
    --disable-sandbox \
    --serial type=stdout \
    path/to/bzImage
```

---

## Success Criteria

1. `cargo build -p arch` succeeds on macOS
2. `cargo build -p aarch64` succeeds on macOS
3. `cargo build -p crosvm` succeeds on macOS
4. Basic VM boot works with HVF backend

---

## Notes for Agents

- **Read first:** Always read the Linux implementation before creating macOS stubs
- **Minimize changes:** Prefer cfg guards over duplicating code
- **Test incrementally:** Build after each change to catch issues early
- **Document:** Add comments explaining why macOS differs from Linux
- **Commit often:** Small, focused commits are easier to review

---

## Estimated Effort

| Stream | Effort | Can Parallelize |
|--------|--------|-----------------|
| A | 2-3 hours | No (must be first) |
| B | 3-4 hours | Yes (after A) |
| C | 1-2 hours | Yes (after A) |
| D | 1 hour | Yes (after A) |
| E | 1 hour | Yes (after A) |
| F | 30 min | Yes (after A) |

**Total sequential:** ~9-11 hours
**With parallelization:** ~5-6 hours
