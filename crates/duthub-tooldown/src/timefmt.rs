//! Minimal RFC3339 UTC timestamp formatting for the install manifest (no chrono dependency).

use std::time::{SystemTime, UNIX_EPOCH};

/// Format seconds since the unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn unix_secs_to_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Current UTC time as an RFC3339 string. Falls back to the epoch only if the
/// system clock is set before 1970.
pub(crate) fn now_rfc3339_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());
    unix_secs_to_rfc3339(secs)
}

/// Days since 1970-01-01 -> proleptic Gregorian date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(31); // [1, 31]
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(12); // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::unix_secs_to_rfc3339;

    #[test]
    fn formats_known_epochs() {
        assert_eq!(unix_secs_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_secs_to_rfc3339(86_400), "1970-01-02T00:00:00Z");
        // 2000-01-01T00:00:00Z
        assert_eq!(unix_secs_to_rfc3339(946_684_800), "2000-01-01T00:00:00Z");
        // Leap day: 2024-02-29T00:00:00Z
        assert_eq!(unix_secs_to_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn handles_negative_epoch_times() {
        // One second before the epoch.
        assert_eq!(unix_secs_to_rfc3339(-1), "1969-12-31T23:59:59Z");
    }
}
