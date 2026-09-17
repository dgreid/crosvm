# PciVirtualConfigMmio short-read probe

Guest module that issues `readb` and `readw` at the VCFG window. With
`--mem 256` that window is at `0x100000000`.

Before `devices: pci: zero-fill short virtual-config reads`, crosvm
aborts in `PciVirtualConfigMmio::read`. After that commit the handler
zero-fills the requested length, the guest prints `0`, and crosvm exits
on the sysrq reset.

```bash
KERNELDIR=/path/to/guest/headers/build \
VMLINUZ=/path/to/guest/vmlinuz \
  ./mk-initrd.sh

crosvm run --disable-sandbox --mem 256 \
  --initrd /tmp/pci-vcfg-short-read/initrd.cpio \
  -p "console=ttyS0,115200 rdinit=/init" \
  /path/to/guest/vmlinuz
```

`repro.pci_vcfg_base=` on the kernel command line overrides the GPA.
