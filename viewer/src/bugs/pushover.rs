//! Pushover: the one way nashcode tells a person that something changed.
//!
//! Two halves that never touch each other's state except through SQLite. The
//! **notifier** runs inside the digest and decides whether a landing is worth a
//! human's attention; it writes a row and returns. The **sender** is a single task
//! that drains those rows to the API. Nothing on the request path ever waits for a
//! phone.
//!
//! The rule that matters most is what does *not* get sent. Notifications go out on
//! state changes — a new issue, a regression, an unmute — plus one extra at each rung
//! of the escalation ladder. Never per event. An issue taking a thousand events an
//! hour is one push, not a thousand; that is the difference between a tool you keep
//! notifications on for and one you mute for ever.
//!
//! Failure handling is deliberately asymmetric, and the asymmetry comes from what
//! Pushover means by each answer. A 5xx is "I did not read this", so it retries. A
//! 4xx is "I read this and it is wrong" — a bad token, a message that breaks a rule —
//! and retrying it forever would wedge the queue behind a message that can never
//! succeed. A 429 is neither: the message was fine and the account is out of budget,
//! so the whole queue parks until the stated reset and nothing is lost.

use std::time::Duration;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::bugs::group;
use crate::bugs::index::{Issue, Landing, Project, state};
use crate::config::Pushover as PushoverConfig;
use crate::db::{Db, DbResult, from_unix, now, now_offset};

/// Pushover truncates a longer title itself; we would rather choose where the cut is.
pub const TITLE_MAX: usize = 250;
/// The API's message limit. A message over it is rejected outright, which is a 4xx,
/// which is final — so the clip happens here, before the row is ever written.
pub const MESSAGE_MAX: usize = 1024;
/// The escalation ladder. One extra push as an unresolved issue crosses each rung.
pub const LADDER: [i64; 3] = [10, 100, 1000];
/// How many messages may leave in one rolling hour before the queue parks itself.
pub const HOURLY_CAP: i64 = 20;
/// The floor on a retry after a 5xx, per the goal doc. Doubling from here.
pub const RETRY_FLOOR_SECS: i64 = 5;
/// The ceiling on that doubling. A quarter of an hour is long enough to ride out an
/// outage and short enough that recovery is not an overnight wait.
pub const RETRY_CEILING_SECS: i64 = 900;
/// How many times a message may fail transiently before it is given up on. Twelve
/// doublings off a 5-second floor is a little over two hours of trying.
pub const MAX_ATTEMPTS: i64 = 12;
/// How long the queue parks when a 429 arrives with no readable reset header.
pub const BLIND_PARK_SECS: i64 = 300;

/// Why a message was queued. Stored so a person reading the table can see the shape of
/// a noisy week without opening every row.
pub mod reason {
    pub const NEW: &str = "new";
    pub const REGRESSION: &str = "regression";
    pub const UNMUTE: &str = "unmute";
    pub const LADDER: &str = "ladder";
    pub const SUPPRESSED: &str = "suppressed";
}

// ---- schema ------------------------------------------------------------------------

pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute_batch(SCHEMA)?;
        Ok(())
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bugs_pushes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER,
    issue_id    INTEGER,
    reason      TEXT NOT NULL,
    dedupe_key  TEXT NOT NULL,
    title       TEXT NOT NULL,
    message     TEXT NOT NULL,
    url         TEXT,
    priority    INTEGER NOT NULL DEFAULT 0,
    queued_at   TEXT NOT NULL,
    not_before  TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    sent_at     TEXT,
    failed_at   TEXT,
    last_error  TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS bugs_pushes_dedupe ON bugs_pushes (dedupe_key);
CREATE INDEX IF NOT EXISTS bugs_pushes_pending
    ON bugs_pushes (sent_at, failed_at, not_before, id);

CREATE TABLE IF NOT EXISTS bugs_push_state (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    parked_until  TEXT,
    parked_reason TEXT,
    app_limit     INTEGER,
    remaining     INTEGER,
    reset_at      TEXT,
    suppressed_at TEXT
);
"#;

// ---- the notifier ------------------------------------------------------------------

/// Decides what deserves a push and writes the row. Cheap to clone; holds no
/// connection and does no I/O beyond one SQLite insert.
#[derive(Clone, Debug)]
pub struct Notifier {
    /// False when no credentials are configured. A disabled notifier writes nothing at
    /// all rather than filling a table nobody will ever drain.
    on: bool,
    /// Where an issue lives, from outside this box.
    public_url: String,
}

impl Notifier {
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            on: config.pushover.is_some(),
            public_url: config.public_url.trim_end_matches('/').to_owned(),
        }
    }

    /// A notifier that never queues anything. For the paths that have no config to
    /// hand and no business notifying anyone.
    pub fn off() -> Self {
        Self { on: false, public_url: String::new() }
    }

    pub fn enabled(&self) -> bool {
        self.on
    }

    /// The choke point. Every event the digest indexes comes through here exactly
    /// once, and almost all of them leave without a message.
    ///
    /// `event` is the raw event JSON, for the exception value and the tags. It is
    /// optional because the ladder can fire from a replay that has no payload to hand.
    ///
    /// `added` is how many events this landing put on the issue. The digest adds one,
    /// always, because it is a single writer — but that invariant lives in another
    /// module, and the ladder depends on it: a writer that added five at once would
    /// step straight over a rung and the push would silently never happen. Passing the
    /// step makes the dependency an argument instead of an assumption.
    pub fn landed(
        &self,
        db: &Db,
        project: &Project,
        issue: &Issue,
        landing: Landing,
        added: i64,
        event: Option<&Value>,
    ) {
        if !self.on {
            return;
        }
        for queued in self.messages_for(project, issue, landing, added, event) {
            if let Err(error) = enqueue(db, &queued) {
                // A notification that cannot be queued is not worth failing a digest
                // over: the event itself is indexed and the issue is on the page.
                tracing::warn!(%error, issue = issue.id, "bugs: cannot queue a notification");
            }
        }
    }

    /// An issue has come off mute and is interesting again. Nothing evaluates mutes
    /// yet — that is the crons-and-quotas slice — so this is the door standing ready
    /// rather than a path anything walks today.
    pub fn unmuted(&self, db: &Db, project: &Project, issue: &Issue) {
        if !self.on {
            return;
        }
        let queued = Queued {
            project_id: project.id,
            issue_id: Some(issue.id),
            reason: reason::UNMUTE,
            dedupe_key: format!("issue/{}/unmute/{}", issue.id, issue.events),
            title: title_for(project, issue),
            message: message_for(issue, None, "Unmuted."),
            url: Some(self.issue_url(project, issue)),
            priority: priority_for(issue),
        };
        if let Err(error) = enqueue(db, &queued) {
            tracing::warn!(%error, issue = issue.id, "bugs: cannot queue an unmute notification");
        }
    }

    /// Which messages one landing earns. Pure, so the rules are testable without a
    /// database or a phone.
    fn messages_for(
        &self,
        project: &Project,
        issue: &Issue,
        landing: Landing,
        added: i64,
        event: Option<&Value>,
    ) -> Vec<Queued> {
        let mut queued = Vec::new();
        let url = self.issue_url(project, issue);
        match landing {
            // A duplicate event id is a client retry. Nothing happened.
            Landing::Duplicate => return queued,
            Landing::New => queued.push(Queued {
                project_id: project.id,
                issue_id: Some(issue.id),
                reason: reason::NEW,
                dedupe_key: format!("issue/{}/new", issue.id),
                title: title_for(project, issue),
                message: message_for(issue, event, "New issue."),
                url: Some(url.clone()),
                priority: priority_for(issue),
            }),
            Landing::Regression => queued.push(Queued {
                project_id: project.id,
                issue_id: Some(issue.id),
                reason: reason::REGRESSION,
                // The event count at the moment of the regression is what makes this
                // key unique per regression, so an issue that is fixed and breaks
                // again three times rings three times — and a second event on the
                // reopened issue rings not at all.
                dedupe_key: format!("issue/{}/regression/{}", issue.id, issue.events),
                title: title_for(project, issue),
                message: message_for(issue, event, "Regression: this was resolved and came back."),
                url: Some(url.clone()),
                priority: priority_for(issue),
            }),
            Landing::Repeat => {}
        }

        // The ladder. Only for an issue somebody could still act on: a muted or
        // resolved one is a decision already taken, and a rung is not news.
        //
        // A *range*, not equality on the count. Equality is correct only because the
        // digest is a single writer that adds exactly one, which is an invariant living
        // in another module: the day a batch import adds five at once, equality would
        // step straight over a rung and the push would never happen. Asking whether the
        // rung is inside the step costs nothing and does not care how big the step is.
        let before = issue.events - added.max(0);
        if issue.state == state::UNRESOLVED
            && let Some(rung) =
                LADDER.iter().copied().find(|rung| before < *rung && *rung <= issue.events)
        {
            queued.push(Queued {
                project_id: project.id,
                issue_id: Some(issue.id),
                reason: reason::LADDER,
                // Once ever per rung: the counter only goes up, so a rung is crossed
                // exactly once in an issue's life.
                dedupe_key: format!("issue/{}/ladder/{rung}", issue.id),
                title: title_for(project, issue),
                message: message_for(issue, event, &format!("{rung} events and still open.")),
                url: Some(url),
                priority: priority_for(issue),
            });
        }
        queued
    }

    fn issue_url(&self, project: &Project, issue: &Issue) -> String {
        format!("{}/bugs/{}/issues/{}", self.public_url, project.name, issue.id)
    }
}

/// `{project}: {issue title}`, clipped to what the API takes.
fn title_for(project: &Project, issue: &Issue) -> String {
    clip(&format!("{}: {}", project.name, issue.title), TITLE_MAX)
}

/// Priority 0, or 1 for a fatal. Emergency (2) is a per-project opt-in and is not
/// built: it needs a `cancel_by_tag` on resolve to be anything but an alarm nobody can
/// switch off.
fn priority_for(issue: &Issue) -> i64 {
    i64::from(issue.level == "fatal")
}

/// The message body: what happened, the exception value, and a few tags.
///
/// The exception value is the part that can be any length, so it is the part that gets
/// clipped. Everything else is short and fixed, and it is where the answers are: a
/// four-kilobyte Python repr that pushes `environment=prod` off the end has spent the
/// whole message saying less than the first line did.
///
/// Never empty, either. Pushover answers 4xx to an empty message and a 4xx is final,
/// so an event with nothing readable in it still gets its issue title.
fn message_for(issue: &Issue, event: Option<&Value>, lead: &str) -> String {
    let tags = tag_line(event);
    let value = event
        .and_then(exception_value)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| issue.title.clone());

    // Two newlines' worth of separator, whether or not both parts are there.
    let fixed = lead.chars().count() + tags.chars().count() + 2;
    let room = MESSAGE_MAX.saturating_sub(fixed);

    let mut lines = vec![lead.to_owned()];
    if !value.trim().is_empty() && value != lead && room > 0 {
        lines.push(clip(&value, room));
    }
    if !tags.is_empty() {
        lines.push(tags);
    }
    let body = lines.join("\n");
    let body = if body.trim().is_empty() { issue.title.clone() } else { body };
    let body = if body.trim().is_empty() { "an error".to_owned() } else { body };
    clip(&body, MESSAGE_MAX)
}

/// The exception value, or whatever else the event says in words.
fn exception_value(event: &Value) -> Option<String> {
    if let Some(exception) = group::last_exception(event) {
        let ty = exception.get("type").and_then(Value::as_str);
        let value = exception.get("value").and_then(Value::as_str);
        match (ty, value) {
            (Some(ty), Some(value)) => return Some(format!("{ty}: {value}")),
            (None, Some(value)) => return Some(value.to_owned()),
            (Some(ty), None) => return Some(ty.to_owned()),
            (None, None) => {}
        }
    }
    event
        .get("logentry")
        .and_then(|entry| entry.get("formatted"))
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A few tags is a few: enough to say which host and which release, not the whole
/// context. The message cap is shared with the exception value, and the value is the
/// part a person reads first.
const TAGS_SHOWN: usize = 4;

/// How much of one tag value is worth carrying. A tag is a label, and a label that
/// needs more than this is not answering "where and which build".
const TAG_VALUE_MAX: usize = 64;

fn tag_line(event: Option<&Value>) -> String {
    let Some(tags) = event.and_then(|event| event.get("tags")).and_then(Value::as_object) else {
        return String::new();
    };
    // Ordered by usefulness, then whatever else is there. `environment` and `release`
    // answer "where and which build" — the two questions a phone notification is for.
    let preferred = ["environment", "release", "server_name", "level"];
    let mut chosen: Vec<String> = Vec::new();
    for key in preferred {
        if let Some(value) = tags.get(key).and_then(render_tag) {
            chosen.push(format!("{}={value}", render_key(key)));
        }
    }
    for (key, value) in tags {
        if chosen.len() >= TAGS_SHOWN {
            break;
        }
        if preferred.contains(&key.as_str()) {
            continue;
        }
        if let Some(value) = render_tag(value) {
            chosen.push(format!("{}={value}", render_key(key)));
        }
    }
    chosen.truncate(TAGS_SHOWN);
    chosen.join(" · ")
}

/// A tag key is sender-controlled too, and a four-kilobyte key would spend the whole
/// message budget before the exception value got a character. Same rule as the value.
fn render_key(key: &str) -> String {
    clip(key, TAG_VALUE_MAX)
}

fn render_tag(value: &Value) -> Option<String> {
    let rendered = match value {
        Value::String(text) if text.trim().is_empty() => return None,
        Value::String(text) => text.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    Some(clip(&rendered, TAG_VALUE_MAX))
}

/// Truncate to `max` characters without splitting one, and say so when it happened.
///
/// Bytes would be the wrong unit twice over: a cut inside a multi-byte character is
/// not a string at all, and Pushover counts characters.
fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(1);
    text.chars().take(keep).collect::<String>() + "…"
}

// ---- the queue ---------------------------------------------------------------------

/// One message on its way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub project_id: i64,
    pub issue_id: Option<i64>,
    pub reason: &'static str,
    /// What makes this message this message. A repeat write is thrown away, which is
    /// what keeps a replayed envelope or a re-digested backlog from ringing twice.
    pub dedupe_key: String,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub priority: i64,
}

/// Queue a message. `Ok(false)` means the dedupe key was already there, which is a
/// success: the notification has already been decided.
pub fn enqueue(db: &Db, queued: &Queued) -> DbResult<bool> {
    db.with(|conn| {
        let changed = conn.execute(
            "INSERT OR IGNORE INTO bugs_pushes
               (project_id, issue_id, reason, dedupe_key, title, message, url, priority,
                queued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                queued.project_id,
                queued.issue_id,
                queued.reason,
                queued.dedupe_key,
                queued.title,
                queued.message,
                queued.url,
                queued.priority,
                now(),
            ],
        )?;
        Ok(changed > 0)
    })
}

/// A row waiting to go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: i64,
    pub reason: String,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub priority: i64,
    pub attempts: i64,
}

/// How long a claimed message is invisible to another sender.
///
/// Longer than the HTTP timeout, so a sender that is merely slow keeps its own row; short
/// enough that a sender which died mid-send hands the row back inside a minute.
pub const LEASE_SECS: i64 = 60;

/// Take the next message, and take it *exclusively*.
///
/// The select and the claim are one statement, so two senders cannot both read the same
/// row and both post it. Nothing today runs two — there is one task per process — but
/// "one process" is a deployment property, not a code property: a restart that overlaps
/// its predecessor, or a second viewer pointed at the same SQLite file, would otherwise
/// each send every pending notification. The claim is one `UPDATE ... RETURNING` and
/// costs nothing, which is the argument for doing it now rather than after somebody's
/// phone has buzzed twice.
///
/// The lease is written into `not_before`, which the ordinary retry path already
/// respects — so a sender that dies holding a claim releases it by doing nothing.
fn next_pending(db: &Db) -> DbResult<Option<Pending>> {
    let now = now();
    let lease = now_offset(LEASE_SECS);
    db.with(|conn| {
        conn.query_row(
            "UPDATE bugs_pushes SET not_before = ?2
             WHERE id = (
                 SELECT id FROM bugs_pushes
                 WHERE sent_at IS NULL AND failed_at IS NULL
                   AND (not_before IS NULL OR not_before <= ?1)
                 ORDER BY COALESCE(not_before, queued_at), id
                 LIMIT 1
             )
             RETURNING id, reason, title, message, url, priority, attempts",
            params![now, lease],
            |row| {
                Ok(Pending {
                    id: row.get(0)?,
                    reason: row.get(1)?,
                    title: row.get(2)?,
                    message: row.get(3)?,
                    url: row.get(4)?,
                    priority: row.get(5)?,
                    attempts: row.get(6)?,
                })
            },
        )
        .optional()
    })
}

/// How many messages are still waiting, deferred ones included. What the suppression
/// notice counts.
pub fn pending_count(db: &Db) -> DbResult<i64> {
    db.with(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM bugs_pushes WHERE sent_at IS NULL AND failed_at IS NULL",
            [],
            |row| row.get(0),
        )
    })
}

/// One row of the outbound log, whatever became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    pub id: i64,
    pub reason: String,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub priority: i64,
    pub queued_at: String,
    pub sent_at: Option<String>,
    pub failed_at: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// What has been queued, newest first. The audit trail for "why did my phone ring",
/// and for "why did it not".
pub fn history(db: &Db, limit: i64) -> DbResult<Vec<Record>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, reason, title, message, url, priority, queued_at, sent_at, failed_at,
                    attempts, last_error
             FROM bugs_pushes ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(Record {
                id: row.get(0)?,
                reason: row.get(1)?,
                title: row.get(2)?,
                message: row.get(3)?,
                url: row.get(4)?,
                priority: row.get(5)?,
                queued_at: row.get(6)?,
                sent_at: row.get(7)?,
                failed_at: row.get(8)?,
                attempts: row.get(9)?,
                last_error: row.get(10)?,
            })
        })?;
        rows.collect()
    })
}

fn mark_sent(db: &Db, id: i64) -> DbResult<()> {
    db.with(|conn| {
        conn.execute(
            "UPDATE bugs_pushes SET sent_at = ?2, attempts = attempts + 1, last_error = NULL
             WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    })
}

fn mark_failed(db: &Db, id: i64, why: &str) -> DbResult<()> {
    db.with(|conn| {
        conn.execute(
            "UPDATE bugs_pushes
             SET failed_at = ?2, attempts = attempts + 1, last_error = ?3
             WHERE id = ?1",
            params![id, now(), why],
        )?;
        Ok(())
    })
}

fn defer(db: &Db, id: i64, seconds: i64, why: &str) -> DbResult<()> {
    db.with(|conn| {
        conn.execute(
            "UPDATE bugs_pushes
             SET attempts = attempts + 1, not_before = ?2, last_error = ?3
             WHERE id = ?1",
            params![id, now_offset(seconds), why],
        )?;
        Ok(())
    })
}

/// The delay before attempt `attempts + 1`: doubling off the floor, capped.
pub fn backoff_secs(attempts: i64) -> i64 {
    let steps = attempts.clamp(0, 16) as u32;
    RETRY_FLOOR_SECS.saturating_mul(1i64 << steps.min(16)).min(RETRY_CEILING_SECS)
}

// ---- queue-wide state --------------------------------------------------------------

/// What the account has left, and whether the queue is allowed to move.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Budget {
    /// `X-Limit-App-Limit`: the monthly allowance for this application token.
    pub limit: Option<i64>,
    /// `X-Limit-App-Remaining`: what is left of it. The number that decides whether a
    /// real incident reaches a phone.
    pub remaining: Option<i64>,
    /// When the allowance resets.
    pub reset_at: Option<String>,
    /// Set while the queue is parked, with the reason in [`Budget::parked_reason`].
    pub parked_until: Option<String>,
    pub parked_reason: Option<String>,
    /// Messages waiting to go out.
    pub pending: i64,
    /// When the hourly cap last said so. Serialized because "you are being throttled"
    /// is the one thing a page about notifications has to admit.
    pub suppressed_at: Option<String>,
}

/// The budget as the project list and the brain stanza show it.
pub fn budget(db: &Db) -> DbResult<Budget> {
    let mut budget = read_state(db)?;
    budget.pending = pending_count(db)?;
    Ok(budget)
}

fn read_state(db: &Db) -> DbResult<Budget> {
    db.with(|conn| {
        conn.query_row(
            "SELECT app_limit, remaining, reset_at, parked_until, parked_reason, suppressed_at
             FROM bugs_push_state WHERE id = 1",
            [],
            |row| {
                Ok(Budget {
                    limit: row.get(0)?,
                    remaining: row.get(1)?,
                    reset_at: row.get(2)?,
                    parked_until: row.get(3)?,
                    parked_reason: row.get(4)?,
                    pending: 0,
                    suppressed_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map(Option::unwrap_or_default)
    })
}

fn record_budget(db: &Db, limit: Option<i64>, remaining: Option<i64>, reset_at: Option<String>) {
    if limit.is_none() && remaining.is_none() && reset_at.is_none() {
        return;
    }
    let written = db.with(|conn| {
        conn.execute(
            "INSERT INTO bugs_push_state (id, app_limit, remaining, reset_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
               app_limit = COALESCE(excluded.app_limit, app_limit),
               remaining = COALESCE(excluded.remaining, remaining),
               reset_at  = COALESCE(excluded.reset_at, reset_at)",
            params![limit, remaining, reset_at],
        )?;
        Ok(())
    });
    if let Err(error) = written {
        tracing::warn!(%error, "bugs: cannot record the Pushover budget");
    }
}

fn park(db: &Db, until: &str, why: &str) -> DbResult<()> {
    db.with(|conn| {
        conn.execute(
            "INSERT INTO bugs_push_state (id, parked_until, parked_reason)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
               parked_until = excluded.parked_until,
               parked_reason = excluded.parked_reason",
            params![until, why],
        )?;
        Ok(())
    })
}

/// Remember that the one suppression notice has gone out, so the next trip of the cap
/// inside the same hour parks quietly instead of ringing again.
fn mark_suppressed(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute(
            "INSERT INTO bugs_push_state (id, suppressed_at) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET suppressed_at = excluded.suppressed_at",
            params![now()],
        )?;
        Ok(())
    })
}

fn unpark(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute(
            "UPDATE bugs_push_state SET parked_until = NULL, parked_reason = NULL WHERE id = 1",
            [],
        )?;
        Ok(())
    })
}

/// How many messages have gone out in the last hour, and when the oldest of them left.
fn sent_in_last_hour(db: &Db) -> DbResult<(i64, Option<String>)> {
    let head = now_offset(-3600);
    db.with(|conn| {
        conn.query_row(
            "SELECT COUNT(*), MIN(sent_at) FROM bugs_pushes WHERE sent_at > ?1",
            params![head],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    })
}

// ---- the sender --------------------------------------------------------------------

/// What one turn of the sender did. Returned rather than logged so a test can drive
/// the loop a step at a time and read the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Nothing is waiting.
    Idle,
    /// The queue is parked until the given time. No request was made.
    Parked { until: String, reason: String },
    Sent(i64),
    /// A 5xx or a transport failure: this one comes back later.
    Deferred { id: i64, seconds: i64 },
    /// A 4xx: this one never comes back.
    Failed(i64),
    /// The hourly cap tripped. One notice went out and the queue is parked.
    Suppressed { pending: i64 },
}

/// The single task that drains the queue.
pub struct Sender {
    db: Db,
    config: PushoverConfig,
    /// Where this viewer is from outside, for the suppression notice's own link.
    public_url: String,
    client: reqwest::Client,
}

impl Sender {
    pub fn new(db: Db, config: PushoverConfig, public_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client builds");
        Self {
            db,
            config,
            public_url: public_url.trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// Drain forever. `interval` is how long to wait when there is nothing to do; a
    /// successful send loops straight round, so a backlog leaves at the speed of the
    /// API rather than one message per interval.
    pub async fn run(self, interval: Duration) {
        loop {
            match self.tick().await {
                Tick::Sent(_) | Tick::Suppressed { .. } => continue,
                _ => tokio::time::sleep(interval).await,
            }
        }
    }

    /// One turn: at most one HTTP request, and none at all when the queue is parked.
    pub async fn tick(&self) -> Tick {
        let state = match read_state(&self.db) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(%error, "bugs: cannot read the Pushover queue state");
                return Tick::Idle;
            }
        };
        if let Some(until) = &state.parked_until {
            if until.as_str() > now().as_str() {
                return Tick::Parked {
                    until: until.clone(),
                    reason: state.parked_reason.unwrap_or_default(),
                };
            }
            let _ = unpark(&self.db);
        }

        let pending = match next_pending(&self.db) {
            Ok(Some(pending)) => pending,
            Ok(None) => return Tick::Idle,
            Err(error) => {
                tracing::warn!(%error, "bugs: cannot read the Pushover queue");
                return Tick::Idle;
            }
        };

        // The cap is checked with a message in hand, not on a timer: an idle hour must
        // not spend the one suppression notice on nobody.
        if let Some(tick) = self.enforce_hourly_cap().await {
            return tick;
        }

        self.deliver_pending(&pending).await
    }

    /// Park the queue and say so once, when more than [`HOURLY_CAP`] messages have
    /// left inside the hour.
    ///
    /// The notice goes out over the cap, deliberately. It is the message that tells a
    /// person the others exist, and holding it back to respect a limit we set
    /// ourselves would make the silence complete.
    ///
    /// **One** notice, though. Parking until the oldest message leaves the window means
    /// the cap trips again the moment the queue moves, and a notice each time would be
    /// exactly the flood the cap exists to prevent. `suppressed_at` is what makes the
    /// second trip inside the hour a silent one.
    async fn enforce_hourly_cap(&self) -> Option<Tick> {
        let (sent, oldest) = sent_in_last_hour(&self.db).ok()?;
        if sent < HOURLY_CAP {
            return None;
        }
        let until = oldest
            .as_deref()
            .and_then(shift_hour)
            .unwrap_or_else(|| now_offset(3600));
        let pending = pending_count(&self.db).unwrap_or(0);
        if let Err(error) = park(&self.db, &until, "the hourly cap") {
            tracing::warn!(%error, "bugs: cannot park the Pushover queue");
        }

        let told_recently = read_state(&self.db)
            .ok()
            .and_then(|state| state.suppressed_at)
            .is_some_and(|at| at > now_offset(-3600));
        if told_recently {
            return Some(Tick::Parked { until, reason: "the hourly cap".to_owned() });
        }

        let notice = Pending {
            id: 0,
            reason: reason::SUPPRESSED.to_owned(),
            title: clip("nashcode: notifications suppressed", TITLE_MAX),
            message: clip(
                &format!(
                    "{HOURLY_CAP} notifications went out in the last hour, so the rest are \
                     held. {pending} pending."
                ),
                MESSAGE_MAX,
            ),
            url: Some(format!("{}/bugs", self.public_url)),
            priority: 0,
            attempts: 0,
        };
        // Best effort. If this one does not go out, the queue is parked either way and
        // nothing is lost — the held messages are still rows.
        let _ = self.send(&notice).await;
        let _ = mark_suppressed(&self.db);
        tracing::warn!(pending, %until, "bugs: the hourly notification cap tripped");
        Some(Tick::Suppressed { pending })
    }

    async fn deliver_pending(&self, pending: &Pending) -> Tick {
        match self.send(pending).await {
            Answer::Sent => {
                if let Err(error) = mark_sent(&self.db, pending.id) {
                    tracing::warn!(%error, "bugs: cannot mark a notification sent");
                }
                Tick::Sent(pending.id)
            }
            // The message was fine and the account is out of budget. Every message is
            // still a row, so parking costs nothing but time.
            Answer::RateLimited { until } => {
                // A reset that has already passed — a clock that disagrees with ours, a
                // header echoed from a cached response — parks the queue until a moment
                // in the past, which is not a park at all: the next turn would send
                // again immediately and be refused again, once every tick for as long
                // as the limit lasts. The floor makes the worst case a retry every five
                // seconds instead of a retry every turn.
                let until = until
                    .unwrap_or_else(|| now_offset(BLIND_PARK_SECS))
                    .max(now_offset(RETRY_FLOOR_SECS));
                if let Err(error) = park(&self.db, &until, "Pushover answered 429") {
                    tracing::warn!(%error, "bugs: cannot park the Pushover queue");
                }
                tracing::warn!(%until, "bugs: Pushover is rate-limiting; the queue is parked");
                Tick::Parked { until, reason: "Pushover answered 429".to_owned() }
            }
            // Pushover read this message and judged it. A bad token or a malformed
            // payload answers the same way to every retry, and retrying would wedge
            // everything behind it.
            Answer::Refused(why) => {
                tracing::error!(id = pending.id, %why, "bugs: Pushover refused a notification");
                if let Err(error) = mark_failed(&self.db, pending.id, &why) {
                    tracing::warn!(%error, "bugs: cannot mark a notification failed");
                }
                Tick::Failed(pending.id)
            }
            Answer::Unavailable(why) => {
                if pending.attempts + 1 >= MAX_ATTEMPTS {
                    tracing::error!(
                        id = pending.id,
                        %why,
                        "bugs: giving up on a notification after {MAX_ATTEMPTS} attempts"
                    );
                    let _ = mark_failed(&self.db, pending.id, &why);
                    return Tick::Failed(pending.id);
                }
                let seconds = backoff_secs(pending.attempts);
                if let Err(error) = defer(&self.db, pending.id, seconds, &why) {
                    tracing::warn!(%error, "bugs: cannot defer a notification");
                }
                Tick::Deferred { id: pending.id, seconds }
            }
        }
    }

    /// One POST. Reads the budget headers whatever the answer was, because a 429 is
    /// the answer that carries the most useful ones.
    async fn send(&self, pending: &Pending) -> Answer {
        let url = format!("{}/1/messages.json", self.config.api_url.trim_end_matches('/'));
        let mut form = vec![
            ("token", self.config.token.clone()),
            ("user", self.config.user.clone()),
            ("title", pending.title.clone()),
            ("message", pending.message.clone()),
            ("priority", pending.priority.to_string()),
        ];
        if let Some(link) = &pending.url {
            form.push(("url", link.clone()));
            form.push(("url_title", "Open in nashcode".to_owned()));
        }

        let response = match self.client.post(&url).form(&form).send().await {
            Ok(response) => response,
            Err(error) => return Answer::Unavailable(error.to_string()),
        };
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<i64>().ok())
        };
        let limit = header("x-limit-app-limit");
        let remaining = header("x-limit-app-remaining");
        let reset = header("x-limit-app-reset").and_then(from_unix);
        record_budget(&self.db, limit, remaining, reset.clone());

        if status.is_success() {
            return Answer::Sent;
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Answer::RateLimited { until: reset };
        }
        let detail = response.text().await.unwrap_or_default();
        let detail = clip(detail.trim(), 300);
        if status.is_client_error() {
            return Answer::Refused(format!("{status}: {detail}"));
        }
        Answer::Unavailable(format!("{status}: {detail}"))
    }
}

/// What Pushover said, in the four shapes that lead to different decisions.
enum Answer {
    Sent,
    /// 429. The queue parks; the message stays pending.
    RateLimited { until: Option<String> },
    /// Any other 4xx. Final.
    Refused(String),
    /// 5xx, or no answer at all. Try again later.
    Unavailable(String),
}

/// One hour after a stored timestamp. Falls back to nothing when the string is not one,
/// which the caller reads as "an hour from now".
fn shift_hour(stamp: &str) -> Option<String> {
    let parsed = time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .ok()?;
    from_unix(parsed.unix_timestamp() + 3600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bugs::index;
    use serde_json::json;

    fn bed() -> (Db, Project) {
        let db = Db::in_memory().unwrap();
        index::migrate(&db).unwrap();
        migrate(&db).unwrap();
        let project = index::create_project(&db, "demo", &"a".repeat(32), None).unwrap();
        (db, project)
    }

    fn issue(events: i64, level: &str, state: &str) -> Issue {
        Issue {
            id: 7,
            project_id: 1,
            grouping_key: "ValueError: boom".to_owned(),
            grouping_hash: "hash".to_owned(),
            mechanism: "nashcode-v1".to_owned(),
            title: "ValueError: boom".to_owned(),
            culprit: None,
            level: level.to_owned(),
            state: state.to_owned(),
            regression: false,
            events,
            first_seen: "2026-08-20T00:00:00.000000Z".to_owned(),
            last_seen: "2026-08-20T00:00:00.000000Z".to_owned(),
            actor: None,
            acted_at: None,
        }
    }

    fn notifier() -> Notifier {
        Notifier { on: true, public_url: "https://nashcode.example".to_owned() }
    }

    #[test]
    fn a_repeat_event_is_never_a_notification() {
        let project = index::Project {
            id: 1,
            name: "demo".to_owned(),
            key: "k".to_owned(),
            repo: None,
            created_at: "x".to_owned(),
            retention_days: 30,
            active: true,
        };
        let messages =
            notifier().messages_for(&project, &issue(2, "error", state::UNRESOLVED), Landing::Repeat, 1, None);
        assert!(messages.is_empty());
        let duplicate = notifier().messages_for(
            &project,
            &issue(1, "error", state::UNRESOLVED),
            Landing::Duplicate,
            1,
            None,
        );
        assert!(duplicate.is_empty());
    }

    #[test]
    fn the_ladder_rings_at_ten_a_hundred_and_a_thousand_and_nowhere_else() {
        let project = index::Project {
            id: 1,
            name: "demo".to_owned(),
            key: "k".to_owned(),
            repo: None,
            created_at: "x".to_owned(),
            retention_days: 30,
            active: true,
        };
        let rungs: Vec<i64> = (1..=1000)
            .filter(|count| {
                !notifier()
                    .messages_for(
                        &project,
                        &issue(*count, "error", state::UNRESOLVED),
                        Landing::Repeat,
                        1,
                        None,
                    )
                    .is_empty()
            })
            .collect();
        assert_eq!(rungs, vec![10, 100, 1000]);

        // A resolved or muted issue is a decision already taken. No rung is news.
        for quiet in [state::RESOLVED, state::MUTED] {
            assert!(
                notifier()
                    .messages_for(&project, &issue(100, "error", quiet), Landing::Repeat, 1, None)
                    .is_empty(),
                "{quiet} rang"
            );
        }
    }

    #[test]
    fn a_title_and_a_message_always_fit_and_a_message_is_never_empty() {
        let project = index::Project {
            id: 1,
            name: "p".repeat(200),
            key: "k".to_owned(),
            repo: None,
            created_at: "x".to_owned(),
            retention_days: 30,
            active: true,
        };
        let mut long = issue(1, "error", state::UNRESOLVED);
        long.title = "t".repeat(4000);
        let messages = notifier().messages_for(&project, &long, Landing::New, 1, None);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].title.chars().count() <= TITLE_MAX);
        assert!(messages[0].message.chars().count() <= MESSAGE_MAX);

        // Nothing readable in the event and nothing readable in the issue still gets a
        // body: Pushover answers 4xx to an empty message, and a 4xx is final.
        let mut blank = issue(1, "error", state::UNRESOLVED);
        blank.title = String::new();
        let blank = notifier().messages_for(&project, &blank, Landing::New, 1, Some(&json!({})));
        assert!(!blank[0].message.trim().is_empty());
    }

    #[test]
    fn clipping_never_splits_a_character() {
        let wide = "é".repeat(400);
        let clipped = clip(&wide, TITLE_MAX);
        assert_eq!(clipped.chars().count(), TITLE_MAX);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short", TITLE_MAX), "short");
    }

    #[test]
    fn the_message_carries_the_exception_and_a_few_tags() {
        let event = json!({
            "exception": {"values": [{"type": "ValueError", "value": "boom"}]},
            "tags": {"release": "abc123", "environment": "prod", "a": "1", "b": "2", "c": "3"},
        });
        let body = message_for(&issue(1, "error", state::UNRESOLVED), Some(&event), "New issue.");
        assert!(body.contains("ValueError: boom"), "{body}");
        assert!(body.contains("environment=prod"), "{body}");
        assert!(body.contains("release=abc123"), "{body}");
        // A few is a few. The value is what a person reads first and the cap is shared.
        assert_eq!(body.matches('=').count(), TAGS_SHOWN);
    }

    #[test]
    fn a_fatal_issue_is_the_only_one_that_raises_the_priority() {
        assert_eq!(priority_for(&issue(1, "fatal", state::UNRESOLVED)), 1);
        for quiet in ["error", "warning", "info"] {
            assert_eq!(priority_for(&issue(1, quiet, state::UNRESOLVED)), 0);
        }
    }

    #[test]
    fn the_same_dedupe_key_queues_once() {
        let (db, project) = bed();
        let queued = Queued {
            project_id: project.id,
            issue_id: Some(1),
            reason: reason::NEW,
            dedupe_key: "issue/1/new".to_owned(),
            title: "t".to_owned(),
            message: "m".to_owned(),
            url: None,
            priority: 0,
        };
        assert!(enqueue(&db, &queued).unwrap());
        assert!(!enqueue(&db, &queued).unwrap(), "the second write is thrown away");
        assert_eq!(pending_count(&db).unwrap(), 1);
    }

    #[test]
    fn a_disabled_notifier_writes_nothing() {
        let (db, project) = bed();
        let off = Notifier::off();
        off.landed(&db, &project, &issue(1, "error", state::UNRESOLVED), Landing::New, 1, None);
        assert_eq!(pending_count(&db).unwrap(), 0);
    }

    #[test]
    fn the_backoff_starts_at_the_floor_and_stops_at_the_ceiling() {
        assert_eq!(backoff_secs(0), RETRY_FLOOR_SECS);
        assert_eq!(backoff_secs(1), 10);
        assert_eq!(backoff_secs(2), 20);
        assert_eq!(backoff_secs(60), RETRY_CEILING_SECS);
        assert!((0..64).all(|attempts| backoff_secs(attempts) >= RETRY_FLOOR_SECS));
    }

    #[test]
    fn a_parked_queue_is_visible_in_the_budget() {
        let (db, _) = bed();
        assert_eq!(budget(&db).unwrap(), Budget::default());
        record_budget(&db, Some(10_000), Some(9_998), Some("2026-09-01T00:00:00.000000Z".to_owned()));
        park(&db, "2026-08-20T01:00:00.000000Z", "the hourly cap").unwrap();
        let shown = budget(&db).unwrap();
        assert_eq!(shown.remaining, Some(9_998));
        assert_eq!(shown.limit, Some(10_000));
        assert_eq!(shown.parked_reason.as_deref(), Some("the hourly cap"));

        // A later answer that carries only `remaining` must not erase the reset.
        record_budget(&db, None, Some(9_997), None);
        let shown = budget(&db).unwrap();
        assert_eq!(shown.remaining, Some(9_997));
        assert_eq!(shown.limit, Some(10_000));
        assert!(shown.reset_at.is_some());

        unpark(&db).unwrap();
        assert!(budget(&db).unwrap().parked_until.is_none());
    }

    /// Every deadline this module stores is compared with `>` against `now()`, so the
    /// two other ways a timestamp gets made here have to sort the same way. They come
    /// out of one formatter today; this is the assertion that notices the day one of
    /// them does not, because the symptom otherwise is a park that never expires or a
    /// retry that never waits — both silent.
    #[test]
    fn every_way_of_making_a_timestamp_sorts_against_every_other() {
        let now = now();
        let past = now_offset(-60);
        let future = now_offset(60);
        assert!(past < now && now < future, "{past} {now} {future}");

        // A Unix second is what Pushover dates its reset with, and parking depends on
        // comparing it to `now()`.
        let epoch_past = from_unix(1).expect("a timestamp");
        assert!(epoch_past < now, "{epoch_past} is not before {now}");
        let epoch_future = from_unix(4_102_444_800).expect("a timestamp");
        assert!(epoch_future > now, "{epoch_future} is not after {now}");

        // Same instant, two routes, one string. This is the property, not the length.
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock")
            .as_secs() as i64;
        let from_epoch = from_unix(seconds).expect("a timestamp");
        assert_eq!(from_epoch.len(), now.len(), "{from_epoch} and {now} must sort as text");
        assert!(from_epoch.ends_with('Z') && now.ends_with('Z'));
    }

    #[test]
    fn an_hour_after_a_stored_timestamp_is_readable_as_one() {
        let shifted = shift_hour("2026-08-20T00:00:00.000000Z").expect("a timestamp");
        assert_eq!(shifted, "2026-08-20T01:00:00.000000Z");
        assert!(shift_hour("not a time").is_none());
    }
}
