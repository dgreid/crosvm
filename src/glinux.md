# Running crosvm on glinux

How to build and run crosvm on glinux with a wayland display.

## Prerequisites

### Termina rootfs

Get vm_rootfs.img from here:https://drive.google.com/file/d/18P316p35QYb-Npt2i04Xj03LPTumzjUr/view?usp=sharing

### Minijail

```
apt install libcap-dev

git clone https://android.googlesource.com/platform/external/minijail
cd minijail
make -j5
cp *.so /lib64
```

### Weston

```
apt install weston
```

## Build the kernel

```
cd linux
git remote add cros https://chromium.googlesource.com/chromiumos/third_party/kernel
git fetch cros
git co cros/chromeos-4.19
make chromiumos-container-vm-x86_64_defconfig
make -j$(nproc)
```

## Build crosvm

```
git clone https://chromium.googlesource.com/chromiumos/platform/crosvm
cd crosvm
cargo build
```

## Run guest

```
weston&

sudo ./target/debug/crosvm run --disable-sandbox -c 48 -m8192 --wayland-sock /run/user/$(id -u)/wayland-0   --host_ip 10.1.1.3 --netmask 255.255.255.0 --mac 70:5a:0f:2f:16:4e   --seccomp-policy-dir=seccomp/x86_64 --rwqcow vm_rootfs.img    -p 'root=/dev/vda rw init=/bin/bash'  --gpu vmlinux.bin

```

## Run wayland test app

This test app will turn the weston display blue. Pressing a key will exit.

Other wayland test apps should work too, they need to be run under `sommelier` which converts to virt-io wayland.

```
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /dev/shm

sommelier /opt/google/cros-containers
```
