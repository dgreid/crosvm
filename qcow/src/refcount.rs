// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io;

use libc::EINVAL;

use vec_cache::{Cacheable, VecCache};
use qcow_raw_file::QcowRawFile;

#[derive(Debug)]
pub enum Error {
    /// `EvictingCache` - Error writing a refblock from the cache to disk.
    EvictingRefCounts(io::Error),
    /// `InvalidIndex` - Address requested isn't within the range of the disk.
    InvalidIndex,
    /// `NeedNewCluster` - Handle this error by allocating a cluster and calling the function again.
    NeedNewCluster(u64),
    /// `ReadingRefCounts` - Error reading the file in to the refcount cache.
    ReadingRefCounts(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct RefCount {
    ref_table: Vec<u64>,
    refcount_table_offset: u64,
    refblock_cache: HashMap<usize, VecCache<u16>>,
    refcount_block_entries: u64, // number of refcounts in a cluster.
    cluster_size: u64,
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
        cluster_size: u64,
    ) -> io::Result<RefCount> {
        let ref_table =
            raw_file.read_pointer_table(refcount_table_offset, refcount_table_entries, None)?;
        Ok(RefCount {
            ref_table,
            refcount_table_offset,
            refblock_cache: HashMap::with_capacity(50),
            refcount_block_entries,
            cluster_size,
        })
    }

    /// Returns the number of refcounts per block.
    pub fn refcounts_per_block(&self) -> u64 {
        self.refcount_block_entries
    }

    // Check if the refblock cache is full and we need to evict.
    fn check_evict(&mut self, raw_file: &mut QcowRawFile, except: usize) -> io::Result<()> {
        // TODO(dgreid) - smarter eviction strategy.
        if self.refblock_cache.len() == self.refblock_cache.capacity() {
            let mut to_evict = 0;
            for k in self.refblock_cache.keys() {
                if *k != except {
                    // Don't remove the one we just added.
                    to_evict = *k;
                    break;
                }
            }
            if let Some(evicted) = self.refblock_cache.remove(&to_evict) {
                if evicted.dirty() {
                    raw_file.write_refcount_block(self.ref_table[to_evict], evicted.addrs())?;
                }
            }
        }
        Ok(())
    }

    // Gets the address of the refcount block and the index into the block for the given address.
    fn get_refcount_index(&self, address: u64) -> (usize, usize) {
        let block_index = (address / self.cluster_size) % self.refcount_block_entries;
        let refcount_table_index = (address / self.cluster_size) / self.refcount_block_entries;
        (refcount_table_index as usize, block_index as usize)
    }

    /// Returns `NewClusterNeeded` if a new cluster needs to be allocated for refcounts. The Caller
    /// should allocate a cluster and call this function again with the cluster.
    /// On success, an optional address of a dropped cluster is returned. The dropped cluster can be
    /// reused for other purposes.
    pub fn set_cluster_refcount(
        &mut self,
        cluster_address: u64,
        refcount: u16,
        mut new_cluster: Option<(u64, VecCache<u16>)>,
    ) -> Result<Option<u64>> {
        let (table_index, block_index) = self.get_refcount_index(cluster_address);

        let block_addr_disk = *self.ref_table.get(table_index).ok_or(Error::InvalidIndex)?;

        match self.refblock_cache.entry(table_index) {
            Entry::Occupied(_) => (),
            Entry::Vacant(e) => {
                if block_addr_disk == 0 {
                    // Need a new cluster
                    if let Some((addr, table)) = new_cluster.take() {
                        self.ref_table[table_index] = addr;
                        e.insert(table);
                    } else {
                        return Err(Error::NeedNewCluster(block_addr_disk));
                    }
                }
            }
        }

        // Unwrap is safe here as the entry was filled directly above.
        let dropped_cluster = if !self.refblock_cache.get(&table_index).unwrap().dirty() {
            // Free the previously used block and use a new one. Writing modified counts to new
            // blocks keeps the on-disk state consistent even if it's out of date.
            if let Some((addr, _)) = new_cluster.take() {
                self.ref_table[table_index] = addr;
                Some(block_addr_disk)
            } else {
                return Err(Error::NeedNewCluster(block_addr_disk));
            }
        } else {
            None
        };

        self.refblock_cache
            .get_mut(&table_index)
            .unwrap()
            .set(block_index, refcount);
        Ok(dropped_cluster)
    }

    pub fn flush_blocks(&mut self, file: &mut QcowRawFile) -> io::Result<()> {
        // Write out all dirty L2 tables.
        for (table_index, block) in self.refblock_cache.iter_mut().filter(|(_k, v)| v.dirty()) {
            // The index must be from valid when we insterted it.
            let addr = self.ref_table[*table_index];
            if addr != 0 {
                file.write_refcount_block(addr, block.addrs())?;
            } else {
                return Err(std::io::Error::from_raw_os_error(EINVAL));
            }
            block.mark_clean();
        }
        Ok(())
    }

    pub fn flush_table(&mut self, file: &mut QcowRawFile) -> io::Result<()> {
        file.write_pointer_table(self.refcount_table_offset, &self.ref_table, 0)
    }

    // Gets the refcount for a cluster with the given address.
    pub fn get_cluster_refcount(
        &mut self,
        file: &mut QcowRawFile,
        address: u64,
    ) -> Result<u16> {
        let (table_index, block_index) = self.get_refcount_index(address);
        let block_addr_disk = *self.ref_table.get(table_index).ok_or(Error::InvalidIndex)?;
        if block_addr_disk == 0 {
            return Ok(0);
        }
        let refcount = match self.refblock_cache.entry(table_index) {
            Entry::Vacant(e) => {
                let table = VecCache::from_vec(
                    file.read_refcount_block(block_addr_disk)
                        .map_err(Error::ReadingRefCounts)?,
                );
                e.insert(table).get(block_index)
            }
            Entry::Occupied(e) => e.get().get(block_index),
        };
        self.check_evict(file, table_index).map_err(Error::EvictingRefCounts)?;
        Ok(refcount)
    }
}
