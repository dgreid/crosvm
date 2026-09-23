# macOS ARM64 Port Status

This document tracks the current status of the macOS ARM64 (Apple Silicon) port of crosvm.

## Overview

The macOS port uses Apple's Hypervisor.framework to run ARM64 virtual machines on Apple Silicon
Macs. This is an experimental port with VM boot, block devices, shared filesystems, GPU, SMP, and
audio working, plus experimental socket_vmnet networking support.

## Current Status

### Working Features

| Feature            | Status          | Notes                                                        |
| ------------------ | --------------- | ------------------------------------------------------------ |
| HVF VM Creation    | ✅ Working      | Creates VM using Hypervisor.framework                        |
| Memory Mapping     | ✅ Working      | Split low/high banks to avoid GIC region                     |
| Kernel Loading     | ✅ Working      | Loads ARM64 Linux kernel at 0x80000000                       |
| Initrd Loading     | ✅ Working      | Loads initramfs for module-based kernels                     |
| FDT Generation     | ✅ Working      | Generates device tree with memory, devices, initrd           |
| VCPU Execution     | ✅ Working      | Runs guest code in EL1                                       |
| PSCI v1.1          | ✅ Working      | VERSION, FEATURES, MIGRATE_INFO_TYPE, CPU_ON                 |
| Serial Output      | ✅ Working      | Earlycon via MMIO UART at 0x3f8                              |
| HVC Handling       | ✅ Working      | Returns PSCI results through the standard SMCCC register ABI |
| GIC Emulation      | ✅ Working      | Userspace GICv3 with HVF IRQ injection                       |
| Virtual Timer      | ✅ Working      | PPI 11 via GIC interrupt delivery                            |
| Block Devices      | ✅ Working      | virtio-blk via MMIO transport, async I/O                     |
| Filesystem Sharing | ✅ Working      | virtiofs via --shared-dir, host-guest file sharing           |
| Network            | ✅ Working      | virtio-net via socket_vmnet; live shared-mode DHCP verified   |
| MMIO Bus           | ✅ Working      | Full virtio MMIO v2 device support                           |
| IRQ Delivery       | ✅ Working      | Edge-triggered SPI injection via IRQ handler thread          |
| GPU (virtio-gpu)   | ✅ Working      | 2D framebuffer via MMIO, DRM/fb0 device in guest             |
| Audio (virtio-snd) | ✅ Working      | CoreAudio playback verified end to end; see Audio below      |
| SMP Boot           | ✅ Working      | PSCI CPU_ON, tested with up to 4 CPUs                        |

### Partially Working

| Feature      | Status        | Notes                                               |
| ------------ | ------------- | --------------------------------------------------- |
| Serial Input | ⚠️ Workaround | Polling-based input, kqueue doesn't work with stdin |

## Boot Progress

The Linux kernel (tested with Debian 6.1.0-49-arm64) boots fully:

1. ✅ Early boot and earlycon initialization
1. ✅ PSCI probe - detects PSCIv1.1
1. ✅ CPU feature detection
1. ✅ Memory zone initialization (split low/high banks)
1. ✅ GIC initialization (userspace GICv3 emulation)
1. ✅ Timer initialization (vtimer PPI 11)
1. ✅ virtio-mmio device detection
1. ✅ virtio-blk driver loads, partition table scanned
1. ⚠️ virtio-net exchanges DHCP traffic with a local protocol helper
1. ✅ ext4 root filesystem mounted
1. ✅ virtiofs shared directories accessible
1. ✅ Init process runs, shell prompt reached

## Audio

The macOS path always creates a virtio-snd device and hard-wires it to the CoreAudio backend, so
there is no flag to pass. Playback has been verified end to end on Apple Silicon.

The one thing that is *not* supplied by crosvm is a guest driver. **Stock Debian arm64 has none:**
`linux-image-arm64` is built with `# CONFIG_SND_VIRTIO is not set`, so there is no `virtio_snd.ko`
under `/lib/modules` and no `virtio:d00000019v*` entry in `modules.alias` for udev to match on. The
guest enumerates the device and then stops after reset+ACKNOWLEDGE because nothing claims it. That
is indistinguishable from a broken device in the boot log, but the device is fine — a successful
virtio-mmio bind is silent, and so is a failure to bind. Check
`/sys/bus/virtio/devices/*/driver` rather than `dmesg`.

### Building a virtio_snd module for the guest

Build `sound/virtio` out of tree against the running kernel's own headers. This is much cheaper than
a full kernel build, but the version must match exactly or the module will not load. Run inside the
guest; the example is Debian bookworm `6.1.0-49-arm64` (Debian 6.1.174-1):

```sh
apt-get update
apt-get install -y --no-install-recommends make gcc curl

# 1. Headers for the exact running kernel. Once the archive rotates to a new
#    ABI these disappear from bookworm, so fetch them from snapshot.debian.org.
#    Resolve the file hashes with, e.g.:
#      https://snapshot.debian.org/mr/binary/linux-headers-6.1.0-49-arm64/6.1.174-1/binfiles
SNAP=https://snapshot.debian.org/file
curl -sSfL -o lh-common.deb $SNAP/364a8db484ae7581c2a11a59c22836c4e1d92fd9
curl -sSfL -o lkbuild.deb   $SNAP/736dbd605162a70000b17d3ca37986e9917faa00
curl -sSfL -o lh-arm64.deb  $SNAP/765fd7b6cd1a77648328d6a1c99d9d7f95c3072b
dpkg -i lh-common.deb lkbuild.deb lh-arm64.deb

# 2. sound/virtio at the matching tag.
mkdir -p virtio && cd virtio
GK=https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/plain/sound/virtio
for f in virtio_card.c virtio_card.h virtio_chmap.c virtio_ctl_msg.c virtio_ctl_msg.h \
         virtio_jack.c virtio_pcm.c virtio_pcm.h virtio_pcm_msg.c virtio_pcm_ops.c; do
    curl -sSfL -o "$f" "$GK/$f?h=v6.1.174"
done

# 3. The upstream Makefile is gated on CONFIG_SND_VIRTIO, which is unset, so it
#    would build nothing. kbuild prefers a Kbuild file when one is present,
#    which overrides the gate without touching the upstream sources.
cat > Kbuild <<'EOF'
obj-m += virtio_snd.o
virtio_snd-objs := virtio_card.o virtio_chmap.o virtio_ctl_msg.o virtio_jack.o \
                   virtio_pcm.o virtio_pcm_msg.o virtio_pcm_ops.o
EOF
make -C "/lib/modules/$(uname -r)/build" M="$PWD" modules

# 4. Install it so depmod resolves the snd/snd-pcm dependencies and udev
#    autoloads the module from the device modalias on every subsequent boot.
cp virtio_snd.ko "/lib/modules/$(uname -r)/updates/"
depmod -a
modprobe virtio_snd
```

`modinfo virtio_snd.ko` should report `vermagic: 6.1.0-49-arm64 SMP mod_unload modversions aarch64`
and `alias: virtio:d00000019v*`. The module is unsigned, so it taints the kernel; that is expected
and harmless here.

### What was verified

With that module in place, on a fresh boot and with no manual `modprobe`:

- `/sys/bus/virtio/devices/virtio2` reports `virtio:d00000019v4D565243` with `drv=virtio_snd`.
- `/proc/asound/cards` shows `0 [SoundCard]: virtio-snd - VirtIO SoundCard`, and `/dev/snd/pcmC0D0p`
  exists. `aplay -l` lists `card 0: SoundCard [VirtIO SoundCard], device 0: virtio-snd [VirtIO PCM 0]`.
- crosvm logs `mmiovirtio-sound activated by the guest driver with 4 queue(s)`.
- Playing a 3 s 440 Hz sine with `aplay` and with `speaker-test -t sine` makes crosvm log a
  `CoreAudio playback stream started: 2 ch, 48000 Hz, S16LE` line followed by progress lines whose
  "bytes rendered by CoreAudio" counter advances at real time. That counter is the ring buffer's read
  index, so it only moves when CoreAudio's render callback actually consumes the samples. The
  reported peak sample amplitude matched the amplitude of the generated tone, which confirms real
  PCM data — not silence — reached CoreAudio.

Not verified: that the audio was audible from the Mac's speakers. There is no loopback capture in
this setup, so the evidence stops at CoreAudio consuming correct samples at the right rate. Audio
*capture* (recording from the guest) was also not tested.

## Key Bug Fixes

### XZR Register in MMIO Data Abort (Critical)

The `get_reg(31)` and `set_reg(31, value)` methods in HVF VCPU incorrectly mapped register 31 to the
PC instead of the zero register (XZR). In ARM64 data abort syndrome encoding, SRT=31 always means
XZR. This caused every `str wzr, [addr]` instruction (write zero) during MMIO to write the PC value
instead, corrupting virtio device status registers and preventing device activation.

### SMCCC Return Handling

Hypervisor.framework reports the guest PC at the instruction following `hvc`. The PSCI handler only
updates X0 with the return value, leaving the PC, stack, and other registers untouched so each Linux
kernel version can complete its own SMCCC wrapper correctly.

### GICR_TYPER 64-bit Access

The GIC redistributor's GICR_TYPER register is 64-bit but was only handling 32-bit reads. Added
8-byte read handling for kernels that read it as a single 64-bit load.

## Building and Running

### Prerequisites

- macOS on Apple Silicon (M1/M2/M3)
- Rust toolchain
- ARM64 Linux kernel image (uncompressed Image format)

### Build

```bash
cargo build --release
codesign --sign - --entitlements crosvm.entitlements --force target/release/crosvm
```

### Run

```bash
# Basic run with kernel
./target/release/crosvm run -m 512 arm64_Image

# With disk and initrd (e.g., Debian)
./target/release/crosvm run -m 4096 \
    --rwdisk disk.raw \
    --initrd initrd.cpio \
    -p "root=/dev/vda1 rw console=ttyS0 earlycon" \
    vmlinuz

# With shared directory
./target/release/crosvm run -m 4096 \
    --rwdisk disk.raw \
    --initrd initrd.cpio \
    --shared-dir "/path/on/host:tagname" \
    -p "root=/dev/vda1 rw console=ttyS0 earlycon" \
    vmlinuz
```

Guest can mount the shared directory:

```bash
mount -t virtiofs tagname /mnt
```

### Networking

The macOS backend uses a privileged, persistent `socket_vmnet` service while crosvm itself remains
unprivileged. Install and start the service once:

```bash
brew install socket_vmnet
brew update
sudo env \
  HOMEBREW_CACHE="$(brew --cache)" \
  HOMEBREW_NO_AUTO_UPDATE=1 \
  "$(command -v brew)" services start socket_vmnet
```

Then pass its Unix socket to crosvm:

```bash
./target/release/crosvm run -m 4096 \
    --rwdisk disk.raw \
    --initrd initrd.cpio \
    --net "socket-vmnet=$(brew --prefix)/var/run/socket_vmnet,mac=02:00:00:00:00:01" \
    -p "root=/dev/vda1 rw console=ttyS0 earlycon" \
    vmlinuz
```

Shared mode supplies DHCP, DNS, NAT, and direct host-to-guest connectivity. Live shared-mode DHCP
against the privileged socket_vmnet service is validated by `./tools/test_macos_boot.sh network`,
which boots Debian, confirms `virtio_net` bound to the `virtio_mmio` transport, and pings the vmnet
gateway from the guest. The macOS backend supports one queue pair per interface and does not
currently expose checksum or segmentation offloads. Do not run crosvm with `sudo`; only the small
socket_vmnet service needs vmnet.framework privileges.

A successful `virtio_net` bind produces no guest console output at all, and the interface keeps the
generic name `eth0` because a virtio-mmio device has no bus path for predictable naming. The absence
of a `virtio_net` line in the boot log is therefore not evidence of a failure — see
[Network](../devices/net.md) for how to confirm the bind from inside the guest.

## Architecture Notes

### HVF PSCI Handling

The PSCI implementation handles the HVC trap, writes the result to X0, and resumes at the PC
reported by Hypervisor.framework. Guest SMCCC wrapper instructions are not skipped or emulated,
which keeps the ABI compatible across Linux kernel versions.

### Memory Layout

```
0x00000000 - 0x7FFFFFFF  Device/unmapped
0x80000000 - 0xBFFFFFFF  Low RAM bank (up to 1 GiB)
0xC0000000 - 0xC00FFFFF  GIC Distributor + Redistributors
0xC0100000 - 0xFFFFFFFF  Unmapped
0x100000000+             High RAM bank (remainder if >1 GiB)
```

### IRQ Delivery

Device interrupts flow through a dedicated IRQ handler thread:

1. Device worker completes I/O → signals IrqEdgeEvent (pipe-based)
1. IRQ handler thread polls device events serially
1. Marks the SPI pending in the userspace GIC model
1. Kicks active VCPUs via `hv_vcpus_exit()` to process the interrupt
1. Calls `hv_vcpu_set_pending_interrupt()` before the VCPU re-enters the guest

## References

- [Apple Hypervisor.framework Documentation](https://developer.apple.com/documentation/hypervisor)
- [socket_vmnet](https://github.com/lima-vm/socket_vmnet)
- [ARM PSCI Specification](https://developer.arm.com/documentation/den0022)
- [ARM SMCCC Specification](https://developer.arm.com/documentation/den0028)
