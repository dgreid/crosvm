// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! HVF IrqChip implementation for macOS ARM64.
//!
//! This module provides a userspace GICv3 emulation for use with Apple's
//! Hypervisor.framework on Apple Silicon Macs.

mod gic;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use base::debug;
use base::error;
use base::info;
use base::Error;
use base::Event;
use base::Result;
use hypervisor::DeviceKind;
use hypervisor::IrqRoute;
use hypervisor::MPState;
use hypervisor::Vcpu;
use resources::SystemAllocator;
use sync::Mutex;

pub use gic::DistributorState;
pub use gic::GicCpuInterface;
pub use gic::GicDistributor;
pub use gic::GicRedistributor;
pub use gic::RedistributorState;
pub use gic::GIC_SPURIOUS_INTID;
pub use gic::GICD_SIZE;
pub use gic::GICR_SIZE;
pub use gic::GIC_NR_PPIS;
pub use gic::GIC_NR_SGIS;
pub use gic::GIC_NR_SPIS;
pub use gic::GIC_SPI_BASE;
pub use gic::VTIMER_INTID;
pub use gic::VTIMER_PPI;

use crate::Bus;
use crate::IrqChip;
use crate::IrqChipAArch64;
use crate::IrqChipCap;
use crate::IrqEdgeEvent;
use crate::IrqEventIndex;
use crate::IrqEventSource;
use crate::IrqLevelEvent;
use crate::VcpuRunState;

/// GIC address constants - place GIC above guest RAM at 3GB mark
/// HVF requires 32MB for redistributor region
pub const HVF_GIC_REDIST_REGION_SIZE: u64 = 0x2000000; // 32MB
pub const AARCH64_GIC_DIST_SIZE: u64 = GICD_SIZE;
pub const AARCH64_GIC_REDIST_SIZE: u64 = GICR_SIZE;
// Place distributor at 3GB + 32MB
pub const AARCH64_GIC_DIST_BASE: u64 = 0xC0000000 + HVF_GIC_REDIST_REGION_SIZE;

/// Calculate redistributor base address (place at 3GB, before distributor)
pub fn gic_redist_base(_num_cpus: usize) -> u64 {
    // HVF requires 32MB region regardless of CPU count
    0xC0000000
}

/// Number of SPIs for the irq chip
pub const AARCH64_GIC_NR_SPIS: u32 = GIC_NR_SPIS;

/// Default ARM routing table. AARCH64_GIC_NR_SPIS pins go to VGIC.
fn default_irq_routing_table() -> Vec<IrqRoute> {
    let mut routes: Vec<IrqRoute> = Vec::new();
    for i in 0..AARCH64_GIC_NR_SPIS {
        routes.push(IrqRoute::gic_irq_route(i));
    }
    routes
}

/// IrqEvent tracking for userspace handling
struct IrqEvent {
    event: Event,
    gsi: u32,
    resample_event: Option<Event>,
    source: IrqEventSource,
}

/// HVF IrqChip implementation with userspace GICv3 emulation.
///
/// This implementation creates GIC distributor and redistributor devices
/// that are registered on the MMIO bus. It handles interrupt routing
/// and injection to VCPUs.
///
/// Interrupt delivery approach:
/// - Userspace GIC (distributor + redistributors) track pending/enabled state
/// - CPU interfaces (ICC registers) are trapped and handled in userspace
/// - When an interrupt can be delivered, we call `hv_vcpu_set_pending_interrupt()`
/// - Guest reads ICC_IAR1_EL1 to acknowledge (trapped, returns INTID from userspace)
/// - Guest writes ICC_EOIR1_EL1 to complete (trapped, clears state in userspace)
pub struct HvfIrqChip {
    /// Number of VCPUs
    num_vcpus: usize,
    /// IRQ routing table
    routes: Arc<Mutex<Vec<IrqRoute>>>,
    /// Registered IRQ events for edge-triggered interrupts
    irq_events: Arc<Mutex<Vec<IrqEvent>>>,
    /// GIC distributor state (shared for reading pending state)
    distributor: Arc<Mutex<GicDistributor>>,
    /// GIC redistributors (one per CPU)
    redistributors: Vec<Arc<Mutex<GicRedistributor>>>,
    /// GIC CPU interfaces (one per CPU) - for ICC register traps
    cpu_interfaces: Arc<Vec<GicCpuInterface>>,
    /// Event to signal delayed IRQ processing
    delayed_event: Event,
}

impl HvfIrqChip {
    /// Create a new HvfIrqChip with the specified number of VCPUs.
    ///
    /// This creates the GIC distributor and redistributor devices but does
    /// not register them on the MMIO bus. Call `register_devices` to add
    /// them to the bus.
    pub fn new(num_vcpus: usize) -> Result<Self> {
        let distributor = GicDistributor::new(num_vcpus);
        let redistributors: Vec<_> = (0..num_vcpus)
            .map(|cpu_id| {
                let is_last = cpu_id == num_vcpus - 1;
                Arc::new(Mutex::new(GicRedistributor::new(cpu_id, is_last)))
            })
            .collect();
        let cpu_interfaces: Vec<_> = (0..num_vcpus)
            .map(|_| GicCpuInterface::new())
            .collect();

        Ok(Self {
            num_vcpus,
            routes: Arc::new(Mutex::new(default_irq_routing_table())),
            irq_events: Arc::new(Mutex::new(Vec::new())),
            distributor: Arc::new(Mutex::new(distributor)),
            redistributors,
            cpu_interfaces: Arc::new(cpu_interfaces),
            delayed_event: Event::new()?,
        })
    }

    /// Register GIC devices on the MMIO bus.
    ///
    /// This should be called after creating the HvfIrqChip to add the
    /// GIC distributor and redistributors to the VM's MMIO bus.
    pub fn register_devices(&self, mmio_bus: &Bus) -> Result<()> {
        // Register the SAME distributor used for interrupt handling
        mmio_bus
            .insert(
                self.distributor.clone(),
                AARCH64_GIC_DIST_BASE,
                AARCH64_GIC_DIST_SIZE,
            )
            .map_err(|e| {
                error!("Failed to insert GIC distributor: {:?}", e);
                Error::new(libc::ENOMEM)
            })?;

        // Register the SAME redistributors
        let redist_base = gic_redist_base(self.num_vcpus);
        for cpu_id in 0..self.num_vcpus {
            let addr = redist_base + (cpu_id as u64 * AARCH64_GIC_REDIST_SIZE);
            mmio_bus
                .insert(self.redistributors[cpu_id].clone(), addr, AARCH64_GIC_REDIST_SIZE)
                .map_err(|e| {
                    error!("Failed to insert GIC redistributor {}: {:?}", cpu_id, e);
                    Error::new(libc::ENOMEM)
                })?;
        }

        debug!(
            "GIC registered: distributor at {:#x}, redistributors at {:#x} ({} CPUs)",
            AARCH64_GIC_DIST_BASE, redist_base, self.num_vcpus
        );

        Ok(())
    }

    /// Get the GIC distributor base address
    pub fn distributor_base(&self) -> u64 {
        AARCH64_GIC_DIST_BASE
    }

    /// Get the GIC redistributor base address
    pub fn redistributor_base(&self) -> u64 {
        gic_redist_base(self.num_vcpus)
    }

    /// Set a PPI as pending for a specific CPU.
    ///
    /// This is called when a timer interrupt fires to mark the
    /// virtual timer PPI as pending in the redistributor.
    pub fn set_ppi_pending(&self, cpu_id: usize, ppi: u32) {
        if cpu_id < self.redistributors.len() {
            self.redistributors[cpu_id].lock().state().set_ppi_pending(ppi);
        }
    }

    /// Clear a PPI pending state for a specific CPU.
    pub fn clear_ppi_pending(&self, cpu_id: usize, ppi: u32) {
        if cpu_id < self.redistributors.len() {
            self.redistributors[cpu_id].lock().state().clear_ppi_pending(ppi);
        }
    }

    /// Set an SPI as pending in the distributor.
    pub fn set_spi_pending(&self, spi: u32) {
        self.distributor.lock().state().set_spi_pending(spi);
    }

    /// Clear an SPI pending state in the distributor.
    pub fn clear_spi_pending(&self, spi: u32) {
        self.distributor.lock().state().clear_spi_pending(spi);
    }

    /// Check if any interrupt is pending and enabled for a CPU.
    ///
    /// Returns the INTID of the highest priority pending interrupt,
    /// or None if no interrupt is pending.
    pub fn get_pending_irq(&self, cpu_id: usize) -> Option<u32> {
        // Check PPIs first (higher priority)
        if cpu_id < self.redistributors.len() {
            let redist = self.redistributors[cpu_id].lock();
            let state = redist.state();
            for intid in 0..32 {
                if state.is_irq_deliverable(intid) {
                    return Some(intid);
                }
            }
        }

        // Then check SPIs
        // For now, return None for SPIs (would need full routing logic)
        None
    }

    /// Get the CPU interface for a specific VCPU.
    pub fn cpu_interface(&self, cpu_id: usize) -> Option<&GicCpuInterface> {
        self.cpu_interfaces.get(cpu_id)
    }

    /// Get the distributor state (for interrupt acknowledgment).
    pub fn distributor_state(&self) -> &Arc<Mutex<GicDistributor>> {
        &self.distributor
    }

    /// Get the redistributor for a specific CPU.
    pub fn redistributor(&self, cpu_id: usize) -> Option<&Arc<Mutex<GicRedistributor>>> {
        self.redistributors.get(cpu_id)
    }

    /// Find the highest priority pending interrupt for a CPU.
    ///
    /// This checks both PPIs (in the redistributor) and SPIs (in the distributor)
    /// and returns the highest priority (lowest numerical priority value) pending
    /// and enabled interrupt.
    ///
    /// Returns (INTID, priority) or None if no interrupt is pending.
    pub fn get_highest_priority_pending(&self, cpu_id: usize) -> Option<(u32, u8)> {
        let mut best: Option<(u32, u8)> = None;

        // Check private interrupts (SGIs/PPIs) in the redistributor
        if let Some(redist) = self.redistributors.get(cpu_id) {
            if let Some((intid, prio)) = redist.lock().state().get_highest_priority_pending() {
                best = Some((intid, prio));
            }
        }

        // Check SPIs in the distributor
        if let Some((intid, prio)) = self.distributor.lock().state().get_highest_priority_pending_spi() {
            match best {
                None => best = Some((intid, prio)),
                Some((_, best_prio)) if prio < best_prio => best = Some((intid, prio)),
                _ => {}
            }
        }

        best
    }

    /// Acknowledge an interrupt for a CPU (called when guest reads ICC_IAR1_EL1).
    ///
    /// Returns the INTID of the acknowledged interrupt (or 1023 for spurious).
    pub fn acknowledge_interrupt(&self, cpu_id: usize) -> u32 {
        let cpu_if = match self.cpu_interfaces.get(cpu_id) {
            Some(c) => c,
            None => return gic::GIC_SPURIOUS_INTID,
        };

        // Find the highest priority pending interrupt
        let pending = self.get_highest_priority_pending(cpu_id);
        let (intid, priority) = match pending {
            Some((id, prio)) => (Some(id), prio),
            None => (None, 0xff),
        };

        // Acknowledge through the CPU interface
        let acked_intid = cpu_if.acknowledge(intid, priority);

        // If we acknowledged an interrupt, clear its pending state
        if acked_intid != gic::GIC_SPURIOUS_INTID {
            if acked_intid < gic::GIC_SPI_BASE {
                // Private interrupt (SGI/PPI) - clear in redistributor
                if let Some(redist) = self.redistributors.get(cpu_id) {
                    redist.lock().state().pending0.fetch_and(
                        !(1 << acked_intid),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                }
            } else {
                // SPI - clear in distributor
                let spi = acked_intid - gic::GIC_SPI_BASE;
                self.distributor.lock().state().clear_spi_pending(spi);
            }
        }

        acked_intid
    }

    /// End of interrupt for a CPU (called when guest writes ICC_EOIR1_EL1).
    pub fn end_of_interrupt(&self, cpu_id: usize, intid: u32) {
        if let Some(cpu_if) = self.cpu_interfaces.get(cpu_id) {
            cpu_if.end_of_interrupt(intid);
        }
    }

    /// Check if an interrupt should be signaled to the VCPU.
    ///
    /// This should be called before vcpu.run() to determine if
    /// hv_vcpu_set_pending_interrupt() should be called.
    pub fn should_signal_irq(&self, cpu_id: usize) -> bool {
        // Check if group 1 is enabled in the CPU interface
        let cpu_if = match self.cpu_interfaces.get(cpu_id) {
            Some(c) => c,
            None => return false,
        };

        if !cpu_if.is_group1_enabled() {
            return false;
        }

        // Check if we have a pending interrupt that can preempt
        if let Some((_, priority)) = self.get_highest_priority_pending(cpu_id) {
            cpu_if.can_preempt(priority)
        } else {
            false
        }
    }

    /// Diagnose why an interrupt cannot be delivered.
    ///
    /// This logs detailed information about the GIC state to help debug
    /// interrupt delivery issues. Call this when an interrupt is pending
    /// but `should_signal_irq()` returns false.
    pub fn diagnose_irq_delivery(&self, cpu_id: usize) {
        let cpu_if = match self.cpu_interfaces.get(cpu_id) {
            Some(c) => c,
            None => {
                info!("IRQ diagnosis: No CPU interface for CPU {}", cpu_id);
                return;
            }
        };

        let group1_enabled = cpu_if.is_group1_enabled();
        let pmr = cpu_if.pmr.load(Ordering::Acquire);
        let running_priority = cpu_if.running_priority.load(Ordering::Acquire);
        let active_intid = cpu_if.active_intid.load(Ordering::Acquire);

        let dist = self.distributor.lock();
        let dist_state = dist.state();
        let dist_ctlr = dist_state.ctlr.load(Ordering::Acquire);
        let pending = dist_state.pending[0].load(Ordering::Acquire);
        let enabled = dist_state.enable[0].load(Ordering::Acquire);
        drop(dist);

        info!(
            "IRQ diagnosis CPU {}: group1_enabled={}, PMR={:#x}, running_priority={:#x}, \
             active_intid={}, dist_ctlr={:#x}",
            cpu_id, group1_enabled, pmr, running_priority, active_intid, dist_ctlr
        );

        info!(
            "IRQ diagnosis CPU {}: SPI pending={:#x}, enabled={:#x}, deliverable={:#x}",
            cpu_id, pending, enabled, pending & enabled
        );

        if let Some((intid, priority)) = self.get_highest_priority_pending(cpu_id) {
            let can_preempt = cpu_if.can_preempt(priority);
            info!(
                "IRQ diagnosis CPU {}: pending INTID {} with priority {:#x}, can_preempt={}",
                cpu_id, intid, priority, can_preempt
            );

            if !can_preempt {
                if priority >= running_priority {
                    info!(
                        "IRQ blocked: priority {:#x} >= running_priority {:#x}",
                        priority, running_priority
                    );
                }
                if priority >= pmr {
                    info!(
                        "IRQ blocked: priority {:#x} >= PMR {:#x}",
                        priority, pmr
                    );
                }
            }
        } else {
            info!("IRQ diagnosis CPU {}: no pending interrupt", cpu_id);
        }
    }

    fn arch_try_clone(&self) -> Result<Self> {
        Ok(Self {
            num_vcpus: self.num_vcpus,
            routes: self.routes.clone(),
            irq_events: self.irq_events.clone(),
            distributor: self.distributor.clone(),
            redistributors: self.redistributors.clone(),
            cpu_interfaces: self.cpu_interfaces.clone(),
            delayed_event: self.delayed_event.try_clone()?,
        })
    }
}

impl IrqChip for HvfIrqChip {
    fn add_vcpu(&mut self, _vcpu_id: usize, _vcpu: &dyn Vcpu) -> Result<()> {
        // VCPUs are tracked by redistributor index
        Ok(())
    }

    fn register_edge_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqEdgeEvent,
        source: IrqEventSource,
    ) -> Result<Option<IrqEventIndex>> {
        let mut events = self.irq_events.lock();
        let index = events.len();
        events.push(IrqEvent {
            event: irq_event.get_trigger().try_clone()?,
            gsi: irq,
            resample_event: None,
            source,
        });
        Ok(Some(index))
    }

    fn unregister_edge_irq_event(&mut self, irq: u32, _irq_event: &IrqEdgeEvent) -> Result<()> {
        let mut events = self.irq_events.lock();
        events.retain(|e| e.gsi != irq);
        Ok(())
    }

    fn register_level_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqLevelEvent,
        source: IrqEventSource,
    ) -> Result<Option<IrqEventIndex>> {
        let mut events = self.irq_events.lock();
        let index = events.len();
        events.push(IrqEvent {
            event: irq_event.get_trigger().try_clone()?,
            gsi: irq,
            resample_event: Some(irq_event.get_resample().try_clone()?),
            source,
        });
        Ok(Some(index))
    }

    fn unregister_level_irq_event(&mut self, irq: u32, _irq_event: &IrqLevelEvent) -> Result<()> {
        let mut events = self.irq_events.lock();
        events.retain(|e| e.gsi != irq);
        Ok(())
    }

    fn route_irq(&mut self, route: IrqRoute) -> Result<()> {
        let mut routes = self.routes.lock();
        routes.retain(|r| r.gsi != route.gsi);
        routes.push(route);
        Ok(())
    }

    fn set_irq_routes(&mut self, routes: &[IrqRoute]) -> Result<()> {
        let mut current_routes = self.routes.lock();
        *current_routes = routes.to_vec();
        Ok(())
    }

    fn irq_event_tokens(&self) -> Result<Vec<(IrqEventIndex, IrqEventSource, Event)>> {
        let events = self.irq_events.lock();
        events
            .iter()
            .enumerate()
            .map(|(idx, e)| Ok((idx, e.source.clone(), e.event.try_clone()?)))
            .collect()
    }

    fn service_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        // Convert GSI to SPI (GSI 0 = SPI 0 = INTID 32)
        if level {
            self.set_spi_pending(irq);
        } else {
            self.clear_spi_pending(irq);
        }
        Ok(())
    }

    fn service_irq_event(&mut self, event_index: IrqEventIndex) -> Result<()> {
        let events = self.irq_events.lock();
        if let Some(irq_event) = events.get(event_index) {
            // Read the event to clear it
            let _ = irq_event.event.wait_timeout(std::time::Duration::ZERO);
            // Set the interrupt pending
            self.set_spi_pending(irq_event.gsi);

            // If there's a resample event, we need to handle EOI
            // For now, just leave it pending until explicitly cleared
        }
        Ok(())
    }

    fn broadcast_eoi(&self, _vector: u8) -> Result<()> {
        // EOI is handled through MMIO writes to ICACTIVER
        Ok(())
    }

    fn inject_interrupts(&self, _vcpu: &dyn Vcpu) -> Result<()> {
        // Interrupt injection is handled in the VCPU run loop
        // by calling hv_vcpu_set_pending_interrupt
        Ok(())
    }

    fn halted(&self, _vcpu_id: usize) {
        // HVF handles WFI internally
    }

    fn wait_until_runnable(&self, _vcpu: &dyn Vcpu) -> Result<VcpuRunState> {
        // HVF handles this internally
        Ok(VcpuRunState::Runnable)
    }

    fn kick_halted_vcpus(&self) {
        // HVF handles this internally
    }

    fn get_mp_state(&self, _vcpu_id: usize) -> Result<MPState> {
        // HVF doesn't expose MP state
        Err(Error::new(libc::ENOENT))
    }

    fn set_mp_state(&mut self, _vcpu_id: usize, _state: &MPState) -> Result<()> {
        // HVF doesn't expose MP state
        Err(Error::new(libc::ENOENT))
    }

    fn try_clone(&self) -> Result<Self> {
        self.arch_try_clone()
    }

    fn finalize_devices(
        &mut self,
        _resources: &mut SystemAllocator,
        _io_bus: &Bus,
        _mmio_bus: &Bus,
    ) -> Result<()> {
        Ok(())
    }

    fn process_delayed_irq_events(&mut self) -> Result<()> {
        Ok(())
    }

    fn irq_delayed_event_token(&self) -> Result<Option<Event>> {
        Ok(Some(self.delayed_event.try_clone()?))
    }

    fn check_capability(&self, c: IrqChipCap) -> bool {
        match c {
            IrqChipCap::MpStateGetSet => false,
        }
    }
}

impl IrqChipAArch64 for HvfIrqChip {
    fn try_box_clone(&self) -> Result<Box<dyn IrqChipAArch64>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn as_irq_chip(&self) -> &dyn IrqChip {
        self
    }

    fn as_irq_chip_mut(&mut self) -> &mut dyn IrqChip {
        self
    }

    fn get_vgic_version(&self) -> DeviceKind {
        DeviceKind::ArmVgicV3
    }

    fn has_vgic_its(&self) -> bool {
        false
    }

    fn finalize(&self) -> Result<()> {
        Ok(())
    }
}
