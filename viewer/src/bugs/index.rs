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

/// Apply the bugs schema. Idempotent, run on every open.
pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute_batch(SCHEMA)?;
        add_missing_columns(conn)
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
        })
    })
}

pub fn project_by_name(db: &Db, name: &str) -> DbResult<Option<Project>> {
    db.with(|conn| {
        conn.query_row(
            "SELECT id, name, key, repo, created_at, retention_days
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
            "SELECT id, name, key, repo, created_at, retention_days
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
            "SELECT p.id, p.name, p.key, p.repo, p.created_at, p.retention_days,
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
                unresolved: row.get(6)?,
                issues: row.get(7)?,
                events: row.get(8)?,
            })
        })?;
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
        tx.execute(
            "INSERT INTO bugs_events
               (project_id, issue_id, event_id, object_key, level, platform, timestamp,
                received_at, keep)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
        let changed = conn.execute(
            "UPDATE bugs_issues
             SET state = ?3, actor = ?4, acted_at = ?5,
                 regression = CASE WHEN ?3 = 'resolved' THEN 0 ELSE regression END
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
    ("bugs_projects", "retention_days", "INTEGER NOT NULL DEFAULT 30"),
];

/// Add a column if the table does not have it yet.
fn add_missing_columns(conn: &Connection) -> DbResult<()> {
    for (table, column, definition) in ADDED_COLUMNS {
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
