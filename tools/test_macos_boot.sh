#!/bin/bash
# Copyright 2026 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Boot and device test script for macOS ARM64
#
# Runs six test tiers:
#   1. Kernel boot (bare kernel, no devices)
#   2. Block device + root mount (with Debian kernel + initrd + disk)
#   3. Virtiofs shared directory (host-guest filesystem sharing)
#   4. GPU (virtio-gpu DRM device detection)
#   5. SMP (multi-CPU boot)
#   6. Network (virtio-net, socket_vmnet DHCP, host reachability)
#
# Usage:
#   ./tools/test_macos_boot.sh [test_tier]
#
#   test_tier: "boot", "block", "virtiofs", "gpu", "smp", "network", or "all"
#              (default: "all")
#
# Environment variables:
#   CROSVM_BIN - Path to crosvm binary (default: target/release/crosvm)
#   CROSVM_KERNEL - Kernel image for bare boot test (default: arm64_Image)
#   DEBIAN_DIR - Directory with Debian test files (default: .macos-debian)
#   CROSVM_SOCKET_VMNET - socket_vmnet Unix socket path
#   CROSVM_NET_MAC - Stable guest MAC address (default: 02:00:00:00:00:01)

set -e

TEST_TIER="${1:-all}"
CROSVM_BIN="${CROSVM_BIN:-target/release/crosvm}"
CROSVM_KERNEL="${CROSVM_KERNEL:-arm64_Image}"
DEBIAN_DIR="${DEBIAN_DIR:-.macos-debian}"
CROSVM_ENTITLEMENTS="${CROSVM_ENTITLEMENTS:-crosvm.entitlements}"
BOOT_LOG="/tmp/crosvm_boot_test.log"
CROSVM_SOCKET_VMNET="${CROSVM_SOCKET_VMNET:-}"
CROSVM_NET_MAC="${CROSVM_NET_MAC:-02:00:00:00:00:01}"

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

fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    ((++FAILED))
    return 0
}

# Helper: block until $BOOT_LOG contains a pattern, the VM exits, or we time out.
# Usage: wait_for_log <pattern> <timeout_seconds> <vm_pid>
wait_for_log() {
    local pattern="$1"
    local timeout_s="$2"
    local vm_pid="$3"
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if grep -Fq "$pattern" "$BOOT_LOG" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$vm_pid" 2>/dev/null; then
            return 1
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    return 1
}

socket_vmnet_path() {
    if [ -n "$CROSVM_SOCKET_VMNET" ]; then
        printf '%s\n' "$CROSVM_SOCKET_VMNET"
    elif command -v brew >/dev/null 2>&1; then
        printf '%s/var/run/socket_vmnet\n' "$(brew --prefix)"
    else
        printf '%s\n' /var/run/socket_vmnet
    fi
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
# Test 6: Network (socket_vmnet shared mode)
# ============================================================
run_network_test() {
    echo "--- Test: Network (socket_vmnet) ---"
    local kernel="$DEBIAN_DIR/debian-vmlinuz"
    local initrd="$DEBIAN_DIR/debian-initrd"
    local disk="$DEBIAN_DIR/debian-work.raw"
    local socket_path
    socket_path="$(socket_vmnet_path)"

    if [ ! -f "$kernel" ] || [ ! -f "$initrd" ] || [ ! -f "$disk" ]; then
        if [ "$TEST_TIER" = network ]; then
            fail "Network test requires Debian test files in $DEBIAN_DIR"
        else
            skip "Network test (missing Debian test files in $DEBIAN_DIR)"
        fi
        return
    fi

    if [ ! -S "$socket_path" ]; then
        if [ "$TEST_TIER" = network ]; then
            fail "socket_vmnet is not running at $socket_path"
        else
            skip "Network test (socket_vmnet is not running at $socket_path)"
        fi
        return
    fi

    # The Debian arm64 kernel is built without CONFIG_IP_PNP, so `ip=dhcp` is
    # handed to userspace instead of configuring the interface, and virtio_net
    # logs nothing at all when it binds successfully. Neither the kernel's
    # "IP-Config:" summary nor the string "virtio_net" can ever appear in the
    # boot log, so ask the booted guest directly instead of grepping for them.
    local cmd_fifo="/tmp/crosvm_net_test.fifo"
    rm -f "$cmd_fifo"
    mkfifo "$cmd_fifo"

    : > "$BOOT_LOG"
    timeout 240 "$CROSVM_BIN" run -m 4096 \
        --rwdisk "$disk" \
        --initrd "$initrd" \
        --net "socket-vmnet=$socket_path,mac=$CROSVM_NET_MAC" \
        -p "root=/dev/vda1 rw console=ttyS0 earlycon panic=5" \
        "$kernel" < "$cmd_fifo" > "$BOOT_LOG" 2>&1 &
    local vm_pid=$!

    # Hold the write end open so the guest console does not see EOF on stdin.
    exec 9>"$cmd_fifo"

    # The serial line discipline echoes everything typed at the shell, so a
    # marker that appears literally in a command would match its own echo.
    # Pass the marker to the guest once as $M and only ever reference it
    # indirectly; the echoed text then never contains the expanded value.
    local marker="netchk$$"

    # Wait for the shell prompt rather than the login banner: characters sent
    # between autologin and the first prompt are flushed by login(1) and lost.
    if wait_for_log ':~#' 220 "$vm_pid"; then
        sleep 3
        # Report every virtio device the guest enumerated and which driver (if
        # any) claimed it, then the interface name, its bound driver, its parent
        # virtio-mmio device, the DHCP address, and gateway reachability.
        {
            printf 'M=%s\n' "$marker"
            cat << 'GUESTEOF'
for d in /sys/bus/virtio/devices/*; do echo "$M VIRTIO $(basename $d) $(cat $d/modalias) drv=$([ -e $d/driver ] && basename $(readlink -f $d/driver) || echo none)"; done
IFACE=$(ls /sys/class/net | grep -v '^lo$' | head -1)
echo "$M IFACE=$IFACE"
echo "$M DRIVER=$(basename $(readlink -f /sys/class/net/$IFACE/device/driver))"
echo "$M DEVICE=$(basename $(dirname $(readlink -f /sys/class/net/$IFACE/device)))"
echo "$M ADDR=$(ip -4 -o addr show dev $IFACE | awk '{print $4}' | cut -d/ -f1)"
GW=$(ip -4 route show default | awk '{print $3; exit}')
echo "$M GATEWAY=$GW"
ping -c 3 -W 2 $GW > /dev/null 2>&1 && echo "$M PING_OK"
echo "$M TEST_DONE"
GUESTEOF
        } >&9
        wait_for_log "$marker TEST_DONE" 90 "$vm_pid" || true
    fi

    # Echo the guest's own report so a failure shows what it actually saw.
    grep -a "$marker " "$BOOT_LOG" 2>/dev/null | tr -d '\r' | sed 's/^/    /' || true

    check_output "$marker DRIVER=virtio_net" "virtio-net driver bound to the guest interface"

    local net_dev
    net_dev="$(sed -nE "s/.*$marker DEVICE=(.*)/\1/p" "$BOOT_LOG" | tr -d '\r' | head -1)"
    case "$net_dev" in
        *.virtio_mmio)
            echo -e "${GREEN}[PASS]${NC} Interface is backed by a virtio-mmio transport ($net_dev)"
            ((++PASSED))
            ;;
        *)
            fail "Interface is not backed by a virtio-mmio transport${net_dev:+ ($net_dev)}"
            ;;
    esac

    local guest_ip
    guest_ip="$(sed -nE "s/.*$marker ADDR=([0-9.]+).*/\1/p" "$BOOT_LOG" | head -1)"
    if [ -n "$guest_ip" ]; then
        echo -e "${GREEN}[PASS]${NC} DHCP configuration completed ($guest_ip)"
        ((++PASSED))
    else
        fail "DHCP configuration did not complete"
    fi
    check_output "$marker GATEWAY=192." "DHCP supplied a default gateway"
    check_output "$marker PING_OK" "Guest reached the vmnet gateway"

    # The first packet has to wait on ARP, and the guest's interrupts are
    # delivered by a polling thread, so allow several attempts.
    local ping_out=""
    if [ -n "$guest_ip" ]; then
        ping_out="$(ping -c 5 -W 2000 "$guest_ip" 2>&1)" && ping_out="OK$ping_out"
    fi
    case "$ping_out" in
        OK*)
            echo -e "${GREEN}[PASS]${NC} Host reached guest at $guest_ip"
            ((++PASSED))
            ;;
        *"Operation not permitted"*)
            # macOS refuses to originate ICMP on the vmnet bridge under some
            # firewall policies. The guest is still reachable at layer 2, so
            # this says nothing about crosvm.
            skip "Host-to-guest ping (macOS blocked outbound ICMP on the vmnet bridge)"
            ;;
        *)
            fail "Host could not reach DHCP guest address${guest_ip:+ $guest_ip}"
            ;;
    esac

    exec 9>&-
    kill "$vm_pid" 2>/dev/null || true
    wait "$vm_pid" 2>/dev/null || true
    rm -f "$cmd_fifo"
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
    network)
        run_network_test
        ;;
    all)
        run_boot_test
        run_block_test
        run_virtiofs_test
        run_gpu_test
        run_smp_test
        run_network_test
        ;;
    *)
        echo "Unknown test tier: $TEST_TIER"
        echo "Usage: $0 [boot|block|virtiofs|gpu|smp|network|all]"
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
