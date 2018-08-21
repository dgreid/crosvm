// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io;

pub struct RefCount {
    ref_table: Vec<u64>,
    refblock_cache: HashMap<usize, VecCache<u16>>,
    refcount_block_entries: u64, // number of refcounts in a cluster.
}

impl RefCount {
    /// Creates a `RefCount` from `file`, reading the refcount table from `refcount_table_offset`.
    /// `refcount_table_entries` specifies the number of refcount blocks used by this image.
    /// `refcount_block_entries` indicates the number of refcounts in each refcount block.
    /// Each refcount table entry points to a refcount block.
    pub fn new(
        raw_file: &mut QcowRawFile,
        refcount_table_offset: u64,
        refcount_table_entries: u64,
        refcount_block_entries: u64,
    ) -> io::Result<RefCount> {
        let ref_table = raw_file.read_pointer_table(
            refcount_table_offset,
            refcount_table_entries,
            None,
        )?;
        Ok(RefCount {
            ref_table,
            refblock_cache: HashMap::with_capacity(50),
            refcount_block_entries,
        })
    }

    // Check if the refblock cache is full and we need to evict.
    fn check_evict(&mut self, raw_file: &mut QcowRawFile, except: usize) -> io::Result<()> {
        // TODO(dgreid) - smarter eviction strategy.
        if self.refblock_cache.len() == self.refblock_cache.capacity() {
            let mut to_evict = 0;
            for k in self.refblock_cache.keys() {
                if *k != except { // Don't remove the one we just added.
                    to_evict = *k;
                    break;
                }
            }
            if let Some(evicted) = self.refblock_cache.remove(&to_evict) {
                if evicted.dirty() {
                    raw_file.write_refblock_block(self.rec_table[evicted_idx], evicted.addrs())?;
                }
            }
        }
        Ok(())
    }
}
