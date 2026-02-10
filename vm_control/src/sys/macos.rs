// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MacOS implementation of vm_control platform-specific functionality.

#[cfg(feature = "gpu")]
pub(crate) mod gpu;

use std::path::Path;
use std::time::Duration;

use base::error;
use base::Error as SysError;
use base::MemoryMappingArena;
use base::MmapError;
use base::Tube;
use base::UnixSeqpacket;
use hypervisor::MemCacheType;
use hypervisor::MemSlot;
use hypervisor::Vm;
use libc::EINVAL;
use libc::ERANGE;
use resources::Alloc;
use resources::SystemAllocator;
use serde::Deserialize;
use serde::Serialize;
use vm_memory::GuestAddress;

use crate::client::HandleRequestResult;
use crate::VmMappedMemoryRegion;
use crate::VmRequest;

pub fn handle_request<T: AsRef<Path> + std::fmt::Debug>(
    request: &VmRequest,
    socket_path: T,
) -> HandleRequestResult {
    handle_request_with_timeout(request, socket_path, None)
}

pub fn handle_request_with_timeout<T: AsRef<Path> + std::fmt::Debug>(
    request: &VmRequest,
    socket_path: T,
    timeout: Option<Duration>,
) -> HandleRequestResult {
    match UnixSeqpacket::connect(&socket_path) {
        Ok(s) => {
            let socket = Tube::try_from(s).map_err(|_| ())?;
            if timeout.is_some() {
                if let Err(e) = socket.set_recv_timeout(timeout) {
                    error!(
                        "failed to set recv timeout on socket at '{:?}': {}",
                        socket_path, e
                    );
                    return Err(());
                }
            }
            if let Err(e) = socket.send(request) {
                error!(
                    "failed to send request to socket at '{:?}': {}",
                    socket_path, e
                );
                return Err(());
            }
            match socket.recv() {
                Ok(response) => Ok(response),
                Err(e) => {
                    error!(
                        "failed to recv response from socket at '{:?}': {}",
                        socket_path, e
                    );
                    Err(())
                }
            }
        }
        Err(e) => {
            error!("failed to connect to socket at '{:?}': {}", socket_path, e);
            Err(())
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryMappingRequest {
    /// Flush the content of a memory mapping to its backing file.
    MsyncArena {
        slot: MemSlot,
        offset: usize,
        size: usize,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryMappingResponse {
    Ok,
    Err(SysError),
}

impl VmMemoryMappingRequest {
    pub fn execute(&self, vm: &mut impl Vm) -> VmMemoryMappingResponse {
        use self::VmMemoryMappingRequest::*;
        match *self {
            MsyncArena { slot, offset, size } => match vm.msync_memory_region(slot, offset, size) {
                Ok(()) => VmMemoryMappingResponse::Ok,
                Err(e) => VmMemoryMappingResponse::Err(e),
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum FsMappingRequest {
    /// Create an anonymous memory mapping that spans the entire region described by `Alloc`.
    AllocateSharedMemoryRegion(Alloc),
}

pub fn prepare_shared_memory_region(
    vm: &mut dyn Vm,
    allocator: &mut SystemAllocator,
    alloc: Alloc,
    cache: MemCacheType,
) -> Result<VmMappedMemoryRegion, SysError> {
    if !matches!(alloc, Alloc::PciBar { .. }) {
        return Err(SysError::new(EINVAL));
    }
    match allocator.mmio_allocator_any().get(&alloc) {
        Some((range, _)) => {
            let size: usize = match range.len().and_then(|x| x.try_into().ok()) {
                Some(v) => v,
                None => return Err(SysError::new(ERANGE)),
            };
            let arena = match MemoryMappingArena::new(size) {
                Ok(a) => a,
                Err(MmapError::SystemCallFailed(e)) => return Err(e),
                _ => return Err(SysError::new(EINVAL)),
            };

            match vm.add_memory_region(
                GuestAddress(range.start),
                Box::new(arena),
                false,
                false,
                cache,
            ) {
                Ok(slot) => Ok(VmMappedMemoryRegion {
                    guest_address: GuestAddress(range.start),
                    slot,
                }),
                Err(e) => Err(e),
            }
        }
        None => Err(SysError::new(EINVAL)),
    }
}

/// On macOS, we always return true for 64-bit systems since there's enough address space.
pub fn should_prepare_memory_region() -> bool {
    cfg!(target_pointer_width = "64")
}
