// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS-specific platform functions for the arch crate.
//!
//! macOS does not support VFIO platform devices or the Goldfish battery device,
//! so these functions provide stubs that return appropriate errors or empty results.

use std::collections::BTreeMap;
use std::sync::Arc;

use devices::Bus;
use devices::BusDevice;
use devices::DevicePowerManager;
use devices::IommuDevType;
use devices::IrqChip;
use hypervisor::ProtectionType;
use hypervisor::Vm;
use jail::FakeMinijailStub as Minijail;
use resources::SystemAllocator;
use sync::Mutex;

use crate::DeviceRegistrationError;

/// Placeholder type for VfioPlatformDevice on macOS.
///
/// VFIO is a Linux-only subsystem, so this type is never instantiated on macOS.
/// It exists only to provide API compatibility with platform-agnostic code.
pub struct VfioPlatformDevice {
    _private: (),
}

/// Platform bus resources for device tree generation.
///
/// On macOS, platform buses are not supported since VFIO is Linux-only.
/// This struct is provided for API compatibility with code that handles
/// platform device resources.
pub struct PlatformBusResources {
    pub dt_symbol: String,
    pub regions: Vec<(u64, u64)>,
    pub irqs: Vec<(u32, u32)>,
    pub iommus: Vec<(IommuDevType, Option<u32>, Vec<u32>)>,
    pub requires_power_domain: bool,
}

impl PlatformBusResources {
    #[allow(dead_code)]
    const IRQ_TRIGGER_EDGE: u32 = 1;
    #[allow(dead_code)]
    const IRQ_TRIGGER_LEVEL: u32 = 4;

    #[allow(dead_code)]
    fn new(symbol: String) -> Self {
        Self {
            dt_symbol: symbol,
            regions: vec![],
            irqs: vec![],
            iommus: vec![],
            requires_power_domain: false,
        }
    }
}

/// Adds goldfish battery device.
///
/// On macOS, the Goldfish battery device is not supported because it requires
/// Linux-specific power management infrastructure (powerd D-Bus interface) and
/// minijail sandboxing. This function returns an error indicating the feature
/// is not available on this platform.
///
/// For basic VM boot, battery emulation is not required - guests will simply
/// see no battery device present.
#[allow(clippy::too_many_arguments)]
pub fn add_goldfish_battery(
    _amls: &mut Vec<u8>,
    _battery_jail: Option<Minijail>,
    _mmio_bus: &Bus,
    _irq_chip: &mut dyn IrqChip,
    _irq_num: u32,
    _resources: &mut SystemAllocator,
    #[cfg(feature = "swap")] _swap_controller: &mut Option<swap::SwapController>,
) -> Result<(base::Tube, u64), DeviceRegistrationError> {
    // Goldfish battery requires Linux-specific infrastructure:
    // - powerd D-Bus interface for power monitoring
    // - minijail for sandboxing the device process
    // Neither is available on macOS.
    Err(DeviceRegistrationError::PlatformNotSupported(
        "Goldfish battery is not supported on macOS",
    ))
}

/// Creates platform devices for VFIO passthrough.
///
/// On macOS, VFIO platform devices are not supported because VFIO is a
/// Linux-specific subsystem for device passthrough. This function returns
/// empty results, which is safe because no VFIO devices will be configured
/// on macOS.
///
/// The empty return value indicates no platform devices were created,
/// which allows the boot process to continue without platform device support.
#[allow(clippy::too_many_arguments)]
pub fn generate_platform_bus<V: Vm>(
    _devices: Vec<(VfioPlatformDevice, Option<Minijail>)>,
    _irq_chip: &mut dyn IrqChip,
    _mmio_bus: &Bus,
    _resources: &mut SystemAllocator,
    _vm: &mut V,
    #[cfg(feature = "swap")] _swap_controller: &mut Option<swap::SwapController>,
    _dev_pm: &mut Option<DevicePowerManager>,
    _protection_type: ProtectionType,
) -> Result<
    (
        Vec<Arc<Mutex<dyn BusDevice>>>,
        BTreeMap<u32, String>,
        Vec<PlatformBusResources>,
    ),
    DeviceRegistrationError,
> {
    // VFIO is not available on macOS. Return empty results to indicate
    // no platform devices were created. This is expected behavior since
    // macOS device passthrough uses different mechanisms (if available).
    Ok((Vec::new(), BTreeMap::new(), Vec::new()))
}
