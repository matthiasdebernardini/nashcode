//! RFC 3339 in, "3 days ago" out.
//!
//! Small and self-contained: the CLI needs exactly one date operation, and a
//! date crate is a large dependency to carry for it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days from 1970-01-01 to a proleptic-Gregorian date. Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse an RFC 3339 timestamp to Unix seconds. Returns `None` on anything
/// that is not one, which is what the viewer's rows are supposed to carry.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec;

    // Offset: trailing Z, or +HH:MM / -HH:MM after any fractional seconds.
    let tail = &s[19..];
    let tail = tail.strip_prefix('.').map_or(tail, |frac| {
        let end = frac.find(|c: char| !c.is_ascii_digit()).unwrap_or(frac.len());
        &frac[end..]
    });
    if let Some(rest) = tail.strip_prefix(['+', '-']) {
        let sign = if tail.starts_with('-') { -1 } else { 1 };
        let (hh, mm) = match rest.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
            None if rest.len() >= 4 => (rest[..2].parse().ok()?, rest[2..4].parse().ok()?),
            _ => return None,
        };
        secs -= sign * (hh * 3600 + mm * 60);
    }
    Some(secs)
}

/// Civil date from days since 1970-01-01. The inverse of `days_from_civil`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Now, as an RFC 3339 timestamp in UTC.
///
/// This is what `--since` wants: an agent that has just handed a plan to a human
/// polls for comments left from this moment on, so it needs a timestamp the
/// viewer will accept, in the spelling the viewer's own rows use.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Seconds since a timestamp, or `None` when it is unparseable.
pub fn seconds_since(rfc3339: &str) -> Option<i64> {
    let then = parse_rfc3339(rfc3339)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now - then)
}

/// "just now", "5 min. ago", "3 days ago". Matches dgit's own vocabulary.
pub fn ago(secs: i64) -> String {
    if secs < 0 {
        return "in the future".into();
    }
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("{n} {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min. ago", s / 60),
        s if s < 86_400 => plural(s / 3600, "hour"),
        s if s < 7 * 86_400 => plural(s / 86_400, "day"),
        s if s < 31 * 86_400 => plural(s / (7 * 86_400), "week"),
        s if s < 365 * 86_400 => plural(s / (30 * 86_400), "month"),
        s => plural(s / (365 * 86_400), "year"),
    }
}

/// The relative age of a timestamp, falling back to the raw text.
pub fn age_of(rfc3339: &str) -> String {
    seconds_since(rfc3339).map_or_else(|| rfc3339.to_string(), ago)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_a_known_date() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-08-18T00:00:00Z"), Some(1_787_011_200));
    }

    #[test]
    fn offsets_and_fractions() {
        let z = parse_rfc3339("2026-08-18T12:00:00Z").unwrap();
        assert_eq!(parse_rfc3339("2026-08-18T14:00:00+02:00"), Some(z));
        assert_eq!(parse_rfc3339("2026-08-18T10:00:00-02:00"), Some(z));
        assert_eq!(parse_rfc3339("2026-08-18T12:00:00.123456Z"), Some(z));
        assert_eq!(parse_rfc3339("2026-08-18T14:00:00+0200"), Some(z));
    }

    #[test]
    fn rejects_what_is_not_a_timestamp() {
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn ago_reads_like_english() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(300), "5 min. ago");
        assert_eq!(ago(3600), "1 hour ago");
        assert_eq!(ago(7200), "2 hours ago");
        assert_eq!(ago(86_400), "1 day ago");
        assert_eq!(ago(3 * 86_400), "3 days ago");
    }

    #[test]
    fn an_unparseable_stamp_survives_as_itself() {
        assert_eq!(age_of("soon"), "soon");
    }
}
