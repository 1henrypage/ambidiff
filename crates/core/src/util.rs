//! Small dependency-free helpers shared across the core.

/// FNV-1a 32-bit hash, used for stable content-addressed row keys.
pub fn fnv1a32(data: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for b in data.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// FNV-1a 64-bit hash, used for change signatures in the watch controller.
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Format a Unix timestamp (seconds) as an RFC 3339 UTC string, e.g.
/// "2026-09-06T12:34:56Z". Hand-rolled to stay dependency-free and
/// wasm-clean; uses the civil-from-days algorithm.
pub fn format_rfc3339_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days since the Unix epoch to a (year, month, day) civil date.
/// Algorithm from Howard Hinnant's date algorithms (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a32_matches_reference_vectors() {
        assert_eq!(fnv1a32(""), 0x811c9dc5);
        assert_eq!(fnv1a32("a"), 0xe40c292c);
        assert_eq!(fnv1a32("foobar"), 0xbf9cf968);
    }

    #[test]
    fn rfc3339_epoch_and_known_dates() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_757_155_200), "2025-09-06T10:40:00Z");
        // Negative timestamps (pre-epoch) still format sanely.
        assert_eq!(format_rfc3339_utc(-86_400), "1969-12-31T00:00:00Z");
    }
}
