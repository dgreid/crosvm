// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// A qcow file. Allows reading/writing clusters and allocating and freeing clusters.
pub struct QcowRaw {
    file: File,
    cluster_size: u64,
    cluster_mask: u64,
}

impl QcowRaw {
    pub fn from(file: File, cluster_size: u64, cluster_mask: u64) -> Self {
        QcowRaw {
            file,
            cluster_size,
            cluster_mask,
        }
    }

    /// Reads the L1 table from the file and returns a Vec containing the table.
    pub fn read_pointer_table(
        &mut self,
        offset: u64,
        size: usize,
        mask: Option<u64>,
    ) -> std::io::Result<Vec<u64>> {
        let mut table = Vec::with_capacity(size as usize);
        self.file.seek(SeekFrom::Start(offset))?;
        let mask = mask.unwrap_or(0xffff_ffff_ffff_ffff);
        for _ in 0..size {
            table.push(self.file.read_u64::<BigEndian>()? & mask);
        }
        Ok(table)
    }

    /// Writes the L1 table to `file`. Non-zero table entries have `non_zero_flags` ORed before
    /// writing.
    pub fn write_pointer_table(
        &mut self,
        offset: u64,
        table: &Vec<u64>,
        non_zero_flags: u64,
    ) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        for addr in table {
            let val = if *addr == 0 {
                0
            } else {
                *addr | non_zero_flags
            };
            self.file.write_u64::<BigEndian>(val)?;
        }
        Ok(())
    }

    /// Read a refcount block from the file and returns a Vec containing the table.
    fn read_refcount_block(
        &mut self,
        offset: u64,
        size: usize,
    ) -> std::io::Result<VecCache<u16>> {
        let mut table = Vec::with_capacity(size as usize);
        self.file.seek(SeekFrom::Start(offset))?;
        for _ in 0..size {
            table.push(self.file.read_u16::<BigEndian>()?);
        }
        Ok(VecCache::from_vec(table))
    }

    /// Writes a refcount block to the file.
    fn write_refcount_block(
        &mut self,
        offset: u64,
        table: &VecCache<u16>,
    ) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        for count in table.addrs() {
            self.file.write_u16::<BigEndian>(*count)?;
        }
        Ok(())
    }

    /// Allocates a new cluster at the end of the current file, return the address.
    fn add_cluster_end(&mut self) -> std::io::Result<u64>
    {
        // Determine where the new end of the file should be and set_len, which
        // translates to truncate(2).
        let file_end: u64 = file.seek(SeekFrom::End(0))?;
        let cluster_size: u64 = cluster_size;
        let new_cluster_address: u64 = (file_end + cluster_size - 1) & !cluster_mask;
        file.set_len(new_cluster_address + cluster_size)?;

        Ok(new_cluster_address)
    }
}
