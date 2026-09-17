#!/bin/bash
# Initrd that insmods pci_vcfg_short_read.ko as pid 1.
#
# The .ko must match the guest kernel. Point KERNELDIR at that kernel's
# headers build tree and VMLINUZ at its bzImage.
#
#   ./mk-initrd.sh
#   crosvm run --disable-sandbox --mem 256 \
#     --initrd "$OUT/initrd.cpio" \
#     -p "console=ttyS0,115200 rdinit=/init" \
#     "$VMLINUZ"

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
kver=${KVER:-$(uname -r)}
kerneldir=${KERNELDIR:-/lib/modules/$kver/build}
vmlinuz=${VMLINUZ:-/usr/lib/modules/$kver/vmlinuz}
out=${OUT:-/tmp/pci-vcfg-short-read}

if [[ ! -d $kerneldir ]]; then
    echo "KERNELDIR does not exist: $kerneldir" >&2
    exit 1
fi
if [[ ! -f $vmlinuz ]]; then
    echo "VMLINUZ does not exist: $vmlinuz" >&2
    exit 1
fi

make -C "$kerneldir" M="$here" CONFIG_DEBUG_INFO_BTF_MODULES= modules
trap 'make -C "$kerneldir" M="$here" clean >/dev/null' EXIT

rm -rf "$out/root"
mkdir -p "$out/root/bin" "$out/root/proc" "$out/root/sys"
cp "$here/pci_vcfg_short_read.ko" "$out/root/pci_vcfg_short_read.ko"
cp "$(command -v busybox)" "$out/root/bin/busybox"
chmod 755 "$out/root/bin/busybox"

cat >"$out/root/init" <<'EOF'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mkdir -p /proc /sys
mount -t proc proc /proc
mount -t sysfs sysfs /sys

base=0x100000000
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        repro.pci_vcfg_base=*) base=${arg#repro.pci_vcfg_base=} ;;
    esac
done

echo "repro: insmod pci_vcfg_short_read base=$base"
insmod /pci_vcfg_short_read.ko pci_vcfg_base="$base" || echo "repro: insmod failed ($?)"
dmesg | grep pci_vcfg_short_read || true
sleep 1
echo 1 >/proc/sys/kernel/sysrq
echo b >/proc/sysrq-trigger
sleep 5
EOF
chmod 755 "$out/root/init"

(cd "$out/root" && find . -print0 | cpio --null -o --format=newc >"$out/initrd.cpio")
echo "initrd $out/initrd.cpio"
ls -l "$out/initrd.cpio"
