#!/bin/bash
# Copyright 2026 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Boot and device test script for macOS ARM64
#
# Runs five test tiers:
#   1. Kernel boot (bare kernel, no devices)
#   2. Block device + root mount (with Debian kernel + initrd + disk)
#   3. Virtiofs shared directory (host-guest filesystem sharing)
#   4. GPU (virtio-gpu DRM device detection)
#   5. SMP (multi-CPU boot)
#
# Usage:
#   ./tools/test_macos_boot.sh [test_tier]
#
#   test_tier: "boot", "block", "virtiofs", "gpu", "smp", or "all" (default: "all")
#
# Environment variables:
#   CROSVM_BIN - Path to crosvm binary (default: target/release/crosvm)
#   CROSVM_KERNEL - Kernel image for bare boot test (default: arm64_Image)
#   DEBIAN_DIR - Directory with Debian test files (default: .macos-debian)

set -e

TEST_TIER="${1:-all}"
CROSVM_BIN="${CROSVM_BIN:-target/release/crosvm}"
CROSVM_KERNEL="${CROSVM_KERNEL:-arm64_Image}"
DEBIAN_DIR="${DEBIAN_DIR:-.macos-debian}"
CROSVM_ENTITLEMENTS="${CROSVM_ENTITLEMENTS:-crosvm.entitlements}"
BOOT_LOG="/tmp/crosvm_boot_test.log"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

PASSED=0
FAILED=0
SKIPPED=0

check_output() {
    local pattern="$1"
    local description="$2"
    if grep -Fq "$pattern" "$BOOT_LOG" 2>/dev/null; then
        echo -e "${GREEN}[PASS]${NC} $description"
        ((++PASSED))
        return 0
    else
        echo -e "${RED}[FAIL]${NC} $description"
        ((++FAILED))
        return 0
    fi
}

skip() {
    echo -e "${YELLOW}[SKIP]${NC} $1"
    ((++SKIPPED))
    return 0
}

ensure_signed() {
    if [ ! -f "$CROSVM_ENTITLEMENTS" ]; then
        cat > "$CROSVM_ENTITLEMENTS" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
PLIST
    fi
    codesign --sign - --entitlements "$CROSVM_ENTITLEMENTS" --force "$CROSVM_BIN" 2>/dev/null
}

# Helper: build a custom initrd from the base mini-initrd with a custom init script.
# Usage: build_custom_initrd <init_script_path> <output_cpio_path>
build_custom_initrd() {
    local init_script="$1"
    local output="$2"
    local abs_initrd
    abs_initrd=$(cd "$(dirname "$DEBIAN_DIR/mini-initrd.cpio")" && echo "$PWD/$(basename "mini-initrd.cpio")")
    local tmp_dir
    tmp_dir=$(mktemp -d)
    local saved_dir="$PWD"
    cd "$tmp_dir"
    cpio -i < "$abs_initrd" 2>/dev/null || true
    cp "$init_script" init
    chmod +x init
    find . | cpio -o -H newc > "$output" 2>/dev/null
    cd "$saved_dir"
    rm -rf "$tmp_dir"
}

echo "=== crosvm macOS Boot & Device Tests ==="
echo ""

# Build if needed
if [ ! -f "$CROSVM_BIN" ]; then
    echo "Building crosvm..."
    cargo build --release -p crosvm
fi
ensure_signed

# ============================================================
# Test 1: Bare kernel boot
# ============================================================
run_boot_test() {
    echo "--- Test: Kernel Boot ---"
    if [ ! -f "$CROSVM_KERNEL" ]; then
        skip "Kernel boot (no kernel image: $CROSVM_KERNEL)"
        return
    fi

    timeout 15 "$CROSVM_BIN" run "$CROSVM_KERNEL" < /dev/null > "$BOOT_LOG" 2>&1 || true

    check_output "Booting Linux" "Kernel starts booting"
    check_output "psci: PSCIv1.1 detected in firmware." "PSCI 1.1 detected"
    check_output "smp: Brought up 1 node, 1 CPU" "Kernel reaches SMP initialization"
    echo ""
}

# ============================================================
# Test 2: Block device + root mount
# ============================================================
run_block_test() {
    echo "--- Test: Block Device + Root Mount ---"
    local kernel="$DEBIAN_DIR/debian-vmlinuz"
    local initrd="$DEBIAN_DIR/mini-initrd.cpio"
    local disk="$DEBIAN_DIR/debian-work.raw"

    if [ ! -f "$kernel" ] || [ ! -f "$initrd" ] || [ ! -f "$disk" ]; then
        skip "Block device test (missing Debian test files in $DEBIAN_DIR)"
        return
    fi

    timeout 30 "$CROSVM_BIN" run -m 4096 \
        --rwdisk "$disk" \
        --initrd "$initrd" \
        -p "root=/dev/vda1 rw console=ttyS0 earlycon panic=5" \
        "$kernel" < /dev/null > "$BOOT_LOG" 2>&1 || true

    check_output "virtio_blk virtio0" "Block device detected by guest"
    check_output "vda:" "Partition table scanned"
    check_output "EXT4-fs" "ext4 filesystem operations"
    echo ""
}

# ============================================================
# Test 3: Virtiofs shared directory
# ============================================================
run_virtiofs_test() {
    echo "--- Test: Virtiofs Shared Directory ---"
    local kernel="$DEBIAN_DIR/debian-vmlinuz"
    local initrd="$DEBIAN_DIR/mini-initrd.cpio"
    local disk="$DEBIAN_DIR/debian-work.raw"

    if [ ! -f "$kernel" ] || [ ! -f "$initrd" ] || [ ! -f "$disk" ]; then
        skip "Virtiofs test (missing Debian test files in $DEBIAN_DIR)"
        return
    fi

    local shared_dir
    shared_dir=$(mktemp -d)
    echo "VIRTIOFS_TEST_MARKER" > "$shared_dir/test_marker.txt"

    cat > /tmp/crosvm_virtiofs_init << 'INITEOF'
#!/bin/sh
set +e
export PATH=/bin:/usr/bin:/sbin:/usr/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
/bin/insmod /lib/modules/virtio_mmio.ko
/bin/insmod /lib/modules/virtio_blk.ko
/bin/insmod /lib/modules/crc32c_generic.ko
/bin/insmod /lib/modules/libcrc32c.ko
/bin/insmod /lib/modules/crc16.ko
/bin/insmod /lib/modules/mbcache.ko
/bin/insmod /lib/modules/jbd2.ko
/bin/insmod /lib/modules/ext4.ko
sleep 1
/bin/mount -t ext4 /dev/vda1 /newroot
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/fs/fuse/fuse.ko 2>/dev/null
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/fs/fuse/virtiofs.ko 2>/dev/null
/bin/mount -t virtiofs testshare /newroot/mnt 2>&1
if [ -f /newroot/mnt/test_marker.txt ]; then
    /bin/cat /newroot/mnt/test_marker.txt
fi
exec /bin/sh
INITEOF

    local test_initrd="/tmp/crosvm_virtiofs_test.cpio"
    build_custom_initrd /tmp/crosvm_virtiofs_init "$test_initrd"

    timeout 35 "$CROSVM_BIN" run -m 4096 \
        --rwdisk "$disk" \
        --initrd "$test_initrd" \
        --shared-dir "$shared_dir:testshare" \
        -p "root=/dev/vda1 rw console=ttyS0 earlycon panic=5" \
        "$kernel" < /dev/null > "$BOOT_LOG" 2>&1 || true

    check_output "VIRTIOFS_TEST_MARKER" "Virtiofs mount and file read"

    rm -rf "$shared_dir" "$test_initrd" /tmp/crosvm_virtiofs_init
    echo ""
}

# ============================================================
# Test 4: GPU (virtio-gpu DRM detection)
# ============================================================
run_gpu_test() {
    echo "--- Test: GPU (virtio-gpu) ---"
    local kernel="$DEBIAN_DIR/debian-vmlinuz"
    local initrd="$DEBIAN_DIR/mini-initrd.cpio"
    local disk="$DEBIAN_DIR/debian-work.raw"

    if [ ! -f "$kernel" ] || [ ! -f "$initrd" ] || [ ! -f "$disk" ]; then
        skip "GPU test (missing Debian test files in $DEBIAN_DIR)"
        return
    fi

    cat > /tmp/crosvm_gpu_init << 'INITEOF'
#!/bin/sh
set +e
export PATH=/bin:/usr/bin:/sbin:/usr/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
/bin/insmod /lib/modules/virtio_mmio.ko
/bin/insmod /lib/modules/virtio_blk.ko
/bin/insmod /lib/modules/crc32c_generic.ko
/bin/insmod /lib/modules/libcrc32c.ko
/bin/insmod /lib/modules/crc16.ko
/bin/insmod /lib/modules/mbcache.ko
/bin/insmod /lib/modules/jbd2.ko
/bin/insmod /lib/modules/ext4.ko
sleep 1
/bin/mount -t ext4 /dev/vda1 /newroot
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/drivers/gpu/drm/drm.ko 2>/dev/null
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/drivers/gpu/drm/drm_kms_helper.ko 2>/dev/null
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/drivers/gpu/drm/drm_shmem_helper.ko 2>/dev/null
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/drivers/virtio/virtio_dma_buf.ko 2>/dev/null
/bin/insmod /newroot/lib/modules/6.1.0-49-arm64/kernel/drivers/gpu/drm/virtio/virtio-gpu.ko 2>/dev/null
exec /bin/sh
INITEOF

    local test_initrd="/tmp/crosvm_gpu_test.cpio"
    build_custom_initrd /tmp/crosvm_gpu_init "$test_initrd"

    timeout 35 "$CROSVM_BIN" run -m 4096 \
        --rwdisk "$disk" \
        --initrd "$test_initrd" \
        -p "root=/dev/vda1 rw console=ttyS0 earlycon panic=5" \
        "$kernel" < /dev/null > "$BOOT_LOG" 2>&1 || true

    check_output "Initialized virtio_gpu" "virtio-gpu driver initialized"
    check_output "virtio_gpudrmfb" "DRM framebuffer device created"

    rm -rf "$test_initrd" /tmp/crosvm_gpu_init
    echo ""
}

# ============================================================
# Test 5: SMP (multi-CPU boot)
# ============================================================
run_smp_test() {
    echo "--- Test: SMP (2 CPUs) ---"
    local kernel="$DEBIAN_DIR/debian-vmlinuz"
    local initrd="$DEBIAN_DIR/mini-initrd.cpio"
    local disk="$DEBIAN_DIR/debian-work.raw"

    if [ ! -f "$kernel" ] || [ ! -f "$initrd" ] || [ ! -f "$disk" ]; then
        skip "SMP test (missing Debian test files in $DEBIAN_DIR)"
        return
    fi

    timeout 35 "$CROSVM_BIN" run -m 4096 --cpus 2 \
        --rwdisk "$disk" \
        --initrd "$initrd" \
        -p "root=/dev/vda1 rw console=ttyS0 earlycon panic=5" \
        "$kernel" < /dev/null > "$BOOT_LOG" 2>&1 || true

    check_output "CPU1: Booted secondary processor" "Secondary CPU booted"
    check_output "Brought up 1 node, 2 CPUs" "Both CPUs online"
    echo ""
}

# ============================================================
# Run selected tests
# ============================================================
case "$TEST_TIER" in
    boot)
        run_boot_test
        ;;
    block)
        run_block_test
        ;;
    virtiofs)
        run_virtiofs_test
        ;;
    gpu)
        run_gpu_test
        ;;
    smp)
        run_smp_test
        ;;
    all)
        run_boot_test
        run_block_test
        run_virtiofs_test
        run_gpu_test
        run_smp_test
        ;;
    *)
        echo "Unknown test tier: $TEST_TIER"
        echo "Usage: $0 [boot|block|virtiofs|gpu|smp|all]"
        exit 1
        ;;
esac

# ============================================================
# Summary
# ============================================================
echo "=== Summary ==="
echo -e "  ${GREEN}Passed:${NC}  $PASSED"
echo -e "  ${RED}Failed:${NC}  $FAILED"
echo -e "  ${YELLOW}Skipped:${NC} $SKIPPED"
echo ""

if [ $FAILED -gt 0 ]; then
    echo -e "${RED}FAILED${NC} — see $BOOT_LOG for details"
    exit 1
else
    echo -e "${GREEN}ALL TESTS PASSED${NC}"
    exit 0
fi
