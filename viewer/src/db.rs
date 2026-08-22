//! SQLite persistence: comments, CI runs, seen branch tips, and the merge/restack
//! audit trail.
//!
//! Every timestamp is a fixed-width UTC RFC3339 string, so `ORDER BY created_at` and
//! `created_at > ?` are both plain lexicographic comparisons. That is what makes the
//! `?since=` cursor on the comment API reliable for polling agents.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params,
};
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

/// A moment `seconds` from now, in the canonical storage format. A negative offset
/// looks backwards, which is what the head of a rolling window is.
///
/// Every deadline stored here — a retry, a parked queue — is a timestamp string
/// compared lexicographically, so it has to come out of the same formatter as
/// [`now`] or the comparison quietly means nothing.
pub fn now_offset(seconds: i64) -> String {
    format_time(OffsetDateTime::now_utc() + time::Duration::seconds(seconds))
}

/// A Unix epoch second, in the canonical storage format. Third parties date things
/// this way — Pushover's `X-Limit-App-Reset` among them — and a deadline is only
/// useful here once it can be compared with [`now`].
pub fn from_unix(seconds: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(seconds).ok().map(format_time)
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
    /// The heartbeat. The worker rewrites it while the job runs, so a `running` row
    /// that stops moving is a job whose worker is gone.
    pub updated_at: String,
    /// Why a run ended, when there is no log to say so. Empty for an ordinary run.
    pub note: String,
}

impl CiRun {
    /// The status this run really has.
    ///
    /// A `running` row whose heartbeat stopped belongs to a worker that died: nothing
    /// will ever finish it, so it reads as [`status::STUCK`] rather than as a job
    /// somebody is still waiting on. Derived, never stored — the row keeps saying
    /// `running` until a requeue or a restart rewrites it.
    pub fn effective_status(&self) -> &str {
        if self.status == status::RUNNING
            && self.updated_at < now_offset(-status::HEARTBEAT_STALE_SECS)
        {
            status::STUCK
        } else {
            &self.status
        }
    }
}

/// The note left on runs a restart found in flight.
pub const ORPHANED: &str = "orphaned by restart";

/// The status vocabulary. `Skipped` means nothing ran: either the default branch never
/// opted in, or the repo has no `.nashcode/ci` script. The run's log says which.
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const PASSED: &str = "passed";
    pub const FAILED: &str = "failed";
    pub const TIMEOUT: &str = "timeout";
    pub const ERROR: &str = "error";
    pub const SKIPPED: &str = "skipped";
    /// Derived, never stored: a `running` row with a dead heartbeat.
    pub const STUCK: &str = "stuck";

    /// How long a `running` row may go without a heartbeat before it is stuck.
    /// Five missed beats — the worker writes one a minute.
    pub const HEARTBEAT_STALE_SECS: i64 = 5 * 60;

    /// A merge is blocked unless CI is green or there is nothing to run.
    ///
    /// `stuck` does not block. A run nothing is executing says exactly as much about
    /// the commit as a run that never happened, and blocking on it is the gate that
    /// can never be satisfied — the branch page offers a requeue instead.
    pub fn blocks_merge(status: Option<&str>) -> bool {
        !matches!(status, Some(PASSED) | Some(SKIPPED) | Some(STUCK) | None)
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

/// Did another connection hold the write lock? The caller can just try again.
fn busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == ErrorCode::DatabaseBusy || e.code == ErrorCode::DatabaseLocked
    )
}

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
        crate::bugs::index::add_columns(&conn, CI_RUN_COLUMNS)?;
        // A run that was queued or running belongs to a process that is no longer
        // here: nothing will ever finish it, and a merge waiting on it waits forever.
        // Reconciling at open is what makes "in flight" mean "in flight in *this*
        // process".
        conn.execute(
            "UPDATE ci_runs SET status = ?1, note = ?2, updated_at = ?3
             WHERE status IN (?4, ?5)",
            params![status::ERROR, ORPHANED, now(), status::QUEUED, status::RUNNING],
        )?;
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
                "INSERT INTO ci_runs
                    (repo, branch, commit_id, status, duration_ms, created_at, updated_at, note)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, '')",
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
                "UPDATE ci_runs SET status = ?2, duration_ms = ?3, log_path = COALESCE(?4, log_path),
                        updated_at = ?5
                 WHERE id = ?1",
                params![id, new_status, duration_ms, log_path, now()],
            )?;
            Ok(())
        })
    }

    /// The heartbeat a running job writes. A row that stops moving is a job whose
    /// worker died; [`CiRun::effective_status`] is what reads that.
    pub fn touch_run(&self, id: i64) -> DbResult<()> {
        self.with(|conn| {
            conn.execute("UPDATE ci_runs SET updated_at = ?2 WHERE id = ?1", params![id, now()])?;
            Ok(())
        })
    }

    /// Put a run back in the queue. Keeps the row, so the requeued run still answers
    /// for the commit it was recorded against. Returns false when there is no row.
    pub fn requeue_run(&self, id: i64) -> DbResult<bool> {
        self.with(|conn| {
            let changed = conn.execute(
                "UPDATE ci_runs SET status = ?2, duration_ms = 0, note = '', updated_at = ?3
                 WHERE id = ?1",
                params![id, status::QUEUED, now()],
            )?;
            Ok(changed > 0)
        })
    }

    /// The most recent run for a commit, which is what a branch's status dot shows.
    pub fn latest_run(&self, repo: &str, commit: &str) -> DbResult<Option<CiRun>> {
        self.with(|conn| {
            conn.query_row(
                "SELECT id, repo, branch, commit_id, status, duration_ms, log_path, created_at,
                        updated_at, note
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
                "SELECT id, repo, branch, commit_id, status, duration_ms, log_path, created_at,
                        updated_at, note
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
    /// `(repo, session, seq)` sent by a client is ignored, which is what makes batch
    /// retries safe. Returns true when a row was written.
    ///
    /// Allocation and insert are one `BEGIN IMMEDIATE` critical section. As two
    /// statements they are a race: a second writer reads the same `MAX(seq)`, picks the
    /// same number, and its event then vanishes into `INSERT OR IGNORE` with nothing
    /// said about it.
    pub fn add_trace_event(&self, event: &NewTraceEvent) -> DbResult<bool> {
        self.with(|conn| {
            // `busy_timeout` already waits out a held write lock. These retries cover
            // the case where it gives up anyway, under a long-running writer.
            let mut busy_error = None;
            for _ in 0..5 {
                match Self::insert_trace_event(conn, event) {
                    Err(error) if busy(&error) => busy_error = Some(error),
                    outcome => return outcome,
                }
            }
            Err(busy_error.expect("the loop returns early on every non-busy outcome"))
        })
    }

    /// One attempt at allocate-and-insert, as a single critical section.
    fn insert_trace_event(conn: &Connection, event: &NewTraceEvent) -> DbResult<bool> {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let seq: i64 = match event.seq {
            Some(seq) => seq,
            None => tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM trace_events
                 WHERE repo = ?1 AND session = ?2",
                params![event.repo, event.session],
                |row| row.get(0),
            )?,
        };
        // A client-supplied seq may legitimately repeat: that is the retry contract. An
        // allocated one cannot, so let the unique index raise rather than swallow it.
        let sql = if event.seq.is_some() {
            "INSERT OR IGNORE INTO trace_events
                 (repo, session, seq, kind, payload, head, agent, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        } else {
            "INSERT INTO trace_events
                 (repo, session, seq, kind, payload, head, agent, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        };
        let changed = tx.execute(
            sql,
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
        tx.commit()?;
        Ok(changed > 0)
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

    /// Every prompt recorded in a repo, newest first.
    ///
    /// A prompt is any event whose payload carries a `prompt` field, so this works for
    /// any harness that reports one without needing to know its hook names.
    pub fn prompts(
        &self,
        repo: &str,
        query: Option<&str>,
        session: Option<&str>,
        limit: usize,
    ) -> DbResult<Vec<Prompt>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT session, seq, json_extract(payload, '$.prompt'), head, agent, created_at
                 FROM trace_events
                 WHERE repo = ?1
                   AND json_extract(payload, '$.prompt') IS NOT NULL
                   AND (?2 IS NULL OR json_extract(payload, '$.prompt') LIKE '%' || ?2 || '%')
                   AND (?3 IS NULL OR session = ?3)
                 ORDER BY created_at DESC, seq DESC
                 LIMIT ?4",
            )?;
            let rows = statement
                .query_map(params![repo, query, session, limit as i64], |row| {
                    Ok(Prompt {
                        session: row.get(0)?,
                        seq: row.get(1)?,
                        text: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        head: row.get(3)?,
                        agent: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                })?
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

    // ---- code intelligence -------------------------------------------------------

    /// Which blobs of `candidates` this repo already holds chunks for.
    ///
    /// This is the incrementality check: whatever comes back needs no parsing and no
    /// embedding, whichever path it now sits at.
    pub fn code_known_blobs(&self, repo: &str, candidates: &[String]) -> DbResult<Vec<String>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        self.with(|conn| {
            let mut known = Vec::new();
            // `code_seen_blobs` is what makes a blob that produces *no* chunks — a
            // minified bundle, an empty file, a two-line stub — still count as done.
            // Asking `code_chunks` alone would re-read and re-parse it on every run
            // forever, which on a large repo is most of the run.
            let mut statement = conn.prepare(
                "SELECT 1 FROM code_seen_blobs WHERE repo = ?1 AND blob = ?2 LIMIT 1",
            )?;
            for blob in candidates {
                if statement.exists(params![repo, blob])? {
                    known.push(blob.clone());
                }
            }
            Ok(known)
        })
    }

    /// Store one blob's chunks and graph entries, replacing anything held for it.
    ///
    /// One transaction per blob: an index run that dies halfway leaves every blob it
    /// finished intact, and the next run picks up exactly where it stopped.
    pub fn code_put_blob(&self, entry: &CodeBlob) -> DbResult<()> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM code_chunks WHERE repo = ?1 AND blob = ?2",
                params![entry.repo, entry.blob],
            )?;
            tx.execute(
                "DELETE FROM code_symbols WHERE repo = ?1 AND blob = ?2",
                params![entry.repo, entry.blob],
            )?;
            tx.execute(
                "DELETE FROM code_refs WHERE repo = ?1 AND blob = ?2",
                params![entry.repo, entry.blob],
            )?;
            // Mark it read whatever came of it: a blob with no chunks is still a blob
            // this repo has already spent a parse on.
            tx.execute(
                "INSERT OR IGNORE INTO code_seen_blobs (repo, blob) VALUES (?1, ?2)",
                params![entry.repo, entry.blob],
            )?;
            for (ordinal, chunk) in entry.chunks.iter().enumerate() {
                tx.execute(
                    "INSERT INTO code_chunks
                        (repo, blob, ordinal, symbol, start_line, end_line, snippet, vector, dims, model)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        entry.repo,
                        entry.blob,
                        ordinal as i64,
                        chunk.symbol,
                        chunk.start_line,
                        chunk.end_line,
                        chunk.snippet,
                        chunk.vector.as_ref().map(|v| encode_vector(v)),
                        chunk.vector.as_ref().map_or(0, Vec::len) as i64,
                        chunk.model,
                    ],
                )?;
            }
            for (ordinal, symbol) in entry.symbols.iter().enumerate() {
                tx.execute(
                    "INSERT INTO code_symbols
                        (repo, blob, ordinal, name, kind, start_line, end_line, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        entry.repo,
                        entry.blob,
                        ordinal as i64,
                        symbol.name,
                        symbol.kind,
                        symbol.start_line,
                        symbol.end_line,
                        symbol.source,
                    ],
                )?;
            }
            for (ordinal, reference) in entry.refs.iter().enumerate() {
                tx.execute(
                    "INSERT INTO code_refs (repo, blob, ordinal, name, caller, line, kind, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        entry.repo,
                        entry.blob,
                        ordinal as i64,
                        reference.name,
                        reference.caller,
                        reference.line,
                        reference.kind,
                        reference.source,
                    ],
                )?;
            }
            tx.commit()
        })
    }

    /// Record that a blob has been looked at, without storing anything from it.
    /// A binary or oversized file gets this and nothing else, so the next run knows
    /// not to read it again.
    pub fn code_mark_seen(&self, repo: &str, blob: &str) -> DbResult<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO code_seen_blobs (repo, blob) VALUES (?1, ?2)",
                params![repo, blob],
            )?;
            Ok(())
        })
    }

    /// Replace a blob's graph entries alone, leaving its chunks and vectors alone.
    /// The SCIP overlay lands through here: it is more accurate about symbols and
    /// says nothing at all about embeddings.
    pub fn code_put_graph(
        &self,
        repo: &str,
        blob: &str,
        symbols: &[CodeSymbol],
        refs: &[CodeRef],
    ) -> DbResult<()> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM code_symbols WHERE repo = ?1 AND blob = ?2",
                params![repo, blob],
            )?;
            tx.execute("DELETE FROM code_refs WHERE repo = ?1 AND blob = ?2", params![repo, blob])?;
            for (ordinal, symbol) in symbols.iter().enumerate() {
                tx.execute(
                    "INSERT INTO code_symbols
                        (repo, blob, ordinal, name, kind, start_line, end_line, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        repo,
                        blob,
                        ordinal as i64,
                        symbol.name,
                        symbol.kind,
                        symbol.start_line,
                        symbol.end_line,
                        symbol.source,
                    ],
                )?;
            }
            for (ordinal, reference) in refs.iter().enumerate() {
                tx.execute(
                    "INSERT INTO code_refs (repo, blob, ordinal, name, caller, line, kind, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        repo,
                        blob,
                        ordinal as i64,
                        reference.name,
                        reference.caller,
                        reference.line,
                        reference.kind,
                        reference.source,
                    ],
                )?;
            }
            tx.commit()
        })
    }

    /// Point the repo's path table at exactly this set of files, dropping any path
    /// that is no longer in the tree. Content that no path references any more is
    /// swept here too, so a deleted file stops answering queries.
    pub fn code_set_files(&self, repo: &str, files: &[(String, String, String)]) -> DbResult<()> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM code_files WHERE repo = ?1", params![repo])?;
            for (path, blob, lang) in files {
                tx.execute(
                    "INSERT OR REPLACE INTO code_files (repo, path, blob, lang)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![repo, path, blob, lang],
                )?;
            }
            for table in ["code_chunks", "code_symbols", "code_refs", "code_seen_blobs"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE repo = ?1 AND blob NOT IN
                         (SELECT blob FROM code_files WHERE repo = ?1)"
                    ),
                    params![repo],
                )?;
            }
            tx.commit()
        })
    }

    /// Just enough of every embedded chunk to score it: an identity, a path, and the
    /// vector.
    ///
    /// Deliberately not the snippet. A snippet runs to eight kilobytes and the scan
    /// touches every chunk, so carrying them would move megabytes of text through the
    /// global connection mutex to rank a handful of rows. The top-k snippets are
    /// fetched afterwards by id, which is a few rows instead of all of them.
    pub fn code_chunk_vectors(&self, repo: &str) -> DbResult<Vec<ChunkVector>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT c.rowid, f.path, c.vector
                 FROM code_chunks c JOIN code_files f ON f.repo = c.repo AND f.blob = c.blob
                 WHERE c.repo = ?1 AND c.vector IS NOT NULL
                 ORDER BY f.path, c.ordinal",
            )?;
            let rows = statement
                .query_map(params![repo], |row| {
                    let raw: Vec<u8> = row.get(2)?;
                    Ok(ChunkVector {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        vector: decode_vector(&raw),
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The full rows behind a handful of ids, keyed by id so the caller can put them
    /// back in score order.
    pub fn code_chunks_by_id(
        &self,
        repo: &str,
        ids: &[i64],
    ) -> DbResult<std::collections::HashMap<i64, StoredChunk>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT c.rowid, f.path, c.symbol, c.start_line, c.end_line, c.snippet,
                        c.vector, c.model
                 FROM code_chunks c JOIN code_files f ON f.repo = c.repo AND f.blob = c.blob
                 WHERE c.repo = ?1 AND c.rowid = ?2",
            )?;
            let mut found = std::collections::HashMap::with_capacity(ids.len());
            for id in ids {
                let row = statement
                    .query_row(params![repo, id], |row| {
                        let raw: Option<Vec<u8>> = row.get(6)?;
                        Ok((
                            row.get::<_, i64>(0)?,
                            StoredChunk {
                                path: row.get(1)?,
                                symbol: row.get(2)?,
                                start_line: row.get(3)?,
                                end_line: row.get(4)?,
                                snippet: row.get(5)?,
                                vector: raw.as_deref().map(decode_vector),
                                model: row.get(7)?,
                            },
                        ))
                    })
                    .optional()?;
                if let Some((id, chunk)) = row {
                    found.insert(id, chunk);
                }
            }
            Ok(found)
        })
    }

    /// Where a symbol is defined. Exact name match; a repo that never indexed
    /// returns an empty list, which is an answer, not an error.
    pub fn code_definitions(&self, repo: &str, name: &str) -> DbResult<Vec<SymbolHit>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT f.path, s.name, s.kind, s.start_line, s.end_line, s.source
                 FROM code_symbols s JOIN code_files f ON f.repo = s.repo AND f.blob = s.blob
                 WHERE s.repo = ?1 AND s.name = ?2
                 ORDER BY f.path, s.start_line",
            )?;
            let rows = statement
                .query_map(params![repo, name], |row| {
                    Ok(SymbolHit {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        kind: row.get(2)?,
                        start_line: row.get(3)?,
                        end_line: row.get(4)?,
                        source: row.get(5)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Every reference to a symbol. `calls_only` narrows it to call edges, which is
    /// what `/code/callers` is built from.
    pub fn code_references(
        &self,
        repo: &str,
        name: &str,
        calls_only: bool,
    ) -> DbResult<Vec<RefHit>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT f.path, r.name, r.caller, r.line, r.kind, r.source
                 FROM code_refs r JOIN code_files f ON f.repo = r.repo AND f.blob = r.blob
                 WHERE r.repo = ?1 AND r.name = ?2 AND (?3 = 0 OR r.kind = 'call')
                 ORDER BY f.path, r.line",
            )?;
            let rows = statement
                .query_map(params![repo, name, i64::from(calls_only)], |row| {
                    Ok(RefHit {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        caller: row.get(2)?,
                        line: row.get(3)?,
                        kind: row.get(4)?,
                        source: row.get(5)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The repo's whole file inventory: path, language, and the blob behind it.
    pub fn code_file_list(&self, repo: &str) -> DbResult<Vec<CodeFile>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT path, lang, blob FROM code_files WHERE repo = ?1 ORDER BY path",
            )?;
            let rows = statement
                .query_map(params![repo], |row| {
                    Ok(CodeFile { path: row.get(0)?, lang: row.get(1)?, blob: row.get(2)? })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Every definition in the repo, resolved to the path it now lives at.
    pub fn code_all_symbols(&self, repo: &str) -> DbResult<Vec<SymbolHit>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT f.path, s.name, s.kind, s.start_line, s.end_line, s.source
                 FROM code_symbols s JOIN code_files f ON f.repo = s.repo AND f.blob = s.blob
                 WHERE s.repo = ?1
                 ORDER BY f.path, s.start_line, s.name",
            )?;
            let rows = statement
                .query_map(params![repo], |row| {
                    Ok(SymbolHit {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        kind: row.get(2)?,
                        start_line: row.get(3)?,
                        end_line: row.get(4)?,
                        source: row.get(5)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Every reference in the repo, resolved to its path.
    pub fn code_all_refs(&self, repo: &str) -> DbResult<Vec<RefHit>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT f.path, r.name, r.caller, r.line, r.kind, r.source
                 FROM code_refs r JOIN code_files f ON f.repo = r.repo AND f.blob = r.blob
                 WHERE r.repo = ?1
                 ORDER BY f.path, r.line, r.name",
            )?;
            let rows = statement
                .query_map(params![repo], |row| {
                    Ok(RefHit {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        caller: row.get(2)?,
                        line: row.get(3)?,
                        kind: row.get(4)?,
                        source: row.get(5)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Record what an index run did. `/brain` reads the latest of these.
    pub fn code_record_run(&self, run: &CodeRun) -> DbResult<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO code_runs
                    (repo, commit_id, files_seen, files_indexed, chunks, symbols, embedded, note, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    run.repo,
                    run.commit,
                    run.files_seen,
                    run.files_indexed,
                    run.chunks,
                    run.symbols,
                    run.embedded,
                    run.note,
                    now()
                ],
            )?;
            Ok(())
        })
    }

    /// The most recent index run for a repo.
    pub fn code_last_run(&self, repo: &str) -> DbResult<Option<CodeRunRow>> {
        self.with(|conn| {
            conn.query_row(
                "SELECT repo, commit_id, files_seen, files_indexed, chunks, symbols, embedded,
                        note, created_at
                 FROM code_runs WHERE repo = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![repo],
                |row| {
                    Ok(CodeRunRow {
                        run: CodeRun {
                            repo: row.get(0)?,
                            commit: row.get(1)?,
                            files_seen: row.get(2)?,
                            files_indexed: row.get(3)?,
                            chunks: row.get(4)?,
                            symbols: row.get(5)?,
                            embedded: row.get(6)?,
                            note: row.get(7)?,
                        },
                        created_at: row.get(8)?,
                    })
                },
            )
            .optional()
        })
    }

    /// Chunk, symbol, embedded-chunk, and file counts for a repo, in one pass.
    pub fn code_counts(&self, repo: &str) -> DbResult<CodeCounts> {
        self.with(|conn| {
            let scoped = |table: &str| {
                format!(
                    "SELECT COUNT(*) FROM {table} t
                     WHERE t.repo = ?1
                       AND EXISTS (SELECT 1 FROM code_files f
                                   WHERE f.repo = t.repo AND f.blob = t.blob)"
                )
            };
            Ok(CodeCounts {
                files: conn.query_row(
                    "SELECT COUNT(*) FROM code_files WHERE repo = ?1",
                    params![repo],
                    |row| row.get(0),
                )?,
                chunks: conn.query_row(&scoped("code_chunks"), params![repo], |row| row.get(0))?,
                symbols: conn.query_row(&scoped("code_symbols"), params![repo], |row| row.get(0))?,
                embedded: conn.query_row(
                    "SELECT COUNT(*) FROM code_chunks c WHERE c.repo = ?1 AND c.vector IS NOT NULL
                       AND EXISTS (SELECT 1 FROM code_files f
                                   WHERE f.repo = c.repo AND f.blob = c.blob)",
                    params![repo],
                    |row| row.get(0),
                )?,
            })
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
        updated_at: row.get(8)?,
        note: row.get(9)?,
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

/// One prompt a person wrote, with enough context to jump back into its trace.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Prompt {
    pub session: String,
    pub seq: i64,
    pub text: String,
    /// The repo HEAD when it was written, so the page can show what followed.
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

// ---- code intelligence rows --------------------------------------------------------

/// One embeddable piece of a file, on its way in.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeChunk {
    /// The function or class the chunk is, when tree-sitter found one.
    pub symbol: Option<String>,
    /// One-based, inclusive.
    pub start_line: i64,
    pub end_line: i64,
    pub snippet: String,
    /// `None` when the chunk was stored without an embedder available.
    pub vector: Option<Vec<f32>>,
    pub model: String,
}

/// One definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSymbol {
    pub name: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub source: String,
}

/// One use of a name, with the function it happened inside when that is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRef {
    pub name: String,
    pub caller: Option<String>,
    pub line: i64,
    /// `call` or `ref`.
    pub kind: String,
    pub source: String,
}

/// Everything one blob contributes, written as a unit.
#[derive(Debug, Clone, Default)]
pub struct CodeBlob {
    pub repo: String,
    pub blob: String,
    pub chunks: Vec<CodeChunk>,
    pub symbols: Vec<CodeSymbol>,
    pub refs: Vec<CodeRef>,
}

/// A chunk reduced to what scoring needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkVector {
    /// The chunk row's own id, for fetching the rest of it once it has placed.
    pub id: i64,
    pub path: String,
    pub vector: Vec<f32>,
}

/// A chunk read back, resolved to the path it now lives at.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredChunk {
    pub path: String,
    pub symbol: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub snippet: String,
    pub vector: Option<Vec<f32>>,
    pub model: String,
}

/// One file in the index, as the bulk dump reports it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CodeFile {
    pub path: String,
    pub lang: String,
    pub blob: String,
}

/// A definition read back.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub source: String,
}

/// A reference read back.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RefHit {
    pub path: String,
    pub name: String,
    pub caller: Option<String>,
    pub line: i64,
    pub kind: String,
    pub source: String,
}

/// What one index run did.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct CodeRun {
    pub repo: String,
    pub commit: String,
    pub files_seen: i64,
    /// Files whose blob was new to this repo, so actually parsed and embedded.
    pub files_indexed: i64,
    pub chunks: i64,
    pub symbols: i64,
    pub embedded: i64,
    /// Anything that degraded, in words. Empty when everything ran.
    pub note: String,
}

/// An index run with the time it finished.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CodeRunRow {
    #[serde(flatten)]
    pub run: CodeRun,
    pub created_at: String,
}

/// What a repo currently holds.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct CodeCounts {
    pub files: i64,
    pub chunks: i64,
    pub symbols: i64,
    pub embedded: i64,
}

/// Vectors are stored as raw little-endian f32, which is both the smallest and the
/// cheapest form to scan: decoding is a copy, not a parse.
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect()
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

/// Columns `ci_runs` gained after the first release. An old database predates the
/// heartbeat; its rows read as stale, which is what an abandoned run is.
const CI_RUN_COLUMNS: &[(&str, &str, &str)] = &[
    ("ci_runs", "updated_at", "TEXT NOT NULL DEFAULT ''"),
    ("ci_runs", "note", "TEXT NOT NULL DEFAULT ''"),
];

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
    created_at  TEXT NOT NULL,
    -- The heartbeat, and why a run ended when no log says so.
    updated_at  TEXT NOT NULL DEFAULT '',
    note        TEXT NOT NULL DEFAULT ''
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

-- ---- code intelligence -----------------------------------------------------
--
-- Everything derived from a file's *content* is keyed by its blob SHA, and only
-- the path table knows where that content currently lives. An index run that
-- meets a blob it has already seen does no work at all, and a renamed file keeps
-- its chunks, its vectors, and its symbols.

CREATE TABLE IF NOT EXISTS code_files (
    repo TEXT NOT NULL,
    path TEXT NOT NULL,
    blob TEXT NOT NULL,
    lang TEXT NOT NULL,
    PRIMARY KEY (repo, path)
);
CREATE INDEX IF NOT EXISTS code_files_blob ON code_files (repo, blob);

-- Every blob this repo has ever parsed, including the ones that yielded nothing.
-- Without it, a file that produces no chunks looks unindexed forever and is re-read
-- on every run.
CREATE TABLE IF NOT EXISTS code_seen_blobs (
    repo TEXT NOT NULL,
    blob TEXT NOT NULL,
    PRIMARY KEY (repo, blob)
);

CREATE TABLE IF NOT EXISTS code_chunks (
    repo       TEXT NOT NULL,
    blob       TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    symbol     TEXT,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL,
    snippet    TEXT NOT NULL,
    -- Little-endian f32s. NULL when the chunk is stored but not yet embedded.
    vector     BLOB,
    dims       INTEGER NOT NULL DEFAULT 0,
    model      TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (repo, blob, ordinal)
);

CREATE TABLE IF NOT EXISTS code_symbols (
    repo       TEXT NOT NULL,
    blob       TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL,
    -- `treesitter` for the in-process pass, `scip` for the accurate overlay.
    source     TEXT NOT NULL,
    PRIMARY KEY (repo, blob, ordinal)
);
CREATE INDEX IF NOT EXISTS code_symbols_name ON code_symbols (repo, name);

CREATE TABLE IF NOT EXISTS code_refs (
    repo    TEXT NOT NULL,
    blob    TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    name    TEXT NOT NULL,
    caller  TEXT,
    line    INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    source  TEXT NOT NULL,
    PRIMARY KEY (repo, blob, ordinal)
);
CREATE INDEX IF NOT EXISTS code_refs_name ON code_refs (repo, name);

CREATE TABLE IF NOT EXISTS code_runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT NOT NULL,
    commit_id     TEXT NOT NULL,
    files_seen    INTEGER NOT NULL,
    files_indexed INTEGER NOT NULL,
    chunks        INTEGER NOT NULL,
    symbols       INTEGER NOT NULL,
    embedded      INTEGER NOT NULL,
    note          TEXT NOT NULL,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS code_runs_repo ON code_runs (repo, created_at);

CREATE TABLE IF NOT EXISTS architecture_submissions (
    id         INTEGER PRIMARY KEY,
    repo       TEXT NOT NULL,
    mermaid    TEXT NOT NULL,
    title      TEXT,
    note       TEXT,
    author     TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS architecture_repo ON architecture_submissions (repo, created_at, id);

"#;

// ---- architecture ----------------------------------------------------------------

/// A submitted diagram. Exactly the JSON `GET /{repo}/architecture` returns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchitectureSubmission {
    pub id: i64,
    pub repo: String,
    /// The diagram source, stored verbatim. Untrusted: never splice it into HTML.
    pub mermaid: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub author: String,
    pub created_at: String,
}

/// One line of the history list — everything but the diagram itself.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArchitectureEntry {
    pub id: i64,
    pub title: Option<String>,
    pub author: String,
    pub created_at: String,
}

/// A submission on its way in.
#[derive(Debug, Clone)]
pub struct NewArchitecture {
    pub repo: String,
    pub mermaid: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub author: String,
}

impl Db {
    /// Store a diagram and hand back the stored row. Append-only: nothing updates.
    pub fn add_architecture(&self, new: NewArchitecture) -> DbResult<ArchitectureSubmission> {
        let created_at = now();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO architecture_submissions (repo, mermaid, title, note, author, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![new.repo, new.mermaid, new.title, new.note, new.author, created_at],
            )?;
            Ok(ArchitectureSubmission {
                id: conn.last_insert_rowid(),
                repo: new.repo.clone(),
                mermaid: new.mermaid.clone(),
                title: new.title.clone(),
                note: new.note.clone(),
                author: new.author.clone(),
                created_at: created_at.clone(),
            })
        })
    }

    /// The newest submission for a repo, which is what the tab renders.
    pub fn latest_architecture(&self, repo: &str) -> DbResult<Option<ArchitectureSubmission>> {
        self.with(|conn| {
            conn.query_row(
                "SELECT id, repo, mermaid, title, note, author, created_at
                 FROM architecture_submissions WHERE repo = ?1
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![repo],
                read_architecture,
            )
            .optional()
        })
    }

    /// One submission by id, scoped to its repo.
    pub fn architecture(&self, repo: &str, id: i64) -> DbResult<Option<ArchitectureSubmission>> {
        self.with(|conn| {
            conn.query_row(
                "SELECT id, repo, mermaid, title, note, author, created_at
                 FROM architecture_submissions WHERE repo = ?1 AND id = ?2",
                params![repo, id],
                read_architecture,
            )
            .optional()
        })
    }

    /// Every submission, newest first, without the diagram sources.
    pub fn architecture_history(&self, repo: &str) -> DbResult<Vec<ArchitectureEntry>> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, title, author, created_at FROM architecture_submissions
                 WHERE repo = ?1 ORDER BY created_at DESC, id DESC",
            )?;
            let rows = statement
                .query_map(params![repo], |row| {
                    Ok(ArchitectureEntry {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        author: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })?
                .collect::<DbResult<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

fn read_architecture(row: &rusqlite::Row<'_>) -> DbResult<ArchitectureSubmission> {
    Ok(ArchitectureSubmission {
        id: row.get(0)?,
        repo: row.get(1)?,
        mermaid: row.get(2)?,
        title: row.get(3)?,
        note: row.get(4)?,
        author: row.get(5)?,
        created_at: row.get(6)?,
    })
}

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
        // Nothing is running it either, and nothing ever will be.
        assert!(!status::blocks_merge(Some(status::STUCK)));
    }

    #[test]
    fn a_heartbeat_that_stopped_makes_a_running_run_stuck() {
        let db = Db::in_memory().unwrap();
        let id = db.enqueue_run("demo", "main", "abc").unwrap();
        db.set_run_status(id, status::RUNNING, 0, None).unwrap();
        let fresh = db.latest_run("demo", "abc").unwrap().unwrap();
        assert_eq!(fresh.effective_status(), status::RUNNING);

        let stale = CiRun { updated_at: now_offset(-status::HEARTBEAT_STALE_SECS - 1), ..fresh };
        assert_eq!(stale.effective_status(), status::STUCK);

        // The requeue puts the same row back in the queue, commit and all.
        assert!(db.requeue_run(id).unwrap());
        let back = db.latest_run("demo", "abc").unwrap().unwrap();
        assert_eq!(back.status, status::QUEUED);
        assert_eq!(back.commit, "abc");
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
