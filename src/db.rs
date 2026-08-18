//! SQLite persistence: comments, CI runs, seen branch tips, and the merge/restack
//! audit trail.
//!
//! Every timestamp is a fixed-width UTC RFC3339 string, so `ORDER BY created_at` and
//! `created_at > ?` are both plain lexicographic comparisons. That is what makes the
//! `?since=` cursor on the comment API reliable for polling agents.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

/// Fixed-width UTC. Every field is zero-padded and the subsecond part is always six
/// digits, which is what keeps string ordering equal to time ordering.
const TIMESTAMP: &[BorrowedFormatItem<'_>] = format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z"
);

/// The current time in the canonical storage format.
pub fn now() -> String {
    format_time(OffsetDateTime::now_utc())
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .to_offset(time::UtcOffset::UTC)
        .format(&TIMESTAMP)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000000Z".to_owned())
}

/// Normalise any RFC3339 input to the canonical storage format so it can be compared
/// against stored timestamps. Returns `None` when the input is not a timestamp.
pub fn normalize_timestamp(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Ok(value) = OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339) {
        return Some(format_time(value));
    }
    // Accept a bare date-time without an offset by reading it as UTC.
    let naive = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    PrimitiveDateTime::parse(raw, &naive)
        .ok()
        .map(|value| format_time(value.assume_utc()))
}

/// A stored comment. This is exactly the JSON the public API returns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    pub id: i64,
    pub repo: String,
    pub branch: String,
    /// `None` for a pull-request-level comment.
    pub file: Option<String>,
    /// One-based new-side line. `None` for a file-level or PR-level comment.
    pub line: Option<i64>,
    /// The commit the anchor was made against.
    pub commit: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// A recorded CI run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CiRun {
    pub id: i64,
    pub repo: String,
    pub branch: String,
    pub commit: String,
    pub status: String,
    pub duration_ms: i64,
    pub log_path: Option<String>,
    pub created_at: String,
}

/// The status vocabulary. `Skipped` means the repo has no `.nashgit/ci` script.
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const PASSED: &str = "passed";
    pub const FAILED: &str = "failed";
    pub const TIMEOUT: &str = "timeout";
    pub const ERROR: &str = "error";
    pub const SKIPPED: &str = "skipped";

    /// A merge is blocked unless CI is green or there is nothing to run.
    pub fn blocks_merge(status: Option<&str>) -> bool {
        !matches!(status, Some(PASSED) | Some(SKIPPED) | None)
    }
}

/// One line of the merge/restack audit trail.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: i64,
    pub repo: String,
    pub actor: String,
    /// `merge` or `restack`.
    pub action: String,
    pub branch: String,
    pub old_tip: String,
    pub new_tip: String,
    pub detail: String,
    pub created_at: String,
}

/// The database handle. Cheap to clone; shared through Topcoat's app context.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Db")
    }
}

pub type DbResult<T> = Result<T, rusqlite::Error>;

impl Db {
    /// Open (creating if needed) the database at `path` and apply the schema.
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> DbResult<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> DbResult<Self> {
        // WAL keeps the CI worker's writes from blocking page reads.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .unwrap_or_default();
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&Connection) -> DbResult<T>) -> DbResult<T> {
        // A poisoned lock means another thread panicked mid-statement. The connection
        // itself is still sound, so recover rather than cascading the panic into a 500.
        let guard = self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&guard)
    }

    // ---- comments --------------------------------------------------------------

    /// Store a comment and hand back the stored row, ids and timestamp included.
    pub fn add_comment(&self, new: NewComment) -> DbResult<Comment> {
        let created_at = now();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO comments (repo, branch, file, line, commit_id, author, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    new.repo,
                    new.branch,
                    new.file,
                    new.line,
                    new.commit,
                    new.author,
                    new.body,
                    created_at
                ],
            )?;
            Ok(Comment {
                id: conn.last_insert_rowid(),
                repo: new.repo.clone(),
                branch: new.branch.clone(),
                file: new.file.clone(),
                line: new.line,
                commit: new.commit.clone(),
                author: new.author.clone(),
                body: new.body.clone(),
                created_at: created_at.clone(),
            })
        })
    }

    /// Read comments back, oldest first. Every filter is optional.
    ///
    /// The ordering is `created_at, id`: a poller that remembers the `created_at` of
    /// the last row it saw and passes it as `since` sees each later comment once.
    pub fn comments(&self, filter: &CommentFilter) -> DbResult<Vec<Comment>> {
        self.with(|conn| {
            let mut sql = String::from(
                "SELECT id, repo, branch, file, line, commit_id, author, body, created_at
                 FROM comments WHERE repo = ?1",
            );
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(filter.repo.clone())];

            if let Some(branch) = &filter.branch {
                args.push(Box::new(branch.clone()));
                sql.push_str(&format!(" AND branch = ?{}", args.len()));
            }
            if let Some(file) = &filter.file {
                args.push(Box::new(file.clone()));
                sql.push_str(&format!(" AND file = ?{}", args.len()));
            }
            if let Some(since) = &filter.since {
                args.push(Box::new(since.clone()));
                sql.push_str(&format!(" AND created_at > ?{}", args.len()));
            }
            sql.push_str(" ORDER BY created_at, id");

            let mut statement = conn.prepare(&sql)?;
            let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
            let rows = statement
                .query_map(refs.as_slice(), |row| {
                    Ok(Comment {
                        id: row.get(0)?,
                        repo: row.get(1)?,
                        branch: row.get(2)?,
                        file: row.get(3)?,
                        line: row.get(4)?,
                        commit: row.get(5)?,
                        author: row.get(6)?,
                        body: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Delete a comment, but only when `author` wrote it. Returns true when a row went.
    pub fn delete_comment(&self, repo: &str, id: i64, author: &str) -> DbResult<bool> {
        self.with(|conn| {
            let changed = conn.execute(
                "DELETE FROM comments WHERE id = ?1 AND repo = ?2 AND author = ?3",
                params![id, repo, author],
            )?;
            Ok(changed > 0)
        })
    }

    // ---- CI --------------------------------------------------------------------

    /// Record a queued run and return its id.
    pub fn enqueue_run(&self, repo: &str, branch: &str, commit: &str) -> DbResult<i64> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO ci_runs (repo, branch, commit_id, status, duration_ms, created_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![repo, branch, commit, status::QUEUED, now()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn set_run_status(
        &self,
        id: i64,
        new_status: &str,
        duration_ms: i64,
        log_path: Option<&str>,
    ) -> DbResult<()> {
        self.with(|conn| {
            conn.execute(
                "UPDATE ci_runs SET status = ?2, duration_ms = ?3, log_path = COALESCE(?4, log_path)
                 WHERE id = ?1",
                params![id, new_status, duration_ms, log_path],
            )?;
            Ok(())
        })
    }

    /// The most recent run for a commit, which is what a branch's status dot shows.
    pub fn latest_run(&self, repo: &str, commit: &str) -> DbResult<Option<CiRun>> {
        self.with(|conn| {
            conn.query_row(
                "SELECT id, repo, branch, commit_id, status, duration_ms, log_path, created_at
                 FROM ci_runs WHERE repo = ?1 AND commit_id = ?2
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![repo, commit],
                read_run,
            )
            .optional()
        })
    }

    /// Recent runs for a repo, newest first.
    pub fn recent_runs(&self, repo: &str, limit: usize) -> DbResult<Vec<CiRun>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, repo, branch, commit_id, status, duration_ms, log_path, created_at
                 FROM ci_runs WHERE repo = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![repo, limit as i64], read_run)?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ---- seen tips -------------------------------------------------------------

    /// Record a branch tip. Returns true when this tip is new, which is the signal
    /// that a CI job and a `push` webhook are due.
    pub fn observe_tip(&self, repo: &str, branch: &str, commit: &str) -> DbResult<bool> {
        self.with(|conn| {
            let previous: Option<String> = conn
                .query_row(
                    "SELECT commit_id FROM seen_tips WHERE repo = ?1 AND branch = ?2",
                    params![repo, branch],
                    |row| row.get(0),
                )
                .optional()?;
            if previous.as_deref() == Some(commit) {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO seen_tips (repo, branch, commit_id, seen_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo, branch) DO UPDATE SET commit_id = ?3, seen_at = ?4",
                params![repo, branch, commit, now()],
            )?;
            Ok(true)
        })
    }

    // ---- traces ----------------------------------------------------------------

    /// Store one trace event. `seq: None` gets the session's next number; a duplicate
    /// `(repo, session, seq)` is ignored, which is what makes batch retries safe.
    /// Returns true when a row was written.
    pub fn add_trace_event(&self, event: &NewTraceEvent) -> DbResult<bool> {
        self.with(|conn| {
            let seq: i64 = match event.seq {
                Some(seq) => seq,
                None => conn.query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM trace_events
                     WHERE repo = ?1 AND session = ?2",
                    params![event.repo, event.session],
                    |row| row.get(0),
                )?,
            };
            let changed = conn.execute(
                "INSERT OR IGNORE INTO trace_events
                     (repo, session, seq, kind, payload, head, agent, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.repo,
                    event.session,
                    seq,
                    event.kind,
                    event.payload,
                    event.head,
                    event.agent,
                    now()
                ],
            )?;
            Ok(changed > 0)
        })
    }

    /// The head recorded by the session's latest event that carried one.
    pub fn trace_last_head(&self, repo: &str, session: &str) -> DbResult<Option<String>> {
        self.with(|conn| {
            conn.query_row(
                "SELECT head FROM trace_events
                 WHERE repo = ?1 AND session = ?2 AND head IS NOT NULL
                 ORDER BY seq DESC LIMIT 1",
                params![repo, session],
                |row| row.get(0),
            )
            .optional()
        })
    }

    /// Attribute commits to a session. Duplicates are ignored.
    pub fn attribute_commits(&self, repo: &str, session: &str, shas: &[String]) -> DbResult<()> {
        self.with(|conn| {
            for sha in shas {
                conn.execute(
                    "INSERT OR IGNORE INTO trace_commits (repo, session, sha, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![repo, session, sha, now()],
                )?;
            }
            Ok(())
        })
    }

    /// Every session in a repo, newest first.
    pub fn trace_sessions(&self, repo: &str, limit: usize) -> DbResult<Vec<TraceSession>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT e.session,
                        COALESCE(MAX(e.agent), '') AS agent,
                        MIN(e.created_at), MAX(e.created_at), COUNT(*),
                        (SELECT COUNT(*) FROM trace_commits c
                          WHERE c.repo = e.repo AND c.session = e.session)
                 FROM trace_events e WHERE e.repo = ?1
                 GROUP BY e.session ORDER BY MAX(e.created_at) DESC LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![repo, limit as i64], |row| {
                    Ok(TraceSession {
                        session: row.get(0)?,
                        agent: row.get::<_, String>(1).map(|a| (!a.is_empty()).then_some(a))?,
                        started_at: row.get(2)?,
                        last_event_at: row.get(3)?,
                        events: row.get(4)?,
                        commits: row.get(5)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// One session's events, in order.
    pub fn trace_events(&self, repo: &str, session: &str) -> DbResult<Vec<TraceEvent>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT seq, kind, payload, head, agent, created_at FROM trace_events
                 WHERE repo = ?1 AND session = ?2 ORDER BY seq",
            )?;
            let rows = statement
                .query_map(params![repo, session], |row| {
                    Ok(TraceEvent {
                        seq: row.get(0)?,
                        kind: row.get(1)?,
                        payload: row.get(2)?,
                        head: row.get(3)?,
                        agent: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The commits a session produced, oldest first.
    pub fn trace_session_commits(&self, repo: &str, session: &str) -> DbResult<Vec<String>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT sha FROM trace_commits WHERE repo = ?1 AND session = ?2
                 ORDER BY created_at, sha",
            )?;
            let rows = statement
                .query_map(params![repo, session], |row| row.get(0))?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The session(s) that produced a commit.
    pub fn trace_sessions_for_commit(&self, repo: &str, sha: &str) -> DbResult<Vec<String>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT session FROM trace_commits WHERE repo = ?1 AND sha = ?2",
            )?;
            let rows = statement
                .query_map(params![repo, sha], |row| row.get(0))?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ---- audit -----------------------------------------------------------------

    pub fn record_audit(&self, entry: NewAudit) -> DbResult<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO audit (repo, actor, action, branch, old_tip, new_tip, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    entry.repo,
                    entry.actor,
                    entry.action,
                    entry.branch,
                    entry.old_tip,
                    entry.new_tip,
                    entry.detail,
                    now()
                ],
            )?;
            Ok(())
        })
    }

    /// The audit trail for a repo, newest first.
    pub fn audit(&self, repo: &str, limit: usize) -> DbResult<Vec<AuditEntry>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, repo, actor, action, branch, old_tip, new_tip, detail, created_at
                 FROM audit WHERE repo = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![repo, limit as i64], |row| {
                    Ok(AuditEntry {
                        id: row.get(0)?,
                        repo: row.get(1)?,
                        actor: row.get(2)?,
                        action: row.get(3)?,
                        branch: row.get(4)?,
                        old_tip: row.get(5)?,
                        new_tip: row.get(6)?,
                        detail: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

fn read_run(row: &rusqlite::Row<'_>) -> DbResult<CiRun> {
    Ok(CiRun {
        id: row.get(0)?,
        repo: row.get(1)?,
        branch: row.get(2)?,
        commit: row.get(3)?,
        status: row.get(4)?,
        duration_ms: row.get(5)?,
        log_path: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// A comment on its way in.
#[derive(Debug, Clone)]
pub struct NewComment {
    pub repo: String,
    pub branch: String,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub commit: String,
    pub author: String,
    pub body: String,
}

/// Filters for reading comments back.
#[derive(Debug, Clone, Default)]
pub struct CommentFilter {
    pub repo: String,
    pub branch: Option<String>,
    pub file: Option<String>,
    /// Canonical timestamp; only strictly later comments come back.
    pub since: Option<String>,
}

/// A trace event on its way in.
#[derive(Debug, Clone)]
pub struct NewTraceEvent {
    pub repo: String,
    pub session: String,
    /// `None` assigns the session's next number (live hook events); batches carry
    /// explicit numbers so retries are idempotent.
    pub seq: Option<i64>,
    pub kind: String,
    /// The event as JSON text, stored verbatim.
    pub payload: String,
    /// The agent's repo `HEAD` when the event happened, if known.
    pub head: Option<String>,
    pub agent: Option<String>,
}

/// A stored trace event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TraceEvent {
    pub seq: i64,
    pub kind: String,
    pub payload: String,
    pub head: Option<String>,
    pub agent: Option<String>,
    pub created_at: String,
}

/// One agent session, summarized.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TraceSession {
    pub session: String,
    pub agent: Option<String>,
    pub started_at: String,
    pub last_event_at: String,
    pub events: i64,
    pub commits: i64,
}

/// An audit line on its way in.
#[derive(Debug, Clone)]
pub struct NewAudit {
    pub repo: String,
    pub actor: String,
    pub action: String,
    pub branch: String,
    pub old_tip: String,
    pub new_tip: String,
    pub detail: String,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS comments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo       TEXT NOT NULL,
    branch     TEXT NOT NULL,
    file       TEXT,
    line       INTEGER,
    commit_id  TEXT NOT NULL,
    author     TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS comments_lookup ON comments (repo, branch, file, created_at);
CREATE INDEX IF NOT EXISTS comments_cursor ON comments (repo, created_at, id);

CREATE TABLE IF NOT EXISTS ci_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    repo        TEXT NOT NULL,
    branch      TEXT NOT NULL,
    commit_id   TEXT NOT NULL,
    status      TEXT NOT NULL,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    log_path    TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ci_runs_commit ON ci_runs (repo, commit_id, created_at);

CREATE TABLE IF NOT EXISTS seen_tips (
    repo      TEXT NOT NULL,
    branch    TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    seen_at   TEXT NOT NULL,
    PRIMARY KEY (repo, branch)
);

CREATE TABLE IF NOT EXISTS trace_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo       TEXT NOT NULL,
    session    TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    head       TEXT,
    agent      TEXT,
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS trace_events_unique ON trace_events (repo, session, seq);
CREATE INDEX IF NOT EXISTS trace_events_by_session ON trace_events (repo, session, seq);

CREATE TABLE IF NOT EXISTS trace_commits (
    repo       TEXT NOT NULL,
    session    TEXT NOT NULL,
    sha        TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (repo, session, sha)
);
CREATE INDEX IF NOT EXISTS trace_commits_by_sha ON trace_commits (repo, sha);

CREATE TABLE IF NOT EXISTS audit (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo       TEXT NOT NULL,
    actor      TEXT NOT NULL,
    action     TEXT NOT NULL,
    branch     TEXT NOT NULL,
    old_tip    TEXT NOT NULL,
    new_tip    TEXT NOT NULL,
    detail     TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_repo ON audit (repo, created_at);

"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(db: &Db, file: Option<&str>, body: &str) -> Comment {
        db.add_comment(NewComment {
            repo: "demo".into(),
            branch: "main".into(),
            file: file.map(str::to_owned),
            line: file.map(|_| 3),
            commit: "abc123".into(),
            author: "ada@example.invalid".into(),
            body: body.into(),
        })
        .unwrap()
    }

    #[test]
    fn timestamps_sort_lexicographically_in_time_order() {
        let a = normalize_timestamp("2026-01-02T03:04:05Z").unwrap();
        let b = normalize_timestamp("2026-01-02T03:04:06Z").unwrap();
        let c = normalize_timestamp("2026-11-02T03:04:05Z").unwrap();
        assert!(a < b && b < c, "{a} {b} {c}");
        // An offset input normalises to the same instant in UTC.
        assert_eq!(
            normalize_timestamp("2026-01-02T04:04:05+01:00").unwrap(),
            a
        );
    }

    #[test]
    fn a_since_cursor_never_repeats_or_skips() {
        let db = Db::in_memory().unwrap();
        let first = comment(&db, Some("plans/a.md"), "one");
        let second = comment(&db, Some("plans/a.md"), "two");
        let third = comment(&db, Some("plans/a.md"), "three");

        let filter = CommentFilter { repo: "demo".into(), ..Default::default() };
        let all = db.comments(&filter).unwrap();
        assert_eq!(
            all.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![first.id, second.id, third.id]
        );

        let after_first = db
            .comments(&CommentFilter { since: Some(first.created_at.clone()), ..filter.clone() })
            .unwrap();
        assert_eq!(
            after_first.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![second.id, third.id]
        );

        let after_last = db
            .comments(&CommentFilter { since: Some(third.created_at.clone()), ..filter })
            .unwrap();
        assert!(after_last.is_empty());
    }

    #[test]
    fn comments_filter_by_file_and_branch() {
        let db = Db::in_memory().unwrap();
        comment(&db, Some("plans/a.md"), "on the plan");
        comment(&db, Some("src/main.rs"), "on the code");
        comment(&db, None, "on the branch");

        let only_plan = db
            .comments(&CommentFilter {
                repo: "demo".into(),
                file: Some("plans/a.md".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(only_plan.len(), 1);
        assert_eq!(only_plan[0].body, "on the plan");

        let other_branch = db
            .comments(&CommentFilter {
                repo: "demo".into(),
                branch: Some("nope".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(other_branch.is_empty());
    }

    #[test]
    fn only_the_author_can_delete_a_comment() {
        let db = Db::in_memory().unwrap();
        let stored = comment(&db, None, "mine");
        assert!(!db.delete_comment("demo", stored.id, "someone@example.invalid").unwrap());
        assert!(db.delete_comment("demo", stored.id, &stored.author).unwrap());
    }

    #[test]
    fn a_tip_is_new_only_once() {
        let db = Db::in_memory().unwrap();
        assert!(db.observe_tip("demo", "main", "aaa").unwrap());
        assert!(!db.observe_tip("demo", "main", "aaa").unwrap());
        assert!(db.observe_tip("demo", "main", "bbb").unwrap());
    }

    #[test]
    fn a_red_or_running_ci_blocks_a_merge() {
        assert!(status::blocks_merge(Some(status::FAILED)));
        assert!(status::blocks_merge(Some(status::RUNNING)));
        assert!(status::blocks_merge(Some(status::QUEUED)));
        assert!(!status::blocks_merge(Some(status::PASSED)));
        assert!(!status::blocks_merge(Some(status::SKIPPED)));
        // Nothing ever ran: there is no red light to ignore.
        assert!(!status::blocks_merge(None));
    }

    #[test]
    fn latest_run_wins_for_a_commit() {
        let db = Db::in_memory().unwrap();
        let old = db.enqueue_run("demo", "main", "abc").unwrap();
        db.set_run_status(old, status::FAILED, 10, Some("/tmp/old.log")).unwrap();
        let new = db.enqueue_run("demo", "main", "abc").unwrap();
        db.set_run_status(new, status::PASSED, 20, Some("/tmp/new.log")).unwrap();

        let latest = db.latest_run("demo", "abc").unwrap().unwrap();
        assert_eq!(latest.id, new);
        assert_eq!(latest.status, status::PASSED);
    }
}
