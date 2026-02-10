// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hypervisor.framework backend for macOS on aarch64.
//!
//! This module provides VM and VCPU implementations using Apple's Hypervisor.framework,
//! which is available on macOS for both Apple Silicon (arm64) and Intel (x86_64) Macs.

mod hvf_sys;

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::RwLock;

use aarch64_sys_reg::AArch64SysRegId;
use base::AsRawDescriptor;
use base::Error;
use base::Event;
use base::MappedRegion;
use base::Protection;
use base::Result;
use base::SafeDescriptor;
use cros_fdt::Fdt;
use libc::EINVAL;
use libc::ENOTSUP;
use snapshot::AnySnapshot;
use sync::Mutex;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

use crate::BalloonEvent;
use crate::ClockState;
use crate::Datamatch;
use crate::DeviceKind;
use crate::Hypervisor;
use crate::HypervisorCap;
use crate::HypervisorKind;
use crate::IoEventAddress;
use crate::IoOperation;
use crate::IoParams;
use crate::MemCacheType;
use crate::MemSlot;
use crate::Vcpu;
use crate::VcpuAArch64;
use crate::VcpuExit;
use crate::VcpuFeature;
use crate::VcpuRegAArch64;
use crate::Vm;
use crate::VmAArch64;
use crate::VmCap;

pub use hvf_sys::*;

/// Default guest IPA (Intermediate Physical Address) size in bits.
/// Apple Silicon supports 40-bit physical addresses for guests.
const HVF_DEFAULT_IPA_BITS: u8 = 40;

/// The Hypervisor.framework-based hypervisor.
#[derive(Clone)]
pub struct Hvf {
    /// Whether the VM has been created.
    vm_created: Arc<AtomicBool>,
}

impl Hvf {
    /// Creates a new Hvf instance.
    pub fn new() -> Result<Self> {
        Ok(Hvf {
            vm_created: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Get the guest physical address size in bits.
    pub fn get_guest_phys_addr_bits(&self) -> u8 {
        HVF_DEFAULT_IPA_BITS
    }
}

impl Hypervisor for Hvf {
    fn try_clone(&self) -> Result<Self> {
        Ok(self.clone())
    }

    fn check_capability(&self, cap: HypervisorCap) -> bool {
        match cap {
            HypervisorCap::UserMemory => true,
            HypervisorCap::ImmediateExit => true,
            _ => false,
        }
    }
}

/// Memory slot tracking structure
struct MemorySlot {
    guest_addr: GuestAddress,
    region: Box<dyn MappedRegion>,
    read_only: bool,
}

/// A Hypervisor.framework-based VM.
pub struct HvfVm {
    hvf: Hvf,
    guest_mem: GuestMemory,
    /// Memory slots indexed by slot number
    mem_slots: RwLock<BTreeMap<MemSlot, MemorySlot>>,
    /// Next available memory slot
    next_mem_slot: Mutex<MemSlot>,
    /// Registered IO events
    io_events: Mutex<Vec<IoEventRegistration>>,
}

struct IoEventRegistration {
    event: Event,
    addr: IoEventAddress,
    datamatch: Datamatch,
}

impl HvfVm {
    /// Creates a new HvfVm.
    pub fn new(hvf: &Hvf, guest_mem: GuestMemory) -> Result<Self> {
        // Check if a VM is already created (only one VM per process allowed)
        if hvf
            .vm_created
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::new(libc::EEXIST));
        }

        // Create the VM
        // SAFETY: Passing null for config requests a default VM configuration.
        // Only one VM per process is allowed; this is enforced by the compare_exchange above.
        let ret = unsafe { hv_vm_create(std::ptr::null_mut()) };
        hv_result(ret)?;

        let vm = HvfVm {
            hvf: hvf.clone(),
            guest_mem,
            mem_slots: RwLock::new(BTreeMap::new()),
            next_mem_slot: Mutex::new(0),
            io_events: Mutex::new(Vec::new()),
        };

        Ok(vm)
    }

    /// Creates a new VCPU.
    pub fn create_vcpu(&self, id: usize) -> Result<HvfVcpu> {
        let mut vcpu_id: hv_vcpu_t = 0;
        let mut exit_info: *const hv_vcpu_exit_t = std::ptr::null();

        // SAFETY: vcpu_id and exit_info are valid mutable pointers on the stack.
        // The null config requests default VCPU settings.
        let ret = unsafe { hv_vcpu_create(&mut vcpu_id, &mut exit_info, std::ptr::null_mut()) };
        hv_result(ret)?;

        Ok(HvfVcpu {
            id,
            vcpu: vcpu_id,
            exit_info,
            immediate_exit: AtomicBool::new(false),
            last_exit: Mutex::new(None),
        })
    }

    /// Creates the in-kernel GICv3 interrupt controller (macOS 15.0+).
    ///
    /// This must be called AFTER creating the VM but BEFORE creating any VCPUs.
    /// The GIC is configured with the specified distributor and redistributor
    /// base addresses.
    ///
    /// # Arguments
    /// * `distributor_base` - Guest physical address for the GIC distributor
    /// * `redistributor_base` - Guest physical address for the GIC redistributors
    ///
    /// # Returns
    /// Ok(()) on success, or an error if GIC creation fails.
    pub fn create_gic(&self, distributor_base: u64, redistributor_base: u64) -> Result<()> {
        // Create GIC configuration
        // SAFETY: hv_gic_config_create takes no arguments and returns an ARC-managed
        // opaque config pointer (or null on failure).
        let gic_config = unsafe { hv_gic_config_create() };
        if gic_config.is_null() {
            return Err(Error::new(libc::ENOMEM));
        }

        // Set distributor base address
        // SAFETY: gic_config is a non-null pointer returned by hv_gic_config_create above.
        let ret = unsafe { hv_gic_config_set_distributor_base(gic_config, distributor_base) };
        hv_result(ret)?;

        // Set redistributor base address
        // SAFETY: gic_config is a non-null pointer returned by hv_gic_config_create above.
        let ret = unsafe { hv_gic_config_set_redistributor_base(gic_config, redistributor_base) };
        hv_result(ret)?;

        // Create the GIC device
        // SAFETY: gic_config is the non-null, fully-configured object from above.
        let ret = unsafe { hv_gic_create(gic_config) };
        hv_result(ret)?;

        // gic_config is released automatically by ARC

        Ok(())
    }

    /// Triggers a Shared Peripheral Interrupt (SPI) on the in-kernel GIC.
    ///
    /// # Arguments
    /// * `intid` - The interrupt ID (32-1019 for SPIs)
    /// * `level` - true to assert the interrupt, false to deassert
    pub fn gic_set_spi(intid: u32, level: bool) -> Result<()> {
        // SAFETY: This is a stateless HVF call; the GIC must have been created previously.
        // intid and level are plain values with no pointer safety concerns.
        let ret = unsafe { hv_gic_set_spi(intid, level) };
        hv_result(ret)
    }

    /// Gets the GIC distributor size.
    pub fn gic_get_distributor_size() -> Result<usize> {
        let mut size: usize = 0;
        // SAFETY: size is a valid mutable pointer on the stack.
        let ret = unsafe { hv_gic_get_distributor_size(&mut size) };
        hv_result(ret)?;
        Ok(size)
    }

    /// Gets the GIC redistributor size (per VCPU).
    pub fn gic_get_redistributor_size() -> Result<usize> {
        let mut size: usize = 0;
        // SAFETY: size is a valid mutable pointer on the stack.
        let ret = unsafe { hv_gic_get_redistributor_size(&mut size) };
        hv_result(ret)?;
        Ok(size)
    }

    /// Gets the total GIC redistributor region size.
    pub fn gic_get_redistributor_region_size() -> Result<usize> {
        let mut size: usize = 0;
        // SAFETY: size is a valid mutable pointer on the stack.
        let ret = unsafe { hv_gic_get_redistributor_region_size(&mut size) };
        hv_result(ret)?;
        Ok(size)
    }

    /// Gets the SPI interrupt range.
    pub fn gic_get_spi_interrupt_range() -> Result<(u32, u32)> {
        let mut base: u32 = 0;
        let mut count: u32 = 0;
        // SAFETY: base and count are valid mutable pointers on the stack.
        let ret = unsafe { hv_gic_get_spi_interrupt_range(&mut base, &mut count) };
        hv_result(ret)?;
        Ok((base, count))
    }

    /// Resets the GIC device.
    pub fn gic_reset() -> Result<()> {
        // SAFETY: hv_gic_reset takes no arguments; it resets the previously created GIC.
        let ret = unsafe { hv_gic_reset() };
        hv_result(ret)
    }
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        // SAFETY: The VM was created in new() and we are the exclusive owner being dropped.
        let _ = unsafe { hv_vm_destroy() };
        self.hvf.vm_created.store(false, Ordering::SeqCst);
    }
}

impl Vm for HvfVm {
    fn try_clone(&self) -> Result<Self> {
        // HvfVm cannot be cloned as there's only one VM per process
        Err(Error::new(ENOTSUP))
    }

    fn try_clone_descriptor(&self) -> Result<SafeDescriptor> {
        // Hypervisor.framework doesn't use file descriptors
        Err(Error::new(ENOTSUP))
    }

    fn hypervisor_kind(&self) -> HypervisorKind {
        HypervisorKind::Hvf
    }

    fn check_capability(&self, c: VmCap) -> bool {
        match c {
            VmCap::ArmPmuV3 => false,
            VmCap::DirtyLog => false,
            VmCap::PvClock => false,
            VmCap::Protected => false,
            VmCap::EarlyInitCpuid => false,
            VmCap::ReadOnlyMemoryRegion => true,
            VmCap::MemNoncoherentDma => false,
            VmCap::Sve => false,
        }
    }

    fn get_guest_phys_addr_bits(&self) -> u8 {
        self.hvf.get_guest_phys_addr_bits()
    }

    fn get_memory(&self) -> &GuestMemory {
        &self.guest_mem
    }

    fn add_memory_region(
        &mut self,
        guest_addr: GuestAddress,
        mem_region: Box<dyn MappedRegion>,
        read_only: bool,
        _log_dirty_pages: bool,
        _cache: MemCacheType,
    ) -> Result<MemSlot> {
        let host_addr = mem_region.as_ptr();
        let size = mem_region.size();

        let mut flags = HV_MEMORY_READ;
        if !read_only {
            flags |= HV_MEMORY_WRITE;
        }
        flags |= HV_MEMORY_EXEC;

        // SAFETY: host_addr points to a valid MappedRegion of `size` bytes that will
        // remain live while this slot exists. guest_addr is an unused IPA range.
        let ret = unsafe { hv_vm_map(host_addr as *mut _, guest_addr.0, size, flags) };
        hv_result(ret)?;

        let mut next_slot = self.next_mem_slot.lock();
        let slot = *next_slot;
        *next_slot += 1;

        self.mem_slots
            .write()
            .expect("mem_slots lock poisoned")
            .insert(
                slot,
                MemorySlot {
                    guest_addr,
                    region: mem_region,
                    read_only,
                },
            );

        Ok(slot)
    }

    fn msync_memory_region(&mut self, slot: MemSlot, offset: usize, size: usize) -> Result<()> {
        let slots = self.mem_slots.read().expect("mem_slots lock poisoned");
        let mem_slot = slots.get(&slot).ok_or_else(|| Error::new(EINVAL))?;

        // SAFETY: The pointer is within a live MappedRegion retrieved from mem_slots,
        // and offset+size are within that region's bounds.
        let ret = unsafe {
            libc::msync(
                (mem_slot.region.as_ptr() as usize + offset) as *mut _,
                size,
                libc::MS_SYNC,
            )
        };

        if ret < 0 {
            Err(Error::last())
        } else {
            Ok(())
        }
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    fn madvise_pageout_memory_region(
        &mut self,
        _slot: MemSlot,
        _offset: usize,
        _size: usize,
    ) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    fn madvise_remove_memory_region(
        &mut self,
        _slot: MemSlot,
        _offset: usize,
        _size: usize,
    ) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    fn remove_memory_region(&mut self, slot: MemSlot) -> Result<Box<dyn MappedRegion>> {
        let mut slots = self.mem_slots.write().expect("mem_slots lock poisoned");
        let mem_slot = slots.remove(&slot).ok_or_else(|| Error::new(EINVAL))?;

        // SAFETY: guest_addr and size correspond to a region we mapped with hv_vm_map
        // and just removed from mem_slots.
        let ret = unsafe { hv_vm_unmap(mem_slot.guest_addr.0, mem_slot.region.size()) };
        hv_result(ret)?;

        Ok(mem_slot.region)
    }

    fn create_device(&self, _kind: DeviceKind) -> Result<SafeDescriptor> {
        // Hypervisor.framework doesn't support device creation this way
        Err(Error::new(ENOTSUP))
    }

    fn get_dirty_log(&self, _slot: MemSlot, _dirty_log: &mut [u8]) -> Result<()> {
        // Dirty page tracking not supported
        Err(Error::new(ENOTSUP))
    }

    fn register_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
    ) -> Result<()> {
        let mut io_events = self.io_events.lock();
        io_events.push(IoEventRegistration {
            event: evt.try_clone()?,
            addr,
            datamatch,
        });
        Ok(())
    }

    fn unregister_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
    ) -> Result<()> {
        let mut io_events = self.io_events.lock();
        io_events.retain(|reg| {
            !(reg.addr == addr
                && reg.datamatch == datamatch
                && reg.event.as_raw_descriptor() == evt.as_raw_descriptor())
        });
        Ok(())
    }

    fn handle_io_events(&self, addr: IoEventAddress, data: &[u8]) -> Result<()> {
        let io_events = self.io_events.lock();
        for reg in io_events.iter() {
            if reg.addr != addr {
                continue;
            }

            let matched = match (&reg.datamatch, data.len()) {
                (Datamatch::AnyLength, _) => true,
                (Datamatch::U8(None), 1) => true,
                (Datamatch::U8(Some(v)), 1) => data[0] == *v,
                (Datamatch::U16(None), 2) => true,
                (Datamatch::U16(Some(v)), 2) => u16::from_ne_bytes(data.try_into().unwrap()) == *v,
                (Datamatch::U32(None), 4) => true,
                (Datamatch::U32(Some(v)), 4) => u32::from_ne_bytes(data.try_into().unwrap()) == *v,
                (Datamatch::U64(None), 8) => true,
                (Datamatch::U64(Some(v)), 8) => u64::from_ne_bytes(data.try_into().unwrap()) == *v,
                _ => false,
            };

            if matched {
                reg.event.signal()?;
            }
        }
        Ok(())
    }

    fn get_pvclock(&self) -> Result<ClockState> {
        Err(Error::new(ENOTSUP))
    }

    fn set_pvclock(&self, _state: &ClockState) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    fn add_fd_mapping(
        &mut self,
        _slot: u32,
        _offset: usize,
        _size: usize,
        _fd: &dyn base::AsRawDescriptor,
        _fd_offset: u64,
        _prot: Protection,
    ) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    fn remove_mapping(&mut self, _slot: u32, _offset: usize, _size: usize) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    fn handle_balloon_event(&mut self, _event: BalloonEvent) -> Result<()> {
        Ok(())
    }

    fn enable_hypercalls(&mut self, _nr: u64, _count: usize) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }
}

impl VmAArch64 for HvfVm {
    fn get_hypervisor(&self) -> &dyn Hypervisor {
        &self.hvf
    }

    fn load_protected_vm_firmware(
        &mut self,
        _fw_addr: GuestAddress,
        _fw_max_size: u64,
    ) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    fn create_vcpu(&self, id: usize) -> Result<Box<dyn VcpuAArch64>> {
        Ok(Box::new(HvfVm::create_vcpu(self, id)?))
    }

    fn create_fdt(&self, _fdt: &mut Fdt, _phandles: &BTreeMap<&str, u32>) -> cros_fdt::Result<()> {
        Ok(())
    }

    fn init_arch(
        &self,
        _payload_entry_address: GuestAddress,
        _fdt_address: GuestAddress,
        _fdt_size: usize,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn set_counter_offset(&self, _offset: u64) -> Result<()> {
        // TODO: Implement using hv_vcpu_set_vtimer_offset
        Err(Error::new(ENOTSUP))
    }
}

/// Exit information stored between run() and handle_mmio() calls
struct LastExit {
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}

/// A Hypervisor.framework-based VCPU.
pub struct HvfVcpu {
    id: usize,
    vcpu: hv_vcpu_t,
    exit_info: *const hv_vcpu_exit_t,
    immediate_exit: AtomicBool,
    last_exit: Mutex<Option<LastExit>>,
}

// SAFETY: HvfVcpu is Send because all mutable state (last_exit, immediate_exit)
// uses interior synchronization (Mutex, AtomicBool). The vcpu handle is an opaque
// integer that is safe to move between threads.
unsafe impl Send for HvfVcpu {}

// SAFETY: All mutable fields use interior synchronization: last_exit is behind a
// Mutex, immediate_exit is AtomicBool. The vcpu and exit_info fields are read-only
// after construction.
unsafe impl Sync for HvfVcpu {}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        // SAFETY: self.vcpu was created by hv_vcpu_create and we are the exclusive owner
        // being dropped.
        let _ = unsafe { hv_vcpu_destroy(self.vcpu) };
    }
}

impl Vcpu for HvfVcpu {
    fn try_clone(&self) -> Result<Self> {
        // VCPUs cannot be cloned in Hypervisor.framework
        Err(Error::new(ENOTSUP))
    }

    fn as_vcpu(&self) -> &dyn Vcpu {
        self
    }

    fn run(&mut self) -> Result<VcpuExit> {
        if self.immediate_exit.swap(false, Ordering::SeqCst) {
            return Ok(VcpuExit::Intr);
        }

        // SAFETY: self.vcpu is a valid handle created by hv_vcpu_create. We have &mut self,
        // ensuring exclusive access.
        let ret = unsafe { hv_vcpu_run(self.vcpu) };
        hv_result(ret)?;

        // SAFETY: exit_info was set by hv_vcpu_create and remains valid until
        // hv_vcpu_destroy, which only happens in our Drop impl.
        let exit_info = unsafe { &*self.exit_info };

        match exit_info.reason {
            hv_exit_reason_t::HV_EXIT_REASON_CANCELED => Ok(VcpuExit::Intr),
            hv_exit_reason_t::HV_EXIT_REASON_EXCEPTION => {
                let syndrome = exit_info.exception.syndrome;
                let ec = (syndrome >> 26) & 0x3f;

                // Store exit info for handle_mmio
                *self.last_exit.lock() = Some(LastExit {
                    syndrome: exit_info.exception.syndrome,
                    virtual_address: exit_info.exception.virtual_address,
                    physical_address: exit_info.exception.physical_address,
                });

                // EC (Exception Class) values for data/instruction aborts
                const EC_DATA_ABORT_LOWER: u64 = 0x24;
                const EC_DATA_ABORT_CURRENT: u64 = 0x25;
                const EC_HVC: u64 = 0x16;
                const EC_SMC: u64 = 0x17;
                const EC_SYS_REG: u64 = 0x18;
                const EC_WFI_WFE: u64 = 0x01;

                match ec {
                    EC_DATA_ABORT_LOWER | EC_DATA_ABORT_CURRENT => Ok(VcpuExit::Mmio),
                    EC_HVC | EC_SMC => Ok(VcpuExit::Hypercall),
                    EC_SYS_REG => {
                        // System register access (MSR/MRS trap)

                        // Syndrome format for EC=0x18 (per ARM ARM and QEMU syn_aa64_sysregtrap):
                        // Bits 21:20 = Op0 (2 bits, values 2-3 for system regs)
                        // Bits 19:17 = Op2
                        // Bits 16:14 = Op1
                        // Bits 13:10 = CRn
                        // Bits 9:5 = Rt (target register)
                        // Bits 4:1 = CRm
                        // Bit 0 = Direction (0=write MSR, 1=read MRS)
                        let is_read = (syndrome & 1) != 0;
                        let crm = ((syndrome >> 1) & 0xf) as u8;
                        let rt = ((syndrome >> 5) & 0x1f) as u8;
                        let crn = ((syndrome >> 10) & 0xf) as u8;
                        let op1 = ((syndrome >> 14) & 0x7) as u8;
                        let op2 = ((syndrome >> 17) & 0x7) as u8;
                        let op0 = ((syndrome >> 20) & 0x3) as u8;
                        let is_write = !is_read;

                        // Return SystemRegisterTrap so the caller can handle ICC registers
                        Ok(VcpuExit::SystemRegisterTrap {
                            op0,
                            op1,
                            crn,
                            crm,
                            op2,
                            rt,
                            is_write,
                        })
                    }
                    EC_WFI_WFE => Ok(VcpuExit::Hlt),
                    _ => {
                        // Unknown exception
                        Ok(VcpuExit::Exception)
                    }
                }
            }
            hv_exit_reason_t::HV_EXIT_REASON_VTIMER_ACTIVATED => {
                // Virtual timer fired - this should trigger an interrupt
                Ok(VcpuExit::IrqWindowOpen)
            }
            hv_exit_reason_t::HV_EXIT_REASON_UNKNOWN => Ok(VcpuExit::Exception),
        }
    }

    fn id(&self) -> usize {
        self.id
    }

    fn set_immediate_exit(&self, exit: bool) {
        self.immediate_exit.store(exit, Ordering::SeqCst);
        if exit {
            // Force the VCPU to exit immediately
            // SAFETY: self.vcpu is a valid handle and we pass its address with count=1.
            let _ = unsafe { hv_vcpus_exit(&self.vcpu, 1) };
        }
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    fn signal_handle(&self) -> crate::VcpuSignalHandle {
        unimplemented!("signal_handle not supported on macOS")
    }

    fn handle_mmio(&self, handle_fn: &mut dyn FnMut(IoParams) -> Result<()>) -> Result<()> {
        let last_exit = self
            .last_exit
            .lock()
            .take()
            .ok_or_else(|| Error::new(EINVAL))?;

        let syndrome = last_exit.syndrome;
        let physical_address = last_exit.physical_address;

        // Decode the syndrome for a data abort
        let isv = (syndrome >> 24) & 1; // Instruction Syndrome Valid
        let sas = (syndrome >> 22) & 3; // Syndrome Access Size
        let sse = (syndrome >> 21) & 1; // Syndrome Sign Extend
        let srt = (syndrome >> 16) & 0x1f; // Syndrome Register Transfer
        let wnr = (syndrome >> 6) & 1; // Write not Read

        if isv == 0 {
            // ISV not valid - we can't decode the instruction
            return Err(Error::new(EINVAL));
        }

        let access_size = 1usize << sas;

        if wnr == 1 {
            // Write operation
            let mut buf = [0u8; 8];
            let data = &mut buf[..access_size];
            let reg_value = self.get_reg(srt as u8)?;

            for (i, byte) in data.iter_mut().enumerate() {
                *byte = ((reg_value >> (i * 8)) & 0xff) as u8;
            }

            handle_fn(IoParams {
                address: physical_address,
                operation: IoOperation::Write(data),
            })?;
        } else {
            // Read operation
            let mut buf = [0u8; 8];
            let data = &mut buf[..access_size];

            handle_fn(IoParams {
                address: physical_address,
                operation: IoOperation::Read(data),
            })?;

            let mut reg_value: u64 = 0;
            for (i, byte) in data.iter().enumerate() {
                reg_value |= (*byte as u64) << (i * 8);
            }

            // Sign extend if necessary
            if sse == 1 {
                let shift = 64 - (access_size * 8);
                reg_value = ((reg_value as i64) << shift >> shift) as u64;
            }

            self.set_reg(srt as u8, reg_value)?;
        }

        // Advance PC past the faulting instruction
        let pc = self.get_pc()?;
        self.set_pc(pc + 4)?;

        Ok(())
    }

    fn handle_io(&self, _handle_fn: &mut dyn FnMut(IoParams)) -> Result<()> {
        // ARM doesn't have IO ports
        Err(Error::new(ENOTSUP))
    }

    fn on_suspend(&self) -> Result<()> {
        Ok(())
    }

    unsafe fn enable_raw_capability(&self, _cap: u32, _args: &[u64; 4]) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }
}

impl HvfVcpu {
    fn get_reg(&self, reg: u8) -> Result<u64> {
        let hv_reg = match reg {
            0..=30 => {
                // SAFETY: hv_reg_t is #[repr(u32)] with contiguous variants HV_REG_X0(0)
                // through HV_REG_X30(30). The match arm restricts reg to 0..=30, which
                // are all valid discriminant values.
                unsafe { std::mem::transmute::<u32, hv_reg_t>(reg as u32) }
            }
            31 => return self.get_pc(),
            _ => return Err(Error::new(EINVAL)),
        };

        let mut value: u64 = 0;
        // SAFETY: We're reading from a valid VCPU register
        let ret = unsafe { hv_vcpu_get_reg(self.vcpu, hv_reg, &mut value) };
        hv_result(ret)?;
        Ok(value)
    }

    fn set_reg(&self, reg: u8, value: u64) -> Result<()> {
        let hv_reg = match reg {
            0..=30 => {
                // SAFETY: hv_reg_t is #[repr(u32)] with contiguous variants HV_REG_X0(0)
                // through HV_REG_X30(30). The match arm restricts reg to 0..=30, which
                // are all valid discriminant values.
                unsafe { std::mem::transmute::<u32, hv_reg_t>(reg as u32) }
            }
            31 => return self.set_pc(value),
            _ => return Err(Error::new(EINVAL)),
        };

        // SAFETY: We're writing to a valid VCPU register
        let ret = unsafe { hv_vcpu_set_reg(self.vcpu, hv_reg, value) };
        hv_result(ret)
    }

    fn get_pc(&self) -> Result<u64> {
        let mut value: u64 = 0;
        // SAFETY: We're reading from a valid VCPU register
        let ret = unsafe { hv_vcpu_get_reg(self.vcpu, hv_reg_t::HV_REG_PC, &mut value) };
        hv_result(ret)?;
        Ok(value)
    }

    fn set_pc(&self, value: u64) -> Result<()> {
        // SAFETY: We're writing to a valid VCPU register
        let ret = unsafe { hv_vcpu_set_reg(self.vcpu, hv_reg_t::HV_REG_PC, value) };
        hv_result(ret)
    }

    /// Get the raw HVF VCPU handle for timer-related operations.
    pub fn vcpu_handle(&self) -> hv_vcpu_t {
        self.vcpu
    }

    /// Set the virtual timer offset.
    pub fn set_vtimer_offset(&self, offset: u64) -> Result<()> {
        // SAFETY: We're setting the timer offset on a valid VCPU
        let ret = unsafe { hv_vcpu_set_vtimer_offset(self.vcpu, offset) };
        hv_result(ret)
    }

    /// Mask or unmask the virtual timer.
    pub fn set_vtimer_mask(&self, masked: bool) -> Result<()> {
        // SAFETY: We're setting the timer mask on a valid VCPU
        let ret = unsafe { hv_vcpu_set_vtimer_mask(self.vcpu, masked) };
        hv_result(ret)
    }

    /// Set a pending interrupt on the VCPU.
    pub fn set_pending_interrupt(&self, int_type: hv_interrupt_type_t, pending: bool) -> Result<()> {
        // SAFETY: We're setting a pending interrupt on a valid VCPU
        let ret = unsafe { hv_vcpu_set_pending_interrupt(self.vcpu, int_type, pending) };
        hv_result(ret)
    }

    /// Get SP_EL1 (kernel stack pointer at EL1).
    pub fn get_sp_el1(&self) -> Result<u64> {
        let mut value: u64 = 0;
        // SAFETY: We're reading a valid system register
        let ret = unsafe {
            hv_vcpu_get_sys_reg(self.vcpu, hv_sys_reg_t::HV_SYS_REG_SP_EL1, &mut value)
        };
        hv_result(ret)?;
        Ok(value)
    }

    /// Get MPIDR_EL1 (multiprocessor affinity register).
    pub fn get_mpidr_el1(&self) -> Result<u64> {
        let mut value: u64 = 0;
        // SAFETY: We're reading a valid system register
        let ret = unsafe {
            hv_vcpu_get_sys_reg(self.vcpu, hv_sys_reg_t::HV_SYS_REG_MPIDR_EL1, &mut value)
        };
        hv_result(ret)?;
        Ok(value)
    }

    /// Get SP_EL0 (userspace stack pointer).
    pub fn get_sp_el0(&self) -> Result<u64> {
        let mut value: u64 = 0;
        // SAFETY: We're reading a valid system register
        let ret = unsafe {
            hv_vcpu_get_sys_reg(self.vcpu, hv_sys_reg_t::HV_SYS_REG_SP_EL0, &mut value)
        };
        hv_result(ret)?;
        Ok(value)
    }

    /// Get CPSR/PSTATE (current program status register).
    pub fn get_cpsr(&self) -> Result<u64> {
        let mut value: u64 = 0;
        // SAFETY: We're reading a valid register
        let ret = unsafe { hv_vcpu_get_reg(self.vcpu, hv_reg_t::HV_REG_CPSR, &mut value) };
        hv_result(ret)?;
        Ok(value)
    }
}

impl VcpuAArch64 for HvfVcpu {
    fn init(&self, _features: &[VcpuFeature]) -> Result<()> {
        // Hypervisor.framework doesn't require explicit initialization
        Ok(())
    }

    fn init_pmu(&self, _irq: u64) -> Result<()> {
        // PMU not supported
        Err(Error::new(ENOTSUP))
    }

    fn has_pvtime_support(&self) -> bool {
        false
    }

    fn init_pvtime(&self, _pvtime_ipa: u64) -> Result<()> {
        Err(Error::new(ENOTSUP))
    }

    fn set_one_reg(&self, reg_id: VcpuRegAArch64, data: u64) -> Result<()> {
        match reg_id {
            VcpuRegAArch64::X(n @ 0..=30) => {
                // SAFETY: hv_reg_t is #[repr(u32)] with contiguous variants HV_REG_X0(0)
                // through HV_REG_X30(30). The match arm restricts n to 0..=30, which
                // are all valid discriminant values.
                let hv_reg = unsafe { std::mem::transmute::<u32, hv_reg_t>(n as u32) };
                // SAFETY: We're writing to a valid VCPU register
                let ret = unsafe { hv_vcpu_set_reg(self.vcpu, hv_reg, data) };
                hv_result(ret)
            }
            VcpuRegAArch64::X(_) => Err(Error::new(EINVAL)),
            VcpuRegAArch64::Sp => {
                // SP_EL0
                // SAFETY: We're writing to a valid system register
                let ret = unsafe {
                    hv_vcpu_set_sys_reg(self.vcpu, hv_sys_reg_t::HV_SYS_REG_SP_EL0, data)
                };
                hv_result(ret)
            }
            VcpuRegAArch64::Pc => {
                // SAFETY: We're writing to a valid VCPU register
                let ret = unsafe { hv_vcpu_set_reg(self.vcpu, hv_reg_t::HV_REG_PC, data) };
                hv_result(ret)
            }
            VcpuRegAArch64::Pstate => {
                // CPSR
                // SAFETY: We're writing to a valid VCPU register
                let ret = unsafe { hv_vcpu_set_reg(self.vcpu, hv_reg_t::HV_REG_CPSR, data) };
                hv_result(ret)
            }
            VcpuRegAArch64::System(sys_reg) => {
                // Try to map the system register - this is a simplified mapping
                let hv_sys_reg = self.map_sys_reg_to_hvf(sys_reg)?;
                // SAFETY: We're writing to a valid system register
                let ret = unsafe { hv_vcpu_set_sys_reg(self.vcpu, hv_sys_reg, data) };
                hv_result(ret)
            }
        }
    }

    fn get_one_reg(&self, reg_id: VcpuRegAArch64) -> Result<u64> {
        let mut value: u64 = 0;

        match reg_id {
            VcpuRegAArch64::X(n @ 0..=30) => {
                // SAFETY: hv_reg_t is #[repr(u32)] with contiguous variants HV_REG_X0(0)
                // through HV_REG_X30(30). The match arm restricts n to 0..=30, which
                // are all valid discriminant values.
                let hv_reg = unsafe { std::mem::transmute::<u32, hv_reg_t>(n as u32) };
                // SAFETY: We're reading from a valid VCPU register
                let ret = unsafe { hv_vcpu_get_reg(self.vcpu, hv_reg, &mut value) };
                hv_result(ret)?;
            }
            VcpuRegAArch64::X(_) => return Err(Error::new(EINVAL)),
            VcpuRegAArch64::Sp => {
                // SP_EL0
                // SAFETY: We're reading from a valid system register
                let ret = unsafe {
                    hv_vcpu_get_sys_reg(self.vcpu, hv_sys_reg_t::HV_SYS_REG_SP_EL0, &mut value)
                };
                hv_result(ret)?;
            }
            VcpuRegAArch64::Pc => {
                // SAFETY: We're reading from a valid VCPU register
                let ret = unsafe { hv_vcpu_get_reg(self.vcpu, hv_reg_t::HV_REG_PC, &mut value) };
                hv_result(ret)?;
            }
            VcpuRegAArch64::Pstate => {
                // CPSR
                // SAFETY: We're reading from a valid VCPU register
                let ret = unsafe { hv_vcpu_get_reg(self.vcpu, hv_reg_t::HV_REG_CPSR, &mut value) };
                hv_result(ret)?;
            }
            VcpuRegAArch64::System(sys_reg) => {
                let hv_sys_reg = self.map_sys_reg_to_hvf(sys_reg)?;
                // SAFETY: We're reading from a valid system register
                let ret = unsafe { hv_vcpu_get_sys_reg(self.vcpu, hv_sys_reg, &mut value) };
                hv_result(ret)?;
            }
        }

        Ok(value)
    }

    fn set_vector_reg(&self, reg_num: u8, data: u128) -> Result<()> {
        if reg_num > 31 {
            return Err(Error::new(EINVAL));
        }

        // SAFETY: hv_simd_fp_reg_t is #[repr(u32)] with contiguous variants
        // HV_SIMD_FP_REG_Q0(0) through HV_SIMD_FP_REG_Q31(31). reg_num is
        // checked to be <= 31 above.
        let hv_reg = unsafe { std::mem::transmute::<u32, hv_simd_fp_reg_t>(reg_num as u32) };
        let value: hv_simd_fp_uchar16_t = data.to_le_bytes();

        // SAFETY: We're writing to a valid SIMD/FP register
        let ret = unsafe { hv_vcpu_set_simd_fp_reg(self.vcpu, hv_reg, value) };
        hv_result(ret)
    }

    fn get_vector_reg(&self, reg_num: u8) -> Result<u128> {
        if reg_num > 31 {
            return Err(Error::new(EINVAL));
        }

        // SAFETY: hv_simd_fp_reg_t is #[repr(u32)] with contiguous variants
        // HV_SIMD_FP_REG_Q0(0) through HV_SIMD_FP_REG_Q31(31). reg_num is
        // checked to be <= 31 above.
        let hv_reg = unsafe { std::mem::transmute::<u32, hv_simd_fp_reg_t>(reg_num as u32) };
        let mut value: hv_simd_fp_uchar16_t = [0; 16];

        // SAFETY: We're reading from a valid SIMD/FP register
        let ret = unsafe { hv_vcpu_get_simd_fp_reg(self.vcpu, hv_reg, &mut value) };
        hv_result(ret)?;

        Ok(u128::from_le_bytes(value))
    }

    fn get_system_regs(&self) -> Result<BTreeMap<AArch64SysRegId, u64>> {
        // Return a subset of commonly used system registers
        let mut regs = BTreeMap::new();

        // Read some commonly used system registers
        let common_regs = [
            hv_sys_reg_t::HV_SYS_REG_SCTLR_EL1,
            hv_sys_reg_t::HV_SYS_REG_TTBR0_EL1,
            hv_sys_reg_t::HV_SYS_REG_TTBR1_EL1,
            hv_sys_reg_t::HV_SYS_REG_TCR_EL1,
            hv_sys_reg_t::HV_SYS_REG_VBAR_EL1,
            hv_sys_reg_t::HV_SYS_REG_MAIR_EL1,
            hv_sys_reg_t::HV_SYS_REG_SPSR_EL1,
            hv_sys_reg_t::HV_SYS_REG_ELR_EL1,
        ];

        for hv_reg in common_regs {
            let mut value: u64 = 0;
            // SAFETY: We're reading from valid system registers
            let ret = unsafe { hv_vcpu_get_sys_reg(self.vcpu, hv_reg, &mut value) };
            if hv_result(ret).is_ok() {
                if let Some(sys_reg) = self.map_hvf_to_sys_reg(hv_reg) {
                    regs.insert(sys_reg, value);
                }
            }
        }

        Ok(regs)
    }

    fn hypervisor_specific_snapshot(&self) -> anyhow::Result<AnySnapshot> {
        // Minimal snapshot support - return an empty snapshot
        AnySnapshot::to_any(())
    }

    fn hypervisor_specific_restore(&self, _data: AnySnapshot) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_psci_version(&self) -> Result<crate::PsciVersion> {
        // Hypervisor.framework uses PSCI 1.0
        Ok(crate::PSCI_1_0)
    }

    fn set_guest_debug(&self, _addrs: &[GuestAddress], _enable_singlestep: bool) -> Result<()> {
        // Guest debugging not supported
        Err(Error::new(ENOTSUP))
    }

    fn get_max_hw_bps(&self) -> Result<usize> {
        // Hardware breakpoints not exposed
        Ok(0)
    }

    fn get_cache_info(&self) -> Result<BTreeMap<u8, u64>> {
        // Cache info not available
        Ok(BTreeMap::new())
    }

    fn set_cache_info(&self, _cache_info: BTreeMap<u8, u64>) -> Result<()> {
        // Cache info setting not supported
        Err(Error::new(ENOTSUP))
    }
}

impl HvfVcpu {
    /// Map an AArch64SysRegId to an HVF system register
    fn map_sys_reg_to_hvf(&self, sys_reg: AArch64SysRegId) -> Result<hv_sys_reg_t> {
        // This is a simplified mapping - a complete implementation would need
        // to handle all possible system registers

        // Extract all fields from the register ID
        let op0 = sys_reg.op0();
        let op1 = sys_reg.op1();
        let crn = sys_reg.crn();
        let crm = sys_reg.crm();
        let op2 = sys_reg.op2();

        // Map common registers using all five encoding fields (op0, op1, crn, crm, op2)
        match (op0, op1, crn, crm, op2) {
            (3, 0, 1, 0, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_SCTLR_EL1),
            (3, 0, 2, 0, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_TTBR0_EL1),
            (3, 0, 2, 0, 1) => Ok(hv_sys_reg_t::HV_SYS_REG_TTBR1_EL1),
            (3, 0, 2, 0, 2) => Ok(hv_sys_reg_t::HV_SYS_REG_TCR_EL1),
            (3, 0, 4, 0, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_SPSR_EL1),
            (3, 0, 4, 0, 1) => Ok(hv_sys_reg_t::HV_SYS_REG_ELR_EL1),
            (3, 0, 4, 1, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_SP_EL0),
            (3, 0, 5, 1, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_AFSR0_EL1),
            (3, 0, 5, 1, 1) => Ok(hv_sys_reg_t::HV_SYS_REG_AFSR1_EL1),
            (3, 0, 5, 2, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_ESR_EL1),
            (3, 0, 6, 0, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_FAR_EL1),
            (3, 0, 10, 2, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_MAIR_EL1),
            (3, 0, 10, 3, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_AMAIR_EL1),
            (3, 0, 12, 0, 0) => Ok(hv_sys_reg_t::HV_SYS_REG_VBAR_EL1),
            (3, 0, 13, 0, 1) => Ok(hv_sys_reg_t::HV_SYS_REG_CONTEXTIDR_EL1),
            _ => Err(Error::new(EINVAL)),
        }
    }

    /// Map an HVF system register to an AArch64SysRegId
    fn map_hvf_to_sys_reg(&self, hv_reg: hv_sys_reg_t) -> Option<AArch64SysRegId> {
        // This is a reverse mapping - incomplete but covers basics
        match hv_reg {
            hv_sys_reg_t::HV_SYS_REG_SCTLR_EL1 => {
                Some(AArch64SysRegId::new_unchecked(3, 0, 1, 0, 0))
            }
            hv_sys_reg_t::HV_SYS_REG_TTBR0_EL1 => {
                Some(AArch64SysRegId::new_unchecked(3, 0, 2, 0, 0))
            }
            hv_sys_reg_t::HV_SYS_REG_TTBR1_EL1 => {
                Some(AArch64SysRegId::new_unchecked(3, 0, 2, 0, 1))
            }
            hv_sys_reg_t::HV_SYS_REG_TCR_EL1 => Some(AArch64SysRegId::new_unchecked(3, 0, 2, 0, 2)),
            hv_sys_reg_t::HV_SYS_REG_MAIR_EL1 => {
                Some(AArch64SysRegId::new_unchecked(3, 0, 10, 2, 0))
            }
            hv_sys_reg_t::HV_SYS_REG_VBAR_EL1 => {
                Some(AArch64SysRegId::new_unchecked(3, 0, 12, 0, 0))
            }
            hv_sys_reg_t::HV_SYS_REG_SPSR_EL1 => {
                Some(AArch64SysRegId::new_unchecked(3, 0, 4, 0, 0))
            }
            hv_sys_reg_t::HV_SYS_REG_ELR_EL1 => Some(AArch64SysRegId::new_unchecked(3, 0, 4, 0, 1)),
            _ => None,
        }
    }
}

impl AsRawDescriptor for HvfVm {
    fn as_raw_descriptor(&self) -> base::RawDescriptor {
        // Hypervisor.framework doesn't use file descriptors
        // Return -1 to indicate no descriptor
        -1
    }
}
