# Building for risc-v

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

- Need pkg config to find riscv minijail and libcap, must define CROS_RUST to prevent minijail from trying to re-gen bindings during cross compile

```
export PKG_CONFIG_ALLOW_CROSS="true"
export CROS_RUST=1
```

```
cargo build --no-default-features --target=riscv64gc-unknown-linux-gnu --release
```

# Running

This uses a riscv buildroot for the guest rootfs.

qemu command:
```
qemu-system-riscv64 \
		-kernel ~/src/linux/arch/riscv/boot/Image \
		-nographic \
		-machine virt,aia=aplic-imsic,aia-guests=4 \
		-m 4G  \
		-append "root=/dev/vda ro console=ttyS0 earlycon=sbi keep_bootcon bootmem_debug" \
		-bios ~/src/opensbi/build/platform/generic/firmware/fw_jump.bin                 \
		-drive file=~/src/buildroot/output/images/rootfs.ext2,format=raw,id=hd0                 \
		-device virtio-blk-device,drive=hd0                 \
		-virtfs local,path=./,mount_tag=host,security_model=none  \
		-netdev user,id=network0,hostfwd=tcp::1234-:1234  \
		-device e1000,netdev=network0,mac=52:54:00:54:46:69
```
