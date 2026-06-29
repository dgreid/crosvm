# macOS Block Device Debugging State

## Date: 2026-02-02 - UPDATED

## Summary
We are debugging virtio-blk block device support on macOS. The block device is detected, I/O requests are processed correctly, and interrupts are being injected into the GIC, but the guest never receives/processes them.

## Current Status

### What Works
1. ✅ Block device detection: `virtio_blk virtio0: [vda] 126216 512-byte logical blocks`
2. ✅ QUEUE_NOTIFY handling: Guest writes are received and signaled to block worker
3. ✅ Queue event delivery: Block worker receives events via EventAsync
4. ✅ Request processing: `block: handle_queue processed N descriptors`
5. ✅ Interrupt triggering: `block: completed request, triggered_interrupt=true`
6. ✅ IRQ event signaling: Event pipes work correctly between threads
7. ✅ GIC SPI injection: `hv_gic_set_spi(33, true)` succeeds
8. ✅ VCPU kicking: `hv_vcpus_exit()` succeeds

### What Doesn't Work
- ❌ Guest interrupt reception: The guest never sees the interrupt
- ❌ Kernel boot progression: Guest hangs after first I/O batch waiting for interrupt

## Root Cause Analysis

### Initial Issues (FIXED)
1. **VCPU polling deadlock (FIXED):** VCPU loop was blocked in `vcpu.run()` and couldn't poll for interrupts. Solution: Created dedicated IRQ handler thread that monitors interrupt events and injects SPIs.

2. **Event clone confusion (VERIFIED WORKING):** Cross-clone Event signaling works correctly - all clones share the same underlying pipe.

### Current Issue (INVESTIGATING)
The HVF in-kernel GIC (`hv_gic_set_spi()`) accepts SPI injection and `hv_vcpus_exit()` kicks the VCPU, but the guest never receives the interrupt.

Possible causes:
1. **GIC configuration mismatch:** The GIC may need additional initialization or the interrupt may need to be configured differently (edge vs level, active-high vs active-low)
2. **VCPU interrupt masking:** Guest PSTATE may still have interrupts masked (though kernel should unmask them during boot)
3. **HVF GIC implementation:** HVF's in-kernel GIC may not support the SPI injection pattern we're using
4. **Missing GIC ICC registers:** HVF may require system register traps for certain GIC ICC registers that we're not handling
5. **Interrupt priority/preemption:** The interrupt priority may be set incorrectly

## Files Modified (Uncommitted)

### 1. `devices/src/virtio/virtio_mmio_device.rs`
- Added QUEUE_NOTIFY handling for platforms without ioeventfd
- Added debug logging for assign_irq and activate

### 2. `devices/src/virtio/interrupt.rs`
- Added debug logging for MMIO interrupt trigger path

### 3. `devices/src/virtio/block/asynchronous.rs`
- Added debug logging for handle_queue and trigger_interrupt

### 4. `src/crosvm/sys/macos.rs`
- Added IRQ handler thread that monitors interrupt events and injects SPIs via `hv_gic_set_spi()`
- Added VcpuCoordinator.kick_boot_vcpu() to force VCPU exit via `hv_vcpus_exit()`
- Changed virtio device FDT interrupt type from edge to level-triggered
- Added extensive debug logging throughout

### 5. `cros_async/src/sys/macos/` (various files)
- Fixed event handling for macOS pipes

## Architecture Changes

### IRQ Handling Flow (New)
1. Block worker completes request → calls `interrupt.signal_used_queue()`
2. MMIO Interrupt triggers pipe-based IrqEdgeEvent
3. **NEW:** Dedicated IRQ handler thread monitors events with 10ms timeout poll
4. When signaled, thread calls `hv_gic_set_spi(intid, true)` to inject SPI
5. Thread calls `hv_vcpus_exit()` to kick VCPU out of `hv_vcpu_run()`
6. **PROBLEM:** Guest never receives the interrupt

## Next Steps to Investigate

1. **Verify GIC ICC registers:** Check if HVF requires handling ICC_* system register traps
2. **Check interrupt priority:** Verify the SPI priority is set correctly in HVF's GIC
3. **Test with simpler interrupt:** Try triggering a timer interrupt to verify GIC is working at all
4. **Examine HVF GIC documentation:** Look for Apple documentation on hv_gic_set_spi behavior
5. **Try hv_gic_state():** Check if there's a way to query the current GIC state to see if the SPI is pending
6. **Consider IRQ line vs SPI:** Verify INTID calculation (SPIs start at 32, so SPI 1 = INTID 33)

## Test Command
```bash
cargo build --release
codesign --sign - --entitlements crosvm.entitlements --force target/release/crosvm
RUST_LOG=info ./target/release/crosvm run -m 512 --disk rootfs.sqsh -p "keep_bootcon panic=5" arm64_Image
```

## Related Files for Reference
- Plan file: `/Users/dgreid/.claude/plans/goofy-roaming-whistle.md`
- Test logs: `/tmp/blk_debug*.log`
- Kernel image: `arm64_Image`
- Disk image: `rootfs.sqsh` (downloaded from crosvm GCS)

## Commits Made
1. `7854fb76e` - hypervisor: fix system register trap handling in HVF
2. (Need to commit) - MMIO config space access width fix (allows 1/2/4 byte reads to config space per virtio spec 4.2.2.2)
