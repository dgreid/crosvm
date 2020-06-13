//! Operations that can be preformed on disks.

use std::marker::PhantomData;
use std::os::unix::io::RawFd;

/// The operation types and the data needed to start each.
pub enum DiskOperation {
    FSync,
    PunchHole {
        offset: u64,
        length: u64,
    },
    Read {
        offset: u64,
        len: u64,
    },
    Write {
        offset: u64,
        len: u64,
    },
    WriteZeros {
        sector: u64,
        num_sectors: u32,
        flags: u32,
    },
}

impl DiskOperation {
    /// Returns true if this operation is allowed on read-only disks.
    pub fn valid_read_only(&self) -> bool {
        match self {
            DiskOperation::Read {
                offset: _offset,
                len: _len,
            } => true, // Read works on read-only disks.
            _ => {
                // Everything else is invalid on read-only disks.
                false
            }
        }
    }
}

/// Operations that can be performed on the backing FD of a disk. The backing FD could be a single
/// FD for a File, multiple FDs for composite, or multiple FDs at multple locations for qcow.
pub struct BackingOperation<'a> {
    fd: RawFd,
    op: DiskOperation,
    _owner_lifetime: PhantomData<&'a ()>,
}

/// Trait for objects that can process `DiskOperation`s and return operations to be performed on the
/// backing disk representation.
pub trait OpProcess {
    fn process_op(
        &self,
        op: DiskOperation,
    ) -> std::result::Result<&[BackingOperation<'_>], Box<dyn std::error::Error>>; // TODO iter not slice?
}
