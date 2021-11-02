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

This uses a riscv buildroot for the rootfs.

qemu example command to start a riscv machine sharing local sources with `-virtfs`:

```
/scratch/qemu/build/qemu-system-riscv64 \
	-kernel /scratch/linux/arch/riscv/boot/Image \
	-nographic \
	-machine virt,aia=aplic-imsic,aia-guests=4 \
	-cpu rv64 \
	-m 4G  \
	-smp 2 \
	-append "root=/dev/vda ro console=hvc0 earlycon=sbi"                        \
	-drive file=/scratch/buildroot/output/images/rootfs.ext2,format=raw,id=hd0                 \
	-device virtio-blk-device,drive=hd0                 \
	-virtfs local,path=./,mount_tag=host,security_model=none \
	-netdev user,id=network0,hostfwd=tcp::1234-:1234 \
	-device e1000,netdev=network0,mac=52:54:00:54:46:69
```

Then from inside the riscv machine, start a VM with crosvm.
```
./crosvm/target/riscv64gc-unknown-linux-gnu/debug/crosvm run -c 1 --disable-sandbox --root ./buildroot/output/images/rootfs.ext2 ./guest.Image
```
