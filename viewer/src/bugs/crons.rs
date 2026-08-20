//! Cron monitors: the half of error tracking that watches for what did *not* happen.
//!
//! An error tracker hears from a process that broke. A cron monitor hears from one
//! that ran, and its whole value is in the silence: the backup that stopped firing
//! three weeks ago raises no exception anywhere, which is precisely why nobody
//! noticed. So the interesting states here are the two the client can never report —
//! **missed**, meaning the check-in never came, and **timeout**, meaning a run started
//! and never finished.
//!
//! That is the rule the module is built around. The server is the only judge of missed
//! and timeout. A `check_in` item claiming either is coerced on the way in, because a
//! process healthy enough to send "I was missed" was not missed. Everything else the
//! SDK says is taken as sent.
//!
//! A monitor is upserted **only** when a valid `monitor_config` rides along with the
//! check-in. Sentry's SDKs attach one when the code declares a schedule and omit it
//! when the schedule lives in the Sentry UI, and a server that invented a monitor out
//! of a bare check-in would have nothing to compute a deadline from — a monitor with
//! no schedule can never be missed, so it would sit on the page saying "ok" for ever.
//! A check-in for an unknown monitor is still stored; it is evidence, and the first
//! check-in that carries a config adopts the slug.
//!
//! Schedules are `croner` for the 5-field Vixie patterns and plain calendar arithmetic
//! for intervals. Both are evaluated in the monitor's own timezone, because `0 9 * * *`
//! in `America/Chicago` is not nine o'clock anywhere else and a deadline six hours out
//! is a false alarm every morning.

use std::str::FromStr;

use chrono::TimeZone;
use croner::Cron;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::bugs::index::Project;
use crate::bugs::pushover::Notifier;
use crate::db::{Db, DbResult, from_unix, now};

/// How long after a scheduled time a check-in may still arrive, in minutes, unless the
/// `monitor_config` says otherwise. One minute is Sentry's default and it is the right
/// one: a job that fires on the minute and takes two seconds to reach us is not late.
pub const DEFAULT_CHECKIN_MARGIN: i64 = 1;

/// How long a run may stay `in_progress` before it is a timeout, in minutes, unless
/// the `monitor_config` says otherwise. Thirty minutes is Sentry's default.
pub const DEFAULT_MAX_RUNTIME: i64 = 30;

/// How often the sweep runs. A minute is the resolution of a Vixie schedule, so it is
/// also the finest resolution "late" can meaningfully have.
pub const SWEEP_INTERVAL_SECS: u64 = 60;

/// A monitor's state. `ok`, `error` and `in_progress` are what a client reports;
/// `missed` and `timeout` are only ever computed here.
pub mod status {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const IN_PROGRESS: &str = "in_progress";
    pub const MISSED: &str = "missed";
    pub const TIMEOUT: &str = "timeout";

    /// A monitor that has been declared and has never checked in.
    pub const UNKNOWN: &str = "unknown";

    /// Is this a state a person should be told about?
    pub fn is_incident(value: &str) -> bool {
        matches!(value, ERROR | MISSED | TIMEOUT)
    }
}

// ---- schema ------------------------------------------------------------------------

/// Apply the cron schema. Idempotent, run on every open, the same as the rest of the
/// feature's tables — this module owns them, the way `logs` and `pushover` own theirs.
pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute_batch(SCHEMA)?;
        Ok(())
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bugs_monitors (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id          INTEGER NOT NULL REFERENCES bugs_projects(id),
    slug                TEXT NOT NULL,
    schedule_kind       TEXT NOT NULL,
    schedule            TEXT NOT NULL,
    schedule_unit       TEXT,
    checkin_margin      INTEGER NOT NULL DEFAULT 1,
    max_runtime         INTEGER NOT NULL DEFAULT 30,
    timezone            TEXT,
    status              TEXT NOT NULL,
    last_checkin_at     TEXT,
    next_checkin_latest TEXT,
    timeout_at          TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS bugs_monitors_slug
    ON bugs_monitors (project_id, slug);
CREATE INDEX IF NOT EXISTS bugs_monitors_due
    ON bugs_monitors (next_checkin_latest, timeout_at);

CREATE TABLE IF NOT EXISTS bugs_checkins (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES bugs_projects(id),
    monitor_id  INTEGER,
    slug        TEXT NOT NULL,
    check_in_id TEXT NOT NULL,
    status      TEXT NOT NULL,
    coerced     INTEGER NOT NULL DEFAULT 0,
    duration    REAL,
    environment TEXT,
    release     TEXT,
    received_at TEXT NOT NULL
);
-- One check-in id carries two rows in a normal run: the `in_progress` and the terminal
-- one. Keying the uniqueness on the pair is what makes a re-digest cost nothing.
CREATE UNIQUE INDEX IF NOT EXISTS bugs_checkins_unique
    ON bugs_checkins (project_id, check_in_id, status);
CREATE INDEX IF NOT EXISTS bugs_checkins_by_monitor
    ON bugs_checkins (monitor_id, received_at);

CREATE TABLE IF NOT EXISTS bugs_incidents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    monitor_id  INTEGER NOT NULL REFERENCES bugs_monitors(id),
    kind        TEXT NOT NULL,
    detail      TEXT,
    opened_at   TEXT NOT NULL,
    resolved_at TEXT
);
CREATE INDEX IF NOT EXISTS bugs_incidents_open
    ON bugs_incidents (monitor_id, resolved_at, id);
"#;

// ---- the wire shapes ---------------------------------------------------------------

/// A schedule, exactly as a `monitor_config` states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schedule {
    /// `crontab` or `interval`.
    pub kind: String,
    /// The crontab pattern, or the interval count as text.
    pub value: String,
    /// `minute`, `hour`, `day`, `week`, `month` or `year`, for an interval.
    pub unit: Option<String>,
}

pub const CRONTAB: &str = "crontab";
pub const INTERVAL: &str = "interval";

impl Schedule {
    /// Read a `monitor_config.schedule`, and refuse anything this box cannot compute a
    /// deadline from. A schedule that will not parse is not a schedule: accepting it
    /// would create a monitor that can never be late.
    pub fn parse(value: &Value) -> Option<Self> {
        // Sentry also allows the bare-string shorthand `"schedule": "0 * * * *"`.
        if let Some(pattern) = value.as_str() {
            return Self::crontab(pattern);
        }
        let kind = value.get("type").and_then(Value::as_str)?;
        match kind {
            CRONTAB => Self::crontab(value.get("value").and_then(Value::as_str)?),
            INTERVAL => {
                let count = match value.get("value")? {
                    Value::Number(number) => number.as_i64()?,
                    Value::String(text) => text.trim().parse().ok()?,
                    _ => return None,
                };
                if count <= 0 {
                    return None;
                }
                let unit = value.get("unit").and_then(Value::as_str)?.trim().to_ascii_lowercase();
                let unit = unit.strip_suffix('s').unwrap_or(&unit).to_owned();
                if !matches!(unit.as_str(), "minute" | "hour" | "day" | "week" | "month" | "year") {
                    return None;
                }
                Some(Self {
                    kind: INTERVAL.to_owned(),
                    value: count.to_string(),
                    unit: Some(unit),
                })
            }
            _ => None,
        }
    }

    fn crontab(pattern: &str) -> Option<Self> {
        let pattern = pattern.trim();
        let cron = Cron::from_str(pattern).ok()?;
        // Parsing is not enough. `0 0 30 2 *` — the thirtieth of February — is a
        // well-formed pattern that never occurs, and croner accepts it happily. A
        // monitor built on one has no computable deadline, so neither sweep predicate
        // can ever select it: the page says "ok" for ever while covering nothing, which
        // is worse than an empty page because it looks like coverage. Asking for one
        // occurrence is what tells a pattern that fires from a pattern that only parses.
        cron.find_next_occurrence(&chrono::Utc::now(), false).ok()?;
        Some(Self { kind: CRONTAB.to_owned(), value: pattern.to_owned(), unit: None })
    }

    /// How this schedule reads on the page.
    pub fn describe(&self) -> String {
        match self.unit.as_deref() {
            Some(unit) => format!("every {} {unit}", self.value),
            None => self.value.clone(),
        }
    }
}

/// A `monitor_config`, which is what turns a check-in into a monitor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonitorConfig {
    pub schedule: Schedule,
    pub checkin_margin: i64,
    pub max_runtime: i64,
    pub timezone: Option<String>,
}

impl MonitorConfig {
    /// `None` when there is no usable schedule in it. Every other field falls back to a
    /// default rather than failing the config, because a missing margin is not a reason
    /// to refuse a schedule the sender did state.
    pub fn parse(value: &Value) -> Option<Self> {
        let schedule = Schedule::parse(value.get("schedule")?)?;
        let minutes = |key: &str, fallback: i64| {
            value
                .get(key)
                .and_then(Value::as_i64)
                .filter(|number| *number > 0)
                .unwrap_or(fallback)
        };
        // A timezone this box cannot resolve is dropped rather than stored, so the
        // deadline is computed in UTC and the page does not claim a zone it ignored.
        let timezone = value
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.parse::<chrono_tz::Tz>().is_ok())
            .map(str::to_owned);
        Some(Self {
            schedule,
            checkin_margin: minutes("checkin_margin", DEFAULT_CHECKIN_MARGIN),
            max_runtime: minutes("max_runtime", DEFAULT_MAX_RUNTIME),
            timezone,
        })
    }
}

/// One `check_in` envelope item, parsed and judged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckIn {
    pub check_in_id: String,
    pub slug: String,
    /// Already coerced: only `in_progress`, `ok` and `error` ever reach here.
    pub status: String,
    /// True when the client claimed something this server does not let it claim.
    pub coerced: bool,
    pub duration: Option<f64>,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub config: Option<MonitorConfig>,
}

impl CheckIn {
    /// Parse a `check_in` item payload. `None` when there is no monitor slug, which is
    /// the one field without which the check-in cannot be filed anywhere.
    pub fn parse(payload: &Value) -> Option<Self> {
        let slug = payload
            .get("monitor_slug")
            .or_else(|| payload.get("monitor_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|slug| !slug.is_empty())?;
        let raw = payload.get("status").and_then(Value::as_str).unwrap_or("").trim();
        let (status, coerced) = coerce(raw);
        Some(Self {
            // An id is how a run's two halves find each other. A sender that omits it
            // gets one, which files the check-in and forgoes the pairing.
            check_in_id: payload
                .get("check_in_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && !is_sentinel(id))
                .map_or_else(super::digest::new_event_id, str::to_owned),
            slug: slug.to_owned(),
            status,
            coerced,
            duration: payload.get("duration").and_then(Value::as_f64),
            environment: text(payload, "environment"),
            release: text(payload, "release"),
            config: payload.get("monitor_config").and_then(MonitorConfig::parse),
        })
    }
}

/// The all-zero check-in id, which the protocol gives a second meaning: "this updates
/// whichever run of this monitor is still in progress".
///
/// It is a sentinel, not an identity, and storing it as one would be a quiet disaster:
/// the uniqueness that makes a re-digest free is on `(project, check_in_id, status)`,
/// so every `ok` a sender ever sent with the sentinel would collide with the first one
/// and every run after the first would move nothing. A fresh id is minted instead. The
/// cost is that the two halves of that run are not paired, which costs a duration on
/// the page; the alternative costs the monitor.
fn is_sentinel(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte == b'0')
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// What a client is allowed to say, and what everything else becomes.
///
/// `missed` and `timeout` are the two verdicts only this server can reach, so a client
/// claiming either is telling us something it cannot know — and a process alive enough
/// to send "I was missed" plainly was not. Anything unrecognised lands on `error` for
/// the same reason a strange answer from a health check is not a healthy one.
fn coerce(raw: &str) -> (String, bool) {
    match raw.to_ascii_lowercase().as_str() {
        status::OK => (status::OK.to_owned(), false),
        status::IN_PROGRESS => (status::IN_PROGRESS.to_owned(), false),
        status::ERROR => (status::ERROR.to_owned(), false),
        _ => (status::ERROR.to_owned(), true),
    }
}

// ---- rows --------------------------------------------------------------------------

/// A monitor as the page and the JSON show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Monitor {
    pub id: i64,
    pub project_id: i64,
    pub slug: String,
    pub schedule_kind: String,
    pub schedule: String,
    pub schedule_unit: Option<String>,
    pub checkin_margin: i64,
    pub max_runtime: i64,
    pub timezone: Option<String>,
    pub status: String,
    pub last_checkin_at: Option<String>,
    /// When the next check-in has to have arrived by. The sweep reads this and nothing
    /// else to decide "missed".
    pub next_checkin_latest: Option<String>,
    /// When the run that is `in_progress` stops being merely slow. NULL when nothing
    /// is running.
    pub timeout_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Monitor {
    pub fn schedule(&self) -> Schedule {
        Schedule {
            kind: self.schedule_kind.clone(),
            value: self.schedule.clone(),
            unit: self.schedule_unit.clone(),
        }
    }
}

/// One incident: a monitor was unhealthy from `opened_at` until `resolved_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Incident {
    pub id: i64,
    pub monitor_id: i64,
    pub kind: String,
    pub detail: Option<String>,
    pub opened_at: String,
    pub resolved_at: Option<String>,
}

/// One stored check-in, newest first on the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckInRow {
    pub id: i64,
    pub slug: String,
    pub check_in_id: String,
    pub status: String,
    pub coerced: bool,
    pub duration: Option<f64>,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub received_at: String,
}

fn read_monitor(row: &rusqlite::Row<'_>) -> rusqlite::Result<Monitor> {
    Ok(Monitor {
        id: row.get(0)?,
        project_id: row.get(1)?,
        slug: row.get(2)?,
        schedule_kind: row.get(3)?,
        schedule: row.get(4)?,
        schedule_unit: row.get(5)?,
        checkin_margin: row.get(6)?,
        max_runtime: row.get(7)?,
        timezone: row.get(8)?,
        status: row.get(9)?,
        last_checkin_at: row.get(10)?,
        next_checkin_latest: row.get(11)?,
        timeout_at: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

const MONITOR_COLUMNS: &str = "id, project_id, slug, schedule_kind, schedule, schedule_unit,
     checkin_margin, max_runtime, timezone, status, last_checkin_at, next_checkin_latest,
     timeout_at, created_at, updated_at";

/// Every monitor of a project, by slug.
pub fn monitors(db: &Db, project_id: i64) -> DbResult<Vec<Monitor>> {
    db.with(|conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {MONITOR_COLUMNS} FROM bugs_monitors WHERE project_id = ?1 ORDER BY slug"
        ))?;
        let rows = statement.query_map(params![project_id], read_monitor)?;
        rows.collect()
    })
}

pub fn monitor_by_slug(db: &Db, project_id: i64, slug: &str) -> DbResult<Option<Monitor>> {
    db.with(|conn| monitor_row(conn, project_id, slug))
}

/// The same, on a connection the caller already holds — which is every caller inside a
/// transaction, since `Db::with` takes one mutex and taking it twice would deadlock.
fn monitor_row(conn: &Connection, project_id: i64, slug: &str) -> rusqlite::Result<Option<Monitor>> {
    conn.query_row(
        &format!("SELECT {MONITOR_COLUMNS} FROM bugs_monitors WHERE project_id = ?1 AND slug = ?2"),
        params![project_id, slug],
        read_monitor,
    )
    .optional()
}

/// The incidents of a project's monitors, newest first.
pub fn incidents(db: &Db, project_id: i64, limit: i64) -> DbResult<Vec<Incident>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT i.id, i.monitor_id, i.kind, i.detail, i.opened_at, i.resolved_at
             FROM bugs_incidents i
             JOIN bugs_monitors m ON m.id = i.monitor_id
             WHERE m.project_id = ?1
             ORDER BY i.id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![project_id, limit], |row| {
            Ok(Incident {
                id: row.get(0)?,
                monitor_id: row.get(1)?,
                kind: row.get(2)?,
                detail: row.get(3)?,
                opened_at: row.get(4)?,
                resolved_at: row.get(5)?,
            })
        })?;
        rows.collect()
    })
}

/// The stored check-ins of a project, newest first.
pub fn checkins(db: &Db, project_id: i64, limit: i64) -> DbResult<Vec<CheckInRow>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, slug, check_in_id, status, coerced, duration, environment, release,
                    received_at
             FROM bugs_checkins WHERE project_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![project_id, limit], |row| {
            Ok(CheckInRow {
                id: row.get(0)?,
                slug: row.get(1)?,
                check_in_id: row.get(2)?,
                status: row.get(3)?,
                coerced: row.get::<_, i64>(4)? != 0,
                duration: row.get(5)?,
                environment: row.get(6)?,
                release: row.get(7)?,
                received_at: row.get(8)?,
            })
        })?;
        rows.collect()
    })
}

// ---- schedule arithmetic -----------------------------------------------------------

/// The next time this schedule fires strictly after `after`, as a Unix second.
///
/// `None` for a schedule that cannot be evaluated — an unparseable pattern, or a
/// calendar step that runs off the end of the representable range. The caller then
/// stores no deadline, and a monitor with no deadline is never called missed. Silence
/// is the right failure here: inventing a deadline out of a schedule we did not
/// understand rings a phone about nothing.
pub fn next_after(schedule: &Schedule, timezone: Option<&str>, after: i64) -> Option<i64> {
    let zone: chrono_tz::Tz =
        timezone.and_then(|name| name.parse().ok()).unwrap_or(chrono_tz::UTC);
    let start = zone.timestamp_opt(after, 0).single()?;
    let next = match schedule.kind.as_str() {
        INTERVAL => {
            let count = schedule.value.parse::<i64>().ok().filter(|count| *count > 0)?;
            step(start, count, schedule.unit.as_deref()?)?
        }
        // `crontab`, and anything else that got as far as being stored.
        _ => Cron::from_str(&schedule.value).ok()?.find_next_occurrence(&start, false).ok()?,
    };
    Some(next.timestamp())
}

/// One interval step forward. Months and years go through the calendar rather than
/// through a count of seconds, so "every month" from the 31st lands where a person
/// would expect it to and a leap year is not an hour short.
fn step(
    from: chrono::DateTime<chrono_tz::Tz>,
    count: i64,
    unit: &str,
) -> Option<chrono::DateTime<chrono_tz::Tz>> {
    let count = u32::try_from(count).ok()?;
    match unit {
        "minute" => from.checked_add_signed(chrono::TimeDelta::try_minutes(i64::from(count))?),
        "hour" => from.checked_add_signed(chrono::TimeDelta::try_hours(i64::from(count))?),
        "day" => from.checked_add_days(chrono::Days::new(u64::from(count))),
        "week" => from.checked_add_days(chrono::Days::new(u64::from(count) * 7)),
        "month" => from.checked_add_months(chrono::Months::new(count)),
        "year" => from.checked_add_months(chrono::Months::new(count.checked_mul(12)?)),
        _ => None,
    }
}

/// The deadline for the next check-in: the next scheduled time plus the grace margin.
///
/// `None` writes a NULL deadline, and a NULL deadline is invisible to the missed sweep
/// — so it is never silent. A schedule survives [`Schedule::parse`] only by resolving
/// once, so reaching here empty means something later went wrong: a daylight-saving gap
/// an interval step landed inside, or a calendar step off the end of the representable
/// range. Either way the monitor stops being covered, and a person has to be able to
/// find out why from the log.
fn deadline(monitor: &Monitor, after: i64) -> Option<String> {
    let stamp = next_after(&monitor.schedule(), monitor.timezone.as_deref(), after)
        .and_then(|next| next.checked_add(monitor.checkin_margin.saturating_mul(60)))
        .and_then(from_unix);
    if stamp.is_none() {
        tracing::warn!(
            monitor = monitor.slug,
            schedule = monitor.schedule,
            timezone = monitor.timezone.as_deref().unwrap_or("UTC"),
            "bugs: this cron schedule has no next occurrence, so the monitor cannot go missing"
        );
    }
    stamp
}

// ---- the ingest path ---------------------------------------------------------------

/// File one check-in and move its monitor.
///
/// Returns the monitor when the slug has one. Everything here is one SQLite
/// transaction's worth of work on the digest task, which is the single writer.
///
/// A check-in that has already been filed — a re-digest, a redelivered drain row —
/// stops after the `INSERT OR IGNORE` and moves nothing. Replaying history must not
/// reopen an incident that was closed a month ago.
/// What one write decided a person should be told.
///
/// Collected inside the transaction and delivered after it commits. `Db::with` holds
/// one mutex over one connection, so queueing a push from inside a transaction would
/// take that lock twice and wedge the digest task against itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum News {
    Opened { id: i64, kind: String, detail: Option<String> },
    Recovered { id: i64 },
}

impl News {
    fn tell(self, db: &Db, project: &Project, slug: &str, notify: &Notifier) {
        match self {
            Self::Opened { id, kind, detail } => {
                tracing::warn!(monitor = slug, kind, "bugs: a cron monitor opened an incident");
                notify.cron_incident(db, project, slug, &kind, detail.as_deref(), id);
            }
            Self::Recovered { id } => {
                tracing::info!(monitor = slug, "bugs: a cron monitor recovered");
                notify.cron_recovered(db, project, slug, id);
            }
        }
    }
}

/// File one check-in and move its monitor.
///
/// Returns the monitor when the slug has one.
///
/// **Everything here is one transaction, and that is the whole design.** The check-in
/// row and the monitor state it implies used to be two writes with a gap between them,
/// and the gap was reachable twice over. The one-minute sweep could land in it, find a
/// monitor whose deadline had passed because the `ok` had not been applied yet, open a
/// missed incident and push it — and then the `ok` landed, closed the incident it had
/// just opened, and pushed a recovery. Two notifications for a job that ran on time. A
/// crash in the same gap was worse: the check-in row is durable, so redelivery hit
/// `INSERT OR IGNORE`, found the row, and returned before applying anything, leaving the
/// monitor wedged at `unknown` with a NULL deadline that neither sweep predicate can
/// ever select. One transaction closes the first hole outright; [`stale_since`] closes
/// the second for any row already stranded by it.
pub fn record(
    db: &Db,
    project: &Project,
    check_in: &CheckIn,
    notify: &Notifier,
) -> DbResult<Option<Monitor>> {
    let received_at = now();
    let (monitor, news) = db.with(|conn| {
        let tx = conn.unchecked_transaction()?;
        if let Some(config) = &check_in.config {
            upsert_monitor(&tx, project.id, &check_in.slug, config, &received_at)?;
        }
        let monitor = monitor_row(&tx, project.id, &check_in.slug)?;
        let fresh = insert_checkin(
            &tx,
            project.id,
            monitor.as_ref().map(|monitor| monitor.id),
            check_in,
            &received_at,
        )?;

        // No monitor: the check-in is stored as evidence and declares nothing.
        let Some(monitor) = monitor else {
            tx.commit()?;
            return Ok((None, None));
        };

        // A fresh row is always applied, at the moment it arrived. A row that was
        // already there is normally a replay and must move nothing — but see
        // `stale_since` for the one case where it must.
        let at = if fresh {
            Some(received_at.clone())
        } else {
            stale_since(&tx, project.id, check_in, &monitor)?
        };
        let news = match &at {
            Some(at) => apply(&tx, &monitor, &check_in.status, at)?,
            None => None,
        };
        let refreshed = monitor_row(&tx, project.id, &check_in.slug)?;
        tx.commit()?;
        Ok((refreshed, news))
    })?;

    if let (Some(monitor), Some(news)) = (&monitor, news) {
        news.tell(db, project, &monitor.slug, notify);
    }
    Ok(monitor)
}

/// When an already-stored check-in still has work to do on its monitor, or `None` when
/// the monitor has already absorbed it.
///
/// A replay must move nothing: re-digesting a month of envelopes cannot be allowed to
/// reopen incidents that closed a month ago. But a check-in row can exist while the
/// monitor never saw it — a crash in the old two-write gap, or any future path that
/// files a row and fails before applying it — and that monitor is stuck for ever, with
/// no deadline for either sweep predicate to select.
///
/// The monitor's own `last_checkin_at` tells the two apart. If it is at or past the
/// stored row's timestamp, the monitor absorbed this check-in and there is nothing to
/// do. If it is behind, or absent, this row was never applied. The stored timestamp —
/// not `now` — is what the deadline is then computed from, because that is when the job
/// actually ran.
fn stale_since(
    tx: &Connection,
    project_id: i64,
    check_in: &CheckIn,
    monitor: &Monitor,
) -> rusqlite::Result<Option<String>> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT received_at FROM bugs_checkins
             WHERE project_id = ?1 AND check_in_id = ?2 AND status = ?3",
            params![project_id, check_in.check_in_id, check_in.status],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else { return Ok(None) };
    let absorbed =
        monitor.last_checkin_at.as_deref().is_some_and(|last| last >= stored.as_str());
    Ok((!absorbed).then_some(stored))
}

/// Store the check-in row. `false` means it was already there.
fn insert_checkin(
    tx: &Connection,
    project_id: i64,
    monitor_id: Option<i64>,
    check_in: &CheckIn,
    at: &str,
) -> rusqlite::Result<bool> {
    let changed = tx.execute(
        "INSERT OR IGNORE INTO bugs_checkins
           (project_id, monitor_id, slug, check_in_id, status, coerced, duration,
            environment, release, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            project_id,
            monitor_id,
            check_in.slug,
            check_in.check_in_id,
            check_in.status,
            check_in.coerced as i64,
            check_in.duration,
            check_in.environment,
            check_in.release,
            at,
        ],
    )?;
    Ok(changed > 0)
}

/// Create the monitor, or update its schedule. The check-in's own config always wins:
/// the code that runs the job is the thing that knows when it runs.
fn upsert_monitor(
    tx: &Connection,
    project_id: i64,
    slug: &str,
    config: &MonitorConfig,
    at: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO bugs_monitors
           (project_id, slug, schedule_kind, schedule, schedule_unit, checkin_margin,
            max_runtime, timezone, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'unknown', ?9, ?9)
         ON CONFLICT(project_id, slug) DO UPDATE SET
           schedule_kind = excluded.schedule_kind,
           schedule = excluded.schedule,
           schedule_unit = excluded.schedule_unit,
           checkin_margin = excluded.checkin_margin,
           max_runtime = excluded.max_runtime,
           timezone = excluded.timezone,
           updated_at = excluded.updated_at",
        params![
            project_id,
            slug,
            config.schedule.kind,
            config.schedule.value,
            config.schedule.unit,
            config.checkin_margin,
            config.max_runtime,
            config.timezone,
            at,
        ],
    )?;
    Ok(())
}

/// Move a monitor to what this check-in says, and say whether that is news.
fn apply(
    tx: &Connection,
    monitor: &Monitor,
    status: &str,
    at: &str,
) -> rusqlite::Result<Option<News>> {
    let seconds = seconds_of(at);
    let next = deadline(monitor, seconds);
    let timeout_at = if status == status::IN_PROGRESS {
        from_unix(seconds.saturating_add(monitor.max_runtime.saturating_mul(60)))
    } else {
        None
    };

    tx.execute(
        "UPDATE bugs_monitors
         SET status = ?2, last_checkin_at = ?3, next_checkin_latest = ?4,
             timeout_at = ?5, updated_at = ?3
         WHERE id = ?1",
        params![monitor.id, status, at, next, timeout_at],
    )?;

    if status::is_incident(status) {
        open_incident(tx, monitor, status, None, at)
    } else if status == status::OK {
        resolve_incident(tx, monitor, at)
    } else {
        Ok(None)
    }
}

// ---- the sweep ---------------------------------------------------------------------

/// What one sweep decided. Returned rather than logged so a test can read it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub missed: usize,
    pub timed_out: usize,
}

impl Swept {
    pub fn total(&self) -> usize {
        self.missed + self.timed_out
    }
}

/// A run that started and has not reported a result inside its `max_runtime`.
const OVERDUE_RESULT: &str =
    "timeout_at IS NOT NULL AND timeout_at <= ?1 AND status = 'in_progress'";

/// A monitor whose next check-in never arrived. Deliberately excludes `in_progress`: a
/// run that started is not a run that never came, and its own deadline is the one that
/// applies.
const OVERDUE_CHECKIN: &str =
    "next_checkin_latest IS NOT NULL AND next_checkin_latest <= ?1 AND status <> 'in_progress'";

/// Compute `missed` and `timeout` for every monitor that is overdue. The one-minute
/// job; nothing else in this module ever writes those two states.
pub fn sweep(db: &Db, notify: &Notifier) -> DbResult<Swept> {
    sweep_at(db, notify, seconds_of(&now()))
}

/// The same, at a stated moment. This is what a test drives: a monitor that is late
/// tomorrow is late because tomorrow arrived, and waiting for it is not a test.
pub fn sweep_at(db: &Db, notify: &Notifier, now_unix: i64) -> DbResult<Swept> {
    let Some(at) = from_unix(now_unix) else { return Ok(Swept::default()) };
    let mut swept = Swept::default();

    // Timeouts first. A run that overran is a run that started, so its own deadline is
    // the one that matters; deciding "missed" about it as well would file the same
    // silence twice.
    for monitor in due(db, OVERDUE_RESULT, &at)? {
        let detail = format!("no result inside {} minutes", monitor.max_runtime);
        transition(db, &monitor, status::TIMEOUT, &detail, &at, notify)?;
        swept.timed_out += 1;
    }

    for monitor in due(db, OVERDUE_CHECKIN, &at)? {
        let due_by = monitor.next_checkin_latest.as_deref().unwrap_or("its deadline");
        let detail = format!("no check-in by {due_by}");
        transition(db, &monitor, status::MISSED, &detail, &at, notify)?;
        swept.missed += 1;
    }

    if swept.total() > 0 {
        tracing::warn!(
            missed = swept.missed,
            timed_out = swept.timed_out,
            "bugs: cron monitors are late"
        );
    }
    Ok(swept)
}

fn due(db: &Db, predicate: &str, at: &str) -> DbResult<Vec<Monitor>> {
    db.with(|conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {MONITOR_COLUMNS} FROM bugs_monitors WHERE {predicate} ORDER BY id"
        ))?;
        let rows = statement.query_map(params![at], read_monitor)?;
        rows.collect()
    })
}

/// Put a monitor into a state only the server can reach, and push its deadline past
/// `at` so the next sweep does not file the same lateness again.
///
/// One transaction, then the notification, for the same reason [`record`] is one.
fn transition(
    db: &Db,
    monitor: &Monitor,
    status: &str,
    detail: &str,
    at: &str,
    notify: &Notifier,
) -> DbResult<()> {
    let news = db.with(|conn| {
        let tx = conn.unchecked_transaction()?;
        let next = deadline(monitor, seconds_of(at));
        tx.execute(
            "UPDATE bugs_monitors
             SET status = ?2, next_checkin_latest = ?3, timeout_at = NULL, updated_at = ?4
             WHERE id = ?1",
            params![monitor.id, status, next, at],
        )?;
        let news = open_incident(&tx, monitor, status, Some(detail), at)?;
        tx.commit()?;
        Ok(news)
    })?;
    let Some(news) = news else { return Ok(()) };
    let Some(project) = crate::bugs::index::project_by_id(db, monitor.project_id)? else {
        return Ok(());
    };
    news.tell(db, &project, &monitor.slug, notify);
    Ok(())
}

// ---- incidents ---------------------------------------------------------------------

/// Open an incident, unless one is already open.
///
/// One open incident per monitor, whatever went wrong second. A job that errors, then
/// starts missing, then times out is one thing being broken — and a phone that buzzes
/// three times for it is a phone that gets muted before the fourth.
fn open_incident(
    tx: &Connection,
    monitor: &Monitor,
    kind: &str,
    detail: Option<&str>,
    at: &str,
) -> rusqlite::Result<Option<News>> {
    if open_incident_id(tx, monitor.id)?.is_some() {
        return Ok(None);
    }
    tx.execute(
        "INSERT INTO bugs_incidents (monitor_id, kind, detail, opened_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![monitor.id, kind, detail, at],
    )?;
    Ok(Some(News::Opened {
        id: tx.last_insert_rowid(),
        kind: kind.to_owned(),
        detail: detail.map(str::to_owned),
    }))
}

/// Close the open incident. An `ok` after a failure is the other half of the only pair
/// of notifications this feature sends.
fn resolve_incident(
    tx: &Connection,
    monitor: &Monitor,
    at: &str,
) -> rusqlite::Result<Option<News>> {
    let Some(id) = open_incident_id(tx, monitor.id)? else { return Ok(None) };
    tx.execute("UPDATE bugs_incidents SET resolved_at = ?2 WHERE id = ?1", params![id, at])?;
    Ok(Some(News::Recovered { id }))
}

fn open_incident_id(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM bugs_incidents
         WHERE monitor_id = ?1 AND resolved_at IS NULL ORDER BY id DESC LIMIT 1",
        params![monitor_id],
        |row| row.get(0),
    )
    .optional()
}

/// Drop check-in rows past each project's `retention_days`.
///
/// A monitor that checks in every minute files half a million rows a year, and nothing
/// else here would ever remove one. Incidents stay — there are few of them and they are
/// the history a person actually reads — and so does every monitor.
pub fn prune(db: &Db) -> DbResult<usize> {
    let projects: Vec<(i64, i64)> = db.with(|conn| {
        let mut statement = conn.prepare("SELECT id, retention_days FROM bugs_projects")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    })?;

    let mut deleted = 0;
    for (project_id, days) in projects {
        if days <= 0 {
            continue;
        }
        let Some(cutoff) = crate::bugs::logs::days_ago(days) else { continue };
        deleted += db.with(|conn| {
            conn.execute(
                "DELETE FROM bugs_checkins WHERE project_id = ?1 AND received_at < ?2",
                params![project_id, cutoff],
            )
        })?;
    }
    Ok(deleted)
}

/// A stored timestamp as a Unix second, for the arithmetic. Storage stays in the
/// canonical text format the whole tree compares lexicographically; only the schedule
/// maths needs a number, and only for as long as it takes to produce the next one.
fn seconds_of(stamp: &str) -> i64 {
    match time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339) {
        Ok(value) => value.unix_timestamp(),
        Err(error) => {
            // Substituting now keeps the caller moving, which is right, but it silently
            // computes a deadline from the wrong instant — so it is worth a line.
            tracing::warn!(%error, stamp, "bugs: a stored timestamp is not one; reading it as now");
            time::OffsetDateTime::now_utc().unix_timestamp()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bed() -> (Db, Project) {
        let db = Db::in_memory().unwrap();
        crate::bugs::index::migrate(&db).unwrap();
        crate::bugs::pushover::migrate(&db).unwrap();
        migrate(&db).unwrap();
        let project =
            crate::bugs::index::create_project(&db, "demo", &"a".repeat(32), None).unwrap();
        (db, project)
    }

    fn item(slug: &str, status: &str, config: Option<Value>) -> Value {
        let mut payload = json!({
            "check_in_id": format!("{:032x}", 1),
            "monitor_slug": slug,
            "status": status,
        });
        if let Some(config) = config {
            payload["monitor_config"] = config;
        }
        payload
    }

    fn crontab(pattern: &str) -> Value {
        json!({"schedule": {"type": "crontab", "value": pattern}})
    }

    #[test]
    fn only_the_three_statuses_a_client_can_know_survive_the_door() {
        assert_eq!(coerce("ok"), ("ok".to_owned(), false));
        assert_eq!(coerce("IN_PROGRESS"), ("in_progress".to_owned(), false));
        assert_eq!(coerce("error"), ("error".to_owned(), false));
        // The two the server is the only judge of, plus anything unrecognised.
        for claimed in ["missed", "timeout", "unknown", "", "sideways"] {
            assert_eq!(coerce(claimed), ("error".to_owned(), true), "{claimed} was believed");
        }
    }

    #[test]
    fn a_schedule_that_cannot_be_computed_is_not_a_schedule() {
        assert!(Schedule::parse(&json!({"type": "crontab", "value": "0 * * * *"})).is_some());
        // Sentry's bare-string shorthand.
        assert!(Schedule::parse(&json!("*/5 * * * *")).is_some());
        assert!(Schedule::parse(&json!({"type": "interval", "value": 5, "unit": "minute"})).is_some());
        // A plural unit is what several SDKs send.
        assert!(Schedule::parse(&json!({"type": "interval", "value": 2, "unit": "hours"})).is_some());

        for bad in [
            json!({"type": "crontab", "value": "not a cron"}),
            json!({"type": "interval", "value": 0, "unit": "minute"}),
            json!({"type": "interval", "value": 5, "unit": "fortnight"}),
            json!({"type": "interval", "value": 5}),
            json!({"type": "sundial", "value": "noon"}),
            json!({}),
        ] {
            assert!(Schedule::parse(&bad).is_none(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_config_keeps_its_schedule_when_the_rest_of_it_is_missing() {
        let config = MonitorConfig::parse(&crontab("0 * * * *")).expect("a config");
        assert_eq!(config.checkin_margin, DEFAULT_CHECKIN_MARGIN);
        assert_eq!(config.max_runtime, DEFAULT_MAX_RUNTIME);
        assert_eq!(config.timezone, None);

        let stated = MonitorConfig::parse(&json!({
            "schedule": {"type": "crontab", "value": "0 * * * *"},
            "checkin_margin": 5,
            "max_runtime": 10,
            "timezone": "America/Chicago",
        }))
        .expect("a config");
        assert_eq!(stated.checkin_margin, 5);
        assert_eq!(stated.max_runtime, 10);
        assert_eq!(stated.timezone.as_deref(), Some("America/Chicago"));

        // A zone this box cannot resolve is dropped rather than stored and ignored.
        let bogus = MonitorConfig::parse(&json!({
            "schedule": "0 * * * *",
            "timezone": "Mars/Olympus",
        }))
        .expect("a config");
        assert_eq!(bogus.timezone, None);
    }

    #[test]
    fn a_schedule_fires_where_its_timezone_says_it_does() {
        // 2026-08-20T00:00:00Z. Nine in Chicago that day is 14:00 UTC (CDT, UTC-5).
        let midnight = 1_787_184_000;
        let daily = Schedule { kind: CRONTAB.to_owned(), value: "0 9 * * *".to_owned(), unit: None };
        let utc = next_after(&daily, None, midnight).expect("a next time");
        let chicago = next_after(&daily, Some("America/Chicago"), midnight).expect("a next time");
        assert_eq!(utc - midnight, 9 * 3600);
        assert_eq!(chicago - midnight, 14 * 3600, "nine in Chicago is not nine in UTC");
    }

    #[test]
    fn an_interval_steps_through_the_calendar_not_through_a_pile_of_seconds() {
        let at = 1_787_184_000; // 2026-08-20T00:00:00Z
        let every = |value: i64, unit: &str| Schedule {
            kind: INTERVAL.to_owned(),
            value: value.to_string(),
            unit: Some(unit.to_owned()),
        };
        assert_eq!(next_after(&every(5, "minute"), None, at).unwrap() - at, 300);
        assert_eq!(next_after(&every(2, "hour"), None, at).unwrap() - at, 7200);
        assert_eq!(next_after(&every(1, "day"), None, at).unwrap() - at, 86_400);
        assert_eq!(next_after(&every(1, "week"), None, at).unwrap() - at, 7 * 86_400);
        // August has 31 days, so a month is not 30 of them.
        assert_eq!(next_after(&every(1, "month"), None, at).unwrap() - at, 31 * 86_400);
    }

    #[test]
    fn a_checkin_with_no_config_stores_and_declares_no_monitor() {
        let (db, project) = bed();
        let check_in = CheckIn::parse(&item("nightly", "ok", None)).expect("a check-in");
        assert!(record(&db, &project, &check_in, &Notifier::off()).unwrap().is_none());

        assert!(monitors(&db, project.id).unwrap().is_empty(), "no schedule, no monitor");
        let stored = checkins(&db, project.id, 10).unwrap();
        assert_eq!(stored.len(), 1, "the check-in is evidence either way");
        assert_eq!(stored[0].slug, "nightly");
        assert_eq!(stored[0].status, status::OK);
    }

    #[test]
    fn a_config_declares_the_monitor_and_a_later_config_moves_it() {
        let (db, project) = bed();
        let first = CheckIn::parse(&item("nightly", "in_progress", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        let monitor = record(&db, &project, &first, &Notifier::off()).unwrap().expect("a monitor");
        assert_eq!(monitor.schedule, "0 2 * * *");
        assert_eq!(monitor.status, status::IN_PROGRESS);
        assert!(monitor.timeout_at.is_some(), "a run that started has a deadline to finish by");
        assert!(monitor.next_checkin_latest.is_some());

        let mut moved = item("nightly", "ok", Some(crontab("*/15 * * * *")));
        moved["check_in_id"] = json!(format!("{:032x}", 2));
        let moved = CheckIn::parse(&moved).expect("a check-in");
        let monitor = record(&db, &project, &moved, &Notifier::off()).unwrap().expect("a monitor");
        assert_eq!(monitor.schedule, "*/15 * * * *", "the code that runs the job owns the schedule");
        assert_eq!(monitor.status, status::OK);
        assert!(monitor.timeout_at.is_none(), "nothing is running");
        assert_eq!(monitors(&db, project.id).unwrap().len(), 1, "one slug is one monitor");
    }

    #[test]
    fn the_same_checkin_twice_moves_nothing() {
        let (db, project) = bed();
        let start = CheckIn::parse(&item("nightly", "in_progress", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        record(&db, &project, &start, &Notifier::off()).unwrap();

        let mut done = item("nightly", "ok", None);
        done["check_in_id"] = json!(format!("{:032x}", 1));
        let done = CheckIn::parse(&done).expect("a check-in");
        record(&db, &project, &done, &Notifier::off()).unwrap();
        // The replay: same id, same status, and the monitor must not move back.
        record(&db, &project, &done, &Notifier::off()).unwrap();

        assert_eq!(checkins(&db, project.id, 10).unwrap().len(), 2, "in_progress and ok");
        assert_eq!(
            monitor_by_slug(&db, project.id, "nightly").unwrap().expect("a monitor").status,
            status::OK
        );
    }

    #[test]
    fn the_sweep_is_the_only_thing_that_says_missed_or_timed_out() {
        let (db, project) = bed();
        // A client claiming `missed` is not believed; it lands as an error.
        let claimed = CheckIn::parse(&item("nightly", "missed", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        assert_eq!(claimed.status, status::ERROR);
        assert!(claimed.coerced);
        let monitor =
            record(&db, &project, &claimed, &Notifier::off()).unwrap().expect("a monitor");
        assert_eq!(monitor.status, status::ERROR);

        // Nothing is due yet.
        let quiet = sweep_at(&db, &Notifier::off(), seconds_of(&now())).unwrap();
        assert_eq!(quiet, Swept::default());

        // A week later the daily check-in plainly never came.
        let later = seconds_of(&now()) + 7 * 86_400;
        let swept = sweep_at(&db, &Notifier::off(), later).unwrap();
        assert_eq!(swept.missed, 1);
        let monitor = monitor_by_slug(&db, project.id, "nightly").unwrap().expect("a monitor");
        assert_eq!(monitor.status, status::MISSED);

        // And the same lateness is not filed twice: the deadline moved past `later`.
        assert_eq!(sweep_at(&db, &Notifier::off(), later).unwrap(), Swept::default());
    }

    #[test]
    fn a_run_that_never_finishes_times_out_rather_than_going_missing() {
        let (db, project) = bed();
        let config = json!({
            "schedule": {"type": "crontab", "value": "*/5 * * * *"},
            "max_runtime": 10,
        });
        let start = CheckIn::parse(&item("import", "in_progress", Some(config)))
            .expect("a check-in");
        record(&db, &project, &start, &Notifier::off()).unwrap();

        // Eleven minutes on, the run has overrun its ten.
        let later = seconds_of(&now()) + 11 * 60;
        let swept = sweep_at(&db, &Notifier::off(), later).unwrap();
        assert_eq!(swept, Swept { missed: 0, timed_out: 1 }, "a run that started is not missing");
        let monitor = monitor_by_slug(&db, project.id, "import").unwrap().expect("a monitor");
        assert_eq!(monitor.status, status::TIMEOUT);
        assert!(monitor.timeout_at.is_none());
    }

    #[test]
    fn one_broken_monitor_is_one_open_incident_and_an_ok_closes_it() {
        let (db, project) = bed();
        let failed = CheckIn::parse(&item("nightly", "error", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        record(&db, &project, &failed, &Notifier::off()).unwrap();
        assert_eq!(incidents(&db, project.id, 10).unwrap().len(), 1);

        // It goes missing on top of being broken. Still one thing being broken.
        let later = seconds_of(&now()) + 7 * 86_400;
        sweep_at(&db, &Notifier::off(), later).unwrap();
        let open: Vec<_> = incidents(&db, project.id, 10)
            .unwrap()
            .into_iter()
            .filter(|incident| incident.resolved_at.is_none())
            .collect();
        assert_eq!(open.len(), 1, "one monitor, one open incident");
        assert_eq!(open[0].kind, status::ERROR, "the first thing that broke names it");

        let mut fixed = item("nightly", "ok", None);
        fixed["check_in_id"] = json!(format!("{:032x}", 9));
        let fixed = CheckIn::parse(&fixed).expect("a check-in");
        record(&db, &project, &fixed, &Notifier::off()).unwrap();
        assert!(
            incidents(&db, project.id, 10).unwrap().iter().all(|i| i.resolved_at.is_some()),
            "an ok closes the book"
        );
    }

    #[test]
    fn a_checkin_with_no_slug_is_not_a_checkin() {
        assert!(CheckIn::parse(&json!({"status": "ok"})).is_none());
        assert!(CheckIn::parse(&json!({"monitor_slug": "  ", "status": "ok"})).is_none());
        // A missing id gets one rather than a refusal.
        let minted = CheckIn::parse(&json!({"monitor_slug": "x", "status": "ok"}))
            .expect("a check-in");
        assert_eq!(minted.check_in_id.len(), 32);
    }

    #[test]
    fn a_checkin_row_whose_monitor_never_absorbed_it_is_not_left_wedged() {
        let (db, project) = bed();
        // Declare the monitor, then file a check-in row *without* applying it — which
        // is exactly what a crash between the two writes used to leave behind, back
        // when they were two writes. The monitor is then stuck: status `unknown`, no
        // deadline, and both sweep predicates skip a NULL deadline, so it can never go
        // missing and can never recover.
        let declare = CheckIn::parse(&item("nightly", "ok", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        upsert_and_forget(&db, &project, &declare);

        let monitor = monitor_by_slug(&db, project.id, "nightly").unwrap().expect("a monitor");
        assert_eq!(monitor.status, status::UNKNOWN);
        assert!(monitor.next_checkin_latest.is_none(), "wedged: no deadline to be late against");
        assert_eq!(sweep_at(&db, &Notifier::off(), seconds_of(&now()) + 30 * 86_400).unwrap(),
            Swept::default(), "and unreachable by the sweep");

        // Redelivery. The row is already there, so `INSERT OR IGNORE` changes nothing —
        // but the monitor still has not absorbed it, and that is what must be noticed.
        let monitor = record(&db, &project, &declare, &Notifier::off()).unwrap().expect("a monitor");
        assert_eq!(monitor.status, status::OK);
        assert!(monitor.next_checkin_latest.is_some(), "the deadline is set from the stored row");

        // One check-in row still, not two.
        assert_eq!(checkins(&db, project.id, 10).unwrap().len(), 1);

        // And now that it has a deadline it is reachable again.
        let later = seconds_of(&now()) + 30 * 86_400;
        assert_eq!(sweep_at(&db, &Notifier::off(), later).unwrap().missed, 1);
    }

    /// Declare the monitor and file the check-in row, and stop there — the half-written
    /// state the old two-write `record` could crash into.
    fn upsert_and_forget(db: &Db, project: &Project, check_in: &CheckIn) {
        let at = now();
        db.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            upsert_monitor(&tx, project.id, &check_in.slug, check_in.config.as_ref().unwrap(), &at)?;
            let monitor = monitor_row(&tx, project.id, &check_in.slug)?.expect("a monitor");
            insert_checkin(&tx, project.id, Some(monitor.id), check_in, &at)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_replay_of_a_checkin_the_monitor_did_absorb_still_moves_nothing() {
        let (db, project) = bed();
        let failed = CheckIn::parse(&item("nightly", "error", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        record(&db, &project, &failed, &Notifier::off()).unwrap();
        assert_eq!(incidents(&db, project.id, 10).unwrap().len(), 1);

        // Close it, the way an `ok` would.
        let mut fixed = item("nightly", "ok", None);
        fixed["check_in_id"] = json!(format!("{:032x}", 7));
        let fixed = CheckIn::parse(&fixed).expect("a check-in");
        record(&db, &project, &fixed, &Notifier::off()).unwrap();
        assert!(incidents(&db, project.id, 10).unwrap().iter().all(|i| i.resolved_at.is_some()));

        // Replay the failure. It is old news and must not reopen anything.
        record(&db, &project, &failed, &Notifier::off()).unwrap();
        assert!(
            incidents(&db, project.id, 10).unwrap().iter().all(|i| i.resolved_at.is_some()),
            "a replayed check-in reopened an incident that was closed"
        );
        assert_eq!(incidents(&db, project.id, 10).unwrap().len(), 1, "and opened no new one");
    }

    #[test]
    fn a_schedule_that_parses_but_never_fires_is_refused() {
        // The thirtieth of February. croner parses it happily and it never occurs, so a
        // monitor built on it would have no computable deadline and would sit on the
        // page saying "ok" for ever while covering nothing.
        assert!(Schedule::parse(&json!({"type": "crontab", "value": "0 0 30 2 *"})).is_none());
        assert!(Schedule::parse(&json!("0 0 31 2 *")).is_none());
        // A real leap-day schedule does fire, so it stays.
        assert!(Schedule::parse(&json!({"type": "crontab", "value": "0 0 29 2 *"})).is_some());
    }

    #[test]
    fn check_ins_are_pruned_past_retention_and_incidents_are_not() {
        let (db, project) = bed();
        let check_in = CheckIn::parse(&item("nightly", "error", Some(crontab("0 2 * * *"))))
            .expect("a check-in");
        record(&db, &project, &check_in, &Notifier::off()).unwrap();
        assert_eq!(checkins(&db, project.id, 10).unwrap().len(), 1);

        // Nothing is old yet.
        assert_eq!(prune(&db).unwrap(), 0);

        // Age the row past the project's window. A monitor checking in every minute
        // files half a million rows a year, and nothing else would ever remove one.
        db.with(|conn| {
            conn.execute(
                "UPDATE bugs_checkins SET received_at = '2020-01-01T00:00:00.000000Z'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(prune(&db).unwrap(), 1);
        assert!(checkins(&db, project.id, 10).unwrap().is_empty());
        assert_eq!(
            incidents(&db, project.id, 10).unwrap().len(),
            1,
            "incidents are the history a person reads; they stay"
        );
        assert!(monitor_by_slug(&db, project.id, "nightly").unwrap().is_some());
    }

    #[test]
    fn the_all_zero_id_is_a_sentinel_and_never_becomes_an_identity() {
        assert!(is_sentinel(&"0".repeat(32)));
        assert!(!is_sentinel(&format!("{:032x}", 1)));
        assert!(!is_sentinel(""));

        // Two runs, both sending the sentinel. Both have to land: keying on it would
        // make every run after the first a no-op.
        let (db, project) = bed();
        let mut first = item("nightly", "ok", Some(crontab("0 2 * * *")));
        first["check_in_id"] = json!("0".repeat(32));
        let second = first.clone();
        record(&db, &project, &CheckIn::parse(&first).unwrap(), &Notifier::off()).unwrap();
        record(&db, &project, &CheckIn::parse(&second).unwrap(), &Notifier::off()).unwrap();
        assert_eq!(checkins(&db, project.id, 10).unwrap().len(), 2);
    }
}
