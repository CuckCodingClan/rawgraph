//! Wall-clock helpers: milliseconds since the Unix epoch and a local-time
//! label (HH:MM:SS) without a date/time dependency.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| i64::try_from(d.as_millis()).ok()).unwrap_or(0)
}

/// Offset of the local time zone from UTC in seconds (0 when it cannot be
/// determined). Measured as local clock minus UTC clock at this moment, so a
/// daylight-saving change between a stored time and now is not accounted for.
pub fn local_offset_secs() -> i64 {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTimeRaw {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            millis: u16,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLocalTime(out: *mut SystemTimeRaw);
        }
        let mut t = SystemTimeRaw::default();
        // SAFETY: `GetLocalTime` only writes a SYSTEMTIME into the struct we pass.
        unsafe { GetLocalTime(&mut t) };
        let local_secs_of_day = i64::from(t.hour) * 3600 + i64::from(t.minute) * 60 + i64::from(t.second);
        let utc_secs_of_day = (now_unix_ms() / 1000).rem_euclid(86_400);
        let mut diff = local_secs_of_day - utc_secs_of_day;
        // The day boundary can fall between the two readings.
        if diff > 12 * 3600 {
            diff -= 86_400;
        } else if diff < -12 * 3600 {
            diff += 86_400;
        }
        // Zones are whole multiples of 15 minutes; this drops the tiny delay between the readings.
        ((diff + 450).div_euclid(900)) * 900
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// `HH:MM:SS` of a Unix time in milliseconds, shifted by `offset_secs`.
pub fn clock_label(unix_ms: i64, offset_secs: i64) -> String {
    let secs = (unix_ms.div_euclid(1000) + offset_secs).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Civil date `YYYY-MM-DD` of a Unix time in milliseconds, shifted by `offset_secs`.
pub fn date_label(unix_ms: i64, offset_secs: i64) -> String {
    let days = (unix_ms.div_euclid(1000) + offset_secs).div_euclid(86_400);
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// "2026-11-15 14:05:12" in local time.
pub fn local_stamp(unix_ms: i64) -> String {
    let off = local_offset_secs();
    format!("{} {}", date_label(unix_ms, off), clock_label(unix_ms, off))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_offset() {
        // 2026-11-15 12:30:45 UTC
        let ms = 1_794_745_845_000;
        assert_eq!(date_label(ms, 0), "2026-11-15");
        assert_eq!(clock_label(ms, 0), "12:30:45");
        assert_eq!(clock_label(ms, 3600), "13:30:45");
        assert_eq!(date_label(ms, 12 * 3600), "2026-11-16");
        assert_eq!(date_label(0, 0), "1970-01-01");
        assert_eq!(clock_label(-1000, 0), "23:59:59");
    }

    #[test]
    fn the_local_offset_is_a_plausible_zone() {
        let off = local_offset_secs();
        assert!((-12 * 3600..=14 * 3600).contains(&off) && off % 900 == 0, "{off}");
    }
}
