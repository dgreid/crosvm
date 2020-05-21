// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Extension functions to asynchronously access files.

use crate::io_source::IoSource;

/// Extends IoSource with ergonomic methods to perform asynchronous IO.
pub trait IoSourceExt: IoSource {
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
}

impl<T: IoSource + ?Sized> IoSourceExt for T {}
