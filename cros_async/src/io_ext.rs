// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Extension functions to asynchronously access files.

use std::rc::Rc;

use crate::io_source::IoSource;
use crate::uring_mem::{BackingMemory, MemVec};

/// Extends IoSource with ergonomic methods to perform asynchronous IO.
pub trait IoSourceExt: IoSource {
    /// read from the iosource at `file_offset` and fill the given `vec`.
    fn read_to_vec<'a>(
        &'a self,
        file_offset: u64,
        vec: Vec<u8>,
    ) -> crate::read_vec::ReadVec<'a, Self>
    where
        Self: Unpin,
    {
        crate::read_vec::ReadVec::new(self, file_offset, vec)
    }

    /// write from the given `vec` to the file starting at `file_offset`.
    fn write_from_vec<'a>(
        &'a self,
        file_offset: u64,
        vec: Vec<u8>,
    ) -> crate::write_vec::WriteVec<'a, Self>
    where
        Self: Unpin,
    {
        crate::write_vec::WriteVec::new(self, file_offset, vec)
    }

    /// Reads to the given `mem` at the given offsets from the file starting at `file_offset`.
    fn read_to_mem<'a>(
        &'a self,
        file_offset: u64,
        mem: Rc<dyn BackingMemory>,
        mem_offsets: &'a [MemVec],
    ) -> crate::read_mem::ReadMem<'a, Self>
    where
        Self: Unpin,
    {
        crate::read_mem::ReadMem::new(self, file_offset, mem, mem_offsets)
    }

    /// Writes from the given `mem` from the given offsets to the file starting at `file_offset`.
    fn write_from_mem<'a>(
        &'a self,
        file_offset: u64,
        mem: Rc<dyn BackingMemory>,
        mem_offsets: &'a [MemVec],
    ) -> crate::write_mem::WriteMem<'a, Self>
    where
        Self: Unpin,
    {
        crate::write_mem::WriteMem::new(self, file_offset, mem, mem_offsets)
    }
}

impl<T: IoSource + ?Sized> IoSourceExt for T {}
