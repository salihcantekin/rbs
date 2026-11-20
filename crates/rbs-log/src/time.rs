//! UTC times for log lines and file names, without a time zone database.

use std::time::{SystemTime, UNIX_EPOCH};

/// `(year, month, day, hour, minute, second, millisecond)` of `t` in UTC. A time
/// before 1970 reads as 1970-01-01.
pub(crate) fn civil(t: SystemTime) -> (i64, i64, i64, u64, u64, u64, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_millis(),
    )
}

/// `2026-11-20T15:27:35.123Z`.
pub fn utc_millis(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s, ms) = civil(t);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// `20261120T152735Z`, for file names.
pub fn compact(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s, _) = civil(t);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn times_are_utc() {
        let t = UNIX_EPOCH + Duration::from_millis(1_795_188_455_123);
        assert_eq!(utc_millis(t), "2026-11-20T15:27:35.123Z");
        assert_eq!(compact(t), "20261120T152735Z");
        assert_eq!(utc_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(compact(leap), "20000229T000000Z");
    }
}
