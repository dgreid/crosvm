#!/bin/sh
# Copyright 2026 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run inside a disposable guest, against an explicitly selected scratch disk.
set -eu

fail() {
    echo "avail_idx_flood: $*" >&2
    exit 1
}

if [ "$#" -ne 2 ] || [ "$2" != "--disposable" ]; then
    fail "usage: $0 PCI_BDF --disposable"
fi
case "$1" in
    [0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]:[0-9a-fA-F][0-9a-fA-F]:[0-9a-fA-F][0-9a-fA-F].[0-7]) ;;
    *) fail "invalid PCI BDF: $1" ;;
esac
[ "$(id -u)" -eq 0 ] || fail "must run as root in the disposable guest"
if [ ! -r /proc/self/mountinfo ] || [ ! -r /proc/swaps ]; then
    fail "mount /proc first"
fi

pci=/sys/bus/pci/devices/$1
[ -d "$pci" ] || fail "PCI device not found: $1"
[ "$(cat "$pci/vendor")" = "0x1af4" ] || fail "not a virtio PCI device"
case "$(cat "$pci/device")" in
    0x1001|0x1042) ;;
    *) fail "PCI device is not virtio-blk" ;;
esac

virtio=
for candidate in "$pci"/virtio[0-9]*; do
    [ -d "$candidate" ] || continue
    [ -z "$virtio" ] || fail "more than one virtio child at $1"
    virtio=$candidate
done
[ -n "$virtio" ] || fail "no virtio device at $1"
[ "$(cat "$virtio/device")" = "0x0002" ] || fail "not a virtio-blk device"
[ -L "$virtio/driver" ] || fail "virtio-blk driver must be bound"
[ "$(basename "$(readlink "$virtio/driver")")" = "virtio_blk" ] ||
    fail "target is not bound to virtio_blk"

disks=0
for disk in "$virtio"/block/*; do
    [ -d "$disk" ] || continue
    disks=$((disks + 1))
    for partition in "$disk"/*/partition; do
        [ ! -e "$partition" ] || fail "$disk has partitions"
    done
    for holder in "$disk"/holders/*; do
        [ ! -e "$holder" ] || fail "$disk has an active holder"
    done
    disk_id=$(cat "$disk/dev")
    while read -r _mount_id _parent_id mounted_id _rest; do
        [ "$mounted_id" != "$disk_id" ] || fail "$disk is mounted"
    done </proc/self/mountinfo
done
[ "$disks" -eq 1 ] || fail "expected one unpartitioned scratch disk at $1"
swap_lines=$(wc -l </proc/swaps)
[ "$swap_lines" -le 1 ] || fail "disable swap before running the repro"

here=$(unset CDPATH; cd "$(dirname "$0")" && pwd)
[ -f "$here/avail_idx_flood.ko" ] || fail "build and copy avail_idx_flood.ko beside run.sh"
[ ! -d /sys/module/avail_idx_flood ] || fail "module already loaded"

target=$(basename "$virtio")
echo "avail_idx_flood: selecting $target at PCI $1 (disposable, unpartitioned disk)"
insmod "$here/avail_idx_flood.ko" target="$target"
printf '%s\n' "$target" >"$virtio/driver/unbind"
if ! printf '%s\n' "$target" >/sys/bus/virtio/drivers/avail_idx_flood/bind; then
    fail "probe failed: check dmesg; reboot the disposable VM"
fi

dmesg | tail -n 25
echo "avail_idx_flood: do not reuse this guest; reboot or destroy it"
