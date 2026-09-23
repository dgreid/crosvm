#!/bin/bash
# Copyright 2026 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build an initrd for a guest kernel with virtio-pci and virtio-blk built in.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
kver=${KVER:-$(uname -r)}
kerneldir=${KERNELDIR:-/lib/modules/$kver/build}
busybox=${BUSYBOX:-$(command -v busybox)}
[ -d "$kerneldir" ] || { echo "KERNELDIR does not exist: $kerneldir" >&2; exit 1; }
[ -f "$busybox" ] || { echo "BUSYBOX does not exist: $busybox" >&2; exit 1; }

kbuild_args=()
if [[ -n ${LLVM:-} ]]; then
    kbuild_args+=("LLVM=$LLVM")
fi
make -C "$kerneldir" M="$here" "${kbuild_args[@]}" modules

out=$(mktemp -d /tmp/avail-idx-flood.XXXXXX)
mkdir -p "$out/root/bin" "$out/root/proc" "$out/root/sys"
cp "$here/avail_idx_flood.ko" "$out/root/avail_idx_flood.ko"
cp "$here/run.sh" "$out/root/run.sh"
cp "$here/init" "$out/root/init"
cp "$busybox" "$out/root/bin/busybox"

(
    cd "$out/root"
    printf '%s\0' . ./bin ./bin/busybox ./proc ./sys ./init ./run.sh ./avail_idx_flood.ko |
        cpio --null -o --format=newc >"$out/initrd.cpio"
)
echo "initrd $out/initrd.cpio"
