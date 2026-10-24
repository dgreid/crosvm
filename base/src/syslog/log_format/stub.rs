// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Placeholder [`Formatter`](super::Formatter) for builds without the `log_format` feature.

use super::install_default;
use super::Error;

/// Empty formatter. `install` always selects the default UTC format.
pub(in crate::syslog) struct Formatter;

impl Formatter {
    /// Accepts only the default of no format string.
    pub(in crate::syslog) fn new(format: Option<&str>) -> Result<Self, Error> {
        match format {
            Some(_) => Err(Error::Unsupported),
            None => Ok(Self),
        }
    }

    /// Installs the default UTC format.
    pub(in crate::syslog) fn install(&self, builder: &mut env_logger::Builder) {
        install_default(builder);
    }
}
