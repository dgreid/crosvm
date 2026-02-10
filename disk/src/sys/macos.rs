// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use cros_async::Executor;

use crate::DiskFileParams;
use crate::Error;
use crate::Result;
use crate::SingleFileDisk;

pub fn open_raw_disk_image(params: &DiskFileParams) -> Result<File> {
    let mut options = File::options();
    options.read(true).write(!params.is_read_only);

    let raw_image = base::open_file_or_duplicate(&params.path, &options)
        .map_err(|e| Error::OpenFile(params.path.display().to_string(), e))?;

    if params.lock {
        // Lock the disk image to prevent other crosvm instances from using it.
        let lock_op = if params.is_read_only {
            base::FlockOperation::LockShared
        } else {
            base::FlockOperation::LockExclusive
        };
        base::flock(&raw_image, lock_op, true).map_err(Error::LockFileFailure)?;
    }

    // O_DIRECT is not supported on macOS - silently ignore
    // macOS doesn't support O_DIRECT, but we can hint about caching via F_NOCACHE
    // For now, just ignore this flag
    let _ = params.is_direct;

    Ok(raw_image)
}

pub fn apply_raw_disk_file_options(_raw_image: &File, _is_sparse_file: bool) -> Result<()> {
    // No op on unix/macOS.
    Ok(())
}

pub fn read_from_disk(
    mut file: &File,
    offset: u64,
    buf: &mut [u8],
    _overlapped_mode: bool,
) -> Result<()> {
    file.seek(SeekFrom::Start(offset))
        .map_err(Error::SeekingFile)?;
    file.read_exact(buf).map_err(Error::ReadingHeader)
}

impl SingleFileDisk {
    pub fn new(disk: File, ex: &Executor) -> Result<Self> {
        ex.async_from(disk)
            .map_err(Error::CreateSingleFileDisk)
            .map(|inner| SingleFileDisk { inner })
    }
}
