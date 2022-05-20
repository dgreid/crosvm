// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use sync::Mutex;

#[cfg(not(test))]
use base::Clock;
#[cfg(test)]
use base::FakeClock as Clock;
use hypervisor::kvm::{KvmVcpu, KvmVm};
use hypervisor::{
    HypervisorCap, IoapicState, IrqRoute, IrqSource, IrqSourceChip, LapicState, MPState, PicSelect,
    PicState, PitState, Vcpu, VcpuX86_64, Vm, VmX86_64,
};
use kvm_sys::*;
use resources::SystemAllocator;

use base::{error, Error, Event, Result, Tube};

use crate::irqchip::{
    Ioapic, IrqEvent, IrqEventIndex, Pic, VcpuRunState, IOAPIC_BASE_ADDRESS,
    IOAPIC_MEM_LENGTH_BYTES,
};
use crate::{Bus, IrqChip, IrqChipCap, IrqChipX86_64, IrqEdgeEvent, IrqLevelEvent, Pit, PitError};

/// PIT tube 0 timer is connected to IRQ 0
const PIT_CHANNEL0_IRQ: u32 = 0;

/// Default x86 routing table.  Pins 0-7 go to primary pic and ioapic, pins 8-15 go to secondary
/// pic and ioapic, and pins 16-23 go only to the ioapic.
fn kvm_default_irq_routing_table(ioapic_pins: usize) -> Vec<IrqRoute> {
    let mut routes: Vec<IrqRoute> = Vec::new();

    for i in 0..8 {
        routes.push(IrqRoute::pic_irq_route(IrqSourceChip::PicPrimary, i));
        routes.push(IrqRoute::ioapic_irq_route(i));
    }
    for i in 8..16 {
        routes.push(IrqRoute::pic_irq_route(IrqSourceChip::PicSecondary, i));
        routes.push(IrqRoute::ioapic_irq_route(i));
    }
    for i in 16..ioapic_pins as u32 {
        routes.push(IrqRoute::ioapic_irq_route(i));
    }

    routes
}

/// IrqChip implementation where the entire IrqChip is emulated by KVM.
///
/// This implementation will use the KVM API to create and configure the in-kernel irqchip.
pub struct KvmKernelIrqChip {
    pub(super) vm: KvmVm,
    pub(super) vcpus: Arc<Mutex<Vec<Option<KvmVcpu>>>>,
    pub(super) routes: Arc<Mutex<Vec<IrqRoute>>>,
}

impl KvmKernelIrqChip {
    /// Construct a new KvmKernelIrqchip.
    pub fn new(vm: KvmVm, num_vcpus: usize) -> Result<KvmKernelIrqChip> {
        vm.create_irq_chip()?;
        vm.create_pit()?;
        let ioapic_pins = vm.get_ioapic_num_pins()?;

        Ok(KvmKernelIrqChip {
            vm,
            vcpus: Arc::new(Mutex::new((0..num_vcpus).map(|_| None).collect())),
            routes: Arc::new(Mutex::new(kvm_default_irq_routing_table(ioapic_pins))),
        })
    }
    /// Attempt to create a shallow clone of this x86_64 KvmKernelIrqChip instance.
    pub(super) fn arch_try_clone(&self) -> Result<Self> {
        Ok(KvmKernelIrqChip {
            vm: self.vm.try_clone()?,
            vcpus: self.vcpus.clone(),
            routes: self.routes.clone(),
        })
    }
}

impl IrqChipX86_64 for KvmKernelIrqChip {
    fn try_box_clone(&self) -> Result<Box<dyn IrqChipX86_64>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn as_irq_chip(&self) -> &dyn IrqChip {
        self
    }

    fn as_irq_chip_mut(&mut self) -> &mut dyn IrqChip {
        self
    }

    /// Get the current state of the PIC
    fn get_pic_state(&self, select: PicSelect) -> Result<PicState> {
        Ok(PicState::from(&self.vm.get_pic_state(select)?))
    }

    /// Set the current state of the PIC
    fn set_pic_state(&mut self, select: PicSelect, state: &PicState) -> Result<()> {
        self.vm.set_pic_state(select, &kvm_pic_state::from(state))
    }

    /// Get the current state of the IOAPIC
    fn get_ioapic_state(&self) -> Result<IoapicState> {
        Ok(IoapicState::from(&self.vm.get_ioapic_state()?))
    }

    /// Set the current state of the IOAPIC
    fn set_ioapic_state(&mut self, state: &IoapicState) -> Result<()> {
        self.vm.set_ioapic_state(&kvm_ioapic_state::from(state))
    }

    /// Get the current state of the specified VCPU's local APIC
    fn get_lapic_state(&self, vcpu_id: usize) -> Result<LapicState> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => Ok(LapicState::from(&vcpu.get_lapic()?)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Set the current state of the specified VCPU's local APIC
    fn set_lapic_state(&mut self, vcpu_id: usize, state: &LapicState) -> Result<()> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => vcpu.set_lapic(&kvm_lapic_state::from(state)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Retrieves the state of the PIT. Gets the pit state via the KVM API.
    fn get_pit(&self) -> Result<PitState> {
        Ok(PitState::from(&self.vm.get_pit_state()?))
    }

    /// Sets the state of the PIT. Sets the pit state via the KVM API.
    fn set_pit(&mut self, state: &PitState) -> Result<()> {
        self.vm.set_pit_state(&kvm_pit_state2::from(state))
    }

    /// Returns true if the PIT uses port 0x61 for the PC speaker, false if 0x61 is unused.
    /// KVM's kernel PIT doesn't use 0x61.
    fn pit_uses_speaker_port(&self) -> bool {
        false
    }
}

/// The KvmSplitIrqsChip supports KVM's SPLIT_IRQCHIP feature, where the PIC and IOAPIC
/// are emulated in userspace, while the local APICs are emulated in the kernel.
/// The SPLIT_IRQCHIP feature only supports x86/x86_64 so we only define this IrqChip in crosvm
/// for x86/x86_64.
pub struct KvmSplitIrqChip {
    vm: KvmVm,
    vcpus: Arc<Mutex<Vec<Option<KvmVcpu>>>>,
    routes: Arc<Mutex<Vec<IrqRoute>>>,
    pit: Arc<Mutex<Pit>>,
    pic: Arc<Mutex<Pic>>,
    ioapic: Arc<Mutex<Ioapic>>,
    ioapic_pins: usize,
    /// Vec of ioapic irq events that have been delayed because the ioapic was locked when
    /// service_irq was called on the irqchip. This prevents deadlocks when a Vcpu thread has
    /// locked the ioapic and the ioapic sends a AddMsiRoute signal to the main thread (which
    /// itself may be busy trying to call service_irq).
    delayed_ioapic_irq_events: Arc<Mutex<Vec<usize>>>,
    /// Event which is meant to trigger process of any irqs events that were delayed.
    delayed_ioapic_irq_trigger: Event,
    /// Array of Events that devices will use to assert ioapic pins.
    irq_events: Arc<Mutex<Vec<Option<IrqEvent>>>>,
}

fn kvm_dummy_msi_routes(ioapic_pins: usize) -> Vec<IrqRoute> {
    let mut routes: Vec<IrqRoute> = Vec::new();
    for i in 0..ioapic_pins {
        routes.push(
            // Add dummy MSI routes to replace the default IRQChip routes.
            IrqRoute {
                gsi: i as u32,
                source: IrqSource::Msi {
                    address: 0,
                    data: 0,
                },
            },
        );
    }
    routes
}

impl KvmSplitIrqChip {
    /// Construct a new KvmSplitIrqChip.
    pub fn new(
        vm: KvmVm,
        num_vcpus: usize,
        irq_tube: Tube,
        ioapic_pins: Option<usize>,
    ) -> Result<Self> {
        let ioapic_pins = ioapic_pins.unwrap_or(vm.get_ioapic_num_pins()?);
        vm.enable_split_irqchip(ioapic_pins)?;
        let pit_evt = IrqEdgeEvent::new()?;
        let pit = Arc::new(Mutex::new(
            Pit::new(
                pit_evt.get_trigger().try_clone()?,
                Arc::new(Mutex::new(Clock::new())),
            )
            .map_err(|e| match e {
                PitError::CloneEvent(err) => err,
                PitError::CreateEvent(err) => err,
                PitError::CreateWaitContext(err) => err,
                PitError::WaitError(err) => err,
                PitError::TimerCreateError(err) => err,
                PitError::SpawnThread(_) => Error::new(libc::EIO),
            })?,
        ));

        let mut chip = KvmSplitIrqChip {
            vm,
            vcpus: Arc::new(Mutex::new((0..num_vcpus).map(|_| None).collect())),
            routes: Arc::new(Mutex::new(Vec::new())),
            pit,
            pic: Arc::new(Mutex::new(Pic::new())),
            ioapic: Arc::new(Mutex::new(Ioapic::new(irq_tube, ioapic_pins)?)),
            ioapic_pins,
            delayed_ioapic_irq_events: Arc::new(Mutex::new(Vec::new())),
            delayed_ioapic_irq_trigger: Event::new()?,
            irq_events: Arc::new(Mutex::new(Default::default())),
        };

        // crosvm-direct requires 1:1 GSI mapping between host and guest. The predefined IRQ
        // numbering will be exposed to the guest with no option to allocate it dynamically.
        // Tell the IOAPIC to fill in IRQ output events with 1:1 GSI mapping upfront so that
        // IOAPIC wont assign a new GSI but use the same as for host instead.
        #[cfg(feature = "direct")]
        chip.ioapic
            .lock()
            .init_direct_gsi(|gsi, event| chip.vm.register_irqfd(gsi as u32, &event, None))?;

        // Setup standard x86 irq routes
        let mut routes = kvm_default_irq_routing_table(ioapic_pins);
        // Add dummy MSI routes for the first ioapic_pins GSIs
        routes.append(&mut kvm_dummy_msi_routes(ioapic_pins));

        // Set the routes so they get sent to KVM
        chip.set_irq_routes(&routes)?;

        chip.register_edge_irq_event(PIT_CHANNEL0_IRQ, &pit_evt)?;
        Ok(chip)
    }
}

impl KvmSplitIrqChip {
    /// Convenience function for determining which chips the supplied irq routes to.
    fn routes_to_chips(&self, irq: u32) -> Vec<(IrqSourceChip, u32)> {
        let mut chips = Vec::new();
        for route in self.routes.lock().iter() {
            match route {
                IrqRoute {
                    gsi,
                    source: IrqSource::Irqchip { chip, pin },
                } if *gsi == irq => match chip {
                    IrqSourceChip::PicPrimary
                    | IrqSourceChip::PicSecondary
                    | IrqSourceChip::Ioapic => chips.push((*chip, *pin)),
                    IrqSourceChip::Gic => {
                        error!("gic irq should not be possible on a KvmSplitIrqChip")
                    }
                    IrqSourceChip::Aia => {
                        error!("Aia irq should not be possible on x86_64")
                    }
                },
                // Ignore MSIs and other routes
                _ => {}
            }
        }
        chips
    }

    /// Return true if there is a pending interrupt for the specified vcpu. For KvmSplitIrqChip
    /// this calls interrupt_requested on the pic.
    fn interrupt_requested(&self, vcpu_id: usize) -> bool {
        // Pic interrupts for the split irqchip only go to vcpu 0
        if vcpu_id != 0 {
            return false;
        }
        self.pic.lock().interrupt_requested()
    }

    /// Check if the specified vcpu has any pending interrupts. Returns None for no interrupts,
    /// otherwise Some(u32) should be the injected interrupt vector. For KvmSplitIrqChip
    /// this calls get_external_interrupt on the pic.
    fn get_external_interrupt(&self, vcpu_id: usize) -> Option<u32> {
        // Pic interrupts for the split irqchip only go to vcpu 0
        if vcpu_id != 0 {
            return None;
        }
        self.pic
            .lock()
            .get_external_interrupt()
            .map(|vector| vector as u32)
    }

    /// Register an event that can trigger an interrupt for a particular GSI.
    fn register_irq_event(
        &mut self,
        irq: u32,
        irq_event: &Event,
        resample_event: Option<&Event>,
    ) -> Result<Option<IrqEventIndex>> {
        if irq < self.ioapic_pins as u32 {
            let mut evt = IrqEvent {
                gsi: irq,
                event: irq_event.try_clone()?,
                resample_event: None,
            };

            if let Some(resample_event) = resample_event {
                evt.resample_event = Some(resample_event.try_clone()?);
            }

            let mut irq_events = self.irq_events.lock();
            let index = irq_events.len();
            irq_events.push(Some(evt));
            Ok(Some(index))
        } else {
            self.vm.register_irqfd(irq, irq_event, resample_event)?;
            Ok(None)
        }
    }

    /// Unregister an event for a particular GSI.
    fn unregister_irq_event(&mut self, irq: u32, irq_event: &Event) -> Result<()> {
        if irq < self.ioapic_pins as u32 {
            let mut irq_events = self.irq_events.lock();
            for (index, evt) in irq_events.iter().enumerate() {
                if let Some(evt) = evt {
                    if evt.gsi == irq && irq_event.eq(&evt.event) {
                        irq_events[index] = None;
                        break;
                    }
                }
            }
            Ok(())
        } else {
            self.vm.unregister_irqfd(irq, irq_event)
        }
    }
}

/// Convenience function for determining whether or not two irq routes conflict.
/// Returns true if they conflict.
fn routes_conflict(route: &IrqRoute, other: &IrqRoute) -> bool {
    // They don't conflict if they have different GSIs.
    if route.gsi != other.gsi {
        return false;
    }

    // If they're both MSI with the same GSI then they conflict.
    if let (IrqSource::Msi { .. }, IrqSource::Msi { .. }) = (route.source, other.source) {
        return true;
    }

    // If the route chips match and they have the same GSI then they conflict.
    if let (
        IrqSource::Irqchip {
            chip: route_chip, ..
        },
        IrqSource::Irqchip {
            chip: other_chip, ..
        },
    ) = (route.source, other.source)
    {
        return route_chip == other_chip;
    }

    // Otherwise they do not conflict.
    false
}

/// This IrqChip only works with Kvm so we only implement it for KvmVcpu.
impl IrqChip for KvmSplitIrqChip {
    /// Add a vcpu to the irq chip.
    fn add_vcpu(&mut self, vcpu_id: usize, vcpu: &dyn Vcpu) -> Result<()> {
        let vcpu: &KvmVcpu = vcpu
            .downcast_ref()
            .expect("KvmSplitIrqChip::add_vcpu called with non-KvmVcpu");
        self.vcpus.lock()[vcpu_id] = Some(vcpu.try_clone()?);
        Ok(())
    }

    /// Register an event that can trigger an interrupt for a particular GSI.
    fn register_edge_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqEdgeEvent,
    ) -> Result<Option<IrqEventIndex>> {
        self.register_irq_event(irq, irq_event.get_trigger(), None)
    }

    fn unregister_edge_irq_event(&mut self, irq: u32, irq_event: &IrqEdgeEvent) -> Result<()> {
        self.unregister_irq_event(irq, irq_event.get_trigger())
    }

    fn register_level_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqLevelEvent,
    ) -> Result<Option<IrqEventIndex>> {
        self.register_irq_event(irq, irq_event.get_trigger(), Some(irq_event.get_resample()))
    }

    fn unregister_level_irq_event(&mut self, irq: u32, irq_event: &IrqLevelEvent) -> Result<()> {
        self.unregister_irq_event(irq, irq_event.get_trigger())
    }

    /// Route an IRQ line to an interrupt controller, or to a particular MSI vector.
    fn route_irq(&mut self, route: IrqRoute) -> Result<()> {
        let mut routes = self.routes.lock();
        routes.retain(|r| !routes_conflict(r, &route));

        routes.push(route);

        // We only call set_gsi_routing with the msi routes
        let mut msi_routes = routes.clone();
        msi_routes.retain(|r| matches!(r.source, IrqSource::Msi { .. }));

        self.vm.set_gsi_routing(&*msi_routes)
    }

    /// Replace all irq routes with the supplied routes
    fn set_irq_routes(&mut self, routes: &[IrqRoute]) -> Result<()> {
        let mut current_routes = self.routes.lock();
        *current_routes = routes.to_vec();

        // We only call set_gsi_routing with the msi routes
        let mut msi_routes = routes.to_vec();
        msi_routes.retain(|r| matches!(r.source, IrqSource::Msi { .. }));

        self.vm.set_gsi_routing(&*msi_routes)
    }

    /// Return a vector of all registered irq numbers and their associated events and event
    /// indices. These should be used by the main thread to wait for irq events.
    fn irq_event_tokens(&self) -> Result<Vec<(IrqEventIndex, u32, Event)>> {
        let mut tokens: Vec<(IrqEventIndex, u32, Event)> = Vec::new();
        for (index, evt) in self.irq_events.lock().iter().enumerate() {
            if let Some(evt) = evt {
                tokens.push((index, evt.gsi, evt.event.try_clone()?));
            }
        }
        Ok(tokens)
    }

    /// Either assert or deassert an IRQ line.  Sends to either an interrupt controller, or does
    /// a send_msi if the irq is associated with an MSI.
    fn service_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        let chips = self.routes_to_chips(irq);
        for (chip, pin) in chips {
            match chip {
                IrqSourceChip::PicPrimary | IrqSourceChip::PicSecondary => {
                    self.pic.lock().service_irq(pin as u8, level);
                }
                IrqSourceChip::Ioapic => {
                    self.ioapic.lock().service_irq(pin as usize, level);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Service an IRQ event by asserting then deasserting an IRQ line. The associated Event
    /// that triggered the irq event will be read from. If the irq is associated with a resample
    /// Event, then the deassert will only happen after an EOI is broadcast for a vector
    /// associated with the irq line.
    /// For the KvmSplitIrqChip, this function identifies which chips the irq routes to, then
    /// attempts to call service_irq on those chips. If the ioapic is unable to be immediately
    /// locked, we add the irq to the delayed_ioapic_irq_events Vec (though we still read
    /// from the Event that triggered the irq event).
    fn service_irq_event(&mut self, event_index: IrqEventIndex) -> Result<()> {
        if let Some(evt) = &self.irq_events.lock()[event_index] {
            evt.event.read()?;
            let chips = self.routes_to_chips(evt.gsi);

            for (chip, pin) in chips {
                match chip {
                    IrqSourceChip::PicPrimary | IrqSourceChip::PicSecondary => {
                        let mut pic = self.pic.lock();
                        pic.service_irq(pin as u8, true);
                        if evt.resample_event.is_none() {
                            pic.service_irq(pin as u8, false);
                        }
                    }
                    IrqSourceChip::Ioapic => {
                        if let Ok(mut ioapic) = self.ioapic.try_lock() {
                            ioapic.service_irq(pin as usize, true);
                            if evt.resample_event.is_none() {
                                ioapic.service_irq(pin as usize, false);
                            }
                        } else {
                            self.delayed_ioapic_irq_events.lock().push(event_index);
                            self.delayed_ioapic_irq_trigger.write(1).unwrap();
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    /// Broadcast an end of interrupt. For KvmSplitIrqChip this sends the EOI to the ioapic
    fn broadcast_eoi(&self, vector: u8) -> Result<()> {
        self.ioapic.lock().end_of_interrupt(vector);
        Ok(())
    }

    /// Injects any pending interrupts for `vcpu`.
    /// For KvmSplitIrqChip this injects any PIC interrupts on vcpu_id 0.
    fn inject_interrupts(&self, vcpu: &dyn Vcpu) -> Result<()> {
        let vcpu: &KvmVcpu = vcpu
            .downcast_ref()
            .expect("KvmSplitIrqChip::add_vcpu called with non-KvmVcpu");

        let vcpu_id = vcpu.id();
        if !self.interrupt_requested(vcpu_id) || !vcpu.ready_for_interrupt() {
            return Ok(());
        }

        if let Some(vector) = self.get_external_interrupt(vcpu_id) {
            vcpu.interrupt(vector)?;
        }

        // The second interrupt request should be handled immediately, so ask vCPU to exit as soon as
        // possible.
        if self.interrupt_requested(vcpu_id) {
            vcpu.set_interrupt_window_requested(true);
        }
        Ok(())
    }

    /// Notifies the irq chip that the specified VCPU has executed a halt instruction.
    /// For KvmSplitIrqChip this is a no-op because KVM handles VCPU blocking.
    fn halted(&self, _vcpu_id: usize) {}

    /// Blocks until `vcpu` is in a runnable state or until interrupted by
    /// `IrqChip::kick_halted_vcpus`.  Returns `VcpuRunState::Runnable if vcpu is runnable, or
    /// `VcpuRunState::Interrupted` if the wait was interrupted.
    /// For KvmSplitIrqChip this is a no-op and always returns Runnable because KVM handles VCPU
    /// blocking.
    fn wait_until_runnable(&self, _vcpu: &dyn Vcpu) -> Result<VcpuRunState> {
        Ok(VcpuRunState::Runnable)
    }

    /// Makes unrunnable VCPUs return immediately from `wait_until_runnable`.
    /// For KvmSplitIrqChip this is a no-op because KVM handles VCPU blocking.
    fn kick_halted_vcpus(&self) {}

    /// Get the current MP state of the specified VCPU.
    fn get_mp_state(&self, vcpu_id: usize) -> Result<MPState> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => Ok(MPState::from(&vcpu.get_mp_state()?)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Set the current MP state of the specified VCPU.
    fn set_mp_state(&mut self, vcpu_id: usize, state: &MPState) -> Result<()> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => vcpu.set_mp_state(&kvm_mp_state::from(state)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Attempt to clone this IrqChip instance.
    fn try_clone(&self) -> Result<Self> {
        Ok(KvmSplitIrqChip {
            vm: self.vm.try_clone()?,
            vcpus: self.vcpus.clone(),
            routes: self.routes.clone(),
            pit: self.pit.clone(),
            pic: self.pic.clone(),
            ioapic: self.ioapic.clone(),
            ioapic_pins: self.ioapic_pins,
            delayed_ioapic_irq_events: self.delayed_ioapic_irq_events.clone(),
            delayed_ioapic_irq_trigger: Event::new()?,
            irq_events: self.irq_events.clone(),
        })
    }

    /// Finalize irqchip setup. Should be called once all devices have registered irq events and
    /// been added to the io_bus and mmio_bus.
    fn finalize_devices(
        &mut self,
        resources: &mut SystemAllocator,
        io_bus: &Bus,
        mmio_bus: &Bus,
    ) -> Result<()> {
        // Insert pit into io_bus
        io_bus.insert(self.pit.clone(), 0x040, 0x8).unwrap();
        io_bus.insert(self.pit.clone(), 0x061, 0x1).unwrap();

        // Insert pic into io_bus
        io_bus.insert(self.pic.clone(), 0x20, 0x2).unwrap();
        io_bus.insert(self.pic.clone(), 0xa0, 0x2).unwrap();
        io_bus.insert(self.pic.clone(), 0x4d0, 0x2).unwrap();

        // Insert ioapic into mmio_bus
        mmio_bus
            .insert(
                self.ioapic.clone(),
                IOAPIC_BASE_ADDRESS,
                IOAPIC_MEM_LENGTH_BYTES,
            )
            .unwrap();

        // At this point, all of our devices have been created and they have registered their
        // irq events, so we can clone our resample events
        let mut ioapic_resample_events: Vec<Vec<Event>> =
            (0..self.ioapic_pins).map(|_| Vec::new()).collect();
        let mut pic_resample_events: Vec<Vec<Event>> =
            (0..self.ioapic_pins).map(|_| Vec::new()).collect();

        for evt in self.irq_events.lock().iter().flatten() {
            if (evt.gsi as usize) >= self.ioapic_pins {
                continue;
            }
            if let Some(resample_evt) = &evt.resample_event {
                ioapic_resample_events[evt.gsi as usize].push(resample_evt.try_clone()?);
                pic_resample_events[evt.gsi as usize].push(resample_evt.try_clone()?);
            }
        }

        // Register resample events with the ioapic
        self.ioapic
            .lock()
            .register_resample_events(ioapic_resample_events);
        // Register resample events with the pic
        self.pic
            .lock()
            .register_resample_events(pic_resample_events);

        // Make sure all future irq numbers are beyond IO-APIC range.
        let mut irq_num = resources.allocate_irq().unwrap();
        while irq_num < self.ioapic_pins as u32 {
            irq_num = resources.allocate_irq().unwrap();
        }

        Ok(())
    }

    /// The KvmSplitIrqChip's ioapic may be locked because a vcpu thread is currently writing to
    /// the ioapic, and the ioapic may be blocking on adding MSI routes, which requires blocking
    /// socket communication back to the main thread.  Thus, we do not want the main thread to
    /// block on a locked ioapic, so any irqs that could not be serviced because the ioapic could
    /// not be immediately locked are added to the delayed_ioapic_irq_events Vec. This function
    /// processes each delayed event in the vec each time it's called. If the ioapic is still
    /// locked, we keep the queued irqs for the next time this function is called.
    fn process_delayed_irq_events(&mut self) -> Result<()> {
        self.delayed_ioapic_irq_events
            .lock()
            .retain(|&event_index| {
                if let Some(evt) = &self.irq_events.lock()[event_index] {
                    if let Ok(mut ioapic) = self.ioapic.try_lock() {
                        ioapic.service_irq(evt.gsi as usize, true);
                        if evt.resample_event.is_none() {
                            ioapic.service_irq(evt.gsi as usize, false);
                        }

                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            });

        if self.delayed_ioapic_irq_events.lock().is_empty() {
            self.delayed_ioapic_irq_trigger.read()?;
        }

        Ok(())
    }

    fn irq_delayed_event_token(&self) -> Result<Option<Event>> {
        Ok(Some(self.delayed_ioapic_irq_trigger.try_clone()?))
    }

    fn check_capability(&self, c: IrqChipCap) -> bool {
        match c {
            IrqChipCap::TscDeadlineTimer => self
                .vm
                .get_hypervisor()
                .check_capability(HypervisorCap::TscDeadlineTimer),
            IrqChipCap::X2Apic => true,
        }
    }
}

impl IrqChipX86_64 for KvmSplitIrqChip {
    fn try_box_clone(&self) -> Result<Box<dyn IrqChipX86_64>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn as_irq_chip(&self) -> &dyn IrqChip {
        self
    }

    fn as_irq_chip_mut(&mut self) -> &mut dyn IrqChip {
        self
    }

    /// Get the current state of the PIC
    fn get_pic_state(&self, select: PicSelect) -> Result<PicState> {
        Ok(self.pic.lock().get_pic_state(select))
    }

    /// Set the current state of the PIC
    fn set_pic_state(&mut self, select: PicSelect, state: &PicState) -> Result<()> {
        self.pic.lock().set_pic_state(select, state);
        Ok(())
    }

    /// Get the current state of the IOAPIC
    fn get_ioapic_state(&self) -> Result<IoapicState> {
        Ok(self.ioapic.lock().get_ioapic_state())
    }

    /// Set the current state of the IOAPIC
    fn set_ioapic_state(&mut self, state: &IoapicState) -> Result<()> {
        self.ioapic.lock().set_ioapic_state(state);
        Ok(())
    }

    /// Get the current state of the specified VCPU's local APIC
    fn get_lapic_state(&self, vcpu_id: usize) -> Result<LapicState> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => Ok(LapicState::from(&vcpu.get_lapic()?)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Set the current state of the specified VCPU's local APIC
    fn set_lapic_state(&mut self, vcpu_id: usize, state: &LapicState) -> Result<()> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => vcpu.set_lapic(&kvm_lapic_state::from(state)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Retrieves the state of the PIT. Gets the pit state via the KVM API.
    fn get_pit(&self) -> Result<PitState> {
        Ok(self.pit.lock().get_pit_state())
    }

    /// Sets the state of the PIT. Sets the pit state via the KVM API.
    fn set_pit(&mut self, state: &PitState) -> Result<()> {
        self.pit.lock().set_pit_state(state);
        Ok(())
    }

    /// Returns true if the PIT uses port 0x61 for the PC speaker, false if 0x61 is unused.
    /// devices::Pit uses 0x61.
    fn pit_uses_speaker_port(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use base::EventReadResult;
    use hypervisor::{
        kvm::Kvm, IoapicRedirectionTableEntry, PitRWMode, ProtectionType, TriggerMode, Vm, VmX86_64,
    };
    use resources::{MemRegion, SystemAllocator, SystemAllocatorConfig};
    use vm_memory::GuestMemory;

    use crate::irqchip::tests::*;
    use crate::IrqChip;

    /// Helper function for setting up a KvmKernelIrqChip
    fn get_kernel_chip() -> KvmKernelIrqChip {
        let kvm = Kvm::new().expect("failed to instantiate Kvm");
        let mem = GuestMemory::new(&[]).unwrap();
        let vm =
            KvmVm::new(&kvm, mem, ProtectionType::Unprotected).expect("failed tso instantiate vm");

        let mut chip = KvmKernelIrqChip::new(vm.try_clone().expect("failed to clone vm"), 1)
            .expect("failed to instantiate KvmKernelIrqChip");

        let vcpu = vm.create_vcpu(0).expect("failed to instantiate vcpu");
        chip.add_vcpu(0, vcpu.as_vcpu())
            .expect("failed to add vcpu");

        chip
    }

    /// Helper function for setting up a KvmSplitIrqChip
    fn get_split_chip() -> KvmSplitIrqChip {
        let kvm = Kvm::new().expect("failed to instantiate Kvm");
        let mem = GuestMemory::new(&[]).unwrap();
        let vm =
            KvmVm::new(&kvm, mem, ProtectionType::Unprotected).expect("failed tso instantiate vm");

        let (_, device_tube) = Tube::pair().expect("failed to create irq tube");

        let mut chip = KvmSplitIrqChip::new(
            vm.try_clone().expect("failed to clone vm"),
            1,
            device_tube,
            None,
        )
        .expect("failed to instantiate KvmKernelIrqChip");

        let vcpu = vm.create_vcpu(0).expect("failed to instantiate vcpu");
        chip.add_vcpu(0, vcpu.as_vcpu())
            .expect("failed to add vcpu");
        chip
    }

    #[test]
    fn kernel_irqchip_get_pic() {
        test_get_pic(get_kernel_chip());
    }

    #[test]
    fn kernel_irqchip_set_pic() {
        test_set_pic(get_kernel_chip());
    }

    #[test]
    fn kernel_irqchip_get_ioapic() {
        test_get_ioapic(get_kernel_chip());
    }

    #[test]
    fn kernel_irqchip_set_ioapic() {
        test_set_ioapic(get_kernel_chip());
    }

    #[test]
    fn kernel_irqchip_get_pit() {
        test_get_pit(get_kernel_chip());
    }

    #[test]
    fn kernel_irqchip_set_pit() {
        test_set_pit(get_kernel_chip());
    }

    #[test]
    fn kernel_irqchip_get_lapic() {
        test_get_lapic(get_kernel_chip())
    }

    #[test]
    fn kernel_irqchip_set_lapic() {
        test_set_lapic(get_kernel_chip())
    }

    #[test]
    fn kernel_irqchip_route_irq() {
        test_route_irq(get_kernel_chip());
    }

    #[test]
    fn split_irqchip_get_pic() {
        test_get_pic(get_split_chip());
    }

    #[test]
    fn split_irqchip_set_pic() {
        test_set_pic(get_split_chip());
    }

    #[test]
    fn split_irqchip_get_ioapic() {
        test_get_ioapic(get_split_chip());
    }

    #[test]
    fn split_irqchip_set_ioapic() {
        test_set_ioapic(get_split_chip());
    }

    #[test]
    fn split_irqchip_get_pit() {
        test_get_pit(get_split_chip());
    }

    #[test]
    fn split_irqchip_set_pit() {
        test_set_pit(get_split_chip());
    }

    #[test]
    fn split_irqchip_route_irq() {
        test_route_irq(get_split_chip());
    }

    #[test]
    fn split_irqchip_routes_conflict() {
        let mut chip = get_split_chip();
        chip.route_irq(IrqRoute {
            gsi: 5,
            source: IrqSource::Msi {
                address: 4276092928,
                data: 0,
            },
        })
        .expect("failed to set msi rout");
        // this second route should replace the first
        chip.route_irq(IrqRoute {
            gsi: 5,
            source: IrqSource::Msi {
                address: 4276092928,
                data: 32801,
            },
        })
        .expect("failed to set msi rout");
    }

    #[test]
    fn irq_event_tokens() {
        let mut chip = get_split_chip();
        let tokens = chip
            .irq_event_tokens()
            .expect("could not get irq_event_tokens");

        // there should be one token on a fresh split irqchip, for the pit
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].1, 0);

        // register another irq event
        let evt = IrqEdgeEvent::new().expect("failed to create event");
        chip.register_edge_irq_event(6, &evt)
            .expect("failed to register irq event");

        let tokens = chip
            .irq_event_tokens()
            .expect("could not get irq_event_tokens");

        // now there should be two tokens
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].1, 0);
        assert_eq!(tokens[1].1, 6);
        assert_eq!(tokens[1].2, *evt.get_trigger());
    }

    #[test]
    fn finalize_devices() {
        let mut chip = get_split_chip();

        let mmio_bus = Bus::new();
        let io_bus = Bus::new();
        let mut resources = SystemAllocator::new(
            SystemAllocatorConfig {
                io: Some(MemRegion {
                    base: 0xc000,
                    size: 0x4000,
                }),
                low_mmio: MemRegion {
                    base: 0,
                    size: 2048,
                },
                high_mmio: MemRegion {
                    base: 0x1_0000_0000,
                    size: 0x2_0000_0000,
                },
                platform_mmio: None,
                first_irq: 5,
            },
            None,
            &[],
        )
        .expect("failed to create SystemAllocator");

        // Set up a level-triggered interrupt line 1
        let evt = IrqLevelEvent::new().expect("failed to create event");
        let evt_index = chip
            .register_level_irq_event(1, &evt)
            .expect("failed to register irq event")
            .expect("register_irq_event should not return None");

        // Once we finalize devices, the pic/pit/ioapic should be attached to io and mmio busses
        chip.finalize_devices(&mut resources, &io_bus, &mmio_bus)
            .expect("failed to finalize devices");

        // Should not be able to allocate an irq < 24 now
        assert!(resources.allocate_irq().expect("failed to allocate irq") >= 24);

        // set PIT counter 2 to "SquareWaveGen"(aka 3) mode and "Both" access mode
        io_bus.write(0x43, &[0b10110110]);

        let state = chip.get_pit().expect("failed to get pit state");
        assert_eq!(state.channels[2].mode, 3);
        assert_eq!(state.channels[2].rw_mode, PitRWMode::Both);

        // ICW1 0x11: Edge trigger, cascade mode, ICW4 needed.
        // ICW2 0x08: Interrupt vector base address 0x08.
        // ICW3 0xff: Value written does not matter.
        // ICW4 0x13: Special fully nested mode, auto EOI.
        io_bus.write(0x20, &[0x11]);
        io_bus.write(0x21, &[0x08]);
        io_bus.write(0x21, &[0xff]);
        io_bus.write(0x21, &[0x13]);

        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("failed to get pic state");

        // auto eoi and special fully nested mode should be turned on
        assert!(state.auto_eoi);
        assert!(state.special_fully_nested_mode);

        // Need to write to the irq event before servicing it
        evt.trigger().expect("failed to write to event");

        // if we assert irq line one, and then get the resulting interrupt, an auto-eoi should
        // occur and cause the resample_event to be written to
        chip.service_irq_event(evt_index)
            .expect("failed to service irq");

        assert!(chip.interrupt_requested(0));
        assert_eq!(
            chip.get_external_interrupt(0)
                .expect("failed to get external interrupt"),
            // Vector is 9 because the interrupt vector base address is 0x08 and this is irq
            // line 1 and 8+1 = 9
            0x9
        );

        // Clone resample event because read_timeout() needs a mutable reference.
        let resample_evt = evt.get_resample().try_clone().unwrap();
        assert_eq!(
            resample_evt
                .read_timeout(std::time::Duration::from_secs(1))
                .expect("failed to read_timeout"),
            EventReadResult::Count(1)
        );

        // setup a ioapic redirection table entry 14
        let mut entry = IoapicRedirectionTableEntry::default();
        entry.set_vector(44);

        let irq_14_offset = 0x10 + 14 * 2;
        mmio_bus.write(IOAPIC_BASE_ADDRESS, &[irq_14_offset]);
        mmio_bus.write(
            IOAPIC_BASE_ADDRESS + 0x10,
            &(entry.get(0, 32) as u32).to_ne_bytes(),
        );
        mmio_bus.write(IOAPIC_BASE_ADDRESS, &[irq_14_offset + 1]);
        mmio_bus.write(
            IOAPIC_BASE_ADDRESS + 0x10,
            &(entry.get(32, 32) as u32).to_ne_bytes(),
        );

        let state = chip.get_ioapic_state().expect("failed to get ioapic state");

        // redirection table entry 14 should have a vector of 44
        assert_eq!(state.redirect_table[14].get_vector(), 44);
    }

    #[test]
    fn get_external_interrupt() {
        let mut chip = get_split_chip();
        assert!(!chip.interrupt_requested(0));

        chip.service_irq(0, true).expect("failed to service irq");
        assert!(chip.interrupt_requested(0));

        // Should return Some interrupt
        assert_eq!(
            chip.get_external_interrupt(0)
                .expect("failed to get external interrupt"),
            0,
        );

        // interrupt is not requested twice
        assert!(!chip.interrupt_requested(0));
    }

    #[test]
    fn broadcast_eoi() {
        let mut chip = get_split_chip();

        let mmio_bus = Bus::new();
        let io_bus = Bus::new();
        let mut resources = SystemAllocator::new(
            SystemAllocatorConfig {
                io: Some(MemRegion {
                    base: 0xc000,
                    size: 0x4000,
                }),
                low_mmio: MemRegion {
                    base: 0,
                    size: 2048,
                },
                high_mmio: MemRegion {
                    base: 0x1_0000_0000,
                    size: 0x2_0000_0000,
                },
                platform_mmio: None,
                first_irq: 5,
            },
            None,
            &[],
        )
        .expect("failed to create SystemAllocator");

        // setup an event and a resample event for irq line 1
        let evt = Event::new().expect("failed to create event");
        let resample_evt = Event::new().expect("failed to create event");

        chip.register_irq_event(1, &evt, Some(&resample_evt))
            .expect("failed to register_irq_event");

        // Once we finalize devices, the pic/pit/ioapic should be attached to io and mmio busses
        chip.finalize_devices(&mut resources, &io_bus, &mmio_bus)
            .expect("failed to finalize devices");

        // setup a ioapic redirection table entry 1 with a vector of 123
        let mut entry = IoapicRedirectionTableEntry::default();
        entry.set_vector(123);
        entry.set_trigger_mode(TriggerMode::Level);

        let irq_write_offset = 0x10 + 1 * 2;
        mmio_bus.write(IOAPIC_BASE_ADDRESS, &[irq_write_offset]);
        mmio_bus.write(
            IOAPIC_BASE_ADDRESS + 0x10,
            &(entry.get(0, 32) as u32).to_ne_bytes(),
        );
        mmio_bus.write(IOAPIC_BASE_ADDRESS, &[irq_write_offset + 1]);
        mmio_bus.write(
            IOAPIC_BASE_ADDRESS + 0x10,
            &(entry.get(32, 32) as u32).to_ne_bytes(),
        );

        // Assert line 1
        chip.service_irq(1, true).expect("failed to service irq");

        // resample event should not be written to
        assert_eq!(
            resample_evt
                .read_timeout(std::time::Duration::from_millis(10))
                .expect("failed to read_timeout"),
            EventReadResult::Timeout
        );

        // irq line 1 should be asserted
        let state = chip.get_ioapic_state().expect("failed to get ioapic state");
        assert_eq!(state.current_interrupt_level_bitmap, 1 << 1);

        // Now broadcast an eoi for vector 123
        chip.broadcast_eoi(123).expect("failed to broadcast eoi");

        // irq line 1 should be deasserted
        let state = chip.get_ioapic_state().expect("failed to get ioapic state");
        assert_eq!(state.current_interrupt_level_bitmap, 0);

        // resample event should be written to by ioapic
        assert_eq!(
            resample_evt
                .read_timeout(std::time::Duration::from_millis(10))
                .expect("failed to read_timeout"),
            EventReadResult::Count(1)
        );
    }
}
