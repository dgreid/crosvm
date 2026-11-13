# virtio-villain against crosvm

[virtio-villain](https://github.com/weltling/virtio-villain) is a hostile
guest test suite for VMM virtio device models. It boots a minimal
initramfs whose init drives virtio PCI devices directly from userspace
and violates driver MUST rules from the virtio spec on purpose:
malformed descriptor chains, bad ring addresses, out of range queue
indices, odd config space accesses and so on. Each test reports one of

- PASS: the device processed the request.
- REJECT: the device safely ignored or rejected it and still works.
- SKIP: the device or a precondition is missing.
- FAIL: the device did something wrong, or the VMM crashed.
- WEDGED: the device stopped working (for example it set NEEDS_RESET or
  the VMM died before the test reported).

virtio-villain has no crosvm backend upstream yet. `crosvm-backend.patch`
adds one (applies to virtio-villain `94b85dd1`), and `run.sh` applies it,
builds everything and runs the suite. Issues found are listed in
[TODO.md](TODO.md), each written as a prompt that can be handed to an
agent to make the fix. Each crosvm item starts with a small standalone
guest-side reproducer, kept in a separate repository
(`~/src/crosvm-guest-repros`), then the fix, then unit tests.

## Running

```sh
# Whole suite, crosvm built from this tree, kernel built from a Linux
# git tree with virtio-villain's guest config.
tools/virtio-villain/run.sh --linux ~/src/linux

# Selected tests only.
tools/virtio-villain/run.sh --linux ~/src/linux P0021 P0048 T0048

# crosvm's default sandboxed (multiprocess) mode.
tools/virtio-villain/run.sh --linux ~/src/linux --sandbox
```

Results go to `.tmp/virtio-villain/results-{nosandbox,sandbox}/`:
`summary.txt` is the human readable table, `report.json` has every
verdict, and `logs/<TEST>.log` has the guest serial output followed by
crosvm's stderr for each test.

Use `--kernel PATH` instead of `--linux` to boot a prebuilt bzImage.
With neither, virtio-villain downloads its default guest kernel from
GitHub, which may be blocked on corporate networks. `--villain DIR` uses
an existing virtio-villain checkout instead of cloning one. See
`run.sh --help` for the rest.

Requirements: KVM, `/dev/vhost-vsock`, a C compiler that can link static
binaries, Python 3, and the Linux kernel build dependencies when using
`--linux`. Net tests create tap devices with `sudo ip tuntap`. Without
`--sandbox` they also `setcap cap_net_admin` on a copy of crosvm,
because crosvm needs that capability to open more than one queue of a
persistent multiqueue tap. Everything is removed on exit; use `--no-net`
to skip it. virtio-fs tests use `virtiofsd` from the host when one is
installed, otherwise they SKIP.

## Backend notes

- The runner's output goes to the 8250 UART (`ttyS0`). A virtio-console
  is attached separately so the console tests can claim it.
- Devices: block (packed queues by default), net (tap, 4 queue pairs, or
  1 with `--sandbox`), vsock (vhost), balloon, rng, pmem,
  virtio-console, virtio-snd (null backend), 9p (`--shared-dir`) and
  virtio-fs (vhost-user to the host's virtiofsd).
- crosvm has no virtio-scsi, crypto, mem, watchdog, RTC, standalone
  IOMMU or MMIO transport, so those roughly 800 tests SKIP.
- Without `--sandbox` every device runs in the main process, so a device
  panic kills crosvm and the test FAILs. With `--sandbox` the device
  process dies, the device stops working and the test is WEDGED.
  virtio-villain labels the latter CRITICAL because crosvm's panic hook
  ends the child with SIGSEGV. It is the same panic, not memory
  corruption.

## Results

The results below were taken on 2026-11-13 with crosvm at `876a0acc8`
(`origin/main` `57b80f313` plus local block device changes; every issue
in TODO.md was checked against `origin/main`), virtio-villain `94b85dd1`
plus the patch, and a Linux `v7.2-rc1-240-g71dfdfb0209b` guest built
with virtio-villain's x86_64 config:

| mode | pass | reject | skip | fail | wedged |
|------|------|--------|------|------|--------|
| `--disable-sandbox`, 4 net queue pairs | 707 | 450 | 789 | 23 | 5 |
| sandboxed, 1 net queue pair | 724 | 424 | 800 | 18 | 8 |

SND0061 and SND0097 sometimes fail on the first attempt and pass when
retried.
