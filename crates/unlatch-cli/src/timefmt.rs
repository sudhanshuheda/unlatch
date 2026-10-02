//! Minimal UTC time formatting (no timezone database needed).

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) for nanoseconds since the Unix epoch.
pub fn format_ns(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// RFC 3339 UTC timestamp for `SystemTime::now()`.
pub fn now_rfc3339() -> String {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    format_ns(ns).replacen(' ', "T", 1) + "Z"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants() {
        assert_eq!(format_ns(0), "1970-01-01 00:00:00");
        // `date -u -d @1790772896`
        assert_eq!(
            format_ns(1_790_772_896 * 1_000_000_000),
            "2026-09-30 12:54:56"
        );
        // Leap day.
        assert_eq!(
            format_ns(951_782_400 * 1_000_000_000),
            "2000-02-29 00:00:00"
        );
        // Before the epoch.
        assert_eq!(format_ns(-1_000_000_000), "1969-12-31 23:59:59");
    }

    #[test]
    fn rfc3339_shape() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
    }
}
