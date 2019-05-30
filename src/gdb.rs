// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Write;
use std::sync::mpsc;

// add contol messages, send from read gen regs.

use gdb_remote_protocol::Error as ProtoError;
use gdb_remote_protocol::StopReason as GdbStopReason;
use gdb_remote_protocol::{
    ContinueStatus, GdbMessageReader, Handler, MemoryRegion, ProcessType, StopReason,
};

use msg_socket::{MsgReceiver, MsgSender};
use sync::Mutex;
use sys_util::{GuestAddress, GuestMemory};
use vm_control::{
    VCpuControl, VCpuDebug, VCpuDebugStatus, VCpuDebugStatusMessage, VmControlRequestSocket,
    VmRequest,
};

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
pub struct GdbStub {
    handler: GdbHandler,
    reader: GdbMessageReader,
    output: Option<Box<dyn Write + Send>>,
    port: u32,
}

impl GdbStub {
    pub fn new(
        mem: GuestMemory,
        port: u32,
        vm_socket: VmControlRequestSocket,
        vcpu_com: Vec<mpsc::Sender<VCpuControl>>,
        from_vcpu: mpsc::Receiver<VCpuDebugStatusMessage>,
    ) -> Self {
        GdbStub {
            handler: GdbHandler::new(mem, vm_socket, vcpu_com, from_vcpu),
            reader: GdbMessageReader::new(),
            output: None,
            port,
        }
    }
}

impl GdbControl for GdbStub {
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

struct GdbHandler {
    mem: GuestMemory,
    current_cpu: usize,
    cpu_states: Vec<Option<StopReason>>,
    vm_socket: Mutex<VmControlRequestSocket>,
    vcpu_com: Vec<mpsc::Sender<VCpuControl>>,
    from_vcpu: mpsc::Receiver<VCpuDebugStatusMessage>,
}

impl GdbHandler {
    pub fn new(
        mem: GuestMemory,
        vm_socket: VmControlRequestSocket,
        vcpu_com: Vec<mpsc::Sender<VCpuControl>>,
        from_vcpu: mpsc::Receiver<VCpuDebugStatusMessage>,
    ) -> Self {
        let states = (0..vcpu_com.len()).map(|_| Default::default()).collect();
        GdbHandler {
            mem,
            current_cpu: 0,
            cpu_states: states,
            vm_socket: Mutex::new(vm_socket),
            vcpu_com,
            from_vcpu,
        }
    }

    fn stop_all(&self) -> Result<(), ProtoError> {
        self.vm_request(VmRequest::Suspend);
        Ok(())
    }

    fn resume(&self) -> Result<(), ProtoError> {
        self.vm_request(VmRequest::Resume);
        Ok(())
    }

    // Note that this will only work if the vcpu is stopped. Otherwise a signal must be sent to the
    // vcpu thread, causing it to service the channel.
    fn vcpu_request(&self, cpu: usize, request: VCpuControl) -> Result<(), ProtoError> {
        self.vcpu_com[cpu]
            .send(request)
            .map_err(|_| ProtoError::Error(1))?;
        Ok(())
    }

    fn vm_request(&self, request: VmRequest) -> Result<(), ProtoError> {
        let vm_socket = self.vm_socket.lock();
        vm_socket.send(&request).map_err(|_| ProtoError::Error(1))?;
        vm_socket.recv().map_err(|_| ProtoError::Error(1))?;
        Ok(())
    }
}

impl Handler for GdbHandler {
    fn attached(&self, _pid: Option<u64>) -> Result<ProcessType, ProtoError> {
        Ok(ProcessType::Attached)
    }

    fn halt_reason(&self) -> Result<StopReason, ProtoError> {
        Ok(StopReason::Signal(5))
    }

    // TODO(dgreid) - read and write are given guest virtual addresses, they need to translate to
    // physical address.
    fn read_memory(&self, region: MemoryRegion) -> Result<Vec<u8>, ProtoError> {
        let len = region.length as usize;
        self.vcpu_request(
            self.current_cpu,
            VCpuControl::Debug(VCpuDebug::ReadMem(GuestAddress(region.address), len)),
        )
        .map_err(|_| ProtoError::Error(1))?;
        //TODO(dgreid) - allow timeout.
        match self.from_vcpu.recv() {
            Ok(msg) => match msg.msg {
                VCpuDebugStatus::MemoryRegion(r) => Ok(r),
                _ => Err(ProtoError::Error(1)),
            },
            Err(e) => Err(ProtoError::Error(1)),
        }
    }

    fn write_memory(&self, address: u64, bytes: &[u8]) -> Result<(), ProtoError> {
        let len = bytes.len();
        self.vcpu_request(
            self.current_cpu,
            VCpuControl::Debug(VCpuDebug::WriteMem(GuestAddress(address), bytes.to_owned())),
        )
        .map_err(|_| ProtoError::Error(1))?;
        //TODO(dgreid) - allow timeout.
        match self.from_vcpu.recv() {
            Ok(msg) => match msg.msg {
                VCpuDebugStatus::CommandComplete => Ok(()),
                _ => Err(ProtoError::Error(1)),
            },
            Err(e) => Err(ProtoError::Error(1)),
        }
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
        self.vcpu_request(self.current_cpu, VCpuControl::Debug(VCpuDebug::ReadRegs))
            .map_err(|_| ProtoError::Error(1))?;
        //TODO(dgreid) - allow timeout.
        match self.from_vcpu.recv() {
            Ok(msg) => match msg.msg {
                VCpuDebugStatus::RegValues(r) => Ok(r),
                _ => Err(ProtoError::Error(1)),
            },
            Err(e) => Err(ProtoError::Error(1)),
        }
    }
}
