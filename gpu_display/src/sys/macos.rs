// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MacOS GPU display stubs.
//!
//! GPU display is not currently supported on macOS.

use base::AsRawDescriptor;
use base::RawDescriptor;

use crate::DisplayT;
use crate::EventDevice;
use crate::GpuDisplay;
use crate::GpuDisplayExt;
use crate::GpuDisplayResult;

pub(crate) trait MacDisplayT: DisplayT {}

/// MacOS-specific GPU display extensions (stub).
pub trait MacGpuDisplayExt {}

impl MacGpuDisplayExt for GpuDisplay {}

impl GpuDisplayExt for GpuDisplay {
    fn import_event_device(&mut self, _event_device: EventDevice) -> GpuDisplayResult<u32> {
        // GPU display not supported on macOS
        Err(crate::GpuDisplayError::Unsupported)
    }

    fn handle_event_device(&mut self, _event_device_id: u32) {
        // GPU display not supported on macOS
    }
}

impl AsRawDescriptor for GpuDisplay {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.wait_ctx.as_raw_descriptor()
    }
}
