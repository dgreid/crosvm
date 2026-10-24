// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Flexible glog compatible formatting for crosvm log records.

use std::io;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use chrono::Utc;

use super::install_default;
use super::Error;
use crate::platform::gettid;
use crate::platform::log_format::to_local_time;

/// chrono format for the `{wallclock}` token.
const WALL_CLOCK_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";
/// chrono format for the `{glog}` token.
const GLOG_FORMAT: &str = "%m%d %H:%M:%S%.6f";

/// The timezone used to render a date or time field.
#[derive(Copy, Clone)]
enum Zone {
    /// Universal time.
    Utc,
    /// Local timezone initialized when the formatter is created.
    Local,
}

/// One piece of a parsed log format.
///
/// Either literal text to write or an expandable {name} component.
enum Token {
    /// Text copied directly into the output.
    Literal(String),
    /// Seconds elapsed since the formatter was created.
    BootTime,
    /// The process ID.
    Pid,
    /// The Linux thread ID.
    Tid,
    /// The thread name, or `anonymous` if it has no name.
    Thread,
    /// The full log level name.
    Level,
    /// The single-character glog severity.
    LevelChar,
    /// The source file and line, or the log target when either is unavailable.
    Location,
    /// The formatted log message.
    Message,
    /// The current time rendered with a chrono format string in the selected timezone.
    Time(&'static str, Zone),
}

impl Token {
    fn uses_local_time(&self) -> bool {
        matches!(self, Self::Time(_, Zone::Local))
    }
}

impl FromStr for Token {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, zone) = match value.strip_prefix("local") {
            Some(name) => (name, Zone::Local),
            None => (value, Zone::Utc),
        };

        let time_format = match name {
            "year" => Some("%Y"),
            "month" => Some("%m"),
            "day" => Some("%d"),
            "hour" => Some("%H"),
            "minute" => Some("%M"),
            "second" => Some("%S"),
            "micros" => Some("%6f"),
            "offset" => Some("%:z"),
            "glog" => Some(GLOG_FORMAT),
            _ => None,
        };
        if let Some(format) = time_format {
            return Ok(Self::Time(format, zone));
        }

        match value {
            "wallclock" => Ok(Self::Time(WALL_CLOCK_FORMAT, Zone::Utc)),
            "boottime" => Ok(Self::BootTime),
            "pid" => Ok(Self::Pid),
            "tid" => Ok(Self::Tid),
            "thread" => Ok(Self::Thread),
            "level" => Ok(Self::Level),
            "levelchar" => Ok(Self::LevelChar),
            "location" => Ok(Self::Location),
            "msg" => Ok(Self::Message),
            _ => Err(Error::UnknownToken(value.to_string())),
        }
    }
}

fn parse_format(format: &str) -> Result<Vec<Token>, Error> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = format.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '{' => {
                if !literal.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut literal)));
                }

                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(character) => name.push(character),
                        None => return Err(Error::UnterminatedBrace),
                    }
                }
                tokens.push(name.parse()?);
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => return Err(Error::UnmatchedBrace),
            _ => literal.push(character),
        }
    }

    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    Ok(tokens)
}

fn level_char(level: log::Level) -> char {
    match level {
        log::Level::Error => 'E',
        log::Level::Warn => 'W',
        log::Level::Info => 'I',
        log::Level::Debug => 'D',
        log::Level::Trace => 'T',
    }
}

/// A parsed format and the time.
pub(super) struct LogFormatter {
    tokens: Vec<Token>,
    start: Instant,
}

impl LogFormatter {
    /// Parses `format` and initializes the local timezone.
    pub(super) fn new(format: &str) -> Result<Self, Error> {
        let tokens = parse_format(format)?;

        if tokens.iter().any(Token::uses_local_time) {
            // Initialize libc's timezone cache.
            let _ = to_local_time(Utc::now());
        }

        Ok(Self {
            tokens,
            start: Instant::now(),
        })
    }

    /// Writes one formatted log record to `output`.
    pub(super) fn write<W: Write>(
        &self,
        output: &mut W,
        record: &log::Record<'_>,
    ) -> io::Result<()> {
        let boot_time = Instant::now().duration_since(self.start).as_secs_f64();
        let now = Utc::now();
        let mut local = None;

        for token in &self.tokens {
            match token {
                Token::Literal(value) => output.write_all(value.as_bytes())?,
                Token::BootTime => write!(output, "{boot_time:>10.6?}")?,
                Token::Pid => write!(output, "{}", std::process::id())?,
                Token::Tid => write!(output, "{}", gettid())?,
                Token::Thread => write!(
                    output,
                    "{}",
                    thread::current().name().unwrap_or("anonymous")
                )?,
                Token::Level => write!(output, "{}", record.level())?,
                Token::LevelChar => write!(output, "{}", level_char(record.level()))?,
                Token::Location => match (record.file(), record.line()) {
                    (Some(file), Some(line)) => write!(output, "{file}:{line}")?,
                    _ => write!(output, "{}", record.target())?,
                },
                Token::Message => write!(output, "{}", record.args())?,
                Token::Time(format, Zone::Utc) => write!(output, "{}", now.format(format))?,
                Token::Time(format, Zone::Local) => {
                    // Fall back to UTC if unknown.
                    let time = local.get_or_insert_with(|| {
                        to_local_time(now).unwrap_or_else(|_| now.fixed_offset())
                    });
                    write!(output, "{}", time.format(format))?;
                }
            }
        }
        output.write_all(b"\n")
    }
}

/// A parsed formatter, or the default UTC format when no format string was configured.
///
/// # Examples
///
/// Install a glog-style formatter:
///
/// ```ignore
/// fn install_glog(builder: &mut env_logger::Builder) -> Result<(), Error> {
///     let formatter =
///         Formatter::new(Some("{levelchar}{localglog} {tid} {location}] {msg}"))?;
///     formatter.install(builder);
///     Ok(())
/// }
/// ```
pub(in crate::syslog) struct Formatter {
    inner: Option<Arc<LogFormatter>>,
}

impl Formatter {
    /// Parses `format` when it is set.
    pub(in crate::syslog) fn new(format: Option<&str>) -> Result<Self, Error> {
        Ok(Self {
            inner: format.map(LogFormatter::new).transpose()?.map(Arc::new),
        })
    }

    /// Installs the custom format closure, or the default UTC format when none was configured.
    pub(in crate::syslog) fn install(&self, builder: &mut env_logger::Builder) {
        match self.inner.clone() {
            Some(formatter) => {
                builder.format(move |buf, record| formatter.write(buf, record));
            }
            None => install_default(builder),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use chrono::FixedOffset;

    use super::*;

    fn format_record(format: &str, level: log::Level) -> String {
        let formatter = LogFormatter::new(format).unwrap();
        let record = log::Record::builder()
            .args(format_args!("hello"))
            .level(level)
            .target("test_target")
            .file(Some("test.rs"))
            .line(Some(42))
            .build();
        let mut output = Vec::new();
        formatter.write(&mut output, &record).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn escaped_braces_are_literals() {
        assert_eq!(format_record("{{{level}}}", log::Level::Info), "{INFO}\n");
    }

    #[test]
    fn invalid_formats_are_rejected() {
        assert!(matches!(
            LogFormatter::new("{unknown}"),
            Err(Error::UnknownToken(token)) if token == "unknown"
        ));
        assert!(matches!(
            LogFormatter::new("{level"),
            Err(Error::UnterminatedBrace)
        ));
        assert!(matches!(
            LogFormatter::new("level}"),
            Err(Error::UnmatchedBrace)
        ));
    }

    #[test]
    fn level_char_uses_glog_letters() {
        for (level, expected) in [
            (log::Level::Error, "E\n"),
            (log::Level::Warn, "W\n"),
            (log::Level::Info, "I\n"),
            (log::Level::Debug, "D\n"),
            (log::Level::Trace, "T\n"),
        ] {
            assert_eq!(format_record("{levelchar}", level), expected);
        }
    }

    #[test]
    fn formats_known_utc_time() {
        let time = DateTime::<Utc>::from_timestamp(0, 123_456_000).unwrap();

        assert_eq!(
            time.format(WALL_CLOCK_FORMAT).to_string(),
            "1970-01-01T00:00:00.123456Z"
        );
        assert_eq!(time.format(GLOG_FORMAT).to_string(), "0101 00:00:00.123456");
    }

    #[test]
    fn formats_timezone_offsets() {
        for (offset, expected) in [
            (0, "+00:00"),
            (5 * 3600 + 30 * 60, "+05:30"),
            (-8 * 3600, "-08:00"),
        ] {
            let time = DateTime::<Utc>::from_timestamp(0, 0)
                .unwrap()
                .with_timezone(&FixedOffset::east_opt(offset).unwrap());
            assert_eq!(time.format("%:z").to_string(), expected);
        }
    }

    #[test]
    fn glog_format_has_expected_shape() {
        let output = format_record(
            "{levelchar}{localglog} {tid} {location}] {msg}",
            log::Level::Info,
        );
        let bytes = output.as_bytes();

        assert_eq!(bytes[0], b'I');
        for (index, byte) in bytes[..21].iter().enumerate() {
            if [0, 5, 8, 11, 14].contains(&index) {
                continue;
            }
            assert!(byte.is_ascii_digit(), "unexpected timestamp: {output}");
        }
        assert_eq!(bytes[5], b' ');
        assert_eq!(bytes[8], b':');
        assert_eq!(bytes[11], b':');
        assert_eq!(bytes[14], b'.');
        assert!(output.ends_with(&format!(" {} test.rs:42] hello\n", gettid())));
    }

    #[test]
    fn supports_individual_time_fields() {
        let output = format_record(
            "{year}-{month}-{day}T{hour}:{minute}:{second}.{micros}{offset} \
             {localyear}-{localmonth}-{localday}T{localhour}:{localminute}:\
             {localsecond}.{localmicros}{localoffset}",
            log::Level::Info,
        );

        assert!(!output.contains('{'));
        assert!(!output.contains('}'));
        assert!(output.ends_with('\n'));
    }
}
