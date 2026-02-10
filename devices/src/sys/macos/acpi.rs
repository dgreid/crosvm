// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use base::Descriptor;
use sync::Mutex;

use crate::ac_adapter::AcAdapter;
use crate::acpi::ACPIPMError;
use crate::acpi::GpeResource;
use crate::IrqLevelEvent;

/// On macOS, ACPI events are not supported.
/// Returns None to indicate no ACPI event socket is available.
pub(crate) fn get_acpi_event_sock() -> Result<Option<Descriptor>, ACPIPMError> {
    Ok(None)
}

/// No-op on macOS since ACPI events are not supported.
pub(crate) fn acpi_event_run(
    _sci_evt: &IrqLevelEvent,
    _acpi_event_sock: &Option<Descriptor>,
    _gpe0: &Arc<Mutex<GpeResource>>,
    _ignored_gpe: &[u32],
    _ac_adapter: &Option<Arc<Mutex<AcAdapter>>>,
) {
}
