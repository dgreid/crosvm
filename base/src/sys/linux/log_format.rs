// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Platform support for log formatting.

use std::io;
use std::mem::MaybeUninit;

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::Utc;

/// Calls `localtime_r` for `seconds` since the epoch.
fn localtime(seconds: libc::time_t) -> io::Result<libc::tm> {
    let mut time = MaybeUninit::<libc::tm>::uninit();

    // SAFETY: `seconds` and `time` are valid for the duration of the call. `localtime_r`
    // initializes `time` when it returns a non-null pointer, which is checked before it's accessed.
    unsafe {
        if libc::localtime_r(&seconds, time.as_mut_ptr()).is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(time.assume_init())
    }
}

/// Converts `timestamp` to the local timezone using libc's cached timezone.
pub fn to_local_time(timestamp: DateTime<Utc>) -> io::Result<DateTime<FixedOffset>> {
    let offset = localtime(timestamp.timestamp())?
        .tm_gmtoff
        .try_into()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "timezone offset does not fit in i32",
            )
        })?;
    let offset = FixedOffset::east_opt(offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid timezone offset"))?;
    Ok(timestamp.with_timezone(&offset))
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use chrono::Timelike;
    use chrono::Utc;

    use super::to_local_time;

    #[test]
    fn local_time_observes_dst() {
        const CHILD: &str = "CROSVM_LOG_FORMAT_DST_TEST_CHILD";

        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("local_time_observes_dst")
                .env(CHILD, "1")
                .env("TZ", "PST8PDT,M3.2.0,M11.1.0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        for (seconds, hour, offset) in
            [(1_705_320_000, 4, -8 * 3600), (1_721_044_800, 5, -7 * 3600)]
        {
            let timestamp = DateTime::<Utc>::from_timestamp(seconds, 0).unwrap();
            let time = to_local_time(timestamp).unwrap();
            assert_eq!(time.hour(), hour);
            assert_eq!(time.offset().local_minus_utc(), offset);
        }
    }
}
