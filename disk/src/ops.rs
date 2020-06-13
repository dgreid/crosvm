//! Operations that can be preformed on disks.

/// The operation types and the data needed to start each.
pub enum OperationType {
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

impl OperationType {
    /// Returns true if this operation is allowed on read-only disks.
    pub fn valid_read_only(&self) -> bool {
        match self {
            OperationType::Read {
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
