//! Mute rules: the conditions under which a muted issue becomes interesting again.
//!
//! Muting for ever is the only mute anybody actually uses, and it is the reason the
//! mute button stops being trusted. A person mutes a noisy issue on a Tuesday meaning
//! "not now", and on the Friday it triples and nobody hears. So there are three rules
//! rather than one:
//!
//! - **forever** — the decision stands until somebody takes it back.
//! - **mute-for a duration** — quiet until a wall-clock moment, whatever arrives.
//! - **mute-until N events in a period** — quiet while it stays at this volume, loud
//!   when it stops being background. This is Sentry's "ignore until it happens this
//!   many times in this window", and the reading that matters is that the count is over
//!   a *window*, not over the issue's life: an issue at three events an hour is muted
//!   for ever by a rule of ten-in-an-hour, which is the point.
//!
//! Rules are evaluated on ingest, in the digest, where the event lands on the issue —
//! the only place with both the event and the issue in hand, and the only place that
//! already runs single-threaded. Nothing here is a timer: an issue nothing is arriving
//! at does not need to come off mute, because coming off mute is only ever interesting
//! when there is something to be told about.
//!
//! Coming off mute is a state change, so it goes out through
//! [`Notifier::unmuted`](crate::bugs::pushover::Notifier::unmuted) — the hook that has
//! been standing there since the notifications slice. There is no second kind of
//! notification for this and there should not be.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::bugs::index::{self, Issue, Project, state};
use crate::bugs::pushover::Notifier;
use crate::db::{Db, DbResult, now, now_offset};

/// What keeps an issue quiet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Rule {
    /// Until a person says otherwise.
    Forever,
    /// Until a wall-clock moment.
    For { seconds: i64 },
    /// Until `count` events arrive inside `window` seconds of each other's company.
    Until { count: i64, window: i64 },
}

/// The durations the issue page offers, as `(label, seconds)`.
pub const DURATIONS: [(&str, i64); 4] =
    [("1 hour", 3_600), ("24 hours", 86_400), ("7 days", 604_800), ("30 days", 2_592_000)];

/// The volume conditions the issue page offers, as `(label, count, window)`.
pub const CONDITIONS: [(&str, i64, i64); 3] = [
    ("10 events in an hour", 10, 3_600),
    ("100 events in an hour", 100, 3_600),
    ("100 events in a day", 100, 86_400),
];

impl Rule {
    /// Read the wire form the mute form posts: `forever`, `for:<seconds>`, or
    /// `until:<count>:<seconds>`. `None` for anything else, which the handler answers
    /// with a 400 rather than a guess.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() || raw == "forever" {
            return Some(Self::Forever);
        }
        if let Some(seconds) = raw.strip_prefix("for:") {
            let seconds = seconds.trim().parse::<i64>().ok().filter(|value| *value > 0)?;
            return Some(Self::For { seconds });
        }
        if let Some(rest) = raw.strip_prefix("until:") {
            let (count, window) = rest.split_once(':')?;
            let count = count.trim().parse::<i64>().ok().filter(|value| *value > 0)?;
            let window = window.trim().parse::<i64>().ok().filter(|value| *value > 0)?;
            return Some(Self::Until { count, window });
        }
        None
    }

    /// The wire form again, for a form control's value.
    pub fn wire(&self) -> String {
        match self {
            Self::Forever => "forever".to_owned(),
            Self::For { seconds } => format!("for:{seconds}"),
            Self::Until { count, window } => format!("until:{count}:{window}"),
        }
    }

    /// How the rule reads on the page.
    pub fn describe(&self) -> String {
        match self {
            Self::Forever => "muted until somebody unmutes it".to_owned(),
            Self::For { seconds } => format!("muted for {}", duration(*seconds)),
            Self::Until { count, window } => {
                format!("muted until {count} events arrive within {}", duration(*window))
            }
        }
    }
}

fn duration(seconds: i64) -> String {
    match seconds {
        seconds if seconds % 86_400 == 0 && seconds >= 86_400 => plural(seconds / 86_400, "day"),
        seconds if seconds % 3_600 == 0 && seconds >= 3_600 => plural(seconds / 3_600, "hour"),
        seconds if seconds % 60 == 0 && seconds >= 60 => plural(seconds / 60, "minute"),
        seconds => plural(seconds, "second"),
    }
}

fn plural(count: i64, unit: &str) -> String {
    if count == 1 { format!("1 {unit}") } else { format!("{count} {unit}s") }
}

/// Arm a rule on an issue that has just been muted.
///
/// [`index::set_state`] clears these columns on every move, so this always writes onto
/// a clean row and a re-mute never inherits the last rule's half-counted window.
pub fn arm(db: &Db, issue_id: i64, rule: &Rule) -> DbResult<()> {
    let (kind, until, count, window) = match rule {
        Rule::Forever => ("forever", None, None, None),
        Rule::For { seconds } => ("for", Some(now_offset(*seconds)), None, None),
        Rule::Until { count, window } => ("until", None, Some(*count), Some(*window)),
    };
    db.with(|conn| {
        conn.execute(
            "UPDATE bugs_issues
             SET mute_rule = ?2, mute_until = ?3, mute_count = ?4, mute_window = ?5,
                 mute_from = NULL, mute_events = 0
             WHERE id = ?1",
            params![issue_id, kind, until, count, window],
        )?;
        Ok(())
    })
}

/// What a muted issue says about itself on the page, and in the issue JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// `forever`, `for` or `until`, as stored.
    pub kind: String,
    /// One line a person can read.
    pub summary: String,
    /// The wall-clock deadline, for a `mute-for`.
    pub until: Option<String>,
    /// Events counted in the current window, for a `mute-until`.
    pub seen: Option<i64>,
    pub needed: Option<i64>,
    pub window: Option<i64>,
}

/// The mute state of one issue, or `None` when nothing is armed.
pub fn progress(db: &Db, issue_id: i64) -> DbResult<Option<Progress>> {
    let Some(armed) = read(db, issue_id)? else { return Ok(None) };
    let until_rule = matches!(armed.kind.as_str(), "until");
    Ok(Some(Progress {
        summary: armed.summary(),
        kind: armed.kind.clone(),
        until: armed.until.clone(),
        seen: until_rule.then_some(armed.events),
        needed: armed.count,
        window: armed.window,
    }))
}

/// The stored rule columns of one issue.
#[derive(Debug, Clone)]
struct Armed {
    kind: String,
    until: Option<String>,
    count: Option<i64>,
    window: Option<i64>,
    from: Option<String>,
    events: i64,
}

impl Armed {
    /// The rule to *evaluate*. A row whose parameters have gone missing falls back to
    /// `Forever`, which is the safe direction: a rule that cannot be read must not fire
    /// on its own and start notifying about an issue somebody deliberately silenced.
    fn rule(&self) -> Rule {
        match (self.kind.as_str(), self.count, self.window) {
            ("until", Some(count), Some(window)) => Rule::Until { count, window },
            ("for", _, _) => Rule::For { seconds: 0 },
            _ => Rule::Forever,
        }
    }

    /// One readable line. Built from the stored row rather than from [`Rule`], because
    /// what a `mute-for` has left is a deadline and not the duration it was set with —
    /// the duration is gone the moment the rule is armed.
    fn summary(&self) -> String {
        match self.kind.as_str() {
            "for" => match &self.until {
                Some(until) => format!("muted until {until}"),
                None => "muted until somebody unmutes it".to_owned(),
            },
            "until" => match (self.count, self.window) {
                (Some(count), Some(window)) => format!(
                    "muted until {count} events arrive within {} — {} so far",
                    duration(window),
                    self.events
                ),
                _ => "muted until somebody unmutes it".to_owned(),
            },
            _ => "muted until somebody unmutes it".to_owned(),
        }
    }
}

fn read(db: &Db, issue_id: i64) -> DbResult<Option<Armed>> {
    db.with(|conn| {
        conn.query_row(
            "SELECT mute_rule, mute_until, mute_count, mute_window, mute_from, mute_events
             FROM bugs_issues WHERE id = ?1 AND mute_rule IS NOT NULL",
            params![issue_id],
            |row| {
                Ok(Armed {
                    kind: row.get(0)?,
                    until: row.get(1)?,
                    count: row.get(2)?,
                    window: row.get(3)?,
                    from: row.get(4)?,
                    events: row.get(5)?,
                })
            },
        )
        .optional()
    })
}

/// Judge one event against the issue's mute rule.
///
/// Returns the issue as it stands afterwards when the rule fired, and `None` when
/// nothing changed — which is the overwhelmingly common answer, and costs one indexed
/// read for an issue that is muted and nothing at all for an issue that is not.
///
/// The caller passes the notifier it is digesting with, so a reindex running on
/// [`Notifier::off`] can unmute the history without ringing for any of it.
pub fn evaluate(
    db: &Db,
    project: &Project,
    issue: &Issue,
    notify: &Notifier,
) -> DbResult<Option<Issue>> {
    if issue.state != state::MUTED {
        return Ok(None);
    }
    let Some(armed) = read(db, issue.id)? else { return Ok(None) };

    let fires = match armed.rule() {
        Rule::Forever => false,
        // A deadline that will not parse has already passed, as far as this is
        // concerned. An issue stuck muted by a corrupt timestamp is the failure that
        // loses somebody an outage.
        Rule::For { .. } => armed.until.as_deref().is_none_or(|until| until <= now().as_str()),
        Rule::Until { count, window } => count_toward(db, issue.id, &armed, window)? >= count,
    };
    if !fires {
        return Ok(None);
    }

    // Unmuting is an ordinary state change, stamped with who caused it — which here is
    // the rule, not a person.
    let unmuted = index::set_state(db, issue.project_id, issue.id, state::UNRESOLVED, "mute rule")?;
    let Some(unmuted) = unmuted else { return Ok(None) };
    tracing::info!(issue = issue.id, "bugs: a mute rule expired and the issue is open again");
    notify.unmuted(db, project, &unmuted);
    Ok(Some(unmuted))
}

/// Add this event to the rolling window and hand back the count inside it.
///
/// The window is fixed, not sliding: it opens at the first event after a mute and rolls
/// whole once it is older than `window`. A sliding window would need a row per event to
/// be exact, which is a table the size of the log store for a question that is asking
/// "is this loud now" — and a fixed window answers that. The cost is the standard
/// boundary effect: a burst straddling a roll can need up to twice `count` before it
/// speaks up.
fn count_toward(db: &Db, issue_id: i64, armed: &Armed, window: i64) -> DbResult<i64> {
    let now = now();
    // Three cases, and the third is the one worth naming. A window that has rolled
    // starts again from this event. A window still open counts one more. A `mute_from`
    // that will not parse is neither — and reading it as the epoch, which is what a
    // silent zero does, would roll the window on every single event, reset the count to
    // one for ever, and leave the rule unable to fire. That silences an issue
    // permanently on a corrupt row, which is the opposite of the direction the `For`
    // arm above chooses deliberately. So it re-opens the window here and keeps counting.
    let (counted, from) = match armed.from.as_deref() {
        None => (1, now.clone()),
        Some(from) => match parsed_seconds(from) {
            None => (armed.events + 1, now.clone()),
            Some(started) => {
                let ends = crate::db::from_unix(started.saturating_add(window));
                if ends.is_none_or(|ends| ends <= now) {
                    (1, now.clone())
                } else {
                    (armed.events + 1, from.to_owned())
                }
            }
        },
    };
    db.with(|conn| {
        conn.execute(
            "UPDATE bugs_issues SET mute_from = ?2, mute_events = ?3 WHERE id = ?1",
            params![issue_id, from, counted],
        )?;
        Ok(())
    })?;
    Ok(counted)
}

/// A stored timestamp as a Unix second, or `None` when it is not one. Deliberately not
/// a silent zero: see [`count_toward`] for what the epoch would do to a mute window.
fn parsed_seconds(stamp: &str) -> Option<i64> {
    match time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339) {
        Ok(value) => Some(value.unix_timestamp()),
        Err(error) => {
            tracing::warn!(%error, stamp, "bugs: a stored mute window start is not a timestamp");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bugs::index::NewEvent;

    fn bed() -> (Db, Project) {
        let db = Db::in_memory().unwrap();
        index::migrate(&db).unwrap();
        crate::bugs::pushover::migrate(&db).unwrap();
        let project = index::create_project(&db, "demo", &"a".repeat(32), None).unwrap();
        (db, project)
    }

    fn land(db: &Db, project: &Project, n: usize) -> Issue {
        index::record(
            db,
            &NewEvent {
                project_id: project.id,
                event_id: format!("{n:032x}"),
                object_key: format!("projects/{}/events/{n}.json", project.id),
                grouping_key: "Boom".to_owned(),
                grouping_hash: "hash-boom".to_owned(),
                mechanism: "nashcode-v1".to_owned(),
                title: "Boom".to_owned(),
                culprit: None,
                level: "error".to_owned(),
                platform: None,
                timestamp: None,
            },
        )
        .unwrap()
        .0
    }

    #[test]
    fn the_wire_form_survives_a_round_trip_and_refuses_nonsense() {
        for rule in [
            Rule::Forever,
            Rule::For { seconds: 3_600 },
            Rule::Until { count: 10, window: 3_600 },
        ] {
            assert_eq!(Rule::parse(&rule.wire()), Some(rule.clone()), "{rule:?}");
        }
        assert_eq!(Rule::parse(""), Some(Rule::Forever));
        for bad in ["for:0", "for:-1", "for:soon", "until:10", "until:0:60", "until:10:0", "wat"] {
            assert_eq!(Rule::parse(bad), None, "{bad} was accepted");
        }
    }

    #[test]
    fn a_mute_for_ever_is_never_lifted_by_an_event() {
        let (db, project) = bed();
        let issue = land(&db, &project, 1);
        let issue = index::set_state(&db, project.id, issue.id, state::MUTED, "tester")
            .unwrap()
            .expect("muted");
        arm(&db, issue.id, &Rule::Forever).unwrap();

        for n in 2..20 {
            let issue = land(&db, &project, n);
            assert!(evaluate(&db, &project, &issue, &Notifier::off()).unwrap().is_none());
        }
        let issue = index::issue(&db, project.id, issue.id).unwrap().expect("the issue");
        assert_eq!(issue.state, state::MUTED);
    }

    #[test]
    fn a_mute_for_a_duration_lifts_when_the_clock_says_so() {
        let (db, project) = bed();
        let issue = land(&db, &project, 1);
        let issue = index::set_state(&db, project.id, issue.id, state::MUTED, "tester")
            .unwrap()
            .expect("muted");
        arm(&db, issue.id, &Rule::For { seconds: 3_600 }).unwrap();

        let next = land(&db, &project, 2);
        assert!(evaluate(&db, &project, &next, &Notifier::off()).unwrap().is_none());

        // Wind the deadline back rather than waiting an hour.
        db.with(|conn| {
            conn.execute(
                "UPDATE bugs_issues SET mute_until = ?2 WHERE id = ?1",
                params![issue.id, now_offset(-1)],
            )?;
            Ok(())
        })
        .unwrap();

        let next = land(&db, &project, 3);
        let opened = evaluate(&db, &project, &next, &Notifier::off()).unwrap().expect("unmuted");
        assert_eq!(opened.state, state::UNRESOLVED);
        // The rule is spent, so a later event is an ordinary event.
        assert!(read(&db, issue.id).unwrap().is_none(), "set_state cleared the rule");
    }

    #[test]
    fn a_mute_until_needs_the_events_inside_the_window() {
        let (db, project) = bed();
        let issue = land(&db, &project, 1);
        let issue = index::set_state(&db, project.id, issue.id, state::MUTED, "tester")
            .unwrap()
            .expect("muted");
        arm(&db, issue.id, &Rule::Until { count: 5, window: 3_600 }).unwrap();

        // Four is not five.
        for n in 2..=5 {
            let landed = land(&db, &project, n);
            assert!(
                evaluate(&db, &project, &landed, &Notifier::off()).unwrap().is_none(),
                "unmuted after {} events",
                n - 1
            );
        }
        let shown = progress(&db, issue.id).unwrap().expect("a rule");
        assert_eq!(shown.seen, Some(4));
        assert_eq!(shown.needed, Some(5));

        // The fifth inside the window is the one that speaks up.
        let landed = land(&db, &project, 6);
        let opened = evaluate(&db, &project, &landed, &Notifier::off()).unwrap().expect("unmuted");
        assert_eq!(opened.state, state::UNRESOLVED);
    }

    #[test]
    fn a_trickle_never_fills_the_window_however_long_it_goes_on() {
        let (db, project) = bed();
        let issue = land(&db, &project, 1);
        let issue = index::set_state(&db, project.id, issue.id, state::MUTED, "tester")
            .unwrap()
            .expect("muted");
        arm(&db, issue.id, &Rule::Until { count: 5, window: 3_600 }).unwrap();

        // Two events, then the window rolls, then two more. Never five together.
        for round in 0..8 {
            for n in 0..2 {
                let landed = land(&db, &project, round * 10 + n + 2);
                assert!(
                    evaluate(&db, &project, &landed, &Notifier::off()).unwrap().is_none(),
                    "a trickle unmuted at round {round}"
                );
            }
            db.with(|conn| {
                conn.execute(
                    "UPDATE bugs_issues SET mute_from = ?2 WHERE id = ?1",
                    params![issue.id, now_offset(-7_200)],
                )?;
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(
            index::issue(&db, project.id, issue.id).unwrap().expect("issue").state,
            state::MUTED,
            "sixteen events at two an hour is background, which is what the rule said"
        );
    }

    #[test]
    fn an_issue_that_is_not_muted_costs_nothing_to_judge() {
        let (db, project) = bed();
        let issue = land(&db, &project, 1);
        assert_eq!(issue.state, state::UNRESOLVED);
        assert!(evaluate(&db, &project, &issue, &Notifier::off()).unwrap().is_none());
    }

    #[test]
    fn re_muting_starts_the_window_again_rather_than_inheriting_the_last_one() {
        let (db, project) = bed();
        let issue = land(&db, &project, 1);
        let issue = index::set_state(&db, project.id, issue.id, state::MUTED, "tester")
            .unwrap()
            .expect("muted");
        arm(&db, issue.id, &Rule::Until { count: 5, window: 3_600 }).unwrap();
        for n in 2..=4 {
            let landed = land(&db, &project, n);
            evaluate(&db, &project, &landed, &Notifier::off()).unwrap();
        }
        assert_eq!(progress(&db, issue.id).unwrap().expect("a rule").seen, Some(3));

        index::set_state(&db, project.id, issue.id, state::UNRESOLVED, "tester").unwrap();
        index::set_state(&db, project.id, issue.id, state::MUTED, "tester").unwrap();
        arm(&db, issue.id, &Rule::Until { count: 5, window: 3_600 }).unwrap();
        assert_eq!(progress(&db, issue.id).unwrap().expect("a rule").seen, Some(0));
    }
}
