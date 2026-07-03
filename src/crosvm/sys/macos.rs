// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MacOS platform support for crosvm.
//!
//! This module provides the macOS-specific implementation for running VMs using
//! Apple's Hypervisor.framework on arm64 (Apple Silicon) Macs.

pub mod cmdline;
pub mod config;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Seek;
use std::io::stdin;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::debug;
use base::error;
use base::info;
use base::MappedRegion;
use base::open_file_or_duplicate;
use base::trace;
use base::warn;
use base::AsRawDescriptor;
use base::Event;
use base::Terminal;
use base::Tube;
use base::UnixSeqpacket;
use base::UnixSeqpacketListener;
use base::UnlinkUnixSeqpacketListener;
use cros_fdt::Fdt;
use devices::serial_device::SerialHardware;
use devices::serial_device::SerialParameters;
use devices::Bus;
use devices::BusDevice;
use devices::BusType;
use devices::IrqEdgeEvent;
use devices::Serial;
use devices::VirtioMmioDevice;
use devices::irqchip::HvfIrqChip;
use devices::irqchip::AARCH64_GIC_DIST_BASE;
use devices::irqchip::AARCH64_GIC_DIST_SIZE;
use devices::irqchip::AARCH64_GIC_REDIST_SIZE;
use devices::irqchip::HVF_GIC_REDIST_REGION_SIZE;
use devices::irqchip::gic_redist_base;
use devices::irqchip::GicCpuInterface;
use devices::irqchip::GIC_SPI_BASE;
use devices::irqchip::GIC_SPURIOUS_INTID;
use devices::irqchip::VTIMER_PPI;
use devices::virtio::base_features;
use devices::virtio::block::BlockAsync;
use devices::virtio::fs::Fs;
#[cfg(feature = "gpu")]
use devices::virtio::gpu::DisplayBackend;
#[cfg(feature = "gpu")]
use devices::virtio::gpu::Gpu;
#[cfg(feature = "gpu")]
use devices::virtio::gpu::GpuParameters;
#[cfg(feature = "audio")]
use devices::virtio::snd::common_backend::VirtioSnd;
#[cfg(feature = "audio")]
use devices::virtio::snd::parameters::Parameters as SndParameters;
#[cfg(feature = "audio")]
use devices::virtio::snd::parameters::StreamSourceBackend;
#[cfg(feature = "audio")]
use devices::virtio::snd::sys::StreamSourceBackend as SysStreamSourceBackend;
use hypervisor::hvf::hv_interrupt_type_t;
use hypervisor::hvf::hv_result;
use hypervisor::hvf::hv_vm_map;
use hypervisor::hvf::Hvf;
use hypervisor::hvf::HvfVcpu;
use hypervisor::hvf::HvfVm;
use hypervisor::hvf::HV_MEMORY_EXEC;
use hypervisor::hvf::HV_MEMORY_READ;
use hypervisor::hvf::HV_MEMORY_WRITE;
use hypervisor::IoOperation;
use hypervisor::IoParams;
use hypervisor::ProtectionType;
use hypervisor::VcpuAArch64;
use hypervisor::VcpuExit;
use hypervisor::VcpuRegAArch64;
use hypervisor::VmAArch64;
use rand::rngs::OsRng;
use rand::RngCore;
use sync::Mutex;
use vm_control::VmRequest;
use vm_control::VmResponse;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;
use vm_memory::MemoryRegionOptions;

use crate::crosvm::config::Config;
use crate::crosvm::config::Executable;

// ARM64 memory layout constants (from aarch64 crate)
const AARCH64_PHYS_MEM_START: u64 = 0x80000000;
const AARCH64_FDT_ALIGN: u64 = 0x200000;

// GIC is at 0xC0000000, so low memory bank can only go up to there.
const AARCH64_LOW_MEM_MAX: u64 = 0xC0000000 - AARCH64_PHYS_MEM_START; // 1 GiB
// High memory bank starts above 4 GiB device space.
const AARCH64_HIGH_MEM_START: u64 = 0x1_0000_0000;

// Serial device constants
const AARCH64_SERIAL_ADDR: u64 = 0x3f8;
const AARCH64_SERIAL_SIZE: u64 = 0x8;
const AARCH64_SERIAL_IRQ: u32 = 0;

// PSR (Processor State Register) bits for EL1h mode with all interrupts masked
const PSR_MODE_EL1H: u64 = 0x00000005;
const PSR_F_BIT: u64 = 0x00000040;
const PSR_I_BIT: u64 = 0x00000080;
const PSR_A_BIT: u64 = 0x00000100;
const PSR_D_BIT: u64 = 0x00000200;

// PSCI function IDs (ARM DEN 0022E)
const PSCI_VERSION: u64 = 0x84000000;
const PSCI_CPU_SUSPEND_32: u64 = 0x84000001;
const PSCI_CPU_SUSPEND_64: u64 = 0xC4000001;
const PSCI_CPU_OFF: u64 = 0x84000002;
const PSCI_CPU_ON_32: u64 = 0x84000003;
const PSCI_CPU_ON_64: u64 = 0xC4000003;
const PSCI_AFFINITY_INFO_32: u64 = 0x84000004;
const PSCI_AFFINITY_INFO_64: u64 = 0xC4000004;
#[allow(dead_code)]
const PSCI_MIGRATE_32: u64 = 0x84000005;
#[allow(dead_code)]
const PSCI_MIGRATE_64: u64 = 0xC4000005;
const PSCI_MIGRATE_INFO_TYPE: u64 = 0x84000006;
#[allow(dead_code)]
const PSCI_MIGRATE_INFO_UP_CPU_32: u64 = 0x84000007;
#[allow(dead_code)]
const PSCI_MIGRATE_INFO_UP_CPU_64: u64 = 0xC4000007;
const PSCI_SYSTEM_OFF: u64 = 0x84000008;
const PSCI_SYSTEM_RESET: u64 = 0x84000009;
const PSCI_FEATURES: u64 = 0x8400000A;
#[allow(dead_code)]
const PSCI_CPU_FREEZE: u64 = 0x8400000B;
#[allow(dead_code)]
const PSCI_CPU_DEFAULT_SUSPEND_32: u64 = 0x8400000C;
#[allow(dead_code)]
const PSCI_CPU_DEFAULT_SUSPEND_64: u64 = 0xC400000C;
#[allow(dead_code)]
const PSCI_NODE_HW_STATE_32: u64 = 0x8400000D;
#[allow(dead_code)]
const PSCI_NODE_HW_STATE_64: u64 = 0xC400000D;
const PSCI_SYSTEM_SUSPEND_32: u64 = 0x8400000E;
const PSCI_SYSTEM_SUSPEND_64: u64 = 0xC400000E;
#[allow(dead_code)]
const PSCI_SET_SUSPEND_MODE: u64 = 0x8400000F;
#[allow(dead_code)]
const PSCI_STAT_RESIDENCY_32: u64 = 0x84000010;
#[allow(dead_code)]
const PSCI_STAT_RESIDENCY_64: u64 = 0xC4000010;
#[allow(dead_code)]
const PSCI_STAT_COUNT_32: u64 = 0x84000011;
#[allow(dead_code)]
const PSCI_STAT_COUNT_64: u64 = 0xC4000011;

// PSCI return codes
const PSCI_SUCCESS: u64 = 0;
const PSCI_NOT_SUPPORTED: u64 = 0xFFFFFFFF; // -1 as i32 cast to u64
const PSCI_MIGRATE_INFO_TYPE_NOT_SUPPORTED: u64 = 2; // Migration not supported

/// VM exit states
#[allow(dead_code)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExitState {
    /// VM stopped normally
    Stop,
    /// VM requested a reset
    Reset,
    /// VM crashed
    Crash,
    /// Guest triggered a panic
    GuestPanic,
    /// Watchdog triggered a reset
    WatchdogReset,
}

/// State for a secondary CPU waiting to be started via PSCI CPU_ON.
#[derive(Clone)]
struct CpuStartupRequest {
    /// Entry point address for the CPU
    entry_point: u64,
    /// Context ID to pass in X0
    context_id: u64,
}

/// Shared state for coordinating VCPU startup (PSCI CPU_ON).
struct VcpuCoordinator {
    /// Pending startup requests for each VCPU, keyed by VCPU ID
    startup_requests: std::sync::Mutex<HashMap<usize, CpuStartupRequest>>,
    /// Condition variable to wake up waiting VCPUs
    startup_signal: Condvar,
    /// Global shutdown flag
    shutdown: AtomicBool,
    /// Active VCPU handles for kicking out of hv_vcpu_run()
    active_vcpus: std::sync::Mutex<HashMap<usize, u64>>,
    /// Flag indicating a device interrupt is pending and needs to be injected
    device_irq_pending: AtomicBool,
    /// The userspace GIC irq chip (for ICC register handling and interrupt state)
    irq_chip: Arc<HvfIrqChip>,
    /// Flag indicating whether in-kernel GIC is available
    has_in_kernel_gic: AtomicBool,
}

impl VcpuCoordinator {
    fn new(irq_chip: Arc<HvfIrqChip>, has_in_kernel_gic: bool) -> Self {
        Self {
            startup_requests: std::sync::Mutex::new(HashMap::new()),
            startup_signal: Condvar::new(),
            shutdown: AtomicBool::new(false),
            active_vcpus: std::sync::Mutex::new(HashMap::new()),
            device_irq_pending: AtomicBool::new(false),
            irq_chip,
            has_in_kernel_gic: AtomicBool::new(has_in_kernel_gic),
        }
    }

    /// Register an active VCPU handle for interrupt kicking.
    fn register_vcpu(&self, cpu_id: usize, vcpu_handle: u64) {
        self.active_vcpus
            .lock()
            .expect("VcpuCoordinator mutex poisoned")
            .insert(cpu_id, vcpu_handle);
    }

    /// Unregister a VCPU (e.g. on CPU_OFF or shutdown).
    fn unregister_vcpu(&self, cpu_id: usize) {
        self.active_vcpus
            .lock()
            .expect("VcpuCoordinator mutex poisoned")
            .remove(&cpu_id);
    }

    /// Kick all active VCPUs to exit hv_vcpu_run() for interrupt handling.
    fn kick_vcpus(&self) {
        use hypervisor::hvf::hv_vcpus_exit;

        let vcpus = self.active_vcpus.lock().expect("VcpuCoordinator mutex poisoned");
        for (&_cpu_id, &vcpu_handle) in vcpus.iter() {
            // SAFETY: vcpu_handle is a valid HVF handle stored by register_vcpu.
            let ret = unsafe { hv_vcpus_exit(&vcpu_handle, 1) };
            if ret != 0 {
                warn!("Failed to kick VCPU {}: error {}", _cpu_id, ret);
            }
        }
    }

    /// Signal that a device interrupt is pending and needs to be injected
    fn set_device_irq_pending(&self) {
        self.device_irq_pending.store(true, Ordering::Release);
    }

    /// Check and clear the device IRQ pending flag, returning true if it was set
    fn take_device_irq_pending(&self) -> bool {
        self.device_irq_pending.swap(false, Ordering::AcqRel)
    }

    /// Request a CPU to start (called from boot CPU handling PSCI CPU_ON)
    fn request_cpu_start(&self, vcpu_id: usize, entry_point: u64, context_id: u64) {
        let mut requests = self.startup_requests.lock().expect("VcpuCoordinator mutex poisoned");
        requests.insert(vcpu_id, CpuStartupRequest { entry_point, context_id });
        self.startup_signal.notify_all();
    }

    /// Wait for a startup request for this VCPU (called from secondary VCPUs)
    fn wait_for_startup(&self, vcpu_id: usize) -> Option<CpuStartupRequest> {
        let mut requests = self.startup_requests.lock().expect("VcpuCoordinator mutex poisoned");
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(request) = requests.remove(&vcpu_id) {
                return Some(request);
            }
            requests = self.startup_signal.wait(requests).expect("VcpuCoordinator mutex poisoned");
        }
    }

    /// Signal all VCPUs to shut down
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.startup_signal.notify_all();
    }

    /// Request VM shutdown from outside the VCPU threads.
    fn request_shutdown(&self) {
        self.signal_shutdown();
        self.kick_vcpus();
    }
}

fn start_control_server(
    socket_path: Option<PathBuf>,
    coordinator: Arc<VcpuCoordinator>,
) -> Result<Option<JoinHandle<()>>> {
    let Some(socket_path) = socket_path else {
        return Ok(None);
    };

    let listener = UnlinkUnixSeqpacketListener(
        UnixSeqpacketListener::bind(&socket_path)
            .with_context(|| format!("failed to bind control socket {}", socket_path.display()))?,
    );

    let thread = thread::Builder::new()
        .name("control-server".to_string())
        .spawn(move || {
            while !coordinator.shutdown.load(Ordering::Relaxed) {
                match listener.accept_with_timeout(Duration::from_millis(100)) {
                    Ok(socket) => handle_control_connection(socket, &coordinator),
                    Err(e) if e.raw_os_error() == Some(libc::ETIMEDOUT) => {}
                    Err(e) => {
                        warn!("control server accept failed: {}", e);
                        break;
                    }
                }
            }
        })
        .context("failed to spawn control server thread")?;

    Ok(Some(thread))
}

fn handle_control_connection(socket: UnixSeqpacket, coordinator: &VcpuCoordinator) {
    let tube = match Tube::try_from(socket) {
        Ok(tube) => tube,
        Err(e) => {
            warn!("failed to create control tube: {}", e);
            return;
        }
    };

    let request = match tube.recv::<VmRequest>() {
        Ok(request) => request,
        Err(e) => {
            warn!("failed to receive control request: {}", e);
            return;
        }
    };

    let response = match request {
        VmRequest::Exit => {
            coordinator.request_shutdown();
            VmResponse::Ok
        }
        request => VmResponse::ErrString(format!(
            "control request {:?} is not supported on macOS",
            request
        )),
    };

    if let Err(e) = tube.send(&response) {
        warn!("failed to send control response: {}", e);
    }
}

/// Run a VM with the given configuration.
///
/// This is the main entry point for running a VM on macOS using Hypervisor.framework.
pub fn run_config(cfg: Config) -> Result<ExitState> {
    info!("Starting VM on macOS with Hypervisor.framework");

    // Put the terminal into raw mode so keystrokes are delivered to the guest
    // immediately without line buffering or local echo.
    stdin()
        .set_raw_mode()
        .context("failed to set terminal raw mode")?;

    // Install a panic hook that restores the terminal to canonical mode before
    // aborting. Without this, a panic leaves the terminal in raw mode and the
    // user's shell becomes unusable.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort terminal restore; ignore errors since we're panicking.
        let _ = stdin().set_canon_mode();
        default_panic(info);
    }));

    // Get the kernel image
    let mut kernel_image = match &cfg.executable_path {
        Some(Executable::Kernel(ref kernel_path)) => {
            open_file_or_duplicate(kernel_path, OpenOptions::new().read(true))
                .with_context(|| format!("failed to open kernel image {}", kernel_path.display()))?
        }
        Some(Executable::Bios(ref bios_path)) => {
            open_file_or_duplicate(bios_path, OpenOptions::new().read(true))
                .with_context(|| format!("failed to open bios {}", bios_path.display()))?
        }
        None => bail!("No kernel or BIOS image specified"),
    };

    // Get memory size
    let memory_size = cfg.memory.unwrap_or(512) * 1024 * 1024; // Default 512 MB
    let vcpu_count = cfg.vcpu_count.unwrap_or(1);

    info!(
        "VM configuration: memory={}MB, vcpus={}",
        memory_size / (1024 * 1024),
        vcpu_count
    );

    // Create the hypervisor
    let hvf = Hvf::new().context("failed to create HVF hypervisor")?;
    debug!("HVF hypervisor created");

    // Create guest memory layout.
    // The GIC is mapped at 0xC0000000, so RAM must avoid that region.
    // For memory > 1 GiB, split into a low bank (below GIC) and a high bank (above 4 GiB).
    let low_size = memory_size.min(AARCH64_LOW_MEM_MAX);
    let high_size = memory_size.saturating_sub(AARCH64_LOW_MEM_MAX);

    let mut guest_mem_layout = vec![(
        GuestAddress(AARCH64_PHYS_MEM_START),
        low_size,
        MemoryRegionOptions::new(),
    )];
    if high_size > 0 {
        guest_mem_layout.push((
            GuestAddress(AARCH64_HIGH_MEM_START),
            high_size,
            MemoryRegionOptions::new(),
        ));
    }

    let guest_mem = GuestMemory::new_with_options(&guest_mem_layout)
        .context("failed to create guest memory")?;
    debug!("Guest memory created: {} bytes at {:#x}", memory_size, AARCH64_PHYS_MEM_START);

    // Create the VM - HvfVm::new calls hv_vm_create()
    let vm = Arc::new(HvfVm::new(&hvf, guest_mem.clone()).context("failed to create HVF VM")?);
    debug!("HVF VM created");

    // Map the guest memory into the VM's address space using hv_vm_map
    // GuestMemory has already allocated the host memory, we just need to map it
    for region in guest_mem.regions() {
        let host_addr = region.host_addr as *mut std::ffi::c_void;
        let guest_addr = region.guest_addr.offset();
        let size = region.size;
        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;

        // SAFETY: host_addr points to a GuestMemory region that remains live for the
        // VM's lifetime, and size matches the region size.
        let ret = unsafe { hv_vm_map(host_addr, guest_addr, size, flags) };
        hv_result(ret).context("failed to map guest memory region")?;

        debug!(
            "Mapped memory region: guest={:#x}, host={:p}, size={:#x}",
            guest_addr, host_addr, size
        );
    }

    // Create the userspace GIC emulation
    // This provides GICv3 emulation that works with HVF's interrupt injection
    let irq_chip = Arc::new(HvfIrqChip::new(vcpu_count)
        .context("failed to create userspace GIC")?);
    let redist_base = gic_redist_base(vcpu_count);

    // Query HVF's expected GIC sizes for diagnostics
    if let Ok(dist_size) = HvfVm::gic_get_distributor_size() {
        info!("HVF GIC distributor size: {:#x}", dist_size);
    }
    if let Ok(redist_size) = HvfVm::gic_get_redistributor_size() {
        info!("HVF GIC redistributor size (per CPU): {:#x}", redist_size);
    }
    if let Ok(redist_region_size) = HvfVm::gic_get_redistributor_region_size() {
        info!("HVF GIC redistributor region size: {:#x}", redist_region_size);
    }

    // For now, skip in-kernel GIC and use userspace GIC only
    // The in-kernel GIC has issues with redistributor discovery
    // TODO: Investigate HVF in-kernel GIC requirements (MPIDR, VCPU creation order, etc.)
    //
    // Disabled in-kernel GIC - it fails redistributor discovery with MPIDR=0
    // The VCPUs need to be created before hv_gic_create() but they're created
    // on their own threads asynchronously.
    let has_in_kernel_gic = false;
    info!("Using userspace GIC (in-kernel GIC disabled)");

    // Load the kernel
    let kernel_start = GuestAddress(AARCH64_PHYS_MEM_START);
    let loaded_kernel = kernel_loader::load_arm64_kernel(&guest_mem, kernel_start, &mut kernel_image)
        .context("failed to load kernel")?;

    info!(
        "Kernel loaded at {:#x}, entry={:#x}, size={}",
        kernel_start.offset(),
        loaded_kernel.entry.offset(),
        loaded_kernel.size
    );

    // Load initrd if specified
    let initrd_range = if let Some(ref initrd_path) = cfg.initrd_path {
        let mut initrd_file =
            open_file_or_duplicate(initrd_path, OpenOptions::new().read(true))
                .with_context(|| {
                    format!("failed to open initrd {}", initrd_path.display())
                })?;
        let initrd_start = GuestAddress(
            ((loaded_kernel.address_range.end + AARCH64_FDT_ALIGN)
                / AARCH64_FDT_ALIGN)
                * AARCH64_FDT_ALIGN,
        );
        let initrd_size = std::io::Read::read_to_end(&mut initrd_file, &mut Vec::new())
            .context("failed to get initrd size")?;
        initrd_file
            .seek(std::io::SeekFrom::Start(0))
            .context("failed to seek initrd")?;
        let mut initrd_data = vec![0u8; initrd_size];
        std::io::Read::read_exact(&mut initrd_file, &mut initrd_data)
            .context("failed to read initrd")?;
        guest_mem
            .write_at_addr(&initrd_data, initrd_start)
            .context("failed to write initrd to guest memory")?;
        info!(
            "Initrd loaded at {:#x}, size={}",
            initrd_start.offset(),
            initrd_size
        );
        Some((initrd_start.offset(), initrd_start.offset() + initrd_size as u64))
    } else {
        None
    };

    // Calculate FDT address (after kernel and initrd)
    let last_loaded_end = if let Some((_, end)) = initrd_range {
        end
    } else {
        loaded_kernel.address_range.end
    };
    let fdt_address = GuestAddress(
        ((last_loaded_end + 1 + AARCH64_FDT_ALIGN - 1) / AARCH64_FDT_ALIGN) * AARCH64_FDT_ALIGN,
    );
    debug!("FDT will be placed at {:#x}", fdt_address.offset());

    // Create a minimal device tree for the kernel
    // Pass the number of virtio-mmio devices so nodes can be added to FDT
    let mut num_mmio_devices = cfg.disks.len() + cfg.shared_dirs.len();
    #[cfg(feature = "audio")]
    {
        num_mmio_devices += 1; // virtio-snd
    }
    #[cfg(feature = "gpu")]
    {
        num_mmio_devices += 1; // virtio-gpu
    }
    let mut fdt = create_minimal_fdt(
        low_size,
        high_size,
        vcpu_count,
        fdt_address,
        num_mmio_devices,
        &cfg.params,
        initrd_range,
    )?;
    let fdt_data = fdt.finish().context("failed to finish FDT")?;

    // Write FDT to guest memory
    guest_mem
        .write_at_addr(&fdt_data, fdt_address)
        .context("failed to write FDT to guest memory")?;
    debug!("FDT written to guest memory, size={}", fdt_data.len());

    // Set up the MMIO bus for devices
    let mmio_bus = Arc::new(Bus::new(BusType::Mmio));

    // Only register userspace GIC on MMIO bus if in-kernel GIC is NOT available
    // When in-kernel GIC is available, HVF handles GIC MMIO accesses directly
    if !has_in_kernel_gic {
        irq_chip.register_devices(&mmio_bus)
            .context("failed to register GIC devices on MMIO bus")?;
        debug!("Userspace GIC devices registered on MMIO bus");
    } else {
        debug!("Using in-kernel GIC, userspace GIC not registered on MMIO bus");
    }

    // Set up serial device
    let serial_evt = Event::new().context("failed to create serial event")?;
    let serial = create_serial_device(&cfg, &serial_evt)?;
    let serial = Arc::new(Mutex::new(serial));

    mmio_bus
        .insert(serial.clone(), AARCH64_SERIAL_ADDR, AARCH64_SERIAL_SIZE)
        .context("failed to insert serial device into MMIO bus")?;
    debug!(
        "Serial device added at MMIO address {:#x}",
        AARCH64_SERIAL_ADDR
    );

    // Set up block devices (if any)
    // Block devices are added to MMIO addresses starting at AARCH64_MMIO_BASE
    const AARCH64_MMIO_BASE: u64 = 0x10000;
    const VIRTIO_MMIO_SIZE: u64 = 0x200;
    let mut next_mmio_addr = AARCH64_MMIO_BASE;
    let mut next_irq = 1u32; // Start after serial IRQ (0)

    // Collect interrupt events for block devices
    let mut block_irq_events: Vec<(IrqEdgeEvent, u32)> = Vec::new();

    for (i, disk_option) in cfg.disks.iter().enumerate() {
        info!(
            "Setting up block device {}: path={:?}, read_only={}",
            i, disk_option.path, disk_option.read_only
        );

        // Open the disk file
        let disk = match disk_option.open() {
            Ok(disk) => disk,
            Err(e) => {
                error!("Failed to open disk {}: {}", i, e);
                continue;
            }
        };

        // Create the BlockAsync device
        let block_device = match BlockAsync::new(
            base_features(hypervisor::ProtectionType::Unprotected),
            disk,
            disk_option,
            None, // No control tube for now
            None, // Use default queue size
            None, // Use default num queues
        ) {
            Ok(device) => device,
            Err(e) => {
                error!("Failed to create block device {}: {}", i, e);
                continue;
            }
        };

        // Wrap in VirtioMmioDevice
        let mut mmio_device = match VirtioMmioDevice::new(guest_mem.clone(), Box::new(block_device), false) {
            Ok(device) => device,
            Err(e) => {
                error!("Failed to create VirtioMmioDevice for block {}: {}", i, e);
                continue;
            }
        };

        // Create and assign interrupt event
        let mmio_addr = next_mmio_addr;
        let irq = next_irq;

        let irq_evt = match IrqEdgeEvent::new() {
            Ok(evt) => evt,
            Err(e) => {
                error!("Failed to create IrqEdgeEvent for block {}: {}", i, e);
                continue;
            }
        };

        // Clone the event for the interrupt handler before moving into assign_irq
        match irq_evt.try_clone() {
            Ok(cloned_evt) => {
                info!(
                    "IRQ event for block {}: original fd={}, cloned fd={}",
                    i,
                    irq_evt.get_trigger().as_raw_descriptor(),
                    cloned_evt.get_trigger().as_raw_descriptor()
                );
                block_irq_events.push((cloned_evt, irq));
            }
            Err(e) => {
                error!("Failed to clone IrqEdgeEvent for block {}: {}", i, e);
                continue;
            }
        }

        mmio_device.assign_irq(&irq_evt, irq);

        // Add to MMIO bus
        mmio_bus
            .insert(Arc::new(Mutex::new(mmio_device)), mmio_addr, VIRTIO_MMIO_SIZE)
            .context("failed to insert block device into MMIO bus")?;

        info!(
            "Block device {} added at MMIO address {:#x} with IRQ {}",
            i, mmio_addr, irq
        );

        // Advance to next slot
        next_mmio_addr += VIRTIO_MMIO_SIZE;
        next_irq += 1;
    }

    // Set up virtiofs shared directory devices (if any)
    let mut fs_irq_events: Vec<(IrqEdgeEvent, u32)> = Vec::new();

    for (i, shared_dir) in cfg.shared_dirs.iter().enumerate() {
        info!(
            "Setting up virtiofs device {}: src={:?}, tag={}",
            i, shared_dir.src, shared_dir.tag
        );

        let source_dir = shared_dir.src.to_string_lossy();
        let fs_device = match Fs::new_without_tube(
            base_features(ProtectionType::Unprotected),
            &shared_dir.tag,
            1, // num_workers
            shared_dir.fs_cfg.clone(),
            &source_dir,
        ) {
            Ok(device) => device,
            Err(e) => {
                error!("Failed to create Fs device for {}: {}", shared_dir.tag, e);
                continue;
            }
        };

        // Wrap in VirtioMmioDevice
        let mut mmio_device = match VirtioMmioDevice::new(
            guest_mem.clone(),
            Box::new(fs_device),
            false,
        ) {
            Ok(device) => device,
            Err(e) => {
                error!("Failed to create VirtioMmioDevice for virtiofs {}: {}", i, e);
                continue;
            }
        };

        let mmio_addr = next_mmio_addr;
        let irq = next_irq;

        let irq_evt = match IrqEdgeEvent::new() {
            Ok(evt) => evt,
            Err(e) => {
                error!("Failed to create IrqEdgeEvent for virtiofs {}: {}", i, e);
                continue;
            }
        };

        match irq_evt.try_clone() {
            Ok(cloned_evt) => {
                fs_irq_events.push((cloned_evt, irq));
            }
            Err(e) => {
                error!("Failed to clone IrqEdgeEvent for virtiofs {}: {}", i, e);
                continue;
            }
        }

        mmio_device.assign_irq(&irq_evt, irq);

        mmio_bus
            .insert(Arc::new(Mutex::new(mmio_device)), mmio_addr, VIRTIO_MMIO_SIZE)
            .context("failed to insert virtiofs device into MMIO bus")?;

        info!(
            "virtiofs device {} (tag={}) added at MMIO address {:#x} with IRQ {}",
            i, shared_dir.tag, mmio_addr, irq
        );

        next_mmio_addr += VIRTIO_MMIO_SIZE;
        next_irq += 1;
    }

    // Set up virtio-snd device with CoreAudio backend
    #[cfg(feature = "audio")]
    let mut snd_irq_events: Vec<(IrqEdgeEvent, u32)> = Vec::new();
    #[cfg(feature = "audio")]
    let _snd_control_host_tube: Option<Tube>;
    #[cfg(feature = "audio")]
    {
        let mut snd_params = SndParameters::default();
        snd_params.backend =
            StreamSourceBackend::Sys(SysStreamSourceBackend::COREAUDIO);
        let (snd_control_tube, snd_host_tube) = Tube::pair()
            .context("failed to create sound control tube")?;
        _snd_control_host_tube = Some(snd_host_tube);

        match VirtioSnd::new(
            base_features(ProtectionType::Unprotected),
            snd_params,
            snd_control_tube,
        ) {
            Ok(snd_device) => {
                let mut mmio_device = match VirtioMmioDevice::new(
                    guest_mem.clone(),
                    Box::new(snd_device),
                    false,
                ) {
                    Ok(device) => device,
                    Err(e) => {
                        error!("Failed to create VirtioMmioDevice for sound: {}", e);
                        info!("Starting VM execution with {} VCPUs", vcpu_count);
                        // Fall through to rest of setup
                        return Err(anyhow::anyhow!("sound device setup failed: {}", e));
                    }
                };

                let mmio_addr = next_mmio_addr;
                let irq = next_irq;

                let irq_evt = IrqEdgeEvent::new()
                    .context("failed to create IrqEdgeEvent for sound")?;

                match irq_evt.try_clone() {
                    Ok(cloned_evt) => {
                        snd_irq_events.push((cloned_evt, irq));
                    }
                    Err(e) => {
                        error!("Failed to clone IrqEdgeEvent for sound: {}", e);
                    }
                }

                mmio_device.assign_irq(&irq_evt, irq);

                mmio_bus
                    .insert(Arc::new(Mutex::new(mmio_device)), mmio_addr, VIRTIO_MMIO_SIZE)
                    .context("failed to insert sound device into MMIO bus")?;

                info!(
                    "Sound device added at MMIO address {:#x} with IRQ {}",
                    mmio_addr, irq
                );

                next_mmio_addr += VIRTIO_MMIO_SIZE;
                next_irq += 1;
            }
            Err(e) => {
                warn!("Failed to create sound device, continuing without audio: {}", e);
            }
        }
    }

    // Set up virtio-gpu device (2D mode)
    #[cfg(feature = "gpu")]
    let mut gpu_irq_events: Vec<(IrqEdgeEvent, u32)> = Vec::new();
    #[cfg(feature = "gpu")]
    let _gpu_shm_mmap: Option<base::MemoryMapping>; // keep allocation alive
    #[cfg(feature = "gpu")]
    {
        let mut gpu_params = GpuParameters::default();
        // Use a 64 MiB SHM region for MMIO transport instead of the 8 GiB PCI BAR default
        const GPU_SHM_SIZE: u64 = 64 << 20;
        gpu_params.pci_bar_size = GPU_SHM_SIZE;

        let (exit_evt_wrtube, _exit_evt_rdtube) = Tube::directional_pair()
            .context("failed to create GPU exit event tube")?;
        let (gpu_control_tube, _gpu_control_resp) = Tube::pair()
            .context("failed to create GPU control tube")?;

        let gpu_device = Gpu::new(
            exit_evt_wrtube,
            gpu_control_tube,
            Vec::new(), // resource_bridges
            vec![DisplayBackend::MacOs, DisplayBackend::Stub],
            &gpu_params,
            None, // rutabaga_server_descriptor
            Vec::new(), // event_devices
            base_features(ProtectionType::Unprotected),
            &std::collections::BTreeMap::new(), // paths
        );

        let mut mmio_device = match VirtioMmioDevice::new(
            guest_mem.clone(),
            Box::new(gpu_device),
            false,
        ) {
            Ok(device) => device,
            Err(e) => {
                error!("Failed to create VirtioMmioDevice for GPU: {}", e);
                return Err(anyhow::anyhow!("GPU device setup failed: {}", e));
            }
        };

        // Allocate a shared memory region for the GPU's host-visible memory.
        // Place it above the high memory bank.
        let gpu_shm_guest_addr = AARCH64_HIGH_MEM_START + high_size;
        let gpu_shm_guest_addr =
            (gpu_shm_guest_addr + 0xFFF) & !0xFFF; // page-align

        let shm_mmap = base::MemoryMappingBuilder::new(GPU_SHM_SIZE as usize)
            .build()
            .context("failed to allocate GPU SHM region")?;

        // SAFETY: shm_mmap is a valid host mapping that persists for the VM lifetime
        // (held in _gpu_shm_mmap). GPU_SHM_SIZE matches the mapping size.
        let ret = unsafe {
            hv_vm_map(
                shm_mmap.as_ptr() as *mut std::ffi::c_void,
                gpu_shm_guest_addr,
                GPU_SHM_SIZE as usize,
                HV_MEMORY_READ | HV_MEMORY_WRITE,
            )
        };
        hv_result(ret).context("failed to map GPU SHM region into guest")?;
        info!(
            "GPU SHM region mapped at guest={:#x}, size={:#x}",
            gpu_shm_guest_addr, GPU_SHM_SIZE
        );

        mmio_device.set_shm_region(
            devices::virtio::gpu::VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
            gpu_shm_guest_addr,
            GPU_SHM_SIZE,
        );

        _gpu_shm_mmap = Some(shm_mmap);

        // Start the GPU worker thread. On Linux this happens via on_device_sandboxed()
        // when the device is jailed, but macOS has no jails.
        BusDevice::on_sandboxed(&mut mmio_device);

        let mmio_addr = next_mmio_addr;
        let irq = next_irq;

        let irq_evt = IrqEdgeEvent::new()
            .context("failed to create IrqEdgeEvent for GPU")?;

        match irq_evt.try_clone() {
            Ok(cloned_evt) => {
                gpu_irq_events.push((cloned_evt, irq));
            }
            Err(e) => {
                error!("Failed to clone IrqEdgeEvent for GPU: {}", e);
            }
        }

        mmio_device.assign_irq(&irq_evt, irq);

        mmio_bus
            .insert(Arc::new(Mutex::new(mmio_device)), mmio_addr, VIRTIO_MMIO_SIZE)
            .context("failed to insert GPU device into MMIO bus")?;

        info!(
            "GPU device (2D stub) added at MMIO address {:#x} with IRQ {}",
            mmio_addr, irq
        );

        next_mmio_addr += VIRTIO_MMIO_SIZE;
        next_irq += 1;
    }

    // Suppress unused warnings — these will be used when more devices are added.
    let _ = next_mmio_addr;
    let _ = next_irq;

    info!("Starting VM execution with {} VCPUs", vcpu_count);

    // Create VCPU coordinator for multi-VCPU support (must be before IRQ handler)
    let coordinator = Arc::new(VcpuCoordinator::new(irq_chip.clone(), has_in_kernel_gic));
    let control_server_thread = start_control_server(cfg.socket_path.clone(), coordinator.clone())?;

    // Spawn interrupt handler thread to inject device interrupts into the GIC
    // This is needed because vcpu.run() blocks, so we can't poll for interrupts
    // in the VCPU loop without causing a deadlock.
    //
    // Interrupt flow:
    // 1. Device signals irq_evt when it completes an operation
    // 2. IRQ handler thread sees the event and sets the SPI as pending
    //    - If in-kernel GIC: calls HvfVm::gic_set_spi() to inject into HVF's GIC
    //    - If userspace GIC: sets pending in our GIC emulation
    // 3. IRQ handler kicks the boot VCPU to exit hv_vcpu_run()
    // 4. Before running the VCPU again, we check if interrupts are pending and signal via
    //    hv_vcpu_set_pending_interrupt()
    // 5. Guest takes IRQ exception and reads ICC_IAR1_EL1 (which is trapped)
    // 6. Trap handler calls into userspace GIC to acknowledge and get the INTID
    // 7. Guest writes ICC_EOIR1_EL1 when done (also trapped, clears active state)
    let mut irq_events_for_handler = block_irq_events.iter()
        .filter_map(|(evt, irq)| evt.try_clone().ok().map(|e| (e, *irq)))
        .collect::<Vec<_>>();

    // Add virtiofs device interrupt events
    for (evt, irq) in &fs_irq_events {
        if let Ok(cloned) = evt.try_clone() {
            irq_events_for_handler.push((cloned, *irq));
        }
    }

    // Add sound device interrupt events
    #[cfg(feature = "audio")]
    for (evt, irq) in &snd_irq_events {
        if let Ok(cloned) = evt.try_clone() {
            irq_events_for_handler.push((cloned, *irq));
        }
    }

    // Add GPU device interrupt events
    #[cfg(feature = "gpu")]
    for (evt, irq) in &gpu_irq_events {
        if let Ok(cloned) = evt.try_clone() {
            irq_events_for_handler.push((cloned, *irq));
        }
    }

    // Add the serial device's interrupt event so serial input triggers a guest IRQ.
    if let Ok(serial_evt_clone) = serial_evt.try_clone() {
        irq_events_for_handler.push((
            IrqEdgeEvent::from_event(serial_evt_clone),
            AARCH64_SERIAL_IRQ,
        ));
    }

    let irq_handler_shutdown = Arc::new(AtomicBool::new(false));
    let irq_handler_shutdown_clone = irq_handler_shutdown.clone();
    let coordinator_for_handler = coordinator.clone();

    let irq_handler_thread = if !irq_events_for_handler.is_empty() {
        // TODO: Replace polling loop with WaitContext-based event multiplexing.
        // Currently each event is polled sequentially with 10ms timeouts, which
        // scales poorly with the number of devices.
        Some(thread::Builder::new()
            .name("irq-handler".to_string())
            .spawn(move || {
                while !irq_handler_shutdown_clone.load(Ordering::Relaxed) {
                    for (irq_evt, irq_num) in &irq_events_for_handler {
                        // Use a short timeout so we can check shutdown flag
                        match irq_evt.get_trigger().wait_timeout(Duration::from_millis(10)) {
                            Ok(base::EventWaitResult::Signaled) => {
                                // SPI INTID = 32 + irq_num (SPIs start at INTID 32)
                                let intid = 32 + *irq_num;

                                // Inject the SPI into the GIC
                                if coordinator_for_handler.has_in_kernel_gic.load(Ordering::Relaxed) {
                                    // Use HVF's in-kernel GIC
                                    if let Err(e) = HvfVm::gic_set_spi(intid, true) {
                                        error!("IRQ handler: failed to set SPI {} in in-kernel GIC: {:?}", irq_num, e);
                                    }
                                } else {
                                    // Use userspace GIC emulation
                                    coordinator_for_handler.irq_chip.set_spi_pending(*irq_num);
                                }

                                // Set the device IRQ pending flag and kick the VCPU
                                coordinator_for_handler.set_device_irq_pending();
                                coordinator_for_handler.kick_vcpus();
                            }
                            Ok(base::EventWaitResult::TimedOut) => {
                                // No interrupt, continue polling
                            }
                            Err(e) => {
                                warn!("IRQ handler: error checking event: {}", e);
                            }
                        }
                    }
                }
            })
            .expect("Failed to spawn IRQ handler thread"))
    } else {
        None
    };

    // Spawn threads for all VCPUs
    // IMPORTANT: VCPUs MUST be created on the thread that runs them (HVF requirement)
    let mut vcpu_threads: Vec<JoinHandle<ExitState>> = Vec::new();

    // Store configuration needed by boot CPU
    let kernel_entry = loaded_kernel.entry.offset();
    let fdt_addr = fdt_address.offset();

    for vcpu_id in 0..vcpu_count {
        let mmio_bus = mmio_bus.clone();
        let guest_mem = guest_mem.clone();
        let coordinator = coordinator.clone();
        let vm_clone = vm.clone();

        let thread = thread::Builder::new()
            .name(format!("vcpu{}", vcpu_id))
            .spawn(move || {
                // Create VCPU on this thread (HVF requirement)
                let vcpu = match HvfVm::create_vcpu(&*vm_clone, vcpu_id) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("Failed to create VCPU {}: {}", vcpu_id, e);
                        return ExitState::Crash;
                    }
                };

                // Initialize VCPU
                if let Err(e) = vcpu.init(&[]) {
                    error!("Failed to initialize VCPU {}: {}", vcpu_id, e);
                    return ExitState::Crash;
                }

                // Set MPIDR_EL1 so the kernel can identify each CPU.
                // Aff0 = cpu_id, bit 31 set (Res1 in AArch64 MPIDR format).
                let mpidr = (1u64 << 31) | (vcpu_id as u64);
                if let Err(e) = vcpu.set_mpidr_el1(mpidr) {
                    error!("Failed to set MPIDR for VCPU {}: {}", vcpu_id, e);
                    return ExitState::Crash;
                }

                // Set up virtual timer
                if let Err(e) = vcpu.set_vtimer_offset(0) {
                    error!("Failed to set vtimer offset for VCPU {}: {}", vcpu_id, e);
                    return ExitState::Crash;
                }
                if let Err(e) = vcpu.set_vtimer_mask(false) {
                    error!("Failed to unmask vtimer for VCPU {}: {}", vcpu_id, e);
                    return ExitState::Crash;
                }

                // Set PSTATE to EL1h with all interrupts masked
                let pstate = PSR_D_BIT | PSR_A_BIT | PSR_I_BIT | PSR_F_BIT | PSR_MODE_EL1H;
                if let Err(e) = vcpu.set_one_reg(VcpuRegAArch64::Pstate, pstate) {
                    error!("Failed to set PSTATE for VCPU {}: {}", vcpu_id, e);
                    return ExitState::Crash;
                }

                if vcpu_id == 0 {
                    // Boot CPU: set PC to kernel entry point, X0 to FDT address
                    if let Err(e) = vcpu.set_one_reg(VcpuRegAArch64::Pc, kernel_entry) {
                        error!("Failed to set PC for boot VCPU: {}", e);
                        return ExitState::Crash;
                    }
                    if let Err(e) = vcpu.set_one_reg(VcpuRegAArch64::X(0), fdt_addr) {
                        error!("Failed to set X0 for boot VCPU: {}", e);
                        return ExitState::Crash;
                    }
                    // Register the boot VCPU handle for IRQ handler to kick
                    coordinator.register_vcpu(vcpu_id, vcpu.vcpu_handle());
                    debug!("Boot VCPU initialized: PC={:#x}, X0={:#x}", kernel_entry, fdt_addr);
                    // Boot CPU - run immediately
                    run_vcpu_loop(vcpu, vcpu_id, mmio_bus, &guest_mem, &coordinator)
                        .unwrap_or(ExitState::Crash)
                } else {
                    // Secondary CPU - wait for CPU_ON
                    debug!("Secondary VCPU {} initialized, waiting for CPU_ON", vcpu_id);
                    run_secondary_vcpu(vcpu, vcpu_id, mmio_bus, &guest_mem, &coordinator)
                        .unwrap_or(ExitState::Crash)
                }
            })
            .context("failed to spawn VCPU thread")?;

        vcpu_threads.push(thread);
    }

    // Wait for the boot CPU to finish (it will drive the VM execution)
    let exit_state = vcpu_threads
        .remove(0)
        .join()
        .map_err(|_| anyhow::anyhow!("boot VCPU thread panicked"))?;

    // Signal all secondary VCPUs to shut down
    coordinator.signal_shutdown();

    // Wait for secondary VCPUs to finish
    for (i, thread) in vcpu_threads.into_iter().enumerate() {
        if let Err(e) = thread.join() {
            error!("VCPU {} thread panicked: {:?}", i + 1, e);
        }
    }

    // Shutdown IRQ handler thread
    irq_handler_shutdown.store(true, Ordering::Relaxed);
    if let Some(thread) = irq_handler_thread {
        if let Err(e) = thread.join() {
            error!("IRQ handler thread panicked: {:?}", e);
        }
    }

    if let Some(thread) = control_server_thread {
        if let Err(e) = thread.join() {
            error!("control server thread panicked: {:?}", e);
        }
    }

    info!("VM execution completed with state: {:?}", exit_state);

    // Restore terminal to canonical mode before exiting.
    stdin()
        .set_canon_mode()
        .context("failed to restore canonical mode for terminal")?;

    Ok(exit_state)
}

/// Create a minimal FDT (Flattened Device Tree) for booting Linux.
fn create_minimal_fdt(
    low_mem_size: u64,
    high_mem_size: u64,
    vcpu_count: usize,
    _fdt_address: GuestAddress,
    num_mmio_devices: usize,
    extra_kernel_params: &[String],
    initrd_range: Option<(u64, u64)>,
) -> Result<Fdt> {
    let mut fdt = Fdt::new(&[]);

    // GIC phandle constant
    const PHANDLE_GIC: u32 = 1;

    // GIC interrupt type constants
    const GIC_FDT_IRQ_TYPE_SPI: u32 = 0;
    const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;
    const IRQ_TYPE_EDGE_RISING: u32 = 0x00000001;
    const IRQ_TYPE_LEVEL_HIGH: u32 = 0x00000004;
    const IRQ_TYPE_LEVEL_LOW: u32 = 0x00000008;

    // Virtio MMIO constants (must match run_config)
    const AARCH64_MMIO_BASE: u64 = 0x10000;
    const VIRTIO_MMIO_SIZE: u64 = 0x200;

    // Root node
    let root_node = fdt.root_mut();
    root_node.set_prop("#address-cells", 2u32)?;
    root_node.set_prop("#size-cells", 2u32)?;
    root_node.set_prop("compatible", &["linux,dummy-virt", "simple-bus"])?;
    root_node.set_prop("model", "crosvm")?;
    root_node.set_prop("interrupt-parent", PHANDLE_GIC)?;
    root_node.set_prop("ranges", ())?;

    // Chosen node - kernel command line and stdout
    let chosen_node = root_node.subnode_mut("chosen")?;
    // Build command line with base params and any extra params from -p flag
    let mut bootargs = String::from("console=ttyS0");
    for param in extra_kernel_params {
        bootargs.push(' ');
        bootargs.push_str(param);
    }
    chosen_node.set_prop("bootargs", bootargs.as_str())?;
    chosen_node.set_prop("stdout-path", "/uart@3f8")?;

    if let Some((initrd_start, initrd_end)) = initrd_range {
        chosen_node.set_prop("linux,initrd-start", initrd_start)?;
        chosen_node.set_prop("linux,initrd-end", initrd_end)?;
    }

    // TODO: Adding kaslr-seed here would enable kernel ASLR. The HVC
    // workaround in handle_hypercall_with_result_buffer now uses page table
    // walking which handles KASLR, but this hasn't been tested with KASLR
    // enabled yet. Uncomment once verified:
    //   chosen_node.set_prop("kaslr-seed", rand::random::<u64>())?;
    //   let mut rng_seed = [0u8; 256];
    //   OsRng.fill_bytes(&mut rng_seed);
    //   chosen_node.set_prop("rng-seed", &rng_seed)?;

    // Memory node(s) — low bank always present, high bank if memory > 1 GiB.
    let memory_node = root_node.subnode_mut("memory@80000000")?;
    memory_node.set_prop("device_type", "memory")?;
    if high_mem_size > 0 {
        memory_node.set_prop(
            "reg",
            &[
                AARCH64_PHYS_MEM_START, low_mem_size,
                AARCH64_HIGH_MEM_START, high_mem_size,
            ],
        )?;
    } else {
        memory_node.set_prop("reg", &[AARCH64_PHYS_MEM_START, low_mem_size])?;
    }

    // CPUs node
    let cpus_node = root_node.subnode_mut("cpus")?;
    cpus_node.set_prop("#address-cells", 1u32)?;
    cpus_node.set_prop("#size-cells", 0u32)?;

    for cpu_id in 0..vcpu_count {
        let cpu_name = format!("cpu@{}", cpu_id);
        let cpu_node = cpus_node.subnode_mut(&cpu_name)?;
        cpu_node.set_prop("device_type", "cpu")?;
        cpu_node.set_prop("compatible", "arm,armv8")?;
        cpu_node.set_prop("reg", cpu_id as u32)?;
        cpu_node.set_prop("enable-method", "psci")?;
    }

    // PSCI node for CPU power management
    let psci_node = root_node.subnode_mut("psci")?;
    psci_node.set_prop("compatible", "arm,psci-1.0")?;
    psci_node.set_prop("method", "hvc")?;

    // GIC node (interrupt controller)
    let redist_base = gic_redist_base(vcpu_count);
    // For userspace GIC, use per-CPU redistributor size
    let redist_size = AARCH64_GIC_REDIST_SIZE * vcpu_count as u64;
    let gic_reg_prop = [
        AARCH64_GIC_DIST_BASE, AARCH64_GIC_DIST_SIZE, // Distributor
        redist_base, redist_size,                      // Redistributor
    ];

    let intc_node = root_node.subnode_mut("intc")?;
    intc_node.set_prop("compatible", "arm,gic-v3")?;
    intc_node.set_prop("#interrupt-cells", 3u32)?;
    intc_node.set_prop("interrupt-controller", ())?;
    intc_node.set_prop("reg", &gic_reg_prop)?;
    intc_node.set_prop("phandle", PHANDLE_GIC)?;
    intc_node.set_prop("#address-cells", 2u32)?;
    intc_node.set_prop("#size-cells", 2u32)?;

    // Timer node - uses PPIs (type 1) with proper format for GICv3
    // CPU mask in bits [15:8], trigger type in bits [3:0]
    let cpu_mask: u32 = (((1u32 << vcpu_count) - 1) << 8) & 0xff00;
    let timer_node = root_node.subnode_mut("timer")?;
    timer_node.set_prop("compatible", "arm,armv8-timer")?;
    timer_node.set_prop("always-on", ())?;
    // Timer interrupts: secure phys, non-secure phys, virt, hyp
    // Format: <type irq_num flags>
    // PPI type = 1, IRQ number (relative to PPI base), flags = cpu_mask | trigger
    timer_node.set_prop(
        "interrupts",
        &[
            GIC_FDT_IRQ_TYPE_PPI, 13, cpu_mask | IRQ_TYPE_LEVEL_LOW, // Secure Physical Timer
            GIC_FDT_IRQ_TYPE_PPI, 14, cpu_mask | IRQ_TYPE_LEVEL_LOW, // Non-secure Physical Timer
            GIC_FDT_IRQ_TYPE_PPI, 11, cpu_mask | IRQ_TYPE_LEVEL_LOW, // Virtual Timer (PPI 11)
            GIC_FDT_IRQ_TYPE_PPI, 10, cpu_mask | IRQ_TYPE_LEVEL_LOW, // Hypervisor Timer
        ],
    )?;

    // UART node
    let uart_node = root_node.subnode_mut("uart@3f8")?;
    uart_node.set_prop("compatible", "ns16550a")?;
    uart_node.set_prop("reg", &[AARCH64_SERIAL_ADDR, AARCH64_SERIAL_SIZE])?;
    uart_node.set_prop("clock-frequency", 1843200u32)?;
    uart_node.set_prop("interrupts", &[GIC_FDT_IRQ_TYPE_SPI, AARCH64_SERIAL_IRQ, IRQ_TYPE_EDGE_RISING])?;

    // Virtio-MMIO device nodes (block devices + virtiofs devices)
    // Each device gets its own SPI, starting after serial IRQ (0)
    for i in 0..num_mmio_devices {
        let mmio_addr = AARCH64_MMIO_BASE + (i as u64 * VIRTIO_MMIO_SIZE);
        let irq = (i + 1) as u32; // SPI starting at 1 (serial uses 0)

        let node_name = format!("virtio_mmio@{:x}", mmio_addr);
        let virtio_node = root_node.subnode_mut(&node_name)?;
        virtio_node.set_prop("compatible", "virtio,mmio")?;
        virtio_node.set_prop("reg", &[mmio_addr, VIRTIO_MMIO_SIZE])?;
        virtio_node.set_prop("interrupts", &[GIC_FDT_IRQ_TYPE_SPI, irq, IRQ_TYPE_LEVEL_HIGH])?;
        virtio_node.set_prop("dma-coherent", ())?;
    }

    Ok(fdt)
}

/// Create a serial device for console output.
fn create_serial_device(cfg: &Config, evt: &Event) -> Result<Serial> {
    use devices::serial_device::SerialType;

    // Check if serial parameters are specified in config
    let serial_params = cfg
        .serial_parameters
        .get(&(SerialHardware::Serial, 1))
        .cloned()
        .unwrap_or_else(|| SerialParameters {
            type_: SerialType::Stdout,
            hardware: SerialHardware::Serial,
            name: None,
            path: None,
            input: None,
            num: 1,
            console: true,
            earlycon: false,
            stdin: true,
            out_timestamp: false,
            ..Default::default()
        });

    let mut preserved_fds = Vec::new();
    serial_params
        .create_serial_device::<Serial>(
            ProtectionType::Unprotected,
            evt,
            &mut preserved_fds,
        )
        .context("failed to create serial device")
}

/// Run the VCPU loop until exit.
fn run_vcpu_loop(
    mut vcpu: HvfVcpu,
    vcpu_id: usize,
    mmio_bus: Arc<Bus>,
    guest_mem: &GuestMemory,
    coordinator: &VcpuCoordinator,
) -> Result<ExitState> {
    use hypervisor::Vcpu;

    let mut iteration = 0u64;

    info!("VCPU {} starting", vcpu_id);

    loop {
        iteration += 1;


        // Before running the VCPU, check if we need to signal an interrupt
        // Check pending interrupts and signal via hv_vcpu_set_pending_interrupt()
        // before entering the guest.
        if coordinator.irq_chip.should_signal_irq(vcpu_id) {
            trace!("VCPU {}: Signaling pending IRQ before run", vcpu_id);
            if let Err(e) = vcpu.set_pending_interrupt(
                hv_interrupt_type_t::HV_INTERRUPT_TYPE_IRQ,
                true,
            ) {
                error!("Failed to signal pending IRQ: {}", e);
            }
        }

        let exit_result = vcpu.run();
        match exit_result {
            Ok(VcpuExit::Mmio) => {
                if let Err(e) = vcpu.handle_mmio(&mut |IoParams { address, operation }| {
                    match operation {
                        IoOperation::Read(data) => {
                            if !mmio_bus.read(address, data) {
                                trace!("Unmapped MMIO read: {:#x}", address);
                            }
                            Ok(())
                        }
                        IoOperation::Write(data) => {
                            if !mmio_bus.write(address, data) {
                                trace!("Unmapped MMIO write: {:#x}", address);
                            }
                            Ok(())
                        }
                    }
                }) {
                    error!("Failed to handle MMIO: {}", e);
                }
            }
            Ok(VcpuExit::Hlt) => {
                debug!("VCPU halted at iteration {}", iteration);
                // For now, just continue - in a full implementation we'd wait for interrupts
                thread::yield_now();
            }
            Ok(VcpuExit::Shutdown(_)) => {
                info!("VCPU requested shutdown");
                return Ok(ExitState::Stop);
            }
            Ok(VcpuExit::Hypercall) => {
                let sp_el1 = vcpu.get_sp_el1().unwrap_or(0);
                let function_id = vcpu.get_one_reg(VcpuRegAArch64::X(0)).unwrap_or(0);

                debug!(
                    "HVC trap: function_id={:#x} SP_EL1={:#x}",
                    function_id,
                    sp_el1,
                );

                // Translate SP_EL1 to guest physical address via page table walk
                // The SMCCC macro does `stp x29, x30, [sp, #-16]!` before HVC,
                // so SP_EL1 at HVC time has: [SP+0]=saved_x29, [SP+8]=saved_x30,
                // [SP+16]=result_buffer (9th arg), [SP+24]=quirk (10th arg).
                let sp_phys = translate_kernel_va(&vcpu, &guest_mem, sp_el1 + 16);

                let hypercall_exit = if let Some(phys) = sp_phys {
                    let gpa = GuestAddress(phys);
                    let mut stack_data = [0u8; 8];
                    if guest_mem.read_at_addr(&mut stack_data, gpa).is_ok() {
                        let result_buffer_addr = u64::from_le_bytes(stack_data);
                        debug!(
                            "HVC: result_buffer={:#x}",
                            result_buffer_addr
                        );

                        // Handle PSCI call with workaround for HVF ldr issue
                        match handle_hypercall_with_result_buffer(
                            &mut vcpu as &mut dyn VcpuAArch64,
                            &guest_mem,
                            result_buffer_addr,
                            coordinator,
                        ) {
                            Ok(exit) => exit,
                            Err(e) => {
                                error!("Failed to handle hypercall: {}", e);
                                None
                            }
                        }
                    } else {
                        // Fallback if we can't read stack
                        match handle_hypercall(&mut vcpu as &mut dyn VcpuAArch64, &guest_mem, coordinator) {
                            Ok(exit) => exit,
                            Err(e) => {
                                error!("Failed to handle hypercall: {}", e);
                                None
                            }
                        }
                    }
                } else {
                    // Fallback for addresses outside linear map
                    match handle_hypercall(&mut vcpu as &mut dyn VcpuAArch64, &guest_mem, coordinator) {
                        Ok(exit) => exit,
                        Err(e) => {
                            error!("Failed to handle hypercall: {}", e);
                            None
                        }
                    }
                };

                if let Some(exit_state) = hypercall_exit {
                    info!("VCPU {} exiting due to PSCI request: {:?}", vcpu_id, exit_state);
                    return Ok(exit_state);
                }
            }
            Ok(VcpuExit::SystemRegisterTrap { op0, op1, crn, crm, op2, rt, is_write }) => {
                // Handle system register access traps
                // For GICv3, we need to handle ICC_* registers

                if let Err(e) = handle_system_register_trap(
                    &mut vcpu,
                    vcpu_id,
                    coordinator,
                    op0, op1, crn, crm, op2, rt, is_write,
                ) {
                    error!("Failed to handle system register trap: {}", e);
                }
            }
            Ok(VcpuExit::Intr) => {
                // Interrupted - check for pending device interrupts
                trace!("VCPU {} interrupted at iteration {}", vcpu_id, iteration);

                // Check if we were kicked because of a device interrupt
                if coordinator.take_device_irq_pending() {
                    // The SPI is already set as pending in the userspace GIC
                    // The should_signal_irq check at the top of the loop will inject it
                }
                // Continue running - the guest will take the IRQ exception
            }
            Ok(VcpuExit::IrqWindowOpen) => {
                // Virtual timer fired - inject timer interrupt to guest
                // The virtual timer uses PPI 11 (INTID 27 = 16 + 11)
                // With HVF, we inject an IRQ signal which the guest will handle

                // CRITICAL: Set the timer PPI as pending in the redistributor
                // This is necessary for ICC_IAR1_EL1 to return the correct INTID.
                // Without this, the guest reads spurious (1023) and gets stuck
                // in an infinite loop entering/exiting the IRQ handler.
                coordinator.irq_chip.set_ppi_pending(vcpu_id, VTIMER_PPI);

                // Inject IRQ to the guest
                if let Err(e) = vcpu.set_pending_interrupt(
                    hv_interrupt_type_t::HV_INTERRUPT_TYPE_IRQ,
                    true,
                ) {
                    error!("Failed to inject timer IRQ: {}", e);
                }

                // Mask the timer while the interrupt is pending
                // This prevents repeated VTIMER_ACTIVATED exits for the same event
                if let Err(e) = vcpu.set_vtimer_mask(true) {
                    debug!("Failed to mask vtimer: {}", e);
                }

                // Continue running the VCPU - the guest will handle the interrupt
                // and set up the next timer. We'll unmask after the guest runs.
                continue;
            }
            Ok(VcpuExit::Exception) => {
                // Get more info about the exception
                let pc = vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap_or(0);
                error!("VCPU exception at PC={:#x}, iteration {}", pc, iteration);
                return Ok(ExitState::Crash);
            }
            Ok(exit) => {
                debug!("Unhandled VCPU exit: {:?} at iteration {}", exit, iteration);
            }
            Err(e) => {
                if e.errno() == libc::EINTR {
                    // Interrupted by signal - continue
                    continue;
                }
                error!("VCPU run error: {} at iteration {}", e, iteration);
                return Ok(ExitState::Crash);
            }
        }

        // After handling any exit (except timer exits which continue above),
        // unmask the virtual timer so we can receive new timer interrupts.
        // This is safe because:
        // 1. If the timer already fired and the guest hasn't acknowledged it,
        //    we'll get another VTIMER_ACTIVATED exit immediately
        // 2. If the guest has set up a new timer value, HVF will wait until
        //    that time to generate the next exit
        let _ = vcpu.set_vtimer_mask(false);

        // Check for shutdown
        if coordinator.shutdown.load(Ordering::Relaxed) {
            info!("VCPU {} received shutdown signal", vcpu_id);
            return Ok(ExitState::Stop);
        }
    }
}

/// Run a secondary VCPU that waits for PSCI CPU_ON before executing.
fn run_secondary_vcpu(
    mut vcpu: HvfVcpu,
    vcpu_id: usize,
    mmio_bus: Arc<Bus>,
    guest_mem: &GuestMemory,
    coordinator: &VcpuCoordinator,
) -> Result<ExitState> {
    use hypervisor::VcpuAArch64;

    info!("Secondary VCPU {} waiting for CPU_ON", vcpu_id);

    // Wait for CPU_ON request
    let startup_request = match coordinator.wait_for_startup(vcpu_id) {
        Some(request) => request,
        None => {
            info!("Secondary VCPU {} shutting down (no startup request)", vcpu_id);
            return Ok(ExitState::Stop);
        }
    };

    info!(
        "Secondary VCPU {} starting: entry_point={:#x}, context_id={:#x}",
        vcpu_id, startup_request.entry_point, startup_request.context_id
    );

    // Set up VCPU state for secondary boot
    vcpu.set_one_reg(VcpuRegAArch64::Pc, startup_request.entry_point)
        .context("failed to set PC for secondary VCPU")?;
    vcpu.set_one_reg(VcpuRegAArch64::X(0), startup_request.context_id)
        .context("failed to set X0 for secondary VCPU")?;

    // PSTATE: EL1h with all interrupts masked
    let pstate = PSR_D_BIT | PSR_A_BIT | PSR_I_BIT | PSR_F_BIT | PSR_MODE_EL1H;
    vcpu.set_one_reg(VcpuRegAArch64::Pstate, pstate)
        .context("failed to set PSTATE for secondary VCPU")?;

    // Register this VCPU for interrupt delivery
    coordinator.register_vcpu(vcpu_id, vcpu.vcpu_handle());

    let result = run_vcpu_loop(vcpu, vcpu_id, mmio_bus, guest_mem, coordinator);

    coordinator.unregister_vcpu(vcpu_id);
    result
}

/// Walk ARM64 page tables to translate a kernel virtual address to a guest
/// physical address. Uses TTBR1_EL1 as the page table root and TCR_EL1 to
/// determine the translation granule and number of levels.
fn translate_kernel_va(vcpu: &HvfVcpu, guest_mem: &GuestMemory, va: u64) -> Option<u64> {
    let ttbr1 = vcpu.get_ttbr1_el1().ok()?;
    let tcr = vcpu.get_tcr_el1().ok()?;

    let t1sz = ((tcr >> 16) & 0x3F) as u32;
    let va_bits = 64 - t1sz;

    // TG1 field [31:30]: 01=16KB, 10=4KB, 11=64KB
    let tg1 = ((tcr >> 30) & 0x3) as u32;
    let granule_bits: u32 = match tg1 {
        1 => 14,
        3 => 16,
        _ => 12, // 4KB (value 2 or default)
    };
    let bits_per_level = granule_bits - 3; // entries per table = 2^bits_per_level

    let table_base = ttbr1 & 0x0000_FFFF_FFFF_F000;

    // Number of levels needed to cover va_bits
    let levels =
        (va_bits.saturating_sub(granule_bits) + bits_per_level - 1) / bits_per_level;
    let start_level = 4u32.saturating_sub(levels);
    let level_mask = (1u64 << bits_per_level) - 1;

    let mut table_pa = table_base;

    for level in start_level..4 {
        let shift = granule_bits + bits_per_level * (3 - level);
        let index = (va >> shift) & level_mask;

        let entry_pa = table_pa + index * 8;
        let mut entry_bytes = [0u8; 8];
        guest_mem
            .read_at_addr(&mut entry_bytes, GuestAddress(entry_pa))
            .ok()?;
        let entry = u64::from_le_bytes(entry_bytes);

        if entry & 1 == 0 {
            return None;
        }

        if level < 3 && (entry & 0b10) == 0 {
            // Block descriptor
            let block_mask = (1u64 << shift) - 1;
            let output_addr = entry & 0x0000_FFFF_FFFF_F000 & !block_mask;
            return Some(output_addr | (va & block_mask));
        }

        if level == 3 {
            let page_mask = (1u64 << granule_bits) - 1;
            let output_addr = entry & 0x0000_FFFF_FFFF_F000;
            return Some(output_addr | (va & page_mask));
        }

        table_pa = entry & 0x0000_FFFF_FFFF_F000;
    }

    None
}

/// Handle PSCI hypercalls.
fn handle_hypercall(vcpu: &mut dyn VcpuAArch64, _guest_mem: &GuestMemory, coordinator: &VcpuCoordinator) -> Result<Option<ExitState>> {
    // CRITICAL: Per ARM SMCCC specification, X4-X17 are callee-saved registers
    // and MUST be preserved across HVC/SMC calls. The Linux kernel's SMCCC
    // wrapper (__arm_smccc_hvc) passes the result buffer address in X4 and
    // expects it to be unchanged after the hypercall returns.
    //
    // Save X4-X17 before any modifications to ensure we can restore them later.
    let saved_regs: Vec<u64> = (4..=17)
        .map(|i| vcpu.get_one_reg(VcpuRegAArch64::X(i)).unwrap_or(0))
        .collect();

    debug!(
        "HVC saved X4-X7: {:#x} {:#x} {:#x} {:#x}",
        saved_regs[0], saved_regs[1], saved_regs[2], saved_regs[3]
    );
    info!(
        "SMCCC: Saving X4={:#x} before PSCI handling",
        saved_regs[0]
    );

    // Get the function ID from X0
    let function_id = vcpu.get_one_reg(VcpuRegAArch64::X(0)).unwrap_or(0);
    let pc = vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap_or(0);

    info!(
        "PSCI call: function_id={:#x}, X1={:#x}, X2={:#x}, X3={:#x}",
        function_id,
        vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0),
        vcpu.get_one_reg(VcpuRegAArch64::X(2)).unwrap_or(0),
        vcpu.get_one_reg(VcpuRegAArch64::X(3)).unwrap_or(0),
    );

    let (result, exit_state) = match function_id {
        PSCI_VERSION => {
            // Return PSCI version 1.1 (0x00010001)
            info!("PSCI VERSION called, returning 1.1");
            (0x00010001u64, None)
        }
        PSCI_CPU_OFF => {
            info!("PSCI CPU_OFF requested - halting VCPU");
            (PSCI_SUCCESS, Some(ExitState::Stop))
        }
        PSCI_CPU_ON_32 | PSCI_CPU_ON_64 => {
            // CPU_ON: X1 = target_cpu MPIDR, X2 = entry_point, X3 = context_id
            let target_mpidr = vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0);
            let entry_point = vcpu.get_one_reg(VcpuRegAArch64::X(2)).unwrap_or(0);
            let context_id = vcpu.get_one_reg(VcpuRegAArch64::X(3)).unwrap_or(0);

            // For now, assume MPIDR affinity level 0 directly maps to VCPU ID
            // (MPIDR format: Aff2.Aff1.Aff0 - we use Aff0 as VCPU ID)
            let target_vcpu_id = (target_mpidr & 0xFF) as usize;

            info!(
                "PSCI CPU_ON: target_mpidr={:#x} (vcpu_id={}), entry_point={:#x}, context_id={:#x}",
                target_mpidr, target_vcpu_id, entry_point, context_id
            );

            // Request the target VCPU to start
            coordinator.request_cpu_start(target_vcpu_id, entry_point, context_id);
            (PSCI_SUCCESS, None)
        }
        PSCI_SYSTEM_OFF => {
            info!("PSCI SYSTEM_OFF requested - shutting down");
            coordinator.signal_shutdown();
            (PSCI_SUCCESS, Some(ExitState::Stop))
        }
        PSCI_SYSTEM_RESET => {
            info!("PSCI SYSTEM_RESET requested - resetting");
            coordinator.signal_shutdown();
            (PSCI_SUCCESS, Some(ExitState::Reset))
        }
        PSCI_FEATURES => {
            let feature_id = vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0);
            debug!("PSCI FEATURES query for {:#x}", feature_id);
            // Return 0 (supported) for basic functions, -1 for others
            let r = match feature_id {
                PSCI_VERSION | PSCI_CPU_OFF | PSCI_CPU_ON_32 | PSCI_CPU_ON_64
                    | PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET => PSCI_SUCCESS,
                PSCI_MIGRATE_INFO_TYPE => PSCI_SUCCESS,
                _ => PSCI_NOT_SUPPORTED,
            };
            (r, None)
        }
        PSCI_MIGRATE_INFO_TYPE => {
            // Return 2 to indicate migration is not supported (Trusted OS not present)
            debug!("PSCI MIGRATE_INFO_TYPE called, returning NOT_SUPPORTED");
            (PSCI_MIGRATE_INFO_TYPE_NOT_SUPPORTED, None)
        }
        PSCI_AFFINITY_INFO_32 | PSCI_AFFINITY_INFO_64 => {
            let target_affinity = vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0);
            let target_cpu = (target_affinity & 0xFF) as usize;
            let is_on = coordinator.active_vcpus
                .lock()
                .expect("VcpuCoordinator mutex poisoned")
                .contains_key(&target_cpu);
            debug!("PSCI AFFINITY_INFO cpu={} is_on={}", target_cpu, is_on);
            (if is_on { 0 } else { 1 }, None) // 0=ON, 1=OFF
        }
        PSCI_CPU_SUSPEND_32 | PSCI_CPU_SUSPEND_64 => {
            debug!("PSCI CPU_SUSPEND requested");
            (PSCI_NOT_SUPPORTED, None)
        }
        PSCI_SYSTEM_SUSPEND_32 | PSCI_SYSTEM_SUSPEND_64 => {
            debug!("PSCI SYSTEM_SUSPEND requested");
            (PSCI_NOT_SUPPORTED, None)
        }
        _ => {
            debug!("Unknown PSCI function: {:#x}", function_id);
            (PSCI_NOT_SUPPORTED, None)
        }
    };

    // Set return value in X0 (per SMCCC convention)
    vcpu.set_one_reg(VcpuRegAArch64::X(0), result)?;

    // For HVF, we MUST advance PC past the HVC instruction.
    // Note: This fallback path may not work correctly due to HVF ldr issue.
    // The preferred path is handle_hypercall_with_result_buffer which works around this.
    vcpu.set_one_reg(VcpuRegAArch64::Pc, pc + 4)?;

    debug!("PSCI returned: result={:#x}, new_PC={:#x}", result, pc + 4);

    // Restore callee-saved registers (X4-X17) per ARM SMCCC specification
    for (i, &value) in saved_regs.iter().enumerate() {
        let reg_num = (i + 4) as u8;
        if let Err(e) = vcpu.set_one_reg(VcpuRegAArch64::X(reg_num), value) {
            error!("Failed to restore X{}: {:?}", reg_num, e);
        }
    }

    Ok(exit_state)
}

/// Handle PSCI hypercalls with explicit result buffer address.
/// This version skips the problematic `ldr x4, [sp]` instruction by:
/// 1. Setting X4 to the result buffer address ourselves
/// 2. Advancing PC by 4 to skip the ldr instruction (HVF already advanced past HVC)
fn handle_hypercall_with_result_buffer(
    vcpu: &mut dyn VcpuAArch64,
    _guest_mem: &GuestMemory,
    result_buffer_addr: u64,
    coordinator: &VcpuCoordinator,
) -> Result<Option<ExitState>> {
    // Get the function ID from X0
    let function_id = vcpu.get_one_reg(VcpuRegAArch64::X(0)).unwrap_or(0);
    let pc = vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap_or(0);

    info!(
        "PSCI call: function_id={:#x}, result_buf={:#x}",
        function_id, result_buffer_addr
    );

    let (result, exit_state) = match function_id {
        PSCI_VERSION => {
            info!("PSCI VERSION called, returning 1.1");
            (0x00010001u64, None)
        }
        PSCI_CPU_OFF => {
            info!("PSCI CPU_OFF requested - halting VCPU");
            (PSCI_SUCCESS, Some(ExitState::Stop))
        }
        PSCI_CPU_ON_32 | PSCI_CPU_ON_64 => {
            // CPU_ON: X1 = target_cpu MPIDR, X2 = entry_point, X3 = context_id
            let target_mpidr = vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0);
            let entry_point = vcpu.get_one_reg(VcpuRegAArch64::X(2)).unwrap_or(0);
            let context_id = vcpu.get_one_reg(VcpuRegAArch64::X(3)).unwrap_or(0);

            // For now, assume MPIDR affinity level 0 directly maps to VCPU ID
            let target_vcpu_id = (target_mpidr & 0xFF) as usize;

            info!(
                "PSCI CPU_ON: target_mpidr={:#x} (vcpu_id={}), entry_point={:#x}, context_id={:#x}",
                target_mpidr, target_vcpu_id, entry_point, context_id
            );

            // Request the target VCPU to start
            coordinator.request_cpu_start(target_vcpu_id, entry_point, context_id);
            (PSCI_SUCCESS, None)
        }
        PSCI_SYSTEM_OFF => {
            info!("PSCI SYSTEM_OFF requested - shutting down");
            coordinator.signal_shutdown();
            (PSCI_SUCCESS, Some(ExitState::Stop))
        }
        PSCI_SYSTEM_RESET => {
            info!("PSCI SYSTEM_RESET requested - resetting");
            coordinator.signal_shutdown();
            (PSCI_SUCCESS, Some(ExitState::Reset))
        }
        PSCI_FEATURES => {
            let feature_id = vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0);
            debug!("PSCI FEATURES query for {:#x}", feature_id);
            let r = match feature_id {
                PSCI_VERSION | PSCI_CPU_OFF | PSCI_CPU_ON_32 | PSCI_CPU_ON_64
                    | PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET => PSCI_SUCCESS,
                PSCI_MIGRATE_INFO_TYPE => PSCI_SUCCESS,
                _ => PSCI_NOT_SUPPORTED,
            };
            (r, None)
        }
        PSCI_MIGRATE_INFO_TYPE => {
            debug!("PSCI MIGRATE_INFO_TYPE called");
            (PSCI_MIGRATE_INFO_TYPE_NOT_SUPPORTED, None)
        }
        PSCI_AFFINITY_INFO_32 | PSCI_AFFINITY_INFO_64 => {
            let target_affinity = vcpu.get_one_reg(VcpuRegAArch64::X(1)).unwrap_or(0);
            let target_cpu = (target_affinity & 0xFF) as usize;
            let is_on = coordinator.active_vcpus
                .lock()
                .expect("VcpuCoordinator mutex poisoned")
                .contains_key(&target_cpu);
            debug!("PSCI AFFINITY_INFO cpu={} is_on={}", target_cpu, is_on);
            (if is_on { 0 } else { 1 }, None)
        }
        PSCI_CPU_SUSPEND_32 | PSCI_CPU_SUSPEND_64 => {
            debug!("PSCI CPU_SUSPEND requested");
            (PSCI_NOT_SUPPORTED, None)
        }
        PSCI_SYSTEM_SUSPEND_32 | PSCI_SYSTEM_SUSPEND_64 => {
            debug!("PSCI SYSTEM_SUSPEND requested");
            (PSCI_NOT_SUPPORTED, None)
        }
        _ => {
            debug!("Unknown PSCI function: {:#x}", function_id);
            (PSCI_NOT_SUPPORTED, None)
        }
    };

    // Set return value in X0 (per SMCCC convention)
    vcpu.set_one_reg(VcpuRegAArch64::X(0), result)?;

    // Set X4 to the result buffer address and skip the ldr instruction
    // This works around an HVF issue where ldr x4, [sp] doesn't properly load from SP_EL1
    vcpu.set_one_reg(VcpuRegAArch64::X(4), result_buffer_addr)?;

    // Advance PC by 4 to skip the ldr instruction
    // HVF already advances PC past HVC, so PC now points to ldr x4, [sp]
    // We skip to stp x0, x1, [x4] which stores the result
    let new_pc = pc + 4;
    vcpu.set_one_reg(VcpuRegAArch64::Pc, new_pc)?;

    debug!(
        "PSCI: result={:#x}, PC advanced to {:#x}",
        result, new_pc
    );

    Ok(exit_state)
}

/// Handle AArch64 system register trap (ICC_* registers for GICv3).
///
/// This function is called when the guest accesses a system register that is
/// trapped to the hypervisor. For GICv3, we handle the ICC_* (CPU interface)
/// registers to emulate interrupt acknowledgment and completion.
///
/// ICC register encodings (Op0=3, Op1=0, CRn=12):
/// | Register      | CRm | Op2 | Direction | Description                    |
/// |---------------|-----|-----|-----------|--------------------------------|
/// | ICC_IAR0_EL1  | 8   | 0   | R         | Interrupt Acknowledge (G0)     |
/// | ICC_IAR1_EL1  | 12  | 0   | R         | Interrupt Acknowledge (G1)     |
/// | ICC_EOIR0_EL1 | 8   | 1   | W         | End of Interrupt (G0)          |
/// | ICC_EOIR1_EL1 | 12  | 1   | W         | End of Interrupt (G1)          |
/// | ICC_HPPIR0_EL1| 8   | 2   | R         | Highest Priority Pending (G0)  |
/// | ICC_HPPIR1_EL1| 12  | 2   | R         | Highest Priority Pending (G1)  |
/// | ICC_BPR0_EL1  | 8   | 3   | RW        | Binary Point (G0)              |
/// | ICC_BPR1_EL1  | 12  | 3   | RW        | Binary Point (G1)              |
/// | ICC_CTLR_EL1  | 12  | 4   | RW        | Control Register               |
/// | ICC_SRE_EL1   | 12  | 5   | RW        | System Register Enable         |
/// | ICC_IGRPEN0_EL1| 12 | 6   | RW        | Interrupt Group 0 Enable       |
/// | ICC_IGRPEN1_EL1| 12 | 7   | RW        | Interrupt Group 1 Enable       |
/// | ICC_PMR_EL1   | 4   | 6   | RW        | Priority Mask (CRn=4!)         |
/// | ICC_RPR_EL1   | 11  | 3   | R         | Running Priority               |
fn handle_system_register_trap(
    vcpu: &mut HvfVcpu,
    vcpu_id: usize,
    coordinator: &VcpuCoordinator,
    op0: u8,
    op1: u8,
    crn: u8,
    crm: u8,
    op2: u8,
    rt: u8,
    is_write: bool,
) -> Result<()> {
    let irq_chip = &coordinator.irq_chip;
    // Get the PC to advance it after handling
    let pc = vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap_or(0);

    // Check if this is a GICv3 ICC register access (Op0=3, Op1=0)
    if op0 == 3 && op1 == 0 {
        match (crn, crm, op2, is_write) {
            // ICC_PMR_EL1: Priority Mask Register (CRn=4, CRm=6, Op2=0)
            (4, 6, 0, false) => {
                // Read PMR
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_pmr();
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                }
            }
            (4, 6, 0, true) => {
                // Write PMR
                let value = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0);
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    cpu_if.write_pmr(value);
                }
            }

            // ICC_IAR1_EL1: Interrupt Acknowledge Register (CRn=12, CRm=12, Op2=0)
            (12, 12, 0, false) => {
                // Read - acknowledge interrupt
                let intid = irq_chip.acknowledge_interrupt(vcpu_id);
                vcpu.set_one_reg(VcpuRegAArch64::X(rt), intid as u64)?;
            }

            // ICC_EOIR1_EL1: End of Interrupt Register (CRn=12, CRm=12, Op2=1)
            (12, 12, 1, true) => {
                // Write - end of interrupt
                let intid = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0) as u32;
                irq_chip.end_of_interrupt(vcpu_id, intid);
            }

            // ICC_HPPIR1_EL1: Highest Priority Pending Interrupt (CRn=12, CRm=12, Op2=2)
            (12, 12, 2, false) => {
                // Read - get highest priority pending interrupt ID
                let intid = irq_chip.get_highest_priority_pending(vcpu_id)
                    .map(|(id, _)| id)
                    .unwrap_or(GIC_SPURIOUS_INTID);
                vcpu.set_one_reg(VcpuRegAArch64::X(rt), intid as u64)?;
                trace!("ICC_HPPIR1_EL1 read: INTID {}", intid);
            }

            // ICC_BPR1_EL1: Binary Point Register (CRn=12, CRm=12, Op2=3)
            (12, 12, 3, false) => {
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_bpr(1);
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                    trace!("ICC_BPR1_EL1 read: {}", value);
                }
            }
            (12, 12, 3, true) => {
                let value = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0);
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    cpu_if.write_bpr(1, value);
                    trace!("ICC_BPR1_EL1 write: {}", value);
                }
            }

            // ICC_CTLR_EL1: Control Register (CRn=12, CRm=12, Op2=4)
            (12, 12, 4, false) => {
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_ctlr();
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                    trace!("ICC_CTLR_EL1 read: {:#x}", value);
                }
            }
            (12, 12, 4, true) => {
                let value = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0);
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    cpu_if.write_ctlr(value);
                    trace!("ICC_CTLR_EL1 write: {:#x}", value);
                }
            }

            // ICC_SRE_EL1: System Register Enable (CRn=12, CRm=12, Op2=5)
            (12, 12, 5, false) => {
                // Read - always return 0x7 (SRE=1, DFB=1, DIB=1)
                // This indicates system register access is enabled and required
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_sre();
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                    trace!("ICC_SRE_EL1 read: {:#x}", value);
                }
            }
            (12, 12, 5, true) => {
                // Write - ignore (SRE cannot be cleared in this implementation)
                trace!("ICC_SRE_EL1 write: ignored");
            }

            // ICC_IGRPEN0_EL1: Interrupt Group 0 Enable (CRn=12, CRm=12, Op2=6)
            (12, 12, 6, false) => {
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_igrpen(0);
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                    trace!("ICC_IGRPEN0_EL1 read: {}", value);
                }
            }
            (12, 12, 6, true) => {
                let value = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0);
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    cpu_if.write_igrpen(0, value);
                }
            }

            // ICC_IGRPEN1_EL1: Interrupt Group 1 Enable (CRn=12, CRm=12, Op2=7)
            (12, 12, 7, false) => {
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_igrpen(1);
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                    trace!("ICC_IGRPEN1_EL1 read: {}", value);
                }
            }
            (12, 12, 7, true) => {
                let value = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0);
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    cpu_if.write_igrpen(1, value);
                }
            }

            // ICC_RPR_EL1: Running Priority Register (CRn=12, CRm=11, Op2=3)
            (12, 11, 3, false) => {
                if let Some(cpu_if) = irq_chip.cpu_interface(vcpu_id) {
                    let value = cpu_if.read_rpr();
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), value)?;
                    trace!("ICC_RPR_EL1 read: {}", value);
                }
            }

            // ICC_SGI1_EL1: Software Generated Interrupt (Group 1)
            // Used for IPIs between CPUs.
            // NOTE: We assume flat affinity (Aff0=cpu_id, Aff1/2/3=0),
            // so target_list bits map directly to CPU IDs 0-15.
            (12, 11, 5, true) => {
                let value = vcpu.get_one_reg(VcpuRegAArch64::X(rt)).unwrap_or(0);
                let intid = ((value >> 24) & 0xF) as u32;
                let irm = (value >> 40) & 1;
                let target_list = (value & 0xFFFF) as u16;

                if irm == 1 {
                    // IRM=1: target all PEs except self
                    for cpu in 0..irq_chip.num_vcpus() {
                        if cpu != vcpu_id {
                            irq_chip.set_sgi_pending(cpu, intid);
                        }
                    }
                } else {
                    // IRM=0: use target list (bit per CPU in affinity group)
                    for cpu in 0..16usize {
                        if target_list & (1 << cpu) != 0 {
                            irq_chip.set_sgi_pending(cpu, intid);
                        }
                    }
                }

                coordinator.set_device_irq_pending();
                coordinator.kick_vcpus();
                trace!(
                    "ICC_SGI1_EL1 write: INTID={} IRM={} targets={:#x}",
                    intid, irm, target_list
                );
            }

            // ICC_IAR0_EL1: Group 0 Interrupt Acknowledge (CRn=12, CRm=8, Op2=0)
            (12, 8, 0, false) => {
                // For Group 0, return spurious since we only support Group 1
                vcpu.set_one_reg(VcpuRegAArch64::X(rt), GIC_SPURIOUS_INTID as u64)?;
                trace!("ICC_IAR0_EL1 read: spurious (Group 0 not supported)");
            }

            // Unhandled ICC register
            _ => {
                debug!(
                    "Unhandled ICC register: {} Op0={} Op1={} CRn={} CRm={} Op2={} Rt={}",
                    if is_write { "MSR" } else { "MRS" },
                    op0, op1, crn, crm, op2, rt
                );
                // For reads, return 0; for writes, ignore
                if !is_write && rt < 31 {
                    vcpu.set_one_reg(VcpuRegAArch64::X(rt), 0)?;
                }
            }
        }
    } else {
        // Not a GICv3 ICC register - handle generically
        debug!(
            "Unhandled system register: {} Op0={} Op1={} CRn={} CRm={} Op2={} Rt={}",
            if is_write { "MSR" } else { "MRS" },
            op0, op1, crn, crm, op2, rt
        );
        // For reads, return 0; for writes, ignore
        if !is_write && rt < 31 {
            vcpu.set_one_reg(VcpuRegAArch64::X(rt), 0)?;
        }
    }

    // Advance PC past the MSR/MRS instruction
    vcpu.set_one_reg(VcpuRegAArch64::Pc, pc + 4)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the SMCCC stack layout assumption: the result buffer address
    /// is at SP+16, not SP+0, because the SMCCC macro executes
    /// `stp x29, x30, [sp, #-16]!` before the HVC instruction.
    #[test]
    fn smccc_stack_offset_reads_result_buffer_not_saved_regs() {
        let saved_x29: u64 = 0xDEAD_BEEF_DEAD_BEEF;
        let saved_x30: u64 = 0xCAFE_BABE_CAFE_BABE;
        let result_buffer_addr: u64 = 0xFFFF_8000_1234_5678;
        let quirks_ptr: u64 = 0x0000_0000_0000_0000;

        let sp_phys: u64 = 0x1000;

        let guest_mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();

        guest_mem
            .write_at_addr(&saved_x29.to_le_bytes(), GuestAddress(sp_phys))
            .unwrap();
        guest_mem
            .write_at_addr(&saved_x30.to_le_bytes(), GuestAddress(sp_phys + 8))
            .unwrap();
        guest_mem
            .write_at_addr(
                &result_buffer_addr.to_le_bytes(),
                GuestAddress(sp_phys + 16),
            )
            .unwrap();
        guest_mem
            .write_at_addr(&quirks_ptr.to_le_bytes(), GuestAddress(sp_phys + 24))
            .unwrap();

        // BUG (before fix): reading at sp_phys+0 would return saved_x29
        let mut wrong_data = [0u8; 8];
        guest_mem
            .read_at_addr(&mut wrong_data, GuestAddress(sp_phys))
            .unwrap();
        let wrong_value = u64::from_le_bytes(wrong_data);
        assert_eq!(
            wrong_value, saved_x29,
            "SP+0 should be saved x29, not the result buffer"
        );

        // FIX: reading at sp_phys+16 returns the actual result buffer address
        let mut correct_data = [0u8; 8];
        guest_mem
            .read_at_addr(&mut correct_data, GuestAddress(sp_phys + 16))
            .unwrap();
        let correct_value = u64::from_le_bytes(correct_data);
        assert_eq!(
            correct_value, result_buffer_addr,
            "SP+16 should be the result buffer address"
        );
    }

    /// Verify ICC_SGI1_EL1 register value parsing: target list, INTID, IRM.
    #[test]
    fn icc_sgi1_el1_parsing() {
        // IRM=0, INTID=1, target CPUs 0 and 2
        let value: u64 = (1u64 << 24) | 0b0101;
        let intid = ((value >> 24) & 0xF) as u32;
        let irm = (value >> 40) & 1;
        let target_list = (value & 0xFFFF) as u16;

        assert_eq!(intid, 1);
        assert_eq!(irm, 0);
        assert_eq!(target_list, 0b0101);
        assert!(target_list & (1 << 0) != 0); // CPU 0 targeted
        assert!(target_list & (1 << 1) == 0); // CPU 1 not targeted
        assert!(target_list & (1 << 2) != 0); // CPU 2 targeted
    }

    /// Verify ICC_SGI1_EL1 IRM=1 (broadcast to all except self)
    #[test]
    fn icc_sgi1_el1_broadcast() {
        // IRM=1, INTID=0 (SGI 0 to all PEs except self)
        let value: u64 = 1u64 << 40;
        let intid = ((value >> 24) & 0xF) as u32;
        let irm = (value >> 40) & 1;

        assert_eq!(intid, 0);
        assert_eq!(irm, 1);
    }
}