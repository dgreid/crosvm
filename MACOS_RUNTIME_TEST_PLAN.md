# macOS Runtime Test Plan

## Overview

This document outlines runtime tests to verify crosvm functionality on macOS with the HVF (Hypervisor.framework) backend. Tests are organized by component and complexity.

---

## Test Infrastructure

### Test Helper Module

**Location:** `src/sys/macos/test_helpers.rs` (new file)

Provides common utilities:
- VM configuration builders for tests
- Timeout wrappers for async operations
- Output capture and assertion helpers
- Test kernel/initrd paths from environment variables

### Environment Variables

```bash
CROSVM_TEST_KERNEL_IMAGE   # Path to test kernel (bzImage/Image)
CROSVM_TEST_INITRD         # Path to test initrd (optional)
CROSVM_TEST_ROOTFS         # Path to test rootfs image (optional)
```

---

## Test Categories

### Category 1: Hypervisor Backend Tests

**Location:** `hypervisor/tests/macos_hvf_tests.rs`

#### Test 1.1: HVF Availability Check
```rust
#[test]
fn test_hvf_available() {
    // Verify HVF is available on this macOS system
    // Should return Ok or appropriate error on non-supported hardware
}
```

#### Test 1.2: VM Creation
```rust
#[test]
fn test_hvf_create_vm() {
    // Create a basic VM instance via HVF
    // Verify handle is valid
}
```

#### Test 1.3: vCPU Creation
```rust
#[test]
fn test_hvf_create_vcpu() {
    // Create VM, then create vCPU
    // Verify vCPU can be configured
}
```

#### Test 1.4: Memory Region Setup
```rust
#[test]
fn test_hvf_memory_mapping() {
    // Create VM, map guest memory region
    // Verify memory is accessible
}
```

#### Test 1.5: Basic vCPU Run
```rust
#[test]
fn test_hvf_vcpu_run() {
    // Create VM with memory and vCPU
    // Load minimal code (HLT instruction)
    // Run vCPU, verify it exits with HLT
}
```

---

### Category 2: Memory Management Tests

**Location:** `vm_memory/tests/macos_memory_tests.rs`

#### Test 2.1: Guest Memory Allocation
```rust
#[test]
fn test_guest_memory_new() {
    // Allocate GuestMemory with various sizes
    // Verify allocation succeeds
}
```

#### Test 2.2: Memory Read/Write
```rust
#[test]
fn test_guest_memory_read_write() {
    // Write data to guest memory
    // Read it back and verify
}
```

#### Test 2.3: Memory Mapping
```rust
#[test]
fn test_memory_mapping_create() {
    // Create MemoryMapping
    // Verify it can be read/written
}
```

---

### Category 3: Virtio Device Tests

**Location:** `devices/tests/macos_virtio_tests.rs`

#### Test 3.1: Virtio Block Device Creation
```rust
#[test]
fn test_virtio_block_create() {
    // Create a virtio-block device with a temp file
    // Verify device activates correctly
}
```

#### Test 3.2: Virtio Console Creation
```rust
#[test]
fn test_virtio_console_create() {
    // Create a virtio-console device
    // Verify queues are set up correctly
}
```

#### Test 3.3: Virtio RNG Creation
```rust
#[test]
fn test_virtio_rng_create() {
    // Create virtio-rng device
    // Verify it can provide random data
}
```

#### Test 3.4: Virtio Net Creation (vhost-user)
```rust
#[test]
fn test_virtio_net_vhost_user_create() {
    // Create vhost-user net device
    // Verify socket connection setup
}
```

---

### Category 4: Serial Console Tests

**Location:** `devices/tests/macos_serial_tests.rs`

#### Test 4.1: Serial Device Creation
```rust
#[test]
fn test_serial_create() {
    // Create serial device with stdout output
    // Verify IRQ configuration
}
```

#### Test 4.2: Serial Write
```rust
#[test]
fn test_serial_write() {
    // Write bytes to serial device
    // Capture and verify output
}
```

#### Test 4.3: Serial Read
```rust
#[test]
fn test_serial_read() {
    // Provide input to serial device
    // Verify it's readable
}
```

---

### Category 5: Integration Tests

**Location:** `e2e_tests/tests/macos/`

These require a test kernel and rootfs.

#### Test 5.1: Minimal Boot
```rust
#[test]
fn test_minimal_boot() {
    // Boot minimal kernel with serial console
    // Wait for kernel boot messages
    // Verify "Linux version" appears in output
    // Timeout: 30 seconds
}
```

#### Test 5.2: Boot to Userspace
```rust
#[test]
fn test_boot_to_userspace() {
    // Boot with initrd containing /init
    // Verify init process runs
    // Check for expected output
    // Timeout: 60 seconds
}
```

#### Test 5.3: Graceful Shutdown
```rust
#[test]
fn test_graceful_shutdown() {
    // Boot VM
    // Send shutdown command via virtio-console or ACPI
    // Verify clean exit
}
```

#### Test 5.4: Block Device Read
```rust
#[test]
fn test_block_device_read() {
    // Boot with virtio-block device attached
    // Guest reads from block device
    // Verify correct data read
}
```

#### Test 5.5: Block Device Write
```rust
#[test]
fn test_block_device_write() {
    // Boot with writable virtio-block device
    // Guest writes data
    // After shutdown, verify data persisted
}
```

#### Test 5.6: Console I/O
```rust
#[test]
fn test_console_io() {
    // Boot with virtio-console
    // Send input, verify echo
    // Check bidirectional communication
}
```

#### Test 5.7: Multiple vCPUs
```rust
#[test]
fn test_multi_vcpu() {
    // Boot with 2+ vCPUs
    // Verify SMP detection in guest
}
```

#### Test 5.8: Memory Sizes
```rust
#[test]
fn test_various_memory_sizes() {
    // Boot with different memory configurations
    // 256MB, 512MB, 1GB, 2GB
    // Verify guest sees correct memory
}
```

---

## Test Implementation Priority

### Phase 1: Foundation (Must Pass)
1. Test 1.1: HVF Availability
2. Test 1.2: VM Creation
3. Test 2.1: Guest Memory Allocation
4. Test 2.2: Memory Read/Write

### Phase 2: Core Functionality
5. Test 1.3: vCPU Creation
6. Test 1.4: Memory Region Setup
7. Test 1.5: Basic vCPU Run
8. Test 4.1: Serial Device Creation

### Phase 3: Device Layer
9. Test 3.1: Virtio Block
10. Test 3.2: Virtio Console
11. Test 3.3: Virtio RNG
12. Test 4.2-4.3: Serial I/O

### Phase 4: Integration
13. Test 5.1: Minimal Boot
14. Test 5.2: Boot to Userspace
15. Test 5.3: Graceful Shutdown
16. Remaining integration tests

---

## Test Execution

### Unit Tests
```bash
# Run all macOS-specific tests
cargo test --target aarch64-apple-darwin -- macos

# Run specific category
cargo test -p hypervisor -- hvf
cargo test -p vm_memory -- macos
cargo test -p devices -- macos
```

### Integration Tests
```bash
# Set up test environment
export CROSVM_TEST_KERNEL_IMAGE=/path/to/Image
export CROSVM_TEST_INITRD=/path/to/initrd.img

# Run e2e tests
cargo test -p e2e_tests -- macos
```

---

## Success Criteria

1. **Phase 1:** All foundation tests pass - HVF works, memory allocation works
2. **Phase 2:** vCPU can execute code - basic hypervisor functionality verified
3. **Phase 3:** Devices can be created and configured
4. **Phase 4:** Full VM boot works with serial output

---

## Notes

- Tests should be marked `#[ignore]` if they require specific hardware (M1/M2/M3 Mac)
- Integration tests require test assets (kernel, rootfs) not checked into repo
- Some tests may need `#[cfg(target_os = "macos")]` guards
- Consider using `#[serial_test::serial]` for tests that can't run in parallel
