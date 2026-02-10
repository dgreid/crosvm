// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use base::handle_eintr_errno;
use base::AsRawDescriptor;
use base::VolatileSlice;
use remain::sorted;
use thiserror::Error as ThisError;

use super::kqueue_executor;
use super::kqueue_executor::KqueueReactor;
use super::kqueue_executor::RegisteredSource;
use crate::common_executor::RawExecutor;
use crate::mem::BackingMemory;
use crate::AsyncError;
use crate::AsyncResult;
use crate::MemRegion;

#[sorted]
#[derive(ThisError, Debug)]
pub enum Error {
    /// An error occurred attempting to register a waker with the executor.
    #[error("An error occurred attempting to register a waker with the executor: {0}.")]
    AddingWaker(kqueue_executor::Error),
    /// Failed to discard a block
    #[error("Failed to discard a block: {0}")]
    Discard(base::Error),
    /// An executor error occurred.
    #[error("An executor error occurred: {0}")]
    Executor(kqueue_executor::Error),
    /// An error occurred when executing fdatasync synchronously.
    #[error("An error occurred when executing fdatasync synchronously: {0}")]
    Fdatasync(base::Error),
    /// An error occurred when executing fsync synchronously.
    #[error("An error occurred when executing fsync synchronously: {0}")]
    Fsync(base::Error),
    /// An error occurred when reading the FD.
    #[error("An error occurred when reading the FD: {0}.")]
    Read(base::Error),
    /// Can't seek file.
    #[error("An error occurred when seeking the FD: {0}.")]
    Seeking(base::Error),
    /// An error occurred when writing the FD.
    #[error("An error occurred when writing the FD: {0}.")]
    Write(base::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        use Error::*;
        match e {
            AddingWaker(e) => e.into(),
            Executor(e) => e.into(),
            Discard(e) => e.into(),
            Fdatasync(e) => e.into(),
            Fsync(e) => e.into(),
            Read(e) => e.into(),
            Seeking(e) => e.into(),
            Write(e) => e.into(),
        }
    }
}

impl From<Error> for AsyncError {
    fn from(e: Error) -> AsyncError {
        AsyncError::SysVariants(e.into())
    }
}

/// Async wrapper for an IO source that uses the kqueue executor to drive async operations.
pub struct KqueueSource<F> {
    registered_source: RegisteredSource<F>,
}

impl<F: AsRawDescriptor> KqueueSource<F> {
    /// Create a new `KqueueSource` from the given IO source.
    pub fn new(f: F, ex: &Arc<RawExecutor<KqueueReactor>>) -> Result<Self> {
        RegisteredSource::new(ex, f)
            .map({
                |f| KqueueSource {
                    registered_source: f,
                }
            })
            .map_err(Error::Executor)
    }
}

impl<F: AsRawDescriptor> KqueueSource<F> {
    /// Reads from the iosource at `file_offset` and fills the given `vec`.
    pub async fn read_to_vec(
        &self,
        file_offset: Option<u64>,
        mut vec: Vec<u8>,
    ) -> AsyncResult<(usize, Vec<u8>)> {
        loop {
            let res = if let Some(offset) = file_offset {
                // SAFETY: vec is a valid buffer we have exclusive access to, and duped_fd is a
                // valid file descriptor.
                handle_eintr_errno!(unsafe {
                    libc::pread(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_mut_ptr() as *mut libc::c_void,
                        vec.len(),
                        offset as libc::off_t,
                    )
                })
            } else {
                // SAFETY: vec is a valid buffer we have exclusive access to, and duped_fd is a
                // valid file descriptor.
                handle_eintr_errno!(unsafe {
                    libc::read(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_mut_ptr() as *mut libc::c_void,
                        vec.len(),
                    )
                })
            };

            if res >= 0 {
                return Ok((res as usize, vec));
            }

            match base::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .registered_source
                        .wait_readable()
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Read(e).into()),
            }
        }
    }

    /// Reads to the given `mem` at the given offsets from the file starting at `file_offset`.
    pub async fn read_to_mem(
        &self,
        file_offset: Option<u64>,
        mem: Arc<dyn BackingMemory + Send + Sync>,
        mem_offsets: impl IntoIterator<Item = MemRegion>,
    ) -> AsyncResult<usize> {
        let mut iovecs = mem_offsets
            .into_iter()
            .filter_map(|mem_range| mem.get_volatile_slice(mem_range).ok())
            .collect::<Vec<VolatileSlice>>();

        loop {
            let res = if let Some(offset) = file_offset {
                // SAFETY: iovecs contains valid VolatileSlice entries obtained from BackingMemory,
                // and duped_fd is a valid file descriptor.
                handle_eintr_errno!(unsafe {
                    libc::preadv(
                        self.registered_source.duped_fd.as_raw_fd(),
                        iovecs.as_mut_ptr() as *mut _,
                        iovecs.len() as i32,
                        offset as libc::off_t,
                    )
                })
            } else {
                // SAFETY: iovecs contains valid VolatileSlice entries obtained from BackingMemory,
                // and duped_fd is a valid file descriptor.
                handle_eintr_errno!(unsafe {
                    libc::readv(
                        self.registered_source.duped_fd.as_raw_fd(),
                        iovecs.as_mut_ptr() as *mut _,
                        iovecs.len() as i32,
                    )
                })
            };

            if res >= 0 {
                return Ok(res as usize);
            }

            match base::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .registered_source
                        .wait_readable()
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Read(e).into()),
            }
        }
    }

    /// Wait for the FD of `self` to be readable.
    pub async fn wait_readable(&self) -> AsyncResult<()> {
        let op = self
            .registered_source
            .wait_readable()
            .map_err(Error::AddingWaker)?;
        op.await.map_err(Error::Executor)?;
        Ok(())
    }

    /// Writes from the given `vec` to the file starting at `file_offset`.
    pub async fn write_from_vec(
        &self,
        file_offset: Option<u64>,
        vec: Vec<u8>,
    ) -> AsyncResult<(usize, Vec<u8>)> {
        loop {
            let res = if let Some(offset) = file_offset {
                // SAFETY: vec is a valid read-only buffer we own, and duped_fd is a valid file
                // descriptor.
                handle_eintr_errno!(unsafe {
                    libc::pwrite(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_ptr() as *const libc::c_void,
                        vec.len(),
                        offset as libc::off_t,
                    )
                })
            } else {
                // SAFETY: vec is a valid read-only buffer we own, and duped_fd is a valid file
                // descriptor.
                handle_eintr_errno!(unsafe {
                    libc::write(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_ptr() as *const libc::c_void,
                        vec.len(),
                    )
                })
            };

            if res >= 0 {
                return Ok((res as usize, vec));
            }

            match base::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .registered_source
                        .wait_writable()
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Write(e).into()),
            }
        }
    }

    /// Writes from the given `mem` from the given offsets to the file starting at `file_offset`.
    pub async fn write_from_mem(
        &self,
        file_offset: Option<u64>,
        mem: Arc<dyn BackingMemory + Send + Sync>,
        mem_offsets: impl IntoIterator<Item = MemRegion>,
    ) -> AsyncResult<usize> {
        let iovecs = mem_offsets
            .into_iter()
            .map(|mem_range| mem.get_volatile_slice(mem_range))
            .filter_map(|r| r.ok())
            .collect::<Vec<VolatileSlice>>();

        loop {
            let res = if let Some(offset) = file_offset {
                // SAFETY: iovecs contains valid VolatileSlice entries obtained from BackingMemory,
                // and duped_fd is a valid file descriptor.
                handle_eintr_errno!(unsafe {
                    libc::pwritev(
                        self.registered_source.duped_fd.as_raw_fd(),
                        iovecs.as_ptr() as *mut _,
                        iovecs.len() as i32,
                        offset as libc::off_t,
                    )
                })
            } else {
                // SAFETY: iovecs contains valid VolatileSlice entries obtained from BackingMemory,
                // and duped_fd is a valid file descriptor.
                handle_eintr_errno!(unsafe {
                    libc::writev(
                        self.registered_source.duped_fd.as_raw_fd(),
                        iovecs.as_ptr() as *mut _,
                        iovecs.len() as i32,
                    )
                })
            };

            if res >= 0 {
                return Ok(res as usize);
            }

            match base::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .registered_source
                        .wait_writable()
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Write(e).into()),
            }
        }
    }

    /// Sync all completed write operations to the backing storage.
    pub async fn fsync(&self) -> AsyncResult<()> {
        // SAFETY: duped_fd is a valid file descriptor. fsync does not access user memory.
        let ret = handle_eintr_errno!(unsafe {
            libc::fsync(self.registered_source.duped_fd.as_raw_fd())
        });
        if ret == 0 {
            Ok(())
        } else {
            Err(Error::Fsync(base::Error::last()).into())
        }
    }

    /// punch_hole - Not directly supported on MacOS, returns error
    pub async fn punch_hole(&self, _file_offset: u64, _len: u64) -> AsyncResult<()> {
        // MacOS doesn't have fallocate with FALLOC_FL_PUNCH_HOLE.
        // This would need to be implemented differently on MacOS.
        Err(Error::Discard(base::Error::new(libc::ENOTSUP)).into())
    }

    /// write_zeroes_at - Not directly supported on MacOS, returns error
    pub async fn write_zeroes_at(&self, _file_offset: u64, _len: u64) -> AsyncResult<()> {
        // MacOS doesn't have fallocate with FALLOC_FL_ZERO_RANGE.
        // This would need to be implemented differently on MacOS.
        Err(Error::Discard(base::Error::new(libc::ENOTSUP)).into())
    }

    /// Sync all data of completed write operations to the backing storage.
    /// Note: On MacOS, we use fsync as fdatasync is not available.
    pub async fn fdatasync(&self) -> AsyncResult<()> {
        // SAFETY: duped_fd is a valid file descriptor. fsync does not access user memory.
        // MacOS doesn't have fdatasync, so we use fsync with F_FULLFSYNC for full durability
        // or just fsync for similar semantics.
        let ret = handle_eintr_errno!(unsafe {
            libc::fsync(self.registered_source.duped_fd.as_raw_fd())
        });
        if ret == 0 {
            Ok(())
        } else {
            Err(Error::Fdatasync(base::Error::last()).into())
        }
    }

    /// Yields the underlying IO source.
    pub fn into_source(self) -> F {
        self.registered_source.source
    }

    /// Provides a mutable ref to the underlying IO source.
    pub fn as_source_mut(&mut self) -> &mut F {
        &mut self.registered_source.source
    }

    /// Provides a ref to the underlying IO source.
    pub fn as_source(&self) -> &F {
        &self.registered_source.source
    }
}
