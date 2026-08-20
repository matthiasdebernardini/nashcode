//! Per-project ingest quotas.
//!
//! Backpressure and a quota are different refusals that share a status code. The
//! bounded digest queue already answers 429 when *this box* is behind; that is a fact
//! about us and it clears in seconds. A quota is a fact about one project — it has sent
//! more than its share — and it clears when a window rolls. Both have to exist, because
//! a runaway loop in one service must not be able to fill a bucket that ten other
//! services are also writing to.
//!
//! **Pre-parse.** The gate runs the moment the project is known and before a single
//! byte of the body is read. A quota that refuses after decompressing 20 MiB has spent
//! exactly what it exists to save.
//!
//! **Counted on acceptance, checked before.** Two calls, deliberately: [`check`] reads
//! and [`record`] writes, and nothing increments a counter for a request that turned
//! out to be unauthenticated or malformed. A sender that knows a project id but not its
//! key can therefore not spend anybody's quota.
//!
//! Fixed windows, not sliding ones. A sliding window needs a row per request; a fixed
//! window needs a row per project per window and answers the only question the gate
//! asks. The cost is a boundary effect — up to two windows' worth can leave in one
//! window's span, either side of a roll — which is the standard trade and is the right
//! one here, because the number this protects is a bucket bill and not a rate limit on
//! a login form.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::db::{Db, DbResult, now, now_offset};

/// The windows a project is held to, tightest first: name, span in seconds, and how
/// many accepted ingest requests fit in it.
///
/// Both doors count — an envelope and a log batch cost the same, because what this
/// bounds is requests through the front of the pipeline, not bytes through the back.
/// Hardcoded rather than configurable: a per-project override wants a settings form
/// and a migration, and nobody has yet needed one.
pub const WINDOWS: [(&str, i64, i64); 3] =
    [("5m", 5 * 60, 1_000), ("1h", 60 * 60, 5_000), ("month", 30 * 24 * 60 * 60, 1_000_000)];

/// Apply the quota schema. Idempotent, run on every open.
pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute_batch(SCHEMA)?;
        Ok(())
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bugs_quota (
    project_id INTEGER NOT NULL REFERENCES bugs_projects(id),
    window     TEXT NOT NULL,
    started_at TEXT NOT NULL,
    count      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, window)
) WITHOUT ROWID;
"#;

/// What one window has left. Shown on the project page and carried in its JSON: a
/// sender being refused wants to know which of the three said no.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowState {
    pub window: String,
    pub limit: i64,
    pub used: i64,
    /// When this window rolls and the count goes back to nothing.
    pub resets_at: String,
}

impl WindowState {
    pub fn exceeded(&self) -> bool {
        self.used >= self.limit
    }
}

/// The gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Room in every window.
    Allow,
    /// At least one window is full. `retry_after` is whole seconds, never zero.
    Over { window: String, retry_after: i64 },
}

/// Is there room for one more request from this project?
///
/// Read-only, and cheap: three primary-key lookups. **Fails open.** A quota is a
/// budget, and a database that will not answer a question about a budget is not a
/// reason to drop somebody's telemetry — the digest queue's own 429 is still there to
/// stop the box from drowning.
pub fn check(db: &Db, project_id: i64) -> Verdict {
    match state(db, project_id) {
        Ok(windows) => verdict(&windows),
        Err(error) => {
            tracing::warn!(%error, project_id, "bugs: cannot read the ingest quota; allowing");
            Verdict::Allow
        }
    }
}

/// The verdict for a set of windows, given what is used.
///
/// When more than one window is full the *earliest* reset is what the sender is told,
/// which is deliberately optimistic: the cost of being asked back too soon is one
/// wasted request that gets the same answer, and the cost of being asked back too late
/// is an SDK sitting on events it could have delivered.
fn verdict(windows: &[WindowState]) -> Verdict {
    let over = windows
        .iter()
        .filter(|state| state.exceeded())
        .min_by(|left, right| left.resets_at.cmp(&right.resets_at));
    match over {
        None => Verdict::Allow,
        Some(state) => Verdict::Over {
            window: state.window.clone(),
            retry_after: seconds_until(&state.resets_at),
        },
    }
}

/// Count one accepted request against every window. Called after the request has been
/// judged and stored, never before.
pub fn record(db: &Db, project_id: i64) {
    let now = now();
    let written = db.with(|conn| {
        for (window, span, _) in WINDOWS {
            // The roll is decided against a cutoff computed here, not by SQLite's
            // `datetime()`. That function answers `YYYY-MM-DD HH:MM:SS` — a space where
            // the stored format has a `T`, and no fraction and no `Z` — so comparing
            // its answer with a stored timestamp compares two different alphabets and
            // is wrong in a way no test of the count alone would show.
            let cutoff = now_offset(-span);
            // One statement does both jobs. A window that has rolled starts again from
            // this request rather than being cleared and then incremented, which would
            // be two statements with a gap between them.
            conn.execute(
                "INSERT INTO bugs_quota (project_id, window, started_at, count)
                 VALUES (?1, ?2, ?3, 1)
                 ON CONFLICT(project_id, window) DO UPDATE SET
                   started_at = CASE WHEN bugs_quota.started_at <= ?4
                       THEN ?3 ELSE bugs_quota.started_at END,
                   count = CASE WHEN bugs_quota.started_at <= ?4
                       THEN 1 ELSE bugs_quota.count + 1 END",
                params![project_id, window, now, cutoff],
            )?;
        }
        Ok(())
    });
    if let Err(error) = written {
        // A counter that will not increment loses a project its accounting, not its
        // events. Losing the event instead would be the wrong way round.
        tracing::warn!(%error, project_id, "bugs: cannot count an accepted request against the quota");
    }
}

/// Every window of a project, with a rolled window read as empty.
///
/// The roll is decided on read as well as on write, so a project that has sent nothing
/// for a day reports an empty window rather than yesterday's full one.
pub fn state(db: &Db, project_id: i64) -> DbResult<Vec<WindowState>> {
    let now = now();
    let mut windows = Vec::with_capacity(WINDOWS.len());
    for (window, span, limit) in WINDOWS {
        let stored: Option<(String, i64)> = db.with(|conn| {
            conn.query_row(
                "SELECT started_at, count FROM bugs_quota WHERE project_id = ?1 AND window = ?2",
                params![project_id, window],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
        })?;
        let (started_at, used) = match stored {
            Some((started_at, count)) if rolls_at(&started_at, span) > now => (started_at, count),
            // Absent, or long enough ago that the window has rolled.
            _ => (now.clone(), 0),
        };
        windows.push(WindowState {
            window: window.to_owned(),
            limit,
            used,
            resets_at: rolls_at(&started_at, span),
        });
    }
    Ok(windows)
}

/// When a window that opened at `started_at` rolls, in the canonical storage format so
/// it compares as text against everything else stored here.
fn rolls_at(started_at: &str, span: i64) -> String {
    match time::OffsetDateTime::parse(started_at, &time::format_description::well_known::Rfc3339) {
        Ok(value) => crate::db::from_unix(value.unix_timestamp().saturating_add(span))
            .unwrap_or_else(|| now_offset(span)),
        // A stored timestamp that is not one is a corrupt row; treating it as opening
        // now closes the window a span from here rather than never.
        Err(_) => now_offset(span),
    }
}

/// Whole seconds from now until `at`, floored at one. `Retry-After: 0` reads as "come
/// straight back", which is the one instruction that turns a quota into a hot loop.
fn seconds_until(at: &str) -> i64 {
    let Ok(target) = time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
    else {
        return 1;
    };
    (target.unix_timestamp() - time::OffsetDateTime::now_utc().unix_timestamp()).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bugs::index;

    fn bed() -> (Db, i64, i64) {
        let db = Db::in_memory().unwrap();
        index::migrate(&db).unwrap();
        migrate(&db).unwrap();
        let one = index::create_project(&db, "api", &"a".repeat(32), None).unwrap().id;
        let two = index::create_project(&db, "worker", &"b".repeat(32), None).unwrap().id;
        (db, one, two)
    }

    /// The tightest window's allowance. Everything here fills that one.
    const TIGHTEST: i64 = WINDOWS[0].2;

    #[test]
    fn a_fresh_project_has_every_window_empty() {
        let (db, project, _) = bed();
        assert_eq!(check(&db, project), Verdict::Allow);
        let windows = state(&db, project).unwrap();
        assert_eq!(windows.len(), WINDOWS.len());
        assert!(windows.iter().all(|window| window.used == 0 && !window.exceeded()));
    }

    #[test]
    fn the_tightest_window_is_the_one_that_refuses_and_it_names_itself() {
        let (db, project, _) = bed();
        for _ in 0..TIGHTEST {
            assert_eq!(check(&db, project), Verdict::Allow);
            record(&db, project);
        }
        match check(&db, project) {
            Verdict::Over { window, retry_after } => {
                assert_eq!(window, WINDOWS[0].0, "the five-minute window fills first");
                assert!(retry_after >= 1 && retry_after <= WINDOWS[0].1, "{retry_after}");
            }
            Verdict::Allow => panic!("{TIGHTEST} requests fit in a {TIGHTEST} allowance"),
        }
    }

    #[test]
    fn one_project_over_its_quota_costs_another_project_nothing() {
        let (db, loud, quiet) = bed();
        for _ in 0..TIGHTEST {
            record(&db, loud);
        }
        assert!(matches!(check(&db, loud), Verdict::Over { .. }));
        assert_eq!(check(&db, quiet), Verdict::Allow, "quotas are per project");
        assert_eq!(state(&db, quiet).unwrap()[0].used, 0);
    }

    #[test]
    fn a_rolled_window_reads_as_empty_however_full_it_was() {
        let (db, project, _) = bed();
        for _ in 0..TIGHTEST {
            record(&db, project);
        }
        assert!(matches!(check(&db, project), Verdict::Over { .. }));

        // Age the window past its span. This is what the clock would do.
        let long_ago = now_offset(-(WINDOWS[0].1 + 60));
        db.with(|conn| {
            conn.execute(
                "UPDATE bugs_quota SET started_at = ?2 WHERE project_id = ?1 AND window = '5m'",
                params![project, long_ago],
            )?;
            Ok(())
        })
        .unwrap();

        assert_eq!(check(&db, project), Verdict::Allow, "the window rolled");
        assert_eq!(state(&db, project).unwrap()[0].used, 0);
        // And the next request opens the window again from one, not from where it was.
        record(&db, project);
        assert_eq!(state(&db, project).unwrap()[0].used, 1);
        // The hour window did not roll with it, so its count is intact.
        assert_eq!(state(&db, project).unwrap()[1].used, TIGHTEST + 1);
    }

    #[test]
    fn the_earliest_reset_is_what_a_refused_sender_is_told() {
        let over = |window: &str, resets_at: &str| WindowState {
            window: window.to_owned(),
            limit: 1,
            used: 1,
            resets_at: resets_at.to_owned(),
        };
        let verdict = verdict(&[
            over("1h", &now_offset(3600)),
            over("5m", &now_offset(120)),
            WindowState {
                window: "month".to_owned(),
                limit: 10,
                used: 0,
                resets_at: now_offset(86_400),
            },
        ]);
        match verdict {
            Verdict::Over { window, retry_after } => {
                assert_eq!(window, "5m");
                assert!(retry_after <= 120, "{retry_after}");
            }
            Verdict::Allow => panic!("two windows were full"),
        }
        // Nothing full is nothing to say.
        assert_eq!(
            super::verdict(&[WindowState {
                window: "5m".to_owned(),
                limit: 10,
                used: 3,
                resets_at: now_offset(60),
            }]),
            Verdict::Allow
        );
    }

    #[test]
    fn a_retry_after_is_never_zero() {
        assert_eq!(seconds_until(&now_offset(-500)), 1, "a deadline in the past is not 'now'");
        assert_eq!(seconds_until("not a time"), 1);
        assert!(seconds_until(&now_offset(120)) > 100);
    }
}
