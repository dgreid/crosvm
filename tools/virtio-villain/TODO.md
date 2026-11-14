# virtio-villain issues in crosvm

Problems found by running virtio-villain against crosvm (see
[README.md](README.md)). Each item is a complete prompt for an agent
fixing it, ordered most severe first. The first group is crosvm bugs;
the last group is problems in virtio-villain or its crosvm backend.

Every crosvm item follows the same order:

1. Write a small guest-side reproducer in a separate repository
   (`~/src/crosvm-guest-repros`, one directory per item) and confirm it
   shows the bug.
2. Fix crosvm.
3. Add unit tests afterwards, without changing interfaces unless
   approved.

All file references are relative to the crosvm root and were checked
against `origin/main` `57b80f313`.

---

## crosvm

### 1. [HIGH] [DONE] Packed virtqueue: a guest can panic the device with a chain that ends early

Done. crosvm branch `villain-packed-chain-id` (worktree
`~/src/crosvm-packed-chain`, on `master` 6ef7f7bac):

- 55456dfd4 "devices: virtio: reject truncated packed chains": the fix.
- b6a6d0379 "devices: virtio: handle packed rings outside RAM": step 2.3,
  the event suppression and descriptor table `unwrap()`s.

Reproducers in `~/src/crosvm-guest-repros`: `01-packed-chain-no-id`
(P0021, P0048) and `01-packed-ring-outside-ram` (step 2.3). Step 2.2 was
left out on review: the `add_used` id `expect()` stays, since the
iterator fix means a guest can't reach it. A full run has P0004, P0021 and
P0048 fixed and no other P or B group changes.

```
In the crosvm repository, fix a guest-triggerable panic in the packed
virtqueue code.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
Two tests panic the virtio-blk worker thread, which takes down the
whole VMM:

- P0021 "Packed chain with wrong AVAIL flag on middle descriptor"
  (virtio 1.2 2.8.6)
- P0048 "Packed descriptor with split format NEXT bit set" (2.8.6)

Observed (crosvm stderr in .tmp/virtio-villain/results-*/logs/P0021.log):

  ERROR device_virtio_block::asynchronous] block: failed to handle
    request: not enough space in descriptor chain to write status
  thread 'virtio_blk' panicked at
    devices/src/virtio/queue/packed_queue.rs:333:18:
    Packed descriptor chain should have id

Root cause: PackedDescriptorChain's iterator in
devices/src/virtio/queue/packed_descriptor_chain.rs returns Ok(None)
when the next descriptor is not available
(`!desc.is_available(...)`). That's correct for the head of a chain,
but when it happens after a descriptor with VIRTQ_DESC_F_NEXT, the
chain just stops early. `self.id` is only set on the last descriptor
(the one without NEXT), so the DescriptorChain is built with id: None.
The device then completes the request, and PackedQueue::add_used_batch
in devices/src/virtio/queue/packed_queue.rs `.expect()`s the id.

Step 1, guest reproducer. Directory: 01-packed-chain-no-id. Attach
only a virtio-blk device with a packed queue (`--block
path=disk.raw,packed-queue=true`, a 16 MiB raw file made by run.sh) and
blacklist virtio_blk_init. Negotiate VIRTIO_F_VERSION_1 and
VIRTIO_F_RING_PACKED, set up queue 0 as a packed ring, and make a
VIRTIO_BLK_T_IN request whose first descriptor (the request header)
has NEXT set and is available, followed by a descriptor whose
AVAIL/USED bits mark it not available. Kick the queue. Then reset the
device, set it up again and submit a well formed read. Print "REPRO:
OK" if the read completes within a second, "REPRO: BUG" otherwise. A
crosvm panic shows up as a missing marker. Add a second case for
P0048: the last descriptor of the chain also has NEXT set, so the
chain runs into a descriptor that isn't available.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix:
1. In the packed chain iterator, a descriptor that is not available
   after a NEXT descriptor is an error (bail!), the same as other
   malformed chains. Also make sure every path that ends a chain
   successfully has set the id. A chain that wraps all the way around
   the ring without a terminating descriptor must also be rejected
   (check whether the existing count or loop limit already does this).
2. Make add_used_batch not panic on a chain without an id. A missing id
   is a device bug, not something the guest controls, so log an error
   and skip that chain rather than crash. Keep a debug_assert if you
   like.
3. Look for other guest-reachable unwrap()/expect() calls in
   packed_queue.rs and packed_descriptor_chain.rs (for example the
   get_slice_at_addr(...).unwrap() on the descriptor table) and handle
   them the same way, or explain why the guest can't reach them.

Step 3, unit tests. Next to the existing packed queue tests (see
devices/src/virtio/queue/packed_queue.rs and the block device's packed
queue tests in devices/device_virtio_block), build a packed ring whose
middle descriptor has the wrong AVAIL/USED bits, and one whose last
descriptor has NEXT set. Check that pop() returns None or skips the
chain without panicking, and that a valid chain after it still works.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" for both cases with the
  fixed crosvm, and "REPRO: BUG" (or a crash) with the old one;
- cargo test for the touched crates passes, along with tools/presubmit's
  format and clippy checks for the touched files;
- `tools/virtio-villain/run.sh --linux <linux tree> P0021 P0048 P0003
  P0040 T0001` shows no FAIL or WEDGED (REJECT is the expected
  verdict);
- a full `tools/virtio-villain/run.sh` run has no new FAIL or WEDGED in
  the P and B groups.

Write the commit message in crosvm style ("devices: virtio: ...", a
body that explains the bug, then BUG=None and TEST= lines).
```

### 2. [HIGH] Split virtqueue: ring addresses outside guest memory panic the device

```
In the crosvm repository, fix a guest-triggerable panic when a split
virtqueue's ring addresses point outside guest memory.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
Test T0048 "queue_desc/avail/used beyond guest RAM" (virtio 1.2 2.7)
sets queue_desc, queue_driver and queue_device to addresses outside
guest RAM, enables the queue and kicks it. crosvm panics:

  thread 'virtio_blk' panicked at
    devices/src/virtio/queue/split_queue.rs:306:14:
    called `Result::unwrap()` on an `Err` value:
    InvalidGuestAddress(GuestAddress(0xffffffffffff1000))

Root cause: SplitQueue::new in devices/src/virtio/queue/split_queue.rs
only checks that `addr + ring size` doesn't overflow u64. It doesn't
check that the descriptor table, available ring and used ring are
inside guest memory. Later, peek() calls
`self.mem.get_slice_at_addr(self.avail_ring, ...).unwrap()`, and there
are similar unwraps for the used ring and used_event/avail_event
reads and writes.

Step 1, guest reproducer. Directory: 02-split-ring-outside-ram. Attach
only a virtio-blk device with split queues (`--block path=disk.raw`;
packed-queue defaults to false) and blacklist virtio_blk_init. Without
VIRTIO_F_RING_PACKED, set queue 0's queue_desc, queue_driver and
queue_device to 0xffffffffffff0000 (and as a second case, an address
just past the end of guest RAM, e.g. 4 GiB with a 512 MiB guest),
enable the queue, set DRIVER_OK and kick. Then read device_status:
NEEDS_RESET is fine. Reset the device, set it up again with valid rings
and submit a read. Print "REPRO: OK" if that read completes,
"REPRO: BUG" otherwise. A crosvm panic shows up as a missing marker.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix:
1. In SplitQueue::new (and PackedQueue::new for its descriptor ring
   and event suppression areas), check that each ring lies in guest
   memory (for example GuestMemory::get_slice_at_addr(addr, size) or
   is_valid_range), and fail activation with an error naming the ring.
   QueueConfig::activate already returns a Result and
   VirtioPciDevice::activate turns a failed activation into
   NEEDS_RESET, which is the right outcome. Check that path still
   works when the failure happens in queue activation rather than in
   device.activate(). See devices/src/virtio/virtio_pci_device.rs
   activate().
2. Activation-time checks don't cover guests that remove memory later
   (balloon, virtio-mem, hotplug), so replace the runtime unwrap()s in
   split_queue.rs with error handling (log once, treat the queue as
   empty, don't add used entries) instead of panicking.
3. Check that the transport also rejects ring alignment violations, as
   virtio 1.2 2.7 requires (16-byte descriptor table, 2-byte
   available ring, 4-byte used ring for split queues), or document why
   not.

Step 3, unit tests. In split_queue.rs and packed_queue.rs, build a
QueueConfig whose ring addresses are partly or fully outside a small
GuestMemory, and check that activate() returns Err and that nothing
panics. If a runtime path stays guest-reachable (memory removed after
activation), test it too.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" for both cases with the
  fixed crosvm, and fails with the old one;
- cargo test for the touched crates passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> T0048 T0049 T0050
  PCI0009 S0001` has no FAIL (REJECT or WEDGED with NEEDS_RESET are
  acceptable for T0048);
- a full run has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: virtio: ...", a
body, BUG=None, TEST=).
```

### 3. MSI-X Message Control writes that aren't exactly 16 bits at offset 2 are dropped

```
In the crosvm repository, fix MSI-X capability writes so a guest can
enable MSI-X and set the function mask with any naturally aligned
config space write.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
These tests enable MSI-X by writing the capability with a 32-bit write
at the start of the MSI-X capability (cap ID, next pointer and Message
Control together), which PCI allows. crosvm ignores the write:

  ERROR devices::pci::msix] invalid write to MSI-X Capability Structure
    offset 0

so MSI-X never gets enabled and the tests see no interrupts or PBA
bits:

- B0345, B0346, B0350, B0351 (virtio 1.2 4.1.5.1.2): "queue N
  completion did not signal vector N (PBA 0x0)"
- PCI0070, PCI0120: "MSI-X enable or function mask readback is
  incorrect"
- PCI0121: "MSI-X enable was not retained"

Root cause: MsixConfig::write_msix_capability in devices/src/pci/msix.rs
only handles `offset == 2 && data.len() == 2`. Linux's msi code uses
16-bit writes, which is why this hasn't shown up before.

Step 1, guest reproducer. Directory: 03-msix-dword-write. Attach only a
virtio-blk device and blacklist virtio_blk_init. Find the MSI-X
capability (ID 0x11) by walking the capability list in the device's
sysfs config file. Write the capability's first dword with the MSI-X
Enable bit (bit 15 of Message Control, so bit 31 of the dword) set,
using a single 4-byte pwrite at the 4-byte aligned capability offset
(the kernel turns that into one dword config access). Read Message
Control back. Repeat for the Function Mask bit (bit 14), and for 8-bit
writes to offset cap+3. Print "REPRO: BUG" naming the first write
shape whose effect isn't read back, otherwise "REPRO: OK". In run.sh,
also grep crosvm's stderr for "invalid write to MSI-X Capability
Structure" and report it.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix: handle any write that overlaps the Message Control word
(bytes 2-3 of the capability): a 32-bit write at offset 0, a 16-bit
write at offset 2, and 8-bit writes at offset 2 or 3. Pull the Message
Control bytes out of the write and apply them, keeping the existing
enable, mask and PBA handling. Writes to read-only bytes (cap ID, next
pointer) are ignored without an error. Check how the caller in
devices/src/pci/pci_configuration.rs or the device's
write_config_register passes offset and data (relative to the
capability or to the register), and test the real path from a config
write. Check the read side (read_msix_capability) for the same
assumptions.

Step 3, unit tests. In msix.rs, cover each write shape above (enable,
disable, function mask set and clear) and check that a dword write
doesn't change the read-only fields.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p devices passes, along with presubmit format and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> B0345 B0346 B0350
  B0351 PCI0070 PCI0120 PCI0121 PCI0126` no longer fails for MSI-X
  enable or PBA reasons (PCI0122/0123/0126 also depend on TODO item 4);
- a full run has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: pci: msix: ...",
body, BUG=None, TEST=).
```

### 4. Writes to queue_msix_vector on an enabled queue are ignored

```
In the crosvm repository, let the driver change or unmap a queue's
MSI-X vector while the queue is enabled.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
These tests remap or unmap (VIRTIO_MSI_NO_VECTOR, 0xffff) a live
queue's vector through common config queue_msix_vector:

- PCI0122 "live unmapping stops new notifications after a pending
  event" (virtio 1.2 4.1.5.1.2.1)
- PCI0123 "independent MSI-X mappings for two live queues"
- PCI0126 "MSI-X Function Mask suppresses all delivery but not I/O"

All fail at lib/msix_helpers.c:84 "queue 0 vector 0x0000, expected
0xffff" (or 0x0001), and crosvm logs

  WARN devices::virtio::queue] ignoring write to vector on ready queue

Root cause: QueueConfig::set_vector in devices/src/virtio/queue.rs
returns early when the queue is ready. Section 4.1.5.1.2 lets the
driver map a queue to a vector, or to NO_VECTOR, by writing
queue_msix_vector and reading it back, and doesn't restrict this to
disabled queues. Linux does it when changing IRQ affinity or
masking.

Step 0: read the relevant spec text (4.1.4.3, 4.1.5.1.2, 4.1.5.1.2.1)
and confirm a live queue's vector may change. If you conclude it may
not, stop and write up why instead of doing the steps below.

Step 1, guest reproducer. Directory: 04-live-queue-vector. Attach only
a virtio-blk device and blacklist virtio_blk_init. Enable MSI-X with a
16-bit write to Message Control (so this doesn't depend on TODO item
3), map queue 0 to vector 1 and enable it, then set DRIVER_OK. Write
queue_msix_vector = 0xffff for queue 0 and read it back. Then map it to
vector 2, mask vector 2 in the MSI-X table, submit a read and check
that vector 2's PBA bit gets set (and vector 1's doesn't). Print
"REPRO: BUG <which check>" or "REPRO: OK".

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix:
1. Allow set_vector on ready queues. The activated Queue
   (SplitQueue/PackedQueue) copies the vector at activation and the
   Interrupt object signals with it, so the new vector has to reach
   the running worker. Find how the vector is used after activation
   (grep `vector()` in devices/src/virtio/queue/*.rs and
   devices/src/virtio/interrupt.rs) and pick the simplest correct
   mechanism, for example a shared atomic per queue that both
   QueueConfig and the activated queue read.
2. NO_VECTOR (0xffff) means no interrupt for that queue. Check that
   signaling with NO_VECTOR doesn't fall back to INTx or vector 0.
3. Also handle msix_config (config change vector) the same way if it
   has the same restriction.

Step 3, unit tests. In devices/src/virtio, cover changing a ready
queue's vector, reading it back, and the next used buffer notification
going to the new vector (or none for NO_VECTOR).

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test for the touched crates passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> PCI0122 PCI0123
  PCI0126` passes once TODO item 3 (MSI-X dword writes) is also
  fixed. If item 3 isn't fixed yet, show the vector readback check
  passing in the logs;
- a normal Linux guest with virtio-blk and virtio-net still boots and
  does I/O (run the existing e2e tests that cover virtio-blk if you can).

Write the commit message in crosvm style ("devices: virtio: ...", body,
BUG=None, TEST=).
```

### 5. VIRTIO_PCI_CAP_PCI_CFG capability is truncated and doesn't work

```
In the crosvm repository, implement the virtio PCI configuration access
capability (VIRTIO_PCI_CAP_PCI_CFG) as the spec requires.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
Two tests fail:

- PCI0032 (virtio 1.2 4.1.4.9): "cap_len=16 (expected >= 20)"
- PCI0039 (4.1.4.7.2): the guest can't drive the device through the
  capability, "!(ds & VIRTIO_STATUS_DRIVER_OK)"

Root cause: VirtioPciDevice in devices/src/virtio/virtio_pci_device.rs
adds `VirtioPciCap::new(PciCapabilityType::PciConfig, 0, 0, 0)`, a
plain 16-byte virtio_pci_cap. Section 4.1.4.9 defines
struct virtio_pci_cfg_cap { struct virtio_pci_cap cap; u8
pci_cfg_data[4]; } (20 bytes). The driver sets cap.bar, cap.offset and
cap.length (1, 2 or 4) and then reads or writes pci_cfg_data, which the
device turns into an access to that BAR at that offset. crosvm has
neither the data window nor the forwarding.

Step 1, guest reproducer. Directory: 05-pci-cfg-cap. Attach only a
virtio-blk device and blacklist virtio_blk_init. Walk the vendor
capabilities (ID 0x09) in the sysfs config file and find the one with
cfg_type 5. Report "REPRO: BUG cap_len=N" if cap_len < 20. Otherwise
set cap.bar to the common config BAR, cap.offset to common config
offset + 0x14 (device_status) and cap.length to 1, read pci_cfg_data,
and compare it with device_status read through the BAR mmap. Then
write ACKNOWLEDGE through the window and check it through the BAR.
Print "REPRO: OK" if everything matches.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix:
1. Add a VirtioPciCfgCap (20 bytes) and register it instead of the bare
   cap.
2. Make cap.bar, cap.offset and cap.length writable by the driver,
   following the spec's device requirements: ignore or reject a length
   other than 1, 2 or 4 and offsets not aligned to the length, and only
   forward to BARs the device implements.
3. On reads and writes of pci_cfg_data, run the equivalent BAR access
   through the same code path as an MMIO access (read_bar/write_bar on
   the settings BAR), honoring cap.length. Check how config writes
   reach VirtioPciDevice (write_config_register and
   PciConfiguration::write_reg) and add a hook there for this
   capability's offset range.
4. Cloud Hypervisor fixed the same bug in
   https://github.com/cloud-hypervisor/cloud-hypervisor/pull/8238 and
   #8272 (respect cap.length). Use them as a reference only.

Step 3, unit tests. In virtio_pci_device.rs (or a test module in that
file), write the cap fields, read device_status and a common config
field through pci_cfg_data, write device_status through it, and check
that bad lengths and offsets are rejected without side effects.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p devices passes, along with presubmit format and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> PCI0032 PCI0039`
  and the other PCI tests that use the capability (grep
  VIRTIO_PCI_CAP_PCI_CFG in the virtio-villain tests) pass or REJECT;
- a normal Linux guest still boots with virtio devices.

Write the commit message in crosvm style ("devices: virtio: pci: ...",
body, BUG=None, TEST=).
```

### 6. virtio-net rejects VQ_PAIRS_SET for anything but the maximum

```
In the crosvm repository, make virtio-net accept
VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET for any number of queue pairs in range.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
N0077 "Control VQ: set multiqueue pairs" and N0147 "CTRL MQ set pairs
to 1" (virtio 1.2 5.1.6.5.5) send VQ_PAIRS_SET with 1 pair to a device
with 4 pairs and get VIRTIO_NET_ERR:

  ERROR device_virtio_net] Invalid VQ_PAIRS_SET cmd, driver request
    pairs: 1, device vq pairs: 4

Root cause: process_ctrl_request in devices/device_virtio_net/src/lib.rs
rejects the command when `pairs != vq_pairs`. The spec says the
device MUST accept any value from 1 to max_virtqueue_pairs
(VIRTIO_NET_CTRL_MQ_VQ_PAIRS_MIN..MAX), and after it acks, MUST NOT
queue packets on receive queues beyond the requested number.

Step 1, guest reproducer. Directory: 06-net-vq-pairs-set. Attach only
a virtio-net device with 4 queue pairs and blacklist
virtio_net_driver_init. run.sh creates a user-owned multiqueue tap
(`sudo ip tuntap add NAME mode tap multi_queue user $USER`, `ip link
set NAME up`), passes `--net tap-name=NAME,vq-pairs=4`, and deletes the
tap on exit (trap). Opening extra queues needs CAP_NET_ADMIN (TODO item
13), so until that's fixed run.sh does what tools/virtio-villain/run.sh
does: setcap cap_net_admin+ep on a copy of crosvm, removed on exit.
Negotiate VIRTIO_NET_F_MQ and VIRTIO_NET_F_CTRL_VQ, set up the control
queue (index 8), and send VQ_PAIRS_SET with 1, 2, 4, then 0 and 5.
Print "REPRO: BUG" if any of 1..4 isn't acked with VIRTIO_NET_OK or 0/5
isn't rejected, otherwise "REPRO: OK".

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix: accept 1..=max_virtqueue_pairs (return VIRTIO_NET_ERR
outside that range or when VIRTIO_NET_F_MQ wasn't negotiated) and limit
RX to the active pairs. The simplest correct approach: keep all workers
running but have workers for pairs >= the active count stop taking
packets from the tap for RX. Since each pair has its own tap queue fd,
check what happens to packets the kernel steers to an inactive tap
queue; the TUNSETQUEUE ioctl with IFF_DETACH_QUEUE/IFF_ATTACH_QUEUE is
the standard way to stop the kernel steering packets to a queue. TX on
any queue can keep working. Handle the vhost-net path in
devices/device_virtio_net/src/vhost.rs too, or explicitly ERR there if
it can't support it. Reset must restore the default (all pairs, or 1
before the driver sends VQ_PAIRS_SET; check the spec).

Step 3, unit tests. Cover the ctrl command handling (valid range, 0,
max + 1, without MQ) and, if possible with the existing interfaces and
the fake tap in net_util, that RX stops on detached queues.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p device_virtio_net passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> N0077 N0147` passes
  (needs the default nosandbox mode, which uses 4 queue pairs);
- a full run has no new FAIL or WEDGED in the N group;
- a normal multiqueue Linux guest still gets network (ethtool -L to
  change channels).

Write the commit message in crosvm style ("devices: net: ...", body,
BUG=None, TEST=).
```

### 7. virtio-net activation fails if the driver enables more queues than its features allow

```
In the crosvm repository, make virtio-net activation tolerate extra
enabled queues, and fix the misleading error message.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
N0202 "TX notify address with mismatched queue values" (virtio 1.2
4.1.4.4) and N0303 "TX frames so large the tap write fails" (5.1.6.2)
enable every queue the device offers (9 with 4 pairs plus ctrl) but
negotiate features that need fewer (no VIRTIO_NET_F_MQ, so 2 data
queues plus ctrl). Activation fails, the device goes to NEEDS_RESET
and the tests report WEDGED:

  ERROR devices::virtio::virtio_pci_device] pcivirtio-net activate
    failed: net: expected 9 queues, got 9 queues
  ERROR devices::virtio::virtio_pci_device] failed to activate device:
    failed to activate queue: queue is already activated

Root cause: Net::activate in devices/device_virtio_net/src/lib.rs
requires `queues.len() == num_queues_expected`, and its error message
prints self.queue_sizes.len() instead of num_queues_expected.
Separately, after device.activate() fails, VirtioPciDevice::activate
in devices/src/virtio/virtio_pci_device.rs has already called
QueueConfig::activate on every ready queue, so later attempts before a
reset log "queue is already activated".

Step 1, guest reproducer. Directory: 07-net-extra-queues. Attach only a
virtio-net device with 4 queue pairs and blacklist
virtio_net_driver_init. Set up the tap as in TODO item 6: run.sh
creates a user-owned multiqueue tap with sudo, uses a setcap
cap_net_admin copy of crosvm until item 13 is fixed, and removes both
on exit. Negotiate VERSION_1 and CTRL_VQ but not MQ, enable all
num_queues queues, and set DRIVER_OK. Print "REPRO: BUG" if
device_status has NEEDS_RESET or a small TX frame on queue 1 doesn't
complete. Second case: reset, set up again with only the needed
queues, and check TX works, so a failed activation is fully recoverable.
Print "REPRO: OK" if both pass.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix:
1. Fix the error message to print the expected count.
2. In Net::activate, use the queues the negotiated features need (the
   first vq_pairs*2 data queues plus the control queue at index
   max_pairs*2 when VIRTIO_NET_F_CTRL_VQ is negotiated; check the
   spec's queue numbering in 5.1.2) and ignore extra enabled queues
   instead of failing. Still fail if a needed queue is missing.
3. In VirtioPciDevice::activate, when device.activate() fails, either
   undo the per-queue activation or make the "already activated" path
   quiet and harmless, so the only outcome is NEEDS_RESET and a
   driver reset fully recovers.
4. Check the other virtio devices' activate() for the same strict
   queue count check (grep "expected .* queues" under devices/) and
   decide whether each should tolerate extra queues. Only change ones
   where the spec says extra queues are unused.

Step 3, unit tests. Cover Net::activate with extra queues (MQ not
negotiated, all 9 enabled), with a needed queue missing, and the
PCI-level sequence of a failed activation, a reset and a successful
reactivation.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test for the touched crates passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> N0202 N0303` no
  longer reports "expected 9 queues" or "already activated" (check the
  logs). N0303 may still fail for its own reason (a 1 MiB TX frame the
  tap rejects). If so, look into that separately: the frame should be
  dropped and completed, not leave the device in NEEDS_RESET;
- a full run has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: net: ...", body,
BUG=None, TEST=).
```

### 8. vhost-user frontend sends GET_CONFIG without negotiating VHOST_USER_PROTOCOL_F_CONFIG

```
In the crosvm repository, stop the vhost-user frontend from sending
VHOST_USER_GET_CONFIG/SET_CONFIG to backends that didn't negotiate the
CONFIG protocol feature.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
virtio-fs is attached as `--vhost-user type=fs,socket=...` with the
host's virtiofsd 1.13.3 as backend (started without --tag). F0037
"virtio-fs device config tag region non zero" (virtio 1.4 5.11.6) and
F0042 "Read virtio-fs config tag and validate format" (5.11.4) read
device config space. The backend drops the connection and the device is
dead (WEDGED):

  ERROR devices::virtio::vhost_user_frontend] failed to read config:
    failed to get config: client closed the connection
  ERROR devices::virtio::vhost_user_frontend] vhost-user worker thread
    exited with an error: vu-fs: vhost_user_frontend worker failed:
    Backend device disconnected early

Root cause: VhostUserFrontend::read_config and write_config in
devices/src/virtio/vhost_user_frontend/mod.rs always call
backend_client.get_config/set_config. The CONFIG protocol feature is
in allow_protocol_features, but nothing checks whether it was actually
negotiated. Per the vhost-user spec, the frontend must not send
GET_CONFIG unless VHOST_USER_PROTOCOL_F_CONFIG was negotiated.

Step 1, guest reproducer. Directory: 08-vhost-user-get-config. run.sh
starts virtiofsd (`virtiofsd --socket-path S --shared-dir D --cache
never`; look for it on PATH and in /usr/libexec, and print "REPRO:
SKIP" and exit 0 if there isn't one), boots crosvm with `--vhost-user
type=fs,socket=S` and a matching shared memory setup if needed, and
blacklists virtio_fs_init and fuse_init. The guest reads the full
device config (36 bytes: tag and num_request_queues) through the BAR,
then sets up the request queue and sends FUSE_INIT. Print "REPRO: BUG"
if FUSE_INIT doesn't complete within a second or device_status shows
NEEDS_RESET, otherwise "REPRO: OK". run.sh also greps crosvm's stderr
for "disconnected early".

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix:
1. Remember the negotiated protocol features and only forward config
   reads and writes when CONFIG is set.
2. Without CONFIG, serve config space from the frontend. For fs, crosvm
   needs the tag and num_request_queues to build struct
   virtio_fs_config. Look at how `--vhost-user type=fs` is parsed and
   whether a tag can be given (if not, add an optional `tag=` key, or
   fall back to zeroed config and log a warning once). For other
   device types, return zeros and log once rather than killing the
   connection.
3. A guest config read must never tear down the backend connection.
   If get_config fails, return zeros and keep the device running if
   the connection is still usable.

Step 3, unit tests. In the vhost_user_frontend module, use a fake
backend that doesn't offer CONFIG and check that read_config doesn't
send GET_CONFIG and returns the frontend's config. Use the existing
test backends in devices/src/virtio/vhost_user_backend if they help.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p devices passes, along with presubmit format and clippy;
- with the host's virtiofsd installed, `tools/virtio-villain/run.sh
  --linux <linux tree> F0037 F0042 F0039` passes or REJECTs and no
  "disconnected early" appears in the logs;
- a full run of the F group has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: vhost-user: ...",
body, BUG=None, TEST=).
```

### 9. virtio-9p relies on the jail to stop `..` escaping the shared directory

```
In the crosvm repository, make sure a virtio-9p guest can't walk above
the shared directory when crosvm runs with --disable-sandbox.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
PLAN90017 "Keep a parent walk inside the exported root" sends a 9P
Twalk with ".." from the root fid and checks that it stays at the
root. With `--shared-dir DIR:tag:type=p9` it fails with --disable-sandbox
("parent walk escaped the exported root") and passes in the default
sandboxed mode, because the sandbox pivot_roots the device process
into DIR.

Root cause: the 9P server comes from the external `p9` crate (0.3.1,
see Cargo.toml and devices/src/virtio/p9.rs). It resolves ".." with
openat relative to the fid's directory and relies on the jail for
confinement. --disable-sandbox is a supported option, so the shared
directory boundary leaks.

Step 1, guest reproducer. Directory: 09-p9-parent-escape. run.sh makes
a temp directory containing outside-marker and share/, with
share/link -> ../outside-marker, and boots crosvm with
`--shared-dir TMP/share:repro:type=p9`, blacklisting p9_virtio_init.
The guest speaks 9P2000.L directly on the virtio-9p queue: Tversion,
Tattach, then (a) Twalk ".." from the root fid and compare the
resulting qid with the root's qid, (b) Twalk "..", "outside-marker",
and (c) Twalk "link" followed by Tlopen. Print "REPRO: BUG <case>" if
(a) gives a different qid or (b) or (c) succeeds, otherwise "REPRO:
OK". run.sh takes --sandbox to run the same check with the default
sandbox; it reports OK there today, which confirms the jail is what
protects it now.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix (choose after looking at the code):
1. Preferred: confine walks in the server. A walk of ".." from the
   root fid must stay at the root (the 9P2000.L convention), and
   openat-based lookups should use openat2(RESOLVE_BENEATH |
   RESOLVE_NO_MAGICLINKS) or an equivalent check so symlinks and ".."
   can't leave the root. If the p9 crate source is in this tree or
   vendored, change it there; if it's only on crates.io, find its
   upstream (it's maintained by the crosvm authors) and either make
   the change there and bump the dependency, or add a wrapper in
   devices/src/virtio/p9.rs.
2. If that isn't feasible now, refuse `type=p9` shared dirs with
   --disable-sandbox (or warn loudly) and document why in the
   --shared-dir help text.
3. Check the virtio-fs `--shared-dir type=fs` server
   (devices/src/virtio/fs) for the same assumption and handle it the
   same way.

Step 3, unit tests. Create a temp directory, start the 9P server on a
subdirectory, walk "..", "../..", and a symlink pointing outside, and
check that each resolves inside the root or fails. Write it where the
server's existing tests live.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm in
  both modes, and "REPRO: BUG" without --sandbox on the old one;
- cargo test for the touched crates passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> PLAN90017` passes
  in both the default and --sandbox modes, and the PLAN9 group has no
  new failures.

Write the commit message in crosvm style ("devices: p9: ...", body,
BUG=None, TEST=).
```

### 10. virtio-snd reset with pending playback stalls the guest for more than 30 seconds

```
In the crosvm repository, find out why resetting virtio-snd with a
pending playback buffer takes so long, and fix it.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
SND0096 "Recover after reset with pending PCM playback" (virtio 1.2
2.4.2) queues a TX (playback) buffer, resets the device by writing 0 to
device_status, reinitializes it and checks that the old buffer doesn't
complete. With the default 30 second timeout it times out (FAIL) every
time. With `--timeout 120` it finishes (REJECT). The device is attached
as `--virtio-snd backend=null`. SND0061 and SND0097 (TX/RX after
PCM_STOP, reset with pending capture) sometimes fail the same way and
pass on retry. After reinitialization crosvm also logs

  ERROR devices::virtio::virtio_pci_device] pcivirtio-sound activate
    failed: snd: expected 4 queues, got 2

which may be a test or device issue on its own.

Step 1, guest reproducer. Directory: 10-snd-reset-stall. Attach only
`--virtio-snd backend=null` and blacklist the virtio-snd guest driver
(check the init function's name in the guest kernel, e.g.
virtio_snd_driver_init). Set up all four queues (control, event, TX,
RX), then send PCM_SET_PARAMS, PCM_PREPARE and PCM_START for the first
output stream. Queue one or more TX buffers of a few periods each.
Time the device_status = 0 write plus the reads until device_status
reads 0, using clock_gettime(CLOCK_MONOTONIC). Then set the device up
again and check that the old buffers don't show up in the used ring.
Print the measured time and "REPRO: BUG" if the reset took more than a
second or an old buffer completed, otherwise "REPRO: OK". Give run.sh
a generous timeout (e.g. 180 s) so the broken case still reports a
time instead of timing out.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, investigate and fix:
1. Use the reproducer and the virtio-villain test source
   (tests/sound/snd0096*.c, lib/sound_util.h in the virtio-villain
   checkout) to find where the time goes. Likely suspects: the device
   reset path in devices/device_virtio_snd/src/virtio/device.rs and
   common_backend/ waiting for PCM workers to drain or for the null
   backend to consume buffered periods at real-time speed. A vCPU MMIO
   write to device_status blocks until reset() returns, so the guest
   stalls the whole time.
2. Fix it so reset returns promptly (well under a second). Pending
   buffers are dropped without being completed, as reset requires, and
   workers are stopped without waiting for audio playback time.
3. Look at the "expected 4 queues, got 2" activation: does the snd
   device require all 4 queues to be enabled? If the test only enables
   control and TX, decide from the spec (5.14.2) whether the device
   should accept that, and either fix the device or note it as a
   virtio-villain issue.

Step 3, unit tests. Cover the reset path with a pending playback buffer
and the null backend: check that it finishes quickly (with a generous
bound, so the test isn't flaky) and that the buffer isn't completed
afterwards.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" (reset well under a
  second) with the fixed crosvm and "REPRO: BUG" with the old one;
- cargo test -p device_virtio_snd passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> --retries 0 SND0096
  SND0097 SND0061` passes or REJECTs with the default 30 s timeout,
  five runs in a row;
- the full SND group has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: snd: ...", body,
BUG=None, TEST=).
```

### 11. virtio-blk returns IOERR instead of UNSUPP for unsupported requests on read-only disks

```
In the crosvm repository, make virtio-blk return VIRTIO_BLK_S_UNSUPP
for unsupported request types on read-only disks.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
B0321 "Read-only device: unsupported types return UNSUPP" (virtio 1.2
5.2.6.2) attaches a read-only disk and sends VIRTIO_BLK_T_GET_LIFETIME
(10) and type 0x7f. Both come back as status 1 (IOERR) instead of 2
(UNSUPP):

  ERROR device_virtio_block::asynchronous] failed executing disk
    request: read only; request_type=10

Root cause: BlockAsync::execute_request in
devices/device_virtio_block/src/asynchronous.rs checks
`disk_state.read_only && req_type != VIRTIO_BLK_T_IN && req_type !=
VIRTIO_BLK_T_GET_ID` before dispatching on req_type, and
ExecuteError::ReadOnly maps to VIRTIO_BLK_S_IOERR. An unknown or
unsupported type should be UNSUPP whether or not the disk is
read-only.

Step 1, guest reproducer. Directory: 11-blk-ro-unsupp. Attach only
`--block path=disk.raw,ro=true` and blacklist virtio_blk_init. Send
VIRTIO_BLK_T_IN (expect OK), VIRTIO_BLK_T_OUT (expect IOERR),
VIRTIO_BLK_T_GET_ID (expect OK), VIRTIO_BLK_T_GET_LIFETIME and type
0x7f (expect UNSUPP for both). Print "REPRO: BUG" listing each request
whose status differs, otherwise "REPRO: OK".

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix: decide support first, then apply the read-only check only
to supported requests that write (OUT, FLUSH if relevant, DISCARD,
WRITE_ZEROES, SECURE_ERASE). Unsupported types return UNSUPP. Check
what the spec says a read-only device should return for writes (IOERR
is common; keep the current behavior for supported writes unless the
spec says otherwise). Don't log unsupported request types at ERROR
level on every request, since the guest controls them; use a
rate-limited or debug log.

Step 3, unit tests. In the block device tests (see the existing
asynchronous.rs tests), use a read-only disk with IN (OK), OUT (IOERR),
GET_ID (OK), GET_LIFETIME and 0x7f (UNSUPP).

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p device_virtio_block passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> B0321 B0057 B0300`
  passes or REJECTs;
- the full B group has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: block: ...", body,
BUG=None, TEST=).
```

### 12. virtio-blk accepts a zero-length READ at a sector past the end of the disk

```
In the crosvm repository, decide how virtio-blk should handle a
zero-length read past the end of the disk, and make it consistent.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
B0305 "Zero-length READ past capacity rejected consistently" (virtio
1.2 5.2.6.1) sends VIRTIO_BLK_T_IN with no data buffer at a sector
beyond capacity and expects an error. crosvm returns S_OK
("zero length READ past capacity returned S_OK").

Root cause: in BlockAsync::execute_request
(devices/device_virtio_block/src/asynchronous.rs) the VIRTIO_BLK_T_IN
arm returns Ok(()) when data_len == 0, before the sector range check.
OUT, DISCARD and WRITE_ZEROES may differ.

Step 0: read the spec and the virtio-villain test
(tests/blk/b0305_zero_len_read_past_capacity.c) and decide whether an
out-of-range sector should fail even with no data. The test's point is
consistency: the same sector with a nonzero length is rejected, so a
zero-length request should be too. If you disagree, write up why with
spec references, still write the reproducer (showing the current
behavior), and stop.

Step 1, guest reproducer. Directory: 12-blk-zero-len-past-end. Attach
only a 16 MiB `--block path=disk.raw` and blacklist virtio_blk_init.
Read the capacity from device config, then send zero-length IN and OUT
requests (header and status descriptors only) at sector 0,
capacity - 1, capacity, capacity + 8 and u64::MAX >> 9, plus a 512-byte
IN at capacity + 8 for comparison. Print each status. Print "REPRO:
BUG" if a zero-length request past the end returns OK while the
512-byte one fails, otherwise "REPRO: OK".

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix: run the sector range check (sector < capacity, or
sector <= capacity; pick one, document it and use it for every request
type) before the zero-length early return, for IN and OUT. Return
IOERR as the other out-of-range cases do.

Step 3, unit tests. Cover zero-length IN and OUT at sector 0,
capacity - 1, capacity and u64::MAX >> 9.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p device_virtio_block passes, along with presubmit format
  and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> B0305` passes or
  REJECTs and the full B group has no new FAIL or WEDGED.

Write the commit message in crosvm style ("devices: block: ...", body,
BUG=None, TEST=).
```

### 13. Opening a persistent multiqueue tap needs CAP_NET_ADMIN

```
In the crosvm repository, let an unprivileged crosvm use a persistent,
user-owned multiqueue tap device with `--net tap-name=...,vq-pairs=N`.

Context: tools/virtio-villain/run.sh creates taps with
`sudo ip tuntap add NAME mode tap multi_queue user $USER` and
`ip link set NAME up`, then runs crosvm unprivileged with
`--net tap-name=NAME,vq-pairs=4`. crosvm fails:

  failed to set up virtio networking
  failed to open tap device: ioctl failed: Operation not permitted

so run.sh has to setcap cap_net_admin on a copy of the binary.

Root cause: Tap::into_mq_taps in net_util/src/sys/linux/tap.rs calls
tap.enable() (SIOCSIFFLAGS IFF_UP|IFF_RUNNING) for every additional
queue it opens. SIOCSIFFLAGS needs CAP_NET_ADMIN even when the
interface is already up. Opening the extra queues with TUNSETIFF works
without privileges for a tap owned by the user.

Step 1, reproducer. Directory: 13-mq-tap-cap-net-admin. This bug is on
the host side, so the guest part is trivial: repro.c just prints
"REPRO: OK" and powers off, which proves the VM started. run.sh creates
and brings up a user-owned multiqueue tap with sudo (removed on exit),
then runs crosvm unprivileged (no setcap, not as root) with `--net
tap-name=NAME,vq-pairs=2`. It exits 0 only if the guest prints "REPRO:
OK". If crosvm exits with "Operation not permitted", it prints "REPRO:
BUG" and the error. Include a vq-pairs=1 run as a control; it works
today.

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix: in into_mq_taps (and anywhere else that enables a tap the
user named), skip SIOCSIFFLAGS when the interface already has IFF_UP
set (read it with SIOCGIFFLAGS, which needs no privileges), or attempt
it and ignore EPERM when the interface is already up. Keep the current
behavior for taps crosvm creates itself (host-ip/netmask/mac mode).
Check devices/device_virtio_net/src/sys/linux.rs and vhost.rs for
other enable() calls on user-supplied taps. Then drop the setcap copy
from tools/virtio-villain/run.sh and README.md, and from the
reproducers for TODO items 6 and 7.

Step 3, unit tests. Use net_util's FakeTap if it can model an interface
that is already up and an enable() that fails with EPERM. Check that
into_mq_taps succeeds for an up interface and still reports errors for
a down one.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p net_util passes, along with presubmit format and clippy;
- tools/virtio-villain/run.sh runs the N group without the setcap copy.

Write the commit message in crosvm style ("net_util: ...", body,
BUG=None, TEST=).
```

### 14. [LOW] queue_notify_off reads back queue_select for nonexistent queues

```
In the crosvm repository, return a safe queue_notify_off when
queue_select points to a queue the device doesn't have.

Context: virtio-villain is a hostile guest test suite. Its crosvm
runner is tools/virtio-villain/run.sh (see the README.md next to it).
PCI0009 "Select queue index >= num_queues then configure it" and
PCI0012 "Notify with queue index 0xFFFF" (virtio 1.2 4.1.4.3.1,
4.1.4.9.2) select a nonexistent queue (num_queues + 10 or 0xffff) and
read queue_notify_off. crosvm returns queue_select itself (0xffff), the
test computes notify_base + 0xffff * notify_off_multiplier, which is
far outside the notification BAR region, and the guest test segfaults
(virtio-villain reports WEDGED). The test should bounds-check (see the
virtio-villain item below), but the device shouldn't hand out
out-of-range offsets either.

Root cause: VirtioPciCommonConfig::read_common_config_word in
devices/src/virtio/virtio_pci_common_config.rs maps offset 0x1e to
`self.queue_select` with no queue check, unlike the size, vector and
enable fields next to it, which go through with_queue(...).

Step 1, guest reproducer. Directory: 14-notify-off-nonexistent-queue.
Attach only a virtio-blk device and blacklist virtio_blk_init. Read
num_queues and the notify capability's length and
notify_off_multiplier. Set queue_select to num_queues, num_queues + 10
and 0xffff, and read queue_size and queue_notify_off for each. Don't
write to any computed address. Print "REPRO: BUG" if any nonexistent
queue reports queue_size 0 but a notify offset whose address
(notify_off * multiplier) falls outside the notify capability's
length, otherwise "REPRO: OK".

Reproducer requirements. Write the reproducer before changing crosvm,
in the separate repository at ~/src/crosvm-guest-repros, not in crosvm.
Create it with `git init` if it doesn't exist. If it does, read its
README.md and follow its conventions, reusing any shared helpers
already there instead of copying them again. The directory named
above holds:
- repro.c: one static C program (gcc -static, libc only) that runs as
  /init in the guest. It drives the device directly from guest
  userspace the way virtio-villain does: find the device under
  /sys/bus/pci/devices by vendor 0x1af4 and device ID, walk its
  capabilities through the sysfs config file, mmap the BAR through
  resourceN, and put rings and buffers in mlock()ed memory whose
  physical addresses come from /proc/self/pagemap. Copying small
  helpers from virtio-villain (Apache-2.0, the checkout that
  tools/virtio-villain/run.sh uses) is fine if you keep the license
  header and note where they came from. Keep it to the minimum that
  shows the bug.
- run.sh: builds repro.c, packs it as /init into an initramfs (cpio -o
  -H newc | gzip), and boots `crosvm run --disable-sandbox` (unless the
  item says otherwise) with only the devices the repro needs.
  It takes the crosvm binary from $CROSVM and the guest kernel from
  $KERNEL and prints usage if either is unset. The bzImage that
  `tools/virtio-villain/run.sh --linux <linux tree>` builds at
  .tmp/virtio-villain/kernel/images/bzImage-x86_64 in the crosvm tree
  works. Pass console=ttyS0 and initcall_blacklist=<the guest driver's
  init function> so the kernel's driver doesn't claim the device. The
  guest prints exactly one of "REPRO: OK" or "REPRO: BUG <reason>" and
  powers off. run.sh exits 0 only if it sees "REPRO: OK". A crosvm
  crash, a timeout (wrap crosvm in `timeout`) or "REPRO: BUG" makes it
  exit 1 and print the reason and the tail of crosvm's stderr.
- README.md: what the bug is, the exact command, and the output on
  broken and fixed crosvm.
Run it against unpatched crosvm and confirm it shows the bug before
you start the fix. Commit it in ~/src/crosvm-guest-repros with a
message naming this TODO item.

Step 2, fix: return queue_select only for existing queues and 0
otherwise (queue_size already reads 0 for them, which tells the driver
the queue doesn't exist). Keep the notify offset computation in
virtio_pci_device.rs consistent with it.

Step 3, unit tests. Extend the tests in virtio_pci_common_config.rs to
read queue_notify_off with queue_select = num_queues and 0xffff, and
for an existing queue.

Unit test rules. Write the fix first and add the unit tests afterwards.
Don't change any interface just to make the code testable: no new or
changed function signatures, no visibility changes (pub, pub(crate)),
and no struct fields, trait methods, constructors, parameters or
feature flags that only tests use. A #[cfg(test)] mod tests in the
same file can reach private items, so that doesn't count as an
interface change. Interface changes the fix itself needs are fine;
this rule only covers changes made for tests. If a test you think is
needed can't be written against the existing interfaces, stop,
describe the change you want and why, and wait for approval before
making it.

Done when:
- the reproducer's run.sh prints "REPRO: OK" with the fixed crosvm and
  "REPRO: BUG" with the old one;
- cargo test -p devices passes, along with presubmit format and clippy;
- `tools/virtio-villain/run.sh --linux <linux tree> PCI0009 PCI0012`
  no longer segfaults the guest (check logs/<TEST>.log for
  "segfault").

Write the commit message in crosvm style ("devices: virtio: pci: ...",
body, BUG=None, TEST=).
```

---

## virtio-villain and its crosvm backend

These belong in the virtio-villain repository
(https://github.com/weltling/virtio-villain), not crosvm. They don't
get separate guest reproducers: item 15 is tooling, and in item 16 the
failing virtio-villain tests are the reproducers.

### 15. Upstream the crosvm backend

```
In a virtio-villain checkout at 94b85dd1 or later, turn the crosvm
backend from crosvm's tools/virtio-villain/crosvm-backend.patch into an
upstreamable change.

Context: the patch adds a `Crosvm` VmmBackend in
lib/python/villain/vmm.py (detected when the binary name contains
"crosvm"), gives crosvm a share directory for virtio-fs tests when no
virtiofsd is present, and defaults crosvm to 4 vCPUs in `run`. It is
configured through environment variables: VV_CROSVM_TAPS (comma
separated persistent tap names, one per runner thread) and
VV_CROSVM_SANDBOX=1 (don't pass --disable-sandbox).

Do:
1. Apply the patch and read the other backends (CloudHypervisor, Qemu,
   OpenVmm) and docs (README.md "Target VMMs", docs/) to match their
   conventions.
2. Replace the environment variables with proper `run` options (for
   example --crosvm-taps and --crosvm-sandbox) or with whatever
   mechanism the other backends use for host resources. Consider
   having the runner create and delete taps itself when it has
   CAP_NET_ADMIN, as Cloud Hypervisor does with `tap=`.
3. Remove the duplicated `return cmd` in OpenVmm.build_cmd that the
   patch context shows (pre-existing).
4. Add crosvm to README.md (target VMMs, quick start, which device
   families are unsupported: scsi, crypto, mem, watchdog, rtc,
   standalone iommu, mmio).
5. Make sure `run --skip-unsupported` works with crosvm (it probes by
   booting with vv.test=probe).
6. Write the code first, then add selftests for the crosvm command
   builder like the existing ones under selftest/. Don't change
   existing interfaces (function signatures, the VmmBackend API,
   option names other backends use) to make the code testable unless
   approved: if a test needs such a change, stop, describe it and why,
   and wait for approval.

Done when the selftests pass, `./run -m /path/to/crosvm` runs the suite
with no environment variables, and the change is ready to send upstream.
```

### 16. Fix virtio-villain test and harness bugs seen with crosvm

```
In a virtio-villain checkout at 94b85dd1 or later, fix these test and
harness bugs found while running against crosvm. Each is independent;
make a separate commit for each. For each one, make the fix first and
then add a selftest where the harness has them (Python code under
lib/python and the runner). Don't change existing interfaces
(function signatures, test registration macros, helper APIs other
tests use) to make the code testable unless approved: if a test needs
such a change, stop, describe it and why, and wait for approval.

1. SND0074 and SND0075 read the PCM status from the wrong place. Both
   submit an RX (capture) transfer with one 1032-byte device-writable
   buffer and read the status at offset 0
   (snd_wait_pcm_safe(..., (struct virtio_snd_pcm_status *)response,
   ...)). For RX, virtio 1.2 5.14.6.8 puts the status after the data:
   struct { u8 data[]; struct virtio_snd_pcm_status status; }. crosvm
   correctly fast-forwards the writer and writes the status at offset
   1024, so the tests see the 0xff fill and report "invalid PCM transfer
   status 0xffffffff". Read the status at used_len - sizeof(status) (or
   at the end of the writable buffer), checking used_len first.
   Check other RX tests that use snd_wait_pcm_safe the same way.

2. PCI0009 and PCI0012 compute a notify address from queue_notify_off
   for a nonexistent queue and write to it without bounds-checking
   against the notification capability length or the mapped BAR size.
   With crosvm, queue_notify_off reads 0xffff and the guest init
   segfaults ("init[1]: segfault at ... error 6" in the logs), which
   kills the guest and reports WEDGED. Check the computed offset
   against the notify capability's length (and the BAR mapping) before
   writing; if it's out of range, record a REJECT or FAIL with a clear
   message instead of crashing. Find the helper that computes notify
   addresses (lib/virtio_pci.c) and add the check there so every test
   benefits.

3. Security triage mislabels a crashed sandboxed device process as
   CRITICAL. crosvm's panic hook ends a sandboxed device process with
   SIGSEGV after printing "thread '...' panicked at ...". The triage
   (lib/python/villain/diagnostics.py security_triage) sees
   "Segmentation fault" and labels it "guest controlled memory
   corruption". When a Rust panic message precedes the signal, label
   it HIGH (VMM or device panic) instead. Add a selftest with both log
   shapes.

4. The plan9 tests don't build with -Werror on GCC 11.5 (RHEL 9):
   tests/plan9/plan90017_walk_parent.c and plan90018_walk_slash.c fail
   with "'tag' may be used uninitialized" (lib/plan9_util.h:148).
   Initialize `tag` (or restructure so GCC can see it's set) so `make
   initramfs` works with the default CFLAGS.

Done when `make initramfs` builds with the default flags, the selftests
pass, and with crosvm SND0074/SND0075 report REJECT and PCI0009/PCI0012
no longer segfault the guest.
```
