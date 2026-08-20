//! The SQLite index: projects, issues, and event metadata.
//!
//! Nothing here is a payload. Raw envelopes and raw event JSON live in the bucket;
//! these rows only say what exists, how it groups, and where to read it. Losing this
//! database loses no data — `nashcode bugs reindex` rebuilds it from the bucket.
//!
//! The tables live beside the viewer's own in the same file but are owned by this
//! module rather than by `db.rs`, so the feature carries its own schema. `Db::with`
//! is the shared connection; `execute_batch` on open is the same migration pattern
//! `db.rs` uses.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::db::{Db, DbResult, now};

/// Issue lifecycle. `muted` is stored from day one so the schema does not move when
/// the mute rules land.
pub mod state {
    pub const UNRESOLVED: &str = "unresolved";
    pub const RESOLVED: &str = "resolved";
    pub const MUTED: &str = "muted";

    pub fn known(value: &str) -> bool {
        matches!(value, UNRESOLVED | RESOLVED | MUTED)
    }
}

/// A project, which is one DSN.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub name: String,
    /// The 32-hex public key half of the DSN.
    pub key: String,
    /// An optional nashcode repo, for cross-links.
    pub repo: Option<String>,
    pub created_at: String,
    /// How long log rows stay in the hot window. The bucket archive is forever.
    pub retention_days: i64,
    /// Does this project still authenticate? A revoked project stays here so its
    /// issues stay readable, and goes to the public ingester's registry as
    /// `active:false`, which that edge reads as "absent".
    pub active: bool,
}

/// A project plus the counts the list page shows.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectSummary {
    #[serde(flatten)]
    pub project: Project,
    pub unresolved: i64,
    pub issues: i64,
    pub events: i64,
}

/// One issue: a group of events that share a grouping key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Issue {
    pub id: i64,
    pub project_id: i64,
    /// The readable grouping key, as computed. Stored so a human can see why two
    /// events landed together.
    pub grouping_key: String,
    /// The same key, hashed, which is what the unique index is on.
    pub grouping_hash: String,
    /// The grouping mechanism that produced the key (`nashcode-v1`). Stored per issue
    /// so changing the algorithm never silently splits open issues.
    pub mechanism: String,
    pub title: String,
    pub culprit: Option<String>,
    pub level: String,
    pub state: String,
    /// True once the issue has come back after being resolved.
    pub regression: bool,
    pub events: i64,
    pub first_seen: String,
    pub last_seen: String,
    pub actor: Option<String>,
    pub acted_at: Option<String>,
}

/// Event metadata. The payload is the bucket object at `object_key`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventRow {
    pub id: i64,
    pub issue_id: i64,
    pub event_id: String,
    pub object_key: String,
    pub level: Option<String>,
    pub platform: Option<String>,
    /// The client's timestamp, verbatim, in whichever form it sent.
    pub timestamp: Option<String>,
    pub received_at: String,
    /// The first event of the issue and every regression trigger. Eviction never
    /// removes these.
    pub keep: bool,
}

/// What the digest hands the index for one event.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub project_id: i64,
    pub event_id: String,
    pub object_key: String,
    pub grouping_key: String,
    pub grouping_hash: String,
    pub mechanism: String,
    pub title: String,
    pub culprit: Option<String>,
    pub level: String,
    pub platform: Option<String>,
    pub timestamp: Option<String>,
}

/// What indexing one event did to its issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// The issue did not exist.
    New,
    /// The issue was resolved and this event reopened it.
    Regression,
    /// One more event on an issue that was already open (or muted).
    Repeat,
    /// The event id was already indexed; nothing changed.
    Duplicate,
}

/// How long a project keeps log rows in the hot window, unless it says otherwise.
/// Thirty days is a month of "what happened last time"; the NDJSON archive in the
/// bucket keeps everything anyway, so this only decides what search is fast over.
pub const DEFAULT_RETENTION_DAYS: i64 = 30;

/// The same number as SQL sees it. A const cannot be formatted without pulling in a
/// crate for it, so the two are pinned by a test instead — they cannot drift without
/// the suite going red, which is the property that was wanted.
const RETENTION_COLUMN: &str = "INTEGER NOT NULL DEFAULT 30";

/// Apply the bugs schema. Idempotent, run on every open.
pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute_batch(SCHEMA)?;
        add_columns(conn, ADDED_COLUMNS)
    })
}

// ---- projects --------------------------------------------------------------------

/// Create a project with a freshly minted key. The name is the URL segment, so it is
/// validated by the caller ([`crate::bugs::valid_project_name`]).
pub fn create_project(db: &Db, name: &str, key: &str, repo: Option<&str>) -> DbResult<Project> {
    let created_at = now();
    db.with(|conn| {
        conn.execute(
            "INSERT INTO bugs_projects (name, key, repo, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, key, repo, created_at],
        )?;
        Ok(Project {
            id: conn.last_insert_rowid(),
            name: name.to_owned(),
            key: key.to_owned(),
            repo: repo.map(str::to_owned),
            created_at,
            retention_days: DEFAULT_RETENTION_DAYS,
            active: true,
        })
    })
}

pub fn project_by_name(db: &Db, name: &str) -> DbResult<Option<Project>> {
    db.with(|conn| {
        conn.query_row(
            "SELECT id, name, key, repo, created_at, retention_days, active
             FROM bugs_projects WHERE name = ?1",
            params![name],
            read_project,
        )
        .optional()
    })
}

pub fn project_by_id(db: &Db, id: i64) -> DbResult<Option<Project>> {
    db.with(|conn| {
        conn.query_row(
            "SELECT id, name, key, repo, created_at, retention_days, active
             FROM bugs_projects WHERE id = ?1",
            params![id],
            read_project,
        )
        .optional()
    })
}

/// Every project with its open-issue and event counts, newest first.
pub fn projects(db: &Db) -> DbResult<Vec<ProjectSummary>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT p.id, p.name, p.key, p.repo, p.created_at, p.retention_days, p.active,
                    COALESCE(SUM(i.state = 'unresolved'), 0),
                    COUNT(i.id),
                    COALESCE(SUM(i.events), 0)
             FROM bugs_projects p
             LEFT JOIN bugs_issues i ON i.project_id = p.id
             GROUP BY p.id
             ORDER BY p.created_at DESC, p.id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ProjectSummary {
                project: read_project(row)?,
                unresolved: row.get(7)?,
                issues: row.get(8)?,
                events: row.get(9)?,
            })
        })?;
        rows.collect()
    })
}

/// Revoke a project's key, or give it back. The rows stay: an issue that has already
/// been filed is history, and history does not stop being true when a DSN is retired.
pub fn set_project_active(db: &Db, id: i64, active: bool) -> DbResult<bool> {
    db.with(|conn| {
        let changed = conn.execute(
            "UPDATE bugs_projects SET active = ?2 WHERE id = ?1",
            params![id, active],
        )?;
        Ok(changed > 0)
    })
}

/// One project as `/brain` wants it: enough to answer "is anything on fire", and
/// nothing that costs a second query.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrainProject {
    pub name: String,
    /// The nashcode repo this project declares, when it declares one.
    pub repo: Option<String>,
    pub active: bool,
    pub unresolved: i64,
    pub issues: i64,
    pub events: i64,
    /// When the newest issue of this project last moved. `None` for a project that has
    /// never been sent anything, which is a different thing from a quiet one.
    pub last_event_at: Option<String>,
}

/// Every project, or every project declaring `repo`, with the counts `/brain` shows.
pub fn brain_projects(db: &Db, repo: Option<&str>) -> DbResult<Vec<BrainProject>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT p.name, p.repo, p.active,
                    COALESCE(SUM(i.state = 'unresolved'), 0),
                    COUNT(i.id),
                    COALESCE(SUM(i.events), 0),
                    MAX(i.last_seen)
             FROM bugs_projects p
             LEFT JOIN bugs_issues i ON i.project_id = p.id
             WHERE (?1 IS NULL OR p.repo = ?1)
             GROUP BY p.id
             ORDER BY p.name",
        )?;
        let rows = statement.query_map(params![repo], |row| {
            Ok(BrainProject {
                name: row.get(0)?,
                repo: row.get(1)?,
                active: row.get(2)?,
                unresolved: row.get(3)?,
                issues: row.get(4)?,
                events: row.get(5)?,
                last_event_at: row.get(6)?,
            })
        })?;
        rows.collect()
    })
}

/// Every project as the public ingester's registry wants it: id, key, and whether the
/// key still opens the door. Deliberately lean — the registry is pushed on a timer and
/// has no use for issue counts.
pub fn registry(db: &Db) -> DbResult<Vec<(i64, String, bool)>> {
    db.with(|conn| {
        let mut statement =
            conn.prepare("SELECT id, key, active FROM bugs_projects ORDER BY id")?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    })
}

fn read_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        key: row.get(2)?,
        repo: row.get(3)?,
        created_at: row.get(4)?,
        retention_days: row.get(5)?,
        active: row.get(6)?,
    })
}

// ---- issues ----------------------------------------------------------------------

/// Index one event: upsert its issue, then record the event. One transaction, one
/// writer — the digest task is the only caller.
pub fn record(db: &Db, new: &NewEvent) -> DbResult<(Issue, Landing)> {
    let received_at = now();
    db.with(|conn| {
        let tx = conn.unchecked_transaction()?;

        // The same event id twice is a client retry, not a second occurrence.
        let seen: Option<i64> = tx
            .query_row(
                "SELECT issue_id FROM bugs_events WHERE project_id = ?1 AND event_id = ?2",
                params![new.project_id, new.event_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(issue_id) = seen {
            let issue = read_issue_row(&tx, issue_id)?;
            tx.commit()?;
            return Ok((issue, Landing::Duplicate));
        }

        // An event this project has already had and already thrown away. The row is
        // gone, so the check above cannot see it — but the payload is still in an
        // envelope object, and a reindex re-reads those. Without the tombstone this
        // lands as an ordinary repeat, writes the row back, and moves the issue's
        // lifetime counter a second time; that counter only ever goes up and the
        // escalation ladder reads it, so the inflation would be permanent and exactly
        // the size of the eviction. See `bugs::evict`.
        let evicted: Option<i64> = tx
            .query_row(
                "SELECT issue_id FROM bugs_evicted_events
                 WHERE project_id = ?1 AND event_id = ?2",
                params![new.project_id, new.event_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(issue_id) = evicted {
            let issue = read_issue_row(&tx, issue_id)?;
            tx.commit()?;
            return Ok((issue, Landing::Duplicate));
        }

        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, state FROM bugs_issues WHERE project_id = ?1 AND grouping_hash = ?2",
                params![new.project_id, new.grouping_hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let (issue_id, landing) = match existing {
            None => {
                tx.execute(
                    "INSERT INTO bugs_issues
                       (project_id, grouping_key, grouping_hash, mechanism, title, culprit,
                        level, state, regression, events, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'unresolved', 0, 1, ?8, ?8)",
                    params![
                        new.project_id,
                        new.grouping_key,
                        new.grouping_hash,
                        new.mechanism,
                        new.title,
                        new.culprit,
                        new.level,
                        received_at,
                    ],
                )?;
                (tx.last_insert_rowid(), Landing::New)
            }
            Some((id, state)) if state == state::RESOLVED => {
                // Any event on a resolved issue reopens it, flagged as a regression.
                tx.execute(
                    "UPDATE bugs_issues
                     SET state = 'unresolved', regression = 1, events = events + 1,
                         last_seen = ?2, level = ?3, title = ?4, actor = NULL, acted_at = NULL
                     WHERE id = ?1",
                    params![id, received_at, new.level, new.title],
                )?;
                (id, Landing::Regression)
            }
            Some((id, _)) => {
                tx.execute(
                    "UPDATE bugs_issues
                     SET events = events + 1, last_seen = ?2, level = ?3
                     WHERE id = ?1",
                    params![id, received_at, new.level],
                )?;
                (id, Landing::Repeat)
            }
        };

        // First-seen and regression-trigger events are the two an eviction pass must
        // never take, so the flag is set here rather than inferred later.
        let keep = matches!(landing, Landing::New | Landing::Regression);

        // How crowded the issue already is decides how relevant this event will look to
        // a later eviction pass. Computed here because here is the one place holding
        // both the issue and the transaction; the *stored* count is what counts, not the
        // issue's lifetime total, since a thinned issue has genuinely made room.
        let stored_in_issue: i64 = tx.query_row(
            "SELECT COUNT(*) FROM bugs_events WHERE issue_id = ?1",
            params![issue_id],
            |row| row.get(0),
        )?;
        let irrelevance =
            crate::bugs::evict::item_irrelevance(&new.event_id, stored_in_issue + 1);

        tx.execute(
            "INSERT INTO bugs_events
               (project_id, issue_id, event_id, object_key, level, platform, timestamp,
                received_at, keep, irrelevance)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                new.project_id,
                issue_id,
                new.event_id,
                new.object_key,
                new.level,
                new.platform,
                new.timestamp,
                received_at,
                keep as i64,
                irrelevance,
            ],
        )?;

        let issue = read_issue_row(&tx, issue_id)?;
        tx.commit()?;
        Ok((issue, landing))
    })
}

/// Issues of a project, newest activity first. `state` filters when it is known.
pub fn issues(db: &Db, project_id: i64, state: Option<&str>) -> DbResult<Vec<Issue>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, project_id, grouping_key, grouping_hash, mechanism, title, culprit,
                    level, state, regression, events, first_seen, last_seen, actor, acted_at
             FROM bugs_issues
             WHERE project_id = ?1 AND (?2 IS NULL OR state = ?2)
             ORDER BY last_seen DESC, id DESC",
        )?;
        let rows = statement.query_map(params![project_id, state], read_issue)?;
        rows.collect()
    })
}

pub fn issue(db: &Db, project_id: i64, id: i64) -> DbResult<Option<Issue>> {
    db.with(|conn| {
        conn.query_row(
            "SELECT id, project_id, grouping_key, grouping_hash, mechanism, title, culprit,
                    level, state, regression, events, first_seen, last_seen, actor, acted_at
             FROM bugs_issues WHERE project_id = ?1 AND id = ?2",
            params![project_id, id],
            read_issue,
        )
        .optional()
    })
}

/// Move an issue to `state`, stamped with who asked. Returns the stored issue.
pub fn set_state(db: &Db, project_id: i64, id: i64, state: &str, actor: &str) -> DbResult<Option<Issue>> {
    let acted_at = now();
    db.with(|conn| {
        // Resolving closes the book on the regression too. Leaving the flag set would
        // paint the issue "regression" forever, including the next time it is opened
        // fresh, and the flag is what tells a reader "this came back after we said it
        // was fixed".
        //
        // Every move clears the mute rule, including a move *to* muted: the caller arms
        // the new rule immediately after, and starting from nothing is what stops a
        // re-mute from inheriting a half-counted window off the last one. It is also
        // what makes an unmute final — a rule that survived it would fire again.
        let changed = conn.execute(
            "UPDATE bugs_issues
             SET state = ?3, actor = ?4, acted_at = ?5,
                 regression = CASE WHEN ?3 = 'resolved' THEN 0 ELSE regression END,
                 mute_rule = NULL, mute_until = NULL, mute_count = NULL,
                 mute_window = NULL, mute_from = NULL, mute_events = 0
             WHERE project_id = ?1 AND id = ?2",
            params![project_id, id, state, actor, acted_at],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        read_issue_row(conn, id).map(Some)
    })
}

/// The events of an issue, newest first.
pub fn events(db: &Db, issue_id: i64, limit: i64) -> DbResult<Vec<EventRow>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, issue_id, event_id, object_key, level, platform, timestamp,
                    received_at, keep
             FROM bugs_events WHERE issue_id = ?1
             ORDER BY received_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![issue_id, limit], |row| {
            Ok(EventRow {
                id: row.get(0)?,
                issue_id: row.get(1)?,
                event_id: row.get(2)?,
                object_key: row.get(3)?,
                level: row.get(4)?,
                platform: row.get(5)?,
                timestamp: row.get(6)?,
                received_at: row.get(7)?,
                keep: row.get::<_, i64>(8)? != 0,
            })
        })?;
        rows.collect()
    })
}

/// The newest event of an issue, which is what the detail page renders.
pub fn latest_event(db: &Db, issue_id: i64) -> DbResult<Option<EventRow>> {
    Ok(events(db, issue_id, 1)?.into_iter().next())
}

fn read_issue_row(conn: &Connection, id: i64) -> rusqlite::Result<Issue> {
    conn.query_row(
        "SELECT id, project_id, grouping_key, grouping_hash, mechanism, title, culprit,
                level, state, regression, events, first_seen, last_seen, actor, acted_at
         FROM bugs_issues WHERE id = ?1",
        params![id],
        read_issue,
    )
}

fn read_issue(row: &rusqlite::Row<'_>) -> rusqlite::Result<Issue> {
    Ok(Issue {
        id: row.get(0)?,
        project_id: row.get(1)?,
        grouping_key: row.get(2)?,
        grouping_hash: row.get(3)?,
        mechanism: row.get(4)?,
        title: row.get(5)?,
        culprit: row.get(6)?,
        level: row.get(7)?,
        state: row.get(8)?,
        regression: row.get::<_, i64>(9)? != 0,
        events: row.get(10)?,
        first_seen: row.get(11)?,
        last_seen: row.get(12)?,
        actor: row.get(13)?,
        acted_at: row.get(14)?,
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bugs_projects (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    key        TEXT NOT NULL UNIQUE,
    repo       TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS bugs_issues (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id    INTEGER NOT NULL REFERENCES bugs_projects(id),
    grouping_key  TEXT NOT NULL,
    grouping_hash TEXT NOT NULL,
    mechanism     TEXT NOT NULL,
    title         TEXT NOT NULL,
    culprit       TEXT,
    level         TEXT NOT NULL,
    state         TEXT NOT NULL,
    regression    INTEGER NOT NULL DEFAULT 0,
    events        INTEGER NOT NULL DEFAULT 0,
    first_seen    TEXT NOT NULL,
    last_seen     TEXT NOT NULL,
    actor         TEXT,
    acted_at      TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS bugs_issues_group
    ON bugs_issues (project_id, grouping_hash);
CREATE INDEX IF NOT EXISTS bugs_issues_state
    ON bugs_issues (project_id, state, last_seen);

CREATE TABLE IF NOT EXISTS bugs_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES bugs_projects(id),
    issue_id    INTEGER NOT NULL REFERENCES bugs_issues(id),
    event_id    TEXT NOT NULL,
    object_key  TEXT NOT NULL,
    level       TEXT,
    platform    TEXT,
    timestamp   TEXT,
    received_at TEXT NOT NULL,
    keep        INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS bugs_events_unique
    ON bugs_events (project_id, event_id);
CREATE INDEX IF NOT EXISTS bugs_events_by_issue
    ON bugs_events (issue_id, received_at);

-- The eviction tombstones. One row per evicted event, and they are what make eviction
-- stick.
--
-- Without them a reindex undoes the whole pass. `Bugs::sweep(true)` re-reads
-- `bugs_envelopes`, not `bugs_events`, so every evicted event is still inside an
-- envelope object in the bucket; re-digesting it finds no `(project_id, event_id)` row,
-- treats it as an ordinary repeat, writes the row back and increments the issue's
-- lifetime counter a second time. That counter only ever goes up and the escalation
-- ladder reads it, so the damage would be permanent and exactly the size of the
-- eviction. A tombstone makes the second sighting what it is: an event this project has
-- already had, and already thrown away.
--
-- It lives here rather than in `bugs::evict` because `record` reads it on the hot path
-- and the events it tombstones are this module's table. `evict` writes it.
--
-- `issue_id` rides along so a tombstoned id can still be answered with its issue, which
-- is what keeps `record`'s return type honest.
CREATE TABLE IF NOT EXISTS bugs_evicted_events (
    project_id INTEGER NOT NULL REFERENCES bugs_projects(id),
    event_id   TEXT NOT NULL,
    issue_id   INTEGER NOT NULL,
    evicted_at TEXT NOT NULL,
    PRIMARY KEY (project_id, event_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS bugs_envelopes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES bugs_projects(id),
    object_key  TEXT NOT NULL,
    received_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS bugs_envelopes_by_project
    ON bugs_envelopes (project_id, received_at);
"#;

/// Columns added after the first release. `CREATE TABLE IF NOT EXISTS` never revisits
/// a table it already found, so a column added to [`SCHEMA`] alone would exist in a
/// fresh database and nowhere else.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    // When the digest finished with this envelope. NULL means "never", which is what
    // the startup sweep looks for: a crash between the bucket write and the index
    // write leaves the object safe and the row unfinished.
    ("bugs_envelopes", "digested_at", "TEXT"),
    // How long a project's log rows stay in the hot window.
    ("bugs_projects", "retention_days", RETENTION_COLUMN),
    // Whether the project still authenticates. Every project that existed before this
    // column did was authenticating, so the default has to be 1.
    ("bugs_projects", "active", "INTEGER NOT NULL DEFAULT 1"),
    // What eviction weighs. Fixed when the event is stored, from how crowded its issue
    // already was; see `bugs::evict`. Zero for every event that predates the column,
    // which makes those the *most* relevant of all — right, and deliberately so: a
    // database that has been running since before eviction existed holds the history
    // somebody has been reading, and age alone will retire it soon enough.
    ("bugs_events", "irrelevance", "INTEGER NOT NULL DEFAULT 0"),
    // The mute rule, and how far along it is. All six are cleared by `set_state` on
    // every move, so a rule can never outlive the mute that armed it. See `bugs::mute`.
    ("bugs_issues", "mute_rule", "TEXT"),
    ("bugs_issues", "mute_until", "TEXT"),
    ("bugs_issues", "mute_count", "INTEGER"),
    ("bugs_issues", "mute_window", "INTEGER"),
    ("bugs_issues", "mute_from", "TEXT"),
    ("bugs_issues", "mute_events", "INTEGER NOT NULL DEFAULT 0"),
];

/// Add a column if the table does not have it yet.
///
/// Shared with [`crate::bugs::logs`], which owns its own table and so its own list.
pub fn add_columns(conn: &Connection, wanted: &[(&str, &str, &str)]) -> DbResult<()> {
    for (table, column, definition) in wanted {
        let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut present = false;
        let names = statement.query_map([], |row| row.get::<_, String>(1))?;
        for name in names {
            if name? == *column {
                present = true;
            }
        }
        drop(statement);
        if !present {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
        }
    }
    Ok(())
}

/// Record that a raw envelope object landed in the bucket, and hand back the row id.
/// The digest reads events out of it and stamps `digested_at`; the startup sweep
/// re-reads whatever never got that stamp.
pub fn record_envelope(db: &Db, project_id: i64, object_key: &str) -> DbResult<i64> {
    db.with(|conn| {
        conn.execute(
            "INSERT INTO bugs_envelopes (project_id, object_key, received_at)
             VALUES (?1, ?2, ?3)",
            params![project_id, object_key, now()],
        )?;
        Ok(conn.last_insert_rowid())
    })
}

/// Has this event id already been evicted?
///
/// [`record`] enforces this inside its own transaction, which is what makes it true.
/// This is the cheap look ahead of it, so the digest can skip re-writing a bucket
/// object for an event it is about to be told is a duplicate — one indexed read instead
/// of one object-store PUT.
pub fn evicted(db: &Db, project_id: i64, event_id: &str) -> DbResult<bool> {
    db.with(|conn| {
        conn.query_row(
            "SELECT 1 FROM bugs_evicted_events WHERE project_id = ?1 AND event_id = ?2",
            params![project_id, event_id],
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
    })
}

/// The digest is done with this envelope.
pub fn mark_digested(db: &Db, id: i64) -> DbResult<()> {
    db.with(|conn| {
        conn.execute("UPDATE bugs_envelopes SET digested_at = ?2 WHERE id = ?1", params![id, now()])?;
        Ok(())
    })
}

/// One stored envelope waiting to be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEnvelope {
    pub id: i64,
    pub project_id: i64,
    pub object_key: String,
}

/// Envelopes the digest never finished, oldest first. `all` takes every envelope
/// instead, which is what rebuilding the index from the bucket needs.
pub fn undigested(db: &Db, all: bool, limit: i64) -> DbResult<Vec<StoredEnvelope>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, project_id, object_key FROM bugs_envelopes
             WHERE ?1 OR digested_at IS NULL
             ORDER BY received_at, id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![all, limit], |row| {
            Ok(StoredEnvelope { id: row.get(0)?, project_id: row.get(1)?, object_key: row.get(2)? })
        })?;
        rows.collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bed() -> (Db, Project) {
        let db = Db::in_memory().unwrap();
        migrate(&db).unwrap();
        let project = create_project(&db, "demo", &"a".repeat(32), None).unwrap();
        (db, project)
    }

    fn event(project: &Project, id: &str, key: &str) -> NewEvent {
        NewEvent {
            project_id: project.id,
            event_id: id.to_owned(),
            object_key: format!("projects/{}/events/{id}.json", project.id),
            grouping_key: key.to_owned(),
            grouping_hash: format!("hash-{key}"),
            mechanism: "nashcode-v1".to_owned(),
            title: key.to_owned(),
            culprit: None,
            level: "error".to_owned(),
            platform: Some("python".to_owned()),
            timestamp: Some("2026-08-19T00:00:00Z".to_owned()),
        }
    }

    #[test]
    fn the_column_default_and_the_rust_constant_are_the_same_number() {
        assert_eq!(
            RETENTION_COLUMN,
            format!("INTEGER NOT NULL DEFAULT {DEFAULT_RETENTION_DAYS}"),
            "a project created before the column existed would get a different window"
        );
    }

    #[test]
    fn one_grouping_key_is_one_issue_however_many_events() {
        let (db, project) = bed();
        let (first, landing) = record(&db, &event(&project, "a".repeat(32).as_str(), "Boom")).unwrap();
        assert_eq!(landing, Landing::New);
        let (second, landing) = record(&db, &event(&project, "b".repeat(32).as_str(), "Boom")).unwrap();
        assert_eq!(landing, Landing::Repeat);
        assert_eq!(first.id, second.id);
        assert_eq!(second.events, 2);
        assert_eq!(issues(&db, project.id, None).unwrap().len(), 1);
    }

    #[test]
    fn an_event_on_a_resolved_issue_reopens_it_as_a_regression() {
        let (db, project) = bed();
        let (issue, _) = record(&db, &event(&project, &"a".repeat(32), "Boom")).unwrap();
        set_state(&db, project.id, issue.id, state::RESOLVED, "tester").unwrap();

        let (reopened, landing) = record(&db, &event(&project, &"b".repeat(32), "Boom")).unwrap();
        assert_eq!(landing, Landing::Regression);
        assert_eq!(reopened.state, state::UNRESOLVED);
        assert!(reopened.regression);

        // The regression trigger is kept, like the first-seen event.
        let kept: Vec<_> =
            events(&db, issue.id, 10).unwrap().into_iter().filter(|e| e.keep).collect();
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn resolving_a_regression_closes_the_book_on_it() {
        let (db, project) = bed();
        let (issue, _) = record(&db, &event(&project, &"a".repeat(32), "Boom")).unwrap();
        set_state(&db, project.id, issue.id, state::RESOLVED, "tester").unwrap();
        let (reopened, _) = record(&db, &event(&project, &"b".repeat(32), "Boom")).unwrap();
        assert!(reopened.regression);

        let resolved = set_state(&db, project.id, issue.id, state::RESOLVED, "tester")
            .unwrap()
            .expect("the issue");
        assert!(!resolved.regression, "the flag says 'came back after we fixed it'");

        // Muting is not a fix, so it leaves the flag alone.
        record(&db, &event(&project, &"c".repeat(32), "Boom")).unwrap();
        let muted =
            set_state(&db, project.id, issue.id, state::MUTED, "tester").unwrap().expect("issue");
        assert!(muted.regression);
    }

    #[test]
    fn the_same_event_id_twice_counts_once() {
        let (db, project) = bed();
        record(&db, &event(&project, &"a".repeat(32), "Boom")).unwrap();
        let (issue, landing) = record(&db, &event(&project, &"a".repeat(32), "Boom")).unwrap();
        assert_eq!(landing, Landing::Duplicate);
        assert_eq!(issue.events, 1);
    }

    #[test]
    fn the_project_list_carries_its_counts() {
        let (db, project) = bed();
        record(&db, &event(&project, &"a".repeat(32), "Boom")).unwrap();
        record(&db, &event(&project, &"b".repeat(32), "Other")).unwrap();
        let listed = projects(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].unresolved, 2);
        assert_eq!(listed[0].issues, 2);
        assert_eq!(listed[0].events, 2);
    }
}
