// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Log format parsing, behind the `log_format` feature.
//!
//! `Formatter::new(None)` selects the default UTC format, and with the feature disabled any format
//! string is rejected.

use std::io::Write;

use chrono::Utc;
use remain::sorted;
use thiserror::Error as ThisError;

cfg_if::cfg_if! {
    if #[cfg(all(feature = "log_format", target_os = "linux"))] {
        mod formatter;
        pub(in crate::syslog) use formatter::Formatter;
    } else if #[cfg(not(feature = "log_format"))] {
        mod stub;
        pub(in crate::syslog) use stub::Formatter;
    } else {
        compile_error!("The \"log_format\" feature is only supported on Linux.");
    }
}

/// Failure to turn a format string into a logger.
#[sorted]
#[derive(ThisError, Debug)]
pub enum Error {
    /// The format string names a token this parser does not recognize.
    #[error("unknown log format token '{{{0}}}'")]
    UnknownToken(String),
    /// The format string has a `}` that does not close a token.
    #[error("unmatched '}}' in log format")]
    UnmatchedBrace,
    /// A format string was set, but this build does not include the parser.
    #[error("the log_format feature is disabled")]
    Unsupported,
    /// The format string has a `{` that is not closed.
    #[error("unterminated '{{' in log format")]
    UnterminatedBrace,
}

/// Installs the default format: UTC ISO 8601 timestamp, level, and module path.
fn install_default(builder: &mut env_logger::Builder) {
    builder.format(|buf, record| {
        writeln!(
            buf,
            "[{} {:5} {}] {}",
            Utc::now().format("%Y-%m-%dT%H:%M:%S%.9f%:z"),
            record.level(),
            record.module_path().unwrap_or("<missing module path>"),
            record.args()
        )
    });
}
