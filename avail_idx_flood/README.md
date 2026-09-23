# Out-of-window virtio-blk avail.idx probe

Guest module for split virtio-blk queues. It waits for one successful
`GET_ID` request, proving that the backend is active, then reuses its
descriptor throughout the available ring, sets `avail.idx` to 65535 and
kicks. All requests are `GET_ID`; the module does not read or write disk
sectors.

Run only in a disposable VM with a fresh, unpartitioned scratch image. The
guest runner requires its PCI BDF and refuses a device with partitions,
holders or a mounted filesystem, or a guest with swap enabled. It unbinds
the regular block driver and leaves the probe driver bound. Destroy the VM
afterwards; do not run the script on a host or a live guest.

Build against the *guest* kernel headers (which must export
`virtqueue_get_vring`) and copy `avail_idx_flood.ko` and `run.sh` into the
guest. From inside the guest, identify the scratch virtio-blk PCI BDF and
run:

```sh
make KERNELDIR=/path/to/guest/kernel/build LLVM=1
./run.sh 0000:00:05.0 --disposable
```

Alternatively, `mk-initrd.sh` builds the module and an initrd with a static
BusyBox, the runner and `/init`. The guest kernel needs built-in virtio-pci
and virtio-blk support. For example, on a test host with the guest's kernel
build tree and a matching kernel image:

```sh
KERNELDIR=/path/to/guest/kernel/build LLVM=1 ./mk-initrd.sh
REPRO_DIR=$(mktemp -d /tmp/crosvm-avail-idx.XXXXXX)
truncate -s 64M "$REPRO_DIR/disk.raw"
crosvm devices --block vhost="$REPRO_DIR/vhost.sock",path="$REPRO_DIR/disk.raw"
```

In a second terminal, set `REPRO_DIR` to the directory created above and
`INITRD_PATH` to the exact path printed after `initrd` by `mk-initrd.sh`
(for example, `/tmp/avail-idx-flood.A99uf3/initrd.cpio`). Start the
frontend with your test guest's PCI BDF in `repro.bdf`:

```sh
crosvm run \
    --vhost-user block,socket="$REPRO_DIR/vhost.sock" \
    --initrd "$INITRD_PATH" \
    -p "console=ttyS0,115200 rdinit=/init repro.bdf=0000:00:05.0" \
    /path/to/guest/bzImage
```

If the BDF differs, omit `repro.bdf` first and read the candidate PCI BDFs
from the serial console or use `run.sh` in a full guest. The runner never
guesses which block device to unbind. `/init` stays idle after reporting;
terminate the test VM from the host once you have collected the logs.

Before the queue fix, the worker can consume more requests than the ring
contains and logs `VULNERABLE`. A fixed backend should reject the forged
index after the successful control request, but zero completions alone do
not establish that: an unpatched backend could instead be stalled, out of
memory or dead before polling requests. Independently check that its host
PID remains alive and inspect its log before calling the test fixed. A
failed control request or only a few completions is inconclusive. Monitor
the isolated backend and terminate the test VM if needed.
