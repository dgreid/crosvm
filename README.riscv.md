# Building for risc-v

# Nix

```sh
nix build '.?submodules=1#crosvm-riscv64'
```

## Debian/Ubuntu

Install pre-requisites:

```
apt install gcc-riscv64-linux-gnu protobuf-compiler
rustup target add risv64gc-unknown-linux-gnu
```

- need libcap-dev for riscv: Assumes riscv-gnu-toolchain is installed in /opt/riscv

```
git clone git://git.kernel.org/pub/scm/linux/kernel/git/morgan/libcap.git
vim Make.Rules # Change so "BUILD_CC = gcc" otherwise tries to cross-compile host tools
make ARCH=riscv64 CROSS_COMPILE=riscv64-linux-gnu- GOLANG=no
sudo cp libcap/libcap.so* libcap/libpsx.so* /usr/riscv-linux-gnu/lib/
```

- Need pkg config to find riscv minijail and libcap, must define CROS_RUST to prevent minijail from
  trying to re-gen bindings during cross compile

```
export PKG_CONFIG_ALLOW_CROSS="true"
export CROS_RUST=1
```

```
cargo build --no-default-features --target=riscv64gc-unknown-linux-gnu --release
```

# Running

This uses a riscv buildroot for the rootfs.

qemu example command to start a riscv machine sharing local sources with `-virtfs`:

```
/scratch/qemu/build/qemu-system-riscv64 \
	--kernel /scratch/linux/arch/riscv/boot/Image \
	-nographic \
	-smp 4 \
	-m 8G \
	-M virt,aia=aplic-imsic,aia-guests=4 \
	-append "console=ttyS0 root=fsRoot rw rootfstype=9p rootflags=trans=virtio,version=9p2000.L,msize=501M" \
	-fsdev local,security_model=none,multidevs=remap,id=fsdev-fsRoot,path=../riscv_rootfs/ \
	-device virtio-9p-pci,id=fsRoot,fsdev=fsdev-fsRoot,mount_tag=fsRoot \
	-netdev user,id=network0,hostfwd=tcp::1234-:1234 \
	-device e1000,netdev=network0,mac=52:54:00:54:46:69
```

Then from inside the riscv machine, start a VM with crosvm.

```
modprobe kvm
crosvm run -c 1 --disable-sandbox --root=/opt/guest/rootfs.ext2 --serial type=stdout,hardware=virtio-console,console,stdin /opt/guest/Image
```
