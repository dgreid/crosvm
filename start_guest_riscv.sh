#!/bin/sh

./crosvm/target/riscv64gc-unknown-linux-gnu/debug/crosvm run -c 1 --disable-sandbox --root ./buildroot/output/images/rootfs.ext2 ./linux/arch/riscv/boot/Image
