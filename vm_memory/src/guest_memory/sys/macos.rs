// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::MappedRegion;
use base::SharedMemory;
use base::VolatileMemory;
use bitflags::bitflags;

use crate::FileBackedMappingParameters;
use crate::GuestMemory;
use crate::MemoryRegion;
use crate::Result;

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    #[repr(transparent)]
    pub struct MemoryPolicy: u32 {
        const USE_HUGEPAGES = 1;
        const LOCK_GUEST_MEMORY = (1 << 1);
        const USE_PUNCHHOLE_LOCKED = (1 << 2);
    }
}

pub(crate) fn finalize_shm(_shm: &mut SharedMemory) -> Result<()> {
    // macOS shared memory doesn't support sealing like Linux memfd.
    // The memory is already set up correctly at creation time.
    Ok(())
}

impl GuestMemory {
    /// Handles guest memory policy hints/advices.
    pub fn set_memory_policy(&mut self, _mem_policy: MemoryPolicy) {
        // macOS doesn't support huge pages in the same way as Linux.
        // These are optimizations, not requirements.
    }
}

impl FileBackedMappingParameters {
    pub fn open(&self) -> std::io::Result<std::fs::File> {
        // macOS is missing an equivalent to `punch_holes_in_guest_mem_layout_for_mappings`, so
        // in practice this should be unreachable.
        unimplemented!("file-backed memory mappings are not supported on macOS")
    }
}

impl MemoryRegion {
    /// Finds ranges of memory that might have non-zero data (i.e. not unallocated memory). The
    /// ranges are offsets into the region's mmap, not offsets into the backing file.
    ///
    /// For example, if there were three bytes and the second byte was a hole, the return would be
    /// `[1..2]` (in practice these are probably always at least page sized).
    pub(crate) fn find_data_ranges(&self) -> anyhow::Result<Vec<std::ops::Range<usize>>> {
        Ok(vec![0..self.mapping.size()])
    }

    pub(crate) fn zero_range(&self, offset: usize, size: usize) -> anyhow::Result<()> {
        self.mapping.get_slice(offset, size)?.write_bytes(0);
        Ok(())
    }
}
