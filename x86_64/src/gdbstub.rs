use gdb_remote_protocol::Error as ProtoError;

use arch::gdb::GdbArch;

pub struct GdbX86 {}

impl GdbX86 {
    pub fn new() -> Self {
        GdbX86 {}
    }
}

impl GdbArch for GdbX86 {
    fn read_general_registers(&self) -> Result<Vec<u8>, ProtoError> {
        Ok(vec![0u8; 64])
    }
}
