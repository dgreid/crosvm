// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::CStr;
use std::io;
use std::os::unix::io::AsRawFd;

use fuse::filesystem::DirEntry;
use fuse::filesystem::DirectoryIterator;

/// Directory iterator for macOS using `fdopendir`/`readdir`/`seekdir`.
pub struct ReadDir {
    dirp: *mut libc::DIR,
}

// SAFETY: ReadDir holds a DIR* which is only accessed by &mut self methods.
unsafe impl Send for ReadDir {}

impl ReadDir {
    /// Create a new `ReadDir` from an open directory fd at the given offset.
    ///
    /// The fd is `dup()`d internally; the caller retains ownership of the original.
    pub fn new<D: AsRawFd>(dir: &D, offset: i64, _buf_size: usize) -> io::Result<Self> {
        // dup the fd because fdopendir takes ownership.
        // SAFETY: dup is safe and we check the return value.
        let dup_fd = unsafe { libc::dup(dir.as_raw_fd()) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: fdopendir takes ownership of the dup'd fd.
        let dirp = unsafe { libc::fdopendir(dup_fd) };
        if dirp.is_null() {
            // fdopendir failed; close the dup'd fd.
            unsafe { libc::close(dup_fd) };
            return Err(io::Error::last_os_error());
        }

        // Seek to the requested position.
        if offset != 0 {
            unsafe { libc::seekdir(dirp, offset as libc::c_long) };
        }

        Ok(ReadDir {
            dirp,
        })
    }
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        if !self.dirp.is_null() {
            // SAFETY: we own the DIR*.
            unsafe { libc::closedir(self.dirp) };
        }
    }
}

impl DirectoryIterator for ReadDir {
    fn next(&mut self) -> Option<DirEntry> {
        if self.dirp.is_null() {
            return None;
        }

        // Clear errno before calling readdir so we can distinguish EOF from error.
        unsafe { *libc::__error() = 0 };

        // SAFETY: dirp is valid, readdir returns a pointer to a static buffer.
        let ent = unsafe { libc::readdir(self.dirp) };
        if ent.is_null() {
            return None;
        }

        // SAFETY: ent is valid, we read the fields.
        let ent = unsafe { &*ent };

        // On macOS, d_seekoff gives us the seek position for this entry.
        // We use telldir to get the offset *after* this entry for the FUSE protocol.
        let offset = unsafe { libc::telldir(self.dirp) } as u64;

        // d_name is a fixed-size array; find the nul terminator.
        let name_ptr = ent.d_name.as_ptr();
        // SAFETY: d_name is always nul-terminated by the OS, d_namlen gives us the length.
        let name = unsafe { CStr::from_ptr(name_ptr) };

        Some(DirEntry {
            ino: ent.d_ino,
            offset,
            type_: ent.d_type as u32,
            name,
        })
    }
}
