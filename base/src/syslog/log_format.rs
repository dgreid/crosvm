// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cloud Hypervisor-compatible formatting for crosvm log records.
//!
//! Format strings are parsed once during logger initialization. The local timezone is captured at
//! the same point so jailed device processes do not need filesystem access when they emit a log
//! record.

use std::io;
use std::io::Write;
use std::str::FromStr;
use std::thread;
use std::time::Instant;

use jiff::tz::TimeZone;

use super::Error;

/// The timezone used to render a date or time field.
#[derive(Copy, Clone)]
enum Zone {
    /// Coordinated Universal Time.
    Utc,
    /// The local timezone captured when the formatter was created.
    Local,
}

/// A timestamp component selected by an individual format placeholder.
#[derive(Copy, Clone)]
enum TimeField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Micros,
    // Offset from UTC.
    Offset,
}

/// One piece of a parsed log format.
///
/// Literal text is copied unchanged; every other variant expands a `{name}` placeholder.
enum Token {
    /// Text copied directly into the output.
    Literal(String),
    /// Seconds elapsed since the formatter was created.
    BootTime,
    /// The current UTC timestamp with microsecond precision.
    WallClock,
    /// The current UTC time in glog's timestamp format.
    Glog,
    /// The current local time in glog's timestamp format.
    LocalGlog,
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
    /// One timestamp component rendered in the selected timezone.
    Time(TimeField, Zone),
}

impl FromStr for Token {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, zone) = match value.strip_prefix("local") {
            Some(name) => (name, Zone::Local),
            None => (value, Zone::Utc),
        };

        match name {
            "year" => return Ok(Self::Time(TimeField::Year, zone)),
            "month" => return Ok(Self::Time(TimeField::Month, zone)),
            "day" => return Ok(Self::Time(TimeField::Day, zone)),
            "hour" => return Ok(Self::Time(TimeField::Hour, zone)),
            "minute" => return Ok(Self::Time(TimeField::Minute, zone)),
            "second" => return Ok(Self::Time(TimeField::Second, zone)),
            "micros" => return Ok(Self::Time(TimeField::Micros, zone)),
            "offset" => return Ok(Self::Time(TimeField::Offset, zone)),
            _ => {}
        }

        match value {
            "boottime" => Ok(Self::BootTime),
            "wallclock" => Ok(Self::WallClock),
            "glog" => Ok(Self::Glog),
            "localglog" => Ok(Self::LocalGlog),
            "pid" => Ok(Self::Pid),
            "tid" => Ok(Self::Tid),
            "thread" => Ok(Self::Thread),
            "level" => Ok(Self::Level),
            "levelchar" => Ok(Self::LevelChar),
            "location" => Ok(Self::Location),
            "msg" => Ok(Self::Message),
            _ => Err(Error::FormatUnknownToken(value.to_string())),
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
                        None => return Err(Error::FormatUnterminatedBrace),
                    }
                }
                tokens.push(name.parse()?);
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => return Err(Error::FormatUnmatchedBrace),
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

fn thread_id() -> u64 {
    crate::gettid() as u64
}

fn write_time_field<W: Write>(
    output: &mut W,
    field: TimeField,
    time: &jiff::Zoned,
) -> io::Result<()> {
    match field {
        TimeField::Year => write!(output, "{:04}", time.year()),
        TimeField::Month => write!(output, "{:02}", time.month()),
        TimeField::Day => write!(output, "{:02}", time.day()),
        TimeField::Hour => write!(output, "{:02}", time.hour()),
        TimeField::Minute => write!(output, "{:02}", time.minute()),
        TimeField::Second => write!(output, "{:02}", time.second()),
        TimeField::Micros => write!(output, "{:06}", time.subsec_nanosecond() / 1000),
        TimeField::Offset => write!(output, "{}", time.strftime("%:z")),
    }
}

/// A parsed format and the time.
pub(super) struct LogFormatter {
    tokens: Vec<Token>,
    start: Instant,
    // Caching the timezoe avoids checking it from a jailed thread on each log.
    local_tz: TimeZone,
}

impl LogFormatter {
    /// Parses `format` and captures the local timezone before sandboxing.
    pub(super) fn new(format: &str) -> Result<Self, Error> {
        Ok(Self {
            tokens: parse_format(format)?,
            start: Instant::now(),
            local_tz: TimeZone::try_system().unwrap_or(TimeZone::UTC),
        })
    }

    /// Writes one formatted log record to `output`.
    pub(super) fn write<W: Write>(
        &self,
        output: &mut W,
        record: &log::Record<'_>,
    ) -> io::Result<()> {
        let boot_time = Instant::now().duration_since(self.start).as_secs_f32();
        let mut utc = None;
        let mut local = None;

        for token in &self.tokens {
            match token {
                Token::Literal(value) => output.write_all(value.as_bytes())?,
                Token::BootTime => write!(output, "{boot_time:>10.6?}")?,
                Token::WallClock => {
                    let time =
                        utc.get_or_insert_with(|| jiff::Timestamp::now().to_zoned(TimeZone::UTC));
                    write!(output, "{:.6}", time.timestamp())?;
                }
                Token::Glog => {
                    let time =
                        utc.get_or_insert_with(|| jiff::Timestamp::now().to_zoned(TimeZone::UTC));
                    write!(output, "{}", time.strftime("%m%d %H:%M:%S%.6f"))?;
                }
                Token::LocalGlog => {
                    let time = local.get_or_insert_with(|| {
                        jiff::Timestamp::now().to_zoned(self.local_tz.clone())
                    });
                    write!(output, "{}", time.strftime("%m%d %H:%M:%S%.6f"))?;
                }
                Token::Pid => write!(output, "{}", std::process::id())?,
                Token::Tid => write!(output, "{}", thread_id())?,
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
                Token::Time(field, zone) => {
                    let time = match zone {
                        Zone::Utc => utc
                            .get_or_insert_with(|| jiff::Timestamp::now().to_zoned(TimeZone::UTC)),
                        Zone::Local => local.get_or_insert_with(|| {
                            jiff::Timestamp::now().to_zoned(self.local_tz.clone())
                        }),
                    };
                    write_time_field(output, *field, time)?;
                }
            }
        }
        output.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
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
            Err(Error::FormatUnknownToken(token)) if token == "unknown"
        ));
        assert!(matches!(
            LogFormatter::new("{level"),
            Err(Error::FormatUnterminatedBrace)
        ));
        assert!(matches!(
            LogFormatter::new("level}"),
            Err(Error::FormatUnmatchedBrace)
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
        assert!(output.ends_with(&format!(" {} test.rs:42] hello\n", thread_id())));
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
