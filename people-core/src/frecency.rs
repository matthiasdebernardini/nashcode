//! Order by what is warm: how often a thing has matched, decayed by how long ago.
//!
//! Every list of people and projects is ordered this way — the CLI's `ls`, the desktop
//! app's lanes, the inspector's chips — and never alphabetically. With thirty-five
//! client folders in the file, alphabetical puts the client who wrote this morning
//! below the one who left in March. The file keeps its own order; only the views sort.
//!
//! The rule is one line: `count` halved for every fourteen days since `last`. Two
//! weeks is the interval a client relationship goes quiet over without being over — a
//! project untouched for a fortnight is worth half of what it was, and one untouched
//! for a quarter is worth about a fiftieth. Nothing here is tuned to a corpus, because
//! there is no corpus: it is a number a person can predict in their head, which is
//! what an order they have to trust needs.

use crate::model::Seen;

/// Days for a count to be worth half of itself.
pub const HALF_LIFE_DAYS: f64 = 14.0;

const DAY: f64 = 86_400.0;

/// What one `seen` is worth at `now`: `count`, halved every [`HALF_LIFE_DAYS`].
///
/// Never seen is `0.0`. So is a `last` that is not a timestamp: an undateable stamp
/// has no age, and treating it as fresh would float a broken row to the top of every
/// list, which is the one place a person would not think to look for the bug.
///
/// A `last` in the future — two machines, one clock behind — is treated as now rather
/// than as a bonus, so a skewed stamp cannot outrank a real match.
pub fn frecency(seen: Option<&Seen>, now: &str) -> f64 {
    let Some(seen) = seen else {
        return 0.0;
    };
    let (Some(then), Some(now)) = (parse_rfc3339(&seen.last), parse_rfc3339(now)) else {
        return 0.0;
    };
    let age_days = ((now - then).max(0) as f64) / DAY;
    seen.count as f64 * 0.5_f64.powf(age_days / HALF_LIFE_DAYS)
}

/// The items, warmest first. Equal scores — and everything nobody has seen — keep the
/// order they are written in, because the file's order is the operator's own.
pub fn by_frecency<'a, T>(
    items: &'a [T],
    seen: impl Fn(&T) -> Option<&Seen>,
    now: &str,
) -> Vec<&'a T> {
    let mut scored: Vec<(&T, f64)> =
        items.iter().map(|item| (item, frecency(seen(item), now))).collect();
    // Stable, so a tie is decided by nothing at all.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(item, _)| item).collect()
}

/// `3× · 2d ago`, or nothing when it has never matched.
///
/// The one spelling of warmth, so `nashcode people ls` and the desktop card under a
/// name say the same words about the same `seen`. It sits beside [`frecency`] because
/// it reads the same two numbers: a label that disagreed with the order would be worse
/// than no label.
///
/// A stamp neither this nor [`frecency`] can read survives as itself rather than as
/// "now": the operator wrote it, and hiding it would hide the typo.
pub fn seen_label(seen: Option<&Seen>, now: &str) -> Option<String> {
    let seen = seen?;
    let age = match (parse_rfc3339(&seen.last), parse_rfc3339(now)) {
        (Some(then), Some(now)) => short_age(now - then),
        _ => match seen.last.trim() {
            "" => "undated".to_owned(),
            written => written.to_owned(),
        },
    };
    Some(format!("{}× · {age}", seen.count))
}

/// An age in one word, because it sits at the end of a line that already has three
/// things on it.
fn short_age(seconds: i64) -> String {
    match seconds {
        s if s < 0 => "ahead".to_owned(),
        s if s < 60 => "now".to_owned(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 7 * 86_400 => format!("{}d ago", s / 86_400),
        s if s < 365 * 86_400 => format!("{}w ago", s / (7 * 86_400)),
        s => format!("{}y ago", s / (365 * 86_400)),
    }
}

/// Days from 1970-01-01 to a proleptic-Gregorian date. Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An RFC 3339 timestamp as Unix seconds, or `None` when it is not one.
///
/// Written out rather than pulled in: this crate depends on `serde` and on nothing
/// else, so that the iMessage router and the desktop app compile it without a date
/// library, and one subtraction is not worth a dependency. Fractional seconds are read
/// and discarded — the half-life is measured in days.
fn parse_rfc3339(stamp: &str) -> Option<i64> {
    let stamp = stamp.trim();
    if stamp.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { stamp.get(a..b)?.parse().ok() };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }

    let mut seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;

    // What follows the seconds: a fraction to skip, then `Z` or `+HH:MM` / `-HHMM`.
    let tail = &stamp[19..];
    let tail = tail.strip_prefix('.').map_or(tail, |fraction| {
        let end = fraction.find(|c: char| !c.is_ascii_digit()).unwrap_or(fraction.len());
        &fraction[end..]
    });
    if let Some(rest) = tail.strip_prefix(['+', '-']) {
        let sign = if tail.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = match rest.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
            // `+HHMM`. Cut with `get`, not with a range: the stamp is whatever the
            // operator typed, and `+€9x` has no character boundary at byte two —
            // slicing it would panic where garbage should simply score nothing.
            None => (rest.get(..2)?.parse().ok()?, rest.get(2..4)?.parse().ok()?),
        };
        seconds -= sign * (hours * 3600 + minutes * 60);
    }
    Some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-08-23T12:00:00Z";

    fn seen(count: u64, last: &str) -> Seen {
        Seen { count, last: last.to_owned() }
    }

    #[test]
    fn nobody_has_seen_it_and_so_it_is_worth_nothing() {
        assert_eq!(frecency(None, NOW), 0.0);
    }

    #[test]
    fn one_half_life_is_worth_half() {
        let fresh = seen(8, NOW);
        assert!((frecency(Some(&fresh), NOW) - 8.0).abs() < 1e-9);

        let fortnight = seen(8, "2026-08-09T12:00:00Z");
        assert!((frecency(Some(&fortnight), NOW) - 4.0).abs() < 1e-9);

        let month = seen(8, "2026-07-26T12:00:00Z");
        assert!((frecency(Some(&month), NOW) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_stamp_nothing_can_read_is_worth_nothing_rather_than_everything() {
        assert_eq!(frecency(Some(&seen(99, "last tuesday")), NOW), 0.0);
        assert_eq!(frecency(Some(&seen(99, "")), NOW), 0.0);
        assert_eq!(frecency(Some(&seen(99, NOW)), "whenever"), 0.0);
    }

    #[test]
    fn a_clock_that_runs_fast_gets_no_bonus() {
        let future = seen(3, "2027-01-01T00:00:00Z");
        assert!((frecency(Some(&future), NOW) - 3.0).abs() < 1e-9, "capped at now, not amplified");
    }

    #[test]
    fn an_offset_is_the_same_instant_as_the_zulu_spelling() {
        let zulu = seen(4, "2026-08-09T12:00:00Z");
        let offset = seen(4, "2026-08-09T14:00:00+02:00");
        assert_eq!(frecency(Some(&zulu), NOW), frecency(Some(&offset), NOW));
        let fraction = seen(4, "2026-08-09T12:00:00.250Z");
        assert!((frecency(Some(&fraction), NOW) - frecency(Some(&zulu), NOW)).abs() < 1e-6);
    }

    #[test]
    fn warmest_first_and_never_seen_last() {
        // (name, seen)
        let rows = [
            ("cold", Some(seen(9, "2026-01-01T12:00:00Z"))),
            ("never", None),
            ("warm", Some(seen(2, "2026-08-22T12:00:00Z"))),
            ("busy", Some(seen(40, "2026-08-09T12:00:00Z"))),
        ];
        let order: Vec<&str> =
            by_frecency(&rows, |row| row.1.as_ref(), NOW).into_iter().map(|row| row.0).collect();
        assert_eq!(order, ["busy", "warm", "cold", "never"]);
    }

    #[test]
    fn an_equal_score_keeps_the_order_it_was_written_in() {
        let rows = [
            ("first", Some(seen(3, "2026-08-20T12:00:00Z"))),
            ("second", Some(seen(3, "2026-08-20T12:00:00Z"))),
            ("third", None),
            ("fourth", None),
        ];
        let order: Vec<&str> =
            by_frecency(&rows, |row| row.1.as_ref(), NOW).into_iter().map(|row| row.0).collect();
        assert_eq!(order, ["first", "second", "third", "fourth"]);
    }

    #[test]
    fn warmth_reads_as_a_count_and_one_word_of_age() {
        assert_eq!(seen_label(None, NOW), None);
        assert_eq!(seen_label(Some(&seen(3, NOW)), NOW).as_deref(), Some("3× · now"));
        assert_eq!(
            seen_label(Some(&seen(1, "2026-08-23T10:00:00Z")), NOW).as_deref(),
            Some("1× · 2h ago")
        );
        assert_eq!(
            seen_label(Some(&seen(7, "2026-08-21T12:00:00Z")), NOW).as_deref(),
            Some("7× · 2d ago")
        );
        assert_eq!(
            seen_label(Some(&seen(2, "2026-08-02T12:00:00Z")), NOW).as_deref(),
            Some("2× · 3w ago")
        );
        assert_eq!(
            seen_label(Some(&seen(2, "2024-08-02T12:00:00Z")), NOW).as_deref(),
            Some("2× · 2y ago")
        );
    }

    #[test]
    fn a_stamp_the_label_cannot_read_survives_as_the_operator_wrote_it() {
        let broken = seen(3, "whenever");
        assert_eq!(seen_label(Some(&broken), NOW).as_deref(), Some("3× · whenever"));
        assert_eq!(seen_label(Some(&seen(3, "  ")), NOW).as_deref(), Some("3× · undated"));
        // A clock behind the file's is not a bonus here either.
        assert_eq!(
            seen_label(Some(&seen(1, "2027-01-01T00:00:00Z")), NOW).as_deref(),
            Some("1× · ahead")
        );
    }

    #[test]
    fn the_parser_reads_what_the_file_holds_and_refuses_what_it_does_not() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-08-18T00:00:00Z"), Some(1_787_011_200));
        assert_eq!(parse_rfc3339(" 2026-08-18T00:00:00Z "), Some(1_787_011_200));
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-08-18T25:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-08-18"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn a_stamp_with_a_multibyte_character_in_it_scores_nothing_rather_than_panicking() {
        // The offset the operator typed is not two digits and is not even two bytes
        // wide. Reading it must cost the row its score, not the process.
        assert_eq!(parse_rfc3339("2026-08-18T00:00:00+€9x"), None);
        assert_eq!(parse_rfc3339("2026-08-18T00:00:00-€9x"), None);
        assert_eq!(parse_rfc3339("2026-08-18T00:00:00+1"), None, "half an offset is none");
        assert_eq!(parse_rfc3339("€2026-08-18T00:00:00Z"), None, "and a leading one too");
        assert_eq!(frecency(Some(&seen(99, "2026-08-18T00:00:00+€9x")), NOW), 0.0);
        assert_eq!(frecency(Some(&seen(99, "€2026-08-18T00:00:00Z")), NOW), 0.0);
        // The label still shows the operator what they wrote, so the typo is visible.
        assert_eq!(
            seen_label(Some(&seen(2, "2026-08-18T00:00:00+€9x")), NOW).as_deref(),
            Some("2× · 2026-08-18T00:00:00+€9x")
        );
    }
}
