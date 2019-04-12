// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::{Cursor, Write};

use gdb_remote_protocol::Error as ProtoError;
use gdb_remote_protocol::StopReason as GdbStopReason;
use gdb_remote_protocol::{
    ContinueStatus, GdbMessageReader, Handler, MemoryRegion, ProcessType, StopReason,
};

use msg_socket::{MsgReceiver, MsgSender};
use sync::Mutex;
use sys_util::{GuestAddress, GuestMemory};
use vm_control::{VmControlRequestSocket, VmRequest};

/// Architecture specific parts of handling GDB. Must be implemented for any architecture that
/// supports gdb debugging.
pub trait GdbArch {
    fn read_general_registers(&self) -> Result<Vec<u8>, ProtoError>;
}

pub trait GdbControl {
    /// Notify the stub that the CPU has stopped and why or that it has resumed if `reason` in
    /// `None`.
    fn cpu_stopped(&mut self, cpu_id: usize, signal: Option<GdbStopReason>);

    /// Notify the stub that a new byte has been received from the client.
    fn next_byte(&mut self, byte: u8);

    /// Set the output to write gdb replies to.
    fn set_output(&mut self, output: Box<dyn Write + Send + 'static>);

    /// Get the port on which to listen for clients.
    fn port(&self) -> u32;
}

/// A Gdb Stub implementation that can be used to debug code running in a VM.
pub struct GdbStub<T>
where
    T: GdbArch,
{
    handler: GdbHandler<T>,
    reader: GdbMessageReader,
    output: Option<Box<dyn Write + Send>>,
    port: u32,
}

impl<T> GdbStub<T>
where
    T: GdbArch,
{
    pub fn new(
        mem: GuestMemory,
        num_cpus: usize,
        port: u32,
        control_socket: VmControlRequestSocket,
        arch: T,
    ) -> Self {
        GdbStub {
            handler: GdbHandler::new(mem, num_cpus, control_socket, arch),
            reader: GdbMessageReader::new(),
            output: None,
            port,
        }
    }
}

impl<T> GdbControl for GdbStub<T>
where
    T: GdbArch,
{
    fn cpu_stopped(&mut self, cpu_id: usize, signal: Option<GdbStopReason>) {
        self.handler.cpu_states[cpu_id] = signal;
        // TODO - update all the register states.
    }

    fn next_byte(&mut self, byte: u8) {
        if let Some(output) = self.output.as_mut() {
            self.reader.next_byte(byte, &self.handler, output);
        }
    }

    fn set_output(&mut self, output: Box<dyn Write + Send + 'static>) {
        self.output = Some(output);
        self.reader.reset();
        let _ = self.handler.stop_all(); // TODO -handle error
    }

    fn port(&self) -> u32 {
        self.port
    }
}

struct GdbHandler<T>
where
    T: GdbArch,
{
    mem: GuestMemory,
    cpu_states: Vec<Option<StopReason>>,
    control_socket: Mutex<VmControlRequestSocket>,
    arch: T,
}

impl<T> GdbHandler<T>
where
    T: GdbArch,
{
    pub fn new(
        mem: GuestMemory,
        num_cpus: usize,
        control_socket: VmControlRequestSocket,
        arch: T,
    ) -> Self {
        let states = (0..num_cpus).map(|_| Default::default()).collect();
        GdbHandler {
            mem,
            cpu_states: states,
            control_socket: Mutex::new(control_socket),
            arch,
        }
    }

    fn stop_all(&self) -> Result<(), ProtoError> {
        self.send_request(VmRequest::Suspend);
        Ok(())
    }

    fn resume(&self) -> Result<(), ProtoError> {
        self.send_request(VmRequest::Resume);
        Ok(())
    }

    fn send_request(&self, request: VmRequest) {
        let vm_socket = self.control_socket.lock();
        vm_socket.send(&request).unwrap();
        vm_socket.recv().unwrap();
    }
}

impl<T> Handler for GdbHandler<T>
where
    T: GdbArch,
{
    fn attached(&self, _pid: Option<u64>) -> Result<ProcessType, ProtoError> {
        Ok(ProcessType::Attached)
    }

    fn halt_reason(&self) -> Result<StopReason, ProtoError> {
        Ok(StopReason::Signal(5))
    }

    fn read_memory(&self, region: MemoryRegion) -> Result<Vec<u8>, ProtoError> {
        let len = region.length as usize;
        let mut buf = Cursor::new(Vec::with_capacity(len));
        self.mem
            .write_from_memory(GuestAddress(region.address), &mut buf, len)
            .map_err(|_| ProtoError::Error(1))?;
        Ok(buf.into_inner())
    }

    fn write_memory(&self, address: u64, bytes: &[u8]) -> Result<(), ProtoError> {
        self.mem
            .write_all_at_addr(bytes, GuestAddress(address))
            .map_err(|_| ProtoError::Error(1))
    }

    fn cont(&self, _addr: Option<u64>) -> Result<ContinueStatus, ProtoError> {
        self.resume()?;
        Ok(ContinueStatus {})
    }

    fn interrupt(&self) -> Result<StopReason, ProtoError> {
        self.stop_all()?;
        Ok(StopReason::Signal(5))
    }

    fn read_general_registers(&self) -> Result<Vec<u8>, ProtoError> {
        self.arch.read_general_registers()
    }
}
