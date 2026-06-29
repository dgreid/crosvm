# Running crosvm on macOS

This guide explains how to build and run crosvm on macOS using Apple's Hypervisor.framework (HVF).

## Requirements

- **macOS**: 15.0 (Sequoia) or later for in-kernel GICv3 support
- **Hardware**: Apple Silicon (M1/M2/M3/M4) Mac
- **Xcode Command Line Tools**: `xcode-select --install`
- **Rust**: Install via [rustup](https://rustup.rs/)

## Building

```bash
# Clone the repository
git clone https://chromium.googlesource.com/crosvm/crosvm
cd crosvm

# Build in release mode
cargo build --release -p crosvm

# Or debug mode for development
cargo build -p crosvm
```

## Code Signing with Hypervisor Entitlement

macOS requires applications using Hypervisor.framework to be code-signed with the `com.apple.security.hypervisor` entitlement.

### Step 1: Create Entitlements File

Create `crosvm.entitlements` (already included in repo):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
```

### Step 2: Sign the Binary

For local development (ad-hoc signing):

```bash
# Sign release build
codesign --sign - --entitlements crosvm.entitlements --force target/release/crosvm

# Or sign debug build
codesign --sign - --entitlements crosvm.entitlements --force target/debug/crosvm
```

For distribution (requires Apple Developer account):

```bash
codesign --sign "Developer ID Application: Your Name" \
    --entitlements crosvm.entitlements \
    --options runtime \
    target/release/crosvm
```

### Step 3: Verify Signing

```bash
# Check signature
codesign -dv --entitlements - target/release/crosvm

# Should show:
# <key>com.apple.security.hypervisor</key>
# <true/>
```

## Running a VM

### Basic Usage

```bash
# Run with a Linux kernel image
./target/release/crosvm run path/to/arm64_Image
```

### With Options

```bash
# Specify memory and CPUs (when CLI parsing is fixed)
./target/release/crosvm run \
    --mem 1024 \
    --cpus 2 \
    path/to/arm64_Image
```

### Debug Output

```bash
# Enable debug logging
RUST_LOG=debug ./target/release/crosvm run path/to/arm64_Image 2>&1 | tee boot.log
```

## Obtaining a Test Kernel

You need an ARM64 Linux kernel image to run. Options:

### Option 1: Build Your Own

```bash
# Clone Linux kernel
git clone --depth 1 https://github.com/torvalds/linux.git
cd linux

# Configure for ARM64
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- defconfig

# Build
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- -j$(nproc) Image

# The kernel is at arch/arm64/boot/Image
cp arch/arm64/boot/Image ../arm64_Image
```

### Option 2: Use a Pre-built Kernel

Download from a Linux distribution that provides ARM64 kernel images.

## Troubleshooting

### "killed" or Immediate Exit

The binary isn't properly signed. Re-run the codesign command:

```bash
codesign --sign - --entitlements crosvm.entitlements --force target/release/crosvm
```

### "hv_vm_create failed" or Permission Denied

1. Ensure you're on Apple Silicon (not Intel)
2. Ensure macOS is 11.0 or later
3. Check that the entitlements file is correct
4. Try signing again

### No Output from Kernel

1. Check that you're using an ARM64 kernel (not x86_64)
2. Enable debug logging: `RUST_LOG=debug`
3. The kernel should show "Booting Linux" within seconds

### Kernel Panics Looking for Init

This is expected when booting without a rootfs. The kernel successfully boots
but panics when it can't find /init. To boot fully, provide a rootfs:

```bash
./target/release/crosvm run \
    --rwdisk rootfs.img \
    path/to/arm64_Image
```

## Current Limitations

The macOS port is under active development. Current status:

| Feature | Status |
|---------|--------|
| Basic kernel boot | ✅ Full boot to init |
| Serial output | ✅ Works (earlycon) |
| Serial input | ✅ Works (polling) |
| PSCI v1.1 | ✅ Works |
| GICv3 (in-kernel) | ✅ Works |
| Virtual timer | ✅ Works (24MHz) |
| Block devices | ⚠️ Partial (wired, untested with rootfs) |
| Network devices | ❌ Not implemented |
| Multiple VCPUs | ⚠️ Infrastructure ready, untested |

### What Works

The Linux kernel boots successfully to the point of looking for init/rootfs:
- PSCIv1.1 detected and functional
- GICv3 with 988 SPIs detected
- Timer running at 24MHz
- Full kernel subsystem initialization

### Remaining Work

1. **Network devices**: Requires TapT implementation for macOS (vmnet.framework or tun/tap)
2. **Block device testing**: Need to test with a rootfs image
3. **Multi-VCPU testing**: PSCI CPU_ON implemented but needs verification
4. **Unhandled MMIO**: Many kernel probes for missing devices generate errors (harmless)

## Development

### Running Tests

```bash
# Run unit tests
cargo test -p crosvm

# Run E2E boot tests (requires kernel image)
CROSVM_TEST_KERNEL_IMAGE=./arm64_Image cargo test -p e2e_tests --test macos_boot_tests -- --ignored
```

### Formatting and Linting

```bash
./tools/fmt          # Format code
./tools/check        # Run clippy
```

### Quick Development Cycle

```bash
# Build, sign, and run in one command
cargo build -p crosvm && \
codesign --sign - --entitlements crosvm.entitlements --force target/debug/crosvm && \
RUST_LOG=info ./target/debug/crosvm run arm64_Image
```

## Architecture

The macOS port uses:

- **Hypervisor.framework**: Apple's virtualization API for ARM64
- **HVF Backend**: `hypervisor/src/hvf/` - VM and VCPU management
- **macOS Platform**: `src/crosvm/sys/macos/` - Platform-specific code
- **Base Abstractions**: `base/src/sys/macos/` - kqueue, timers, etc.

## Contributing

See `MACOS_PORT_PLAN.md` for the current development plan and open tasks.

Key files:
- `src/crosvm/sys/macos.rs` - Main platform implementation
- `hypervisor/src/hvf/mod.rs` - HVF hypervisor backend
- `hypervisor/src/hvf/hvf_sys.rs` - FFI bindings to Hypervisor.framework
