//! The log store: a hot window in SQLite, an archive in the bucket.
//!
//! Two doors lead here — Sentry `log` envelope items, and the NDJSON route — and both
//! arrive as [`Record`]s, so everything downstream is written once. A batch is
//! archived to the bucket as one NDJSON object *before* its rows are inserted, which
//! is the same ordering the events take: the bucket is the truth and SQLite is the
//! index over it.
//!
//! The schema is OTel's severity model — `severity_text` trace…fatal beside a
//! `severity_number` in 1–24 — so an OTLP receiver later needs no migration. Message
//! search is an FTS5 external-content table: the index holds no second copy of the
//! text, and three triggers keep it honest.
//!
//! Every row indexes its code origin when the attributes carry it. That is the point
//! of the whole feature for a reader: a log line that says which file and line it came
//! from, linked into the code browser. The 2024 OTel names (`code.file.path`,
//! `code.line.number`, `code.function.name`) and the ones before them
//! (`code.filepath`, `code.lineno`, `code.function`) both arrive in the field, so both
//! are read and normalized to one shape.

use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::bugs::store;
use crate::db::{Db, DbResult, now};

/// Where a batch came from. Stored so a row can be read back to its door.
pub mod source {
    /// A `log` item inside a Sentry envelope.
    pub const ENVELOPE: &str = "envelope";
    /// The NDJSON route.
    pub const NDJSON: &str = "ndjson";
}

/// The severity vocabulary, lowest first. OTel numbers each band in fours and leaves
/// room inside a band (`info2`, `error3`); we store the band's base number and the
/// band's name, which is all a filter or a badge needs.
pub const SEVERITIES: [(&str, i64); 6] =
    [("trace", 1), ("debug", 5), ("info", 9), ("warn", 13), ("error", 17), ("fatal", 21)];

/// The default when a record says nothing about its level. `info`, not `error`: an
/// unlabelled line is not a problem until something says it is.
pub const DEFAULT_SEVERITY: (&str, i64) = ("info", 9);

/// One log line, normalized, on its way into the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    /// Canonical UTC, so `ORDER BY ts` is `ORDER BY time`.
    pub ts: String,
    pub severity_text: String,
    pub severity_number: i64,
    pub message: String,
    /// Everything else the sender said, verbatim.
    pub attributes: Map<String, Value>,
    pub code_file: Option<String>,
    pub code_line: Option<i64>,
    pub code_function: Option<String>,
    pub trace_id: Option<String>,
}

/// A stored row. This is the JSON the logs endpoint returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogRow {
    pub id: i64,
    pub project_id: i64,
    pub ts: String,
    pub severity_text: String,
    pub severity_number: i64,
    pub message: String,
    pub attributes: Value,
    pub code_file: Option<String>,
    pub code_line: Option<i64>,
    pub code_function: Option<String>,
    pub trace_id: Option<String>,
    pub source: String,
    /// The NDJSON object in the bucket this row was archived in. The prune deletes
    /// the row and leaves the object.
    pub archive_key: Option<String>,
    pub received_at: String,
}

// ---- schema ----------------------------------------------------------------------

/// Apply the log schema. Idempotent, run on every open.
pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        // Before and after the batch; see `index::migrate` for why both runs matter.
        // Here the before-run is the load-bearing one: the batch indexes
        // `dedupe_key`, which a `bugs_logs` older than that column does not have.
        crate::bugs::index::add_columns(conn, ADDED_COLUMNS)?;
        conn.execute_batch(SCHEMA)?;
        crate::bugs::index::add_columns(conn, ADDED_COLUMNS)
    })
}

/// Columns added to `bugs_logs` after its first release; see [`crate::bugs::index`].
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[("bugs_logs", "dedupe_key", "TEXT")];

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bugs_logs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id      INTEGER NOT NULL REFERENCES bugs_projects(id),
    ts              TEXT NOT NULL,
    severity_text   TEXT NOT NULL,
    severity_number INTEGER NOT NULL,
    message         TEXT NOT NULL,
    attributes      TEXT NOT NULL DEFAULT '{}',
    code_file       TEXT,
    code_line       INTEGER,
    code_function   TEXT,
    trace_id        TEXT,
    source          TEXT NOT NULL,
    archive_key     TEXT,
    dedupe_key      TEXT,
    received_at     TEXT NOT NULL
);
-- What makes a re-digest safe. NULL never collides in SQLite, so rows written
-- before this column existed simply do not dedupe.
CREATE UNIQUE INDEX IF NOT EXISTS bugs_logs_dedupe ON bugs_logs (project_id, dedupe_key);
CREATE INDEX IF NOT EXISTS bugs_logs_by_time ON bugs_logs (project_id, ts DESC, id DESC);
CREATE INDEX IF NOT EXISTS bugs_logs_by_file ON bugs_logs (project_id, code_file);
CREATE INDEX IF NOT EXISTS bugs_logs_by_level ON bugs_logs (project_id, severity_text, ts DESC);

-- External content: the index points at `bugs_logs` instead of copying the message.
CREATE VIRTUAL TABLE IF NOT EXISTS bugs_logs_fts
    USING fts5(message, content='bugs_logs', content_rowid='id', tokenize='unicode61');

CREATE TRIGGER IF NOT EXISTS bugs_logs_fts_insert AFTER INSERT ON bugs_logs BEGIN
    INSERT INTO bugs_logs_fts (rowid, message) VALUES (new.id, new.message);
END;
CREATE TRIGGER IF NOT EXISTS bugs_logs_fts_delete AFTER DELETE ON bugs_logs BEGIN
    INSERT INTO bugs_logs_fts (bugs_logs_fts, rowid, message) VALUES ('delete', old.id, old.message);
END;
CREATE TRIGGER IF NOT EXISTS bugs_logs_fts_update AFTER UPDATE ON bugs_logs BEGIN
    INSERT INTO bugs_logs_fts (bugs_logs_fts, rowid, message) VALUES ('delete', old.id, old.message);
    INSERT INTO bugs_logs_fts (rowid, message) VALUES (new.id, new.message);
END;
"#;

// ---- writing ---------------------------------------------------------------------

/// Archive a batch to the bucket, then index it. Returns how many rows landed.
///
/// The object is written first on purpose: a crash between the two steps costs the
/// index rows, which a reindex rebuilds, and never the lines themselves.
/// `origin` names where the batch came from, stably: the same envelope re-digested
/// must produce the same string, or a sweep would file every line twice. `None` means
/// there is nothing stable to name it by — the NDJSON door, where each POST is its own
/// event — and the fresh archive key stands in.
pub async fn store_batch(
    db: &Db,
    store: &Arc<dyn ObjectStore>,
    project_id: i64,
    records: &[Record],
    from: &str,
    origin: Option<&str>,
) -> Result<usize, String> {
    if records.is_empty() {
        return Ok(0);
    }
    let received_at = now();
    let key = store::log_key(project_id, &received_at, &super::mint_key());
    let mut ndjson = String::new();
    for record in records {
        ndjson.push_str(&serde_json::to_string(record).map_err(|error| error.to_string())?);
        ndjson.push('\n');
    }
    store
        .put(&key, ndjson.into_bytes().into())
        .await
        .map_err(|error| format!("cannot write {key}: {error}"))?;

    let origin = origin.unwrap_or(key.as_ref());
    insert(db, project_id, records, from, key.as_ref(), origin, &received_at)
        .map_err(|error| error.to_string())
}

/// Index one batch. One transaction: a half-written batch would search wrong.
///
/// `INSERT OR IGNORE` on `(project_id, dedupe_key)` is what makes the digest safe to
/// run twice. Events already dedupe on `(project_id, event_id)`; without this, the
/// startup sweep — and `sweep(true)`, which is the reindex primitive — filed every log
/// line again on every pass.
fn insert(
    db: &Db,
    project_id: i64,
    records: &[Record],
    from: &str,
    archive_key: &str,
    origin: &str,
    received_at: &str,
) -> DbResult<usize> {
    db.with(|conn| {
        let mut landed = 0;
        let tx = conn.unchecked_transaction()?;
        {
            let mut statement = tx.prepare(
                "INSERT OR IGNORE INTO bugs_logs
                   (project_id, ts, severity_text, severity_number, message, attributes,
                    code_file, code_line, code_function, trace_id, source, archive_key,
                    dedupe_key, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for (index, record) in records.iter().enumerate() {
                landed += statement.execute(params![
                    project_id,
                    record.ts,
                    record.severity_text,
                    record.severity_number,
                    record.message,
                    Value::Object(record.attributes.clone()).to_string(),
                    record.code_file,
                    record.code_line,
                    record.code_function,
                    record.trace_id,
                    from,
                    archive_key,
                    format!("{origin}#{index}"),
                    received_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(landed)
    })
}

// ---- reading ---------------------------------------------------------------------

/// What the logs page asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Free text, matched against the message through FTS5.
    pub text: Option<String>,
    /// One severity band, exactly.
    pub level: Option<String>,
    /// A fragment of the code origin path.
    pub file: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

/// Split a search box into free text and the `file:` filter.
///
/// `timeout file:src/app.py` narrows to that file and searches for `timeout`. The
/// token is a filter, not a search term, so it never reaches FTS.
pub fn parse_search(raw: &str) -> (Option<String>, Option<String>) {
    let mut terms = Vec::new();
    let mut file = None;
    for token in raw.split_whitespace() {
        match token.strip_prefix("file:") {
            Some(value) if !value.is_empty() => file = Some(value.to_owned()),
            _ => terms.push(token),
        }
    }
    let text = (!terms.is_empty()).then(|| terms.join(" "));
    (text, file)
}

/// The most words one search box is worth.
pub const MAX_SEARCH_TERMS: usize = 32;

/// Turn free text into an FTS5 MATCH expression that cannot be a syntax error.
///
/// Every term is quoted, so `AND`, `*`, `"` and a stray `(` are all searched for
/// rather than parsed. An FTS5 query is a small language, and handing a search box
/// straight to it means a person who types an apostrophe gets a 500 instead of an
/// answer.
pub fn fts_match(text: &str) -> Option<String> {
    // Capped: FTS5 parses the AND chain into an expression tree, and SQLite refuses
    // one deeper than SQLITE_MAX_EXPR_DEPTH. A search box with 500 words in it is
    // not a search, but it should not be a 500 either.
    let terms: Vec<String> = text
        .split_whitespace()
        .map(|term| term.replace('"', ""))
        .filter(|term| !term.is_empty())
        .take(MAX_SEARCH_TERMS)
        .map(|term| format!("\"{term}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

/// Newest first. Reads one row more than `limit` so the caller knows whether there is
/// another page without counting the whole table.
pub fn search(db: &Db, project_id: i64, query: &Query) -> DbResult<Vec<LogRow>> {
    let matching = query.text.as_deref().and_then(fts_match);
    let file = query.file.as_deref().map(|value| format!("%{}%", escape_like(value)));
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, project_id, ts, severity_text, severity_number, message, attributes,
                    code_file, code_line, code_function, trace_id, source, archive_key,
                    received_at
             FROM bugs_logs
             WHERE project_id = ?1
               AND (?2 IS NULL OR id IN (
                     SELECT rowid FROM bugs_logs_fts WHERE bugs_logs_fts MATCH ?2))
               AND (?3 IS NULL OR severity_text = ?3)
               AND (?4 IS NULL OR code_file LIKE ?4 ESCAPE '\\')
             ORDER BY ts DESC, id DESC
             LIMIT ?5 OFFSET ?6",
        )?;
        let rows = statement.query_map(
            params![project_id, matching, query.level, file, query.limit + 1, query.offset],
            read_row,
        )?;
        rows.collect()
    })
}

/// `%` and `_` are wildcards in LIKE, and a path may hold either.
fn escape_like(value: &str) -> String {
    value.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogRow> {
    let attributes: String = row.get(6)?;
    Ok(LogRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        ts: row.get(2)?,
        severity_text: row.get(3)?,
        severity_number: row.get(4)?,
        message: row.get(5)?,
        attributes: serde_json::from_str(&attributes).unwrap_or_else(|_| Value::Object(Map::new())),
        code_file: row.get(7)?,
        code_line: row.get(8)?,
        code_function: row.get(9)?,
        trace_id: row.get(10)?,
        source: row.get(11)?,
        archive_key: row.get(12)?,
        received_at: row.get(13)?,
    })
}

/// How many rows a project is holding. The logs page shows it; the prune proves
/// itself with it.
pub fn count(db: &Db, project_id: i64) -> DbResult<i64> {
    db.with(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM bugs_logs WHERE project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
    })
}

// ---- pruning ---------------------------------------------------------------------

/// Drop every hot row past its project's `retention_days`. Returns how many went.
///
/// Only the SQLite row goes. The NDJSON object it was archived in stays in the
/// bucket: retention here decides what search is fast over, not what is kept.
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
        let Some(cutoff) = days_ago(days) else { continue };
        deleted += db.with(|conn: &Connection| {
            conn.execute(
                "DELETE FROM bugs_logs WHERE project_id = ?1 AND ts < ?2",
                params![project_id, cutoff],
            )
        })?;
    }
    Ok(deleted)
}

/// `days` before now, in the canonical storage format, so it compares as a string
/// against a stored `ts`.
pub fn days_ago(days: i64) -> Option<String> {
    let when = OffsetDateTime::now_utc().checked_sub(time::Duration::days(days))?;
    crate::db::normalize_timestamp(&when.format(&Rfc3339).ok()?)
}

// ---- parsing ---------------------------------------------------------------------

/// The severity band a level name belongs to.
///
/// Every spelling that reaches a log line in the field maps onto one of the six:
/// `warning` and `warn` are the same band, and so are `critical`, `crit`, `panic` and
/// `fatal`. An unknown word is not an error — it is somebody's custom level, and
/// dropping the line over it would be the wrong trade.
pub fn severity(level: &str) -> (String, i64) {
    let level = level.trim().to_ascii_lowercase();
    let normalized = match level.as_str() {
        "trace" | "verbose" => "trace",
        "debug" | "dbg" => "debug",
        "info" | "information" | "notice" | "log" => "info",
        "warn" | "warning" => "warn",
        "error" | "err" | "severe" => "error",
        "fatal" | "critical" | "crit" | "panic" | "emergency" | "alert" => "fatal",
        _ => return (DEFAULT_SEVERITY.0.to_owned(), DEFAULT_SEVERITY.1),
    };
    let number = SEVERITIES
        .iter()
        .find(|(name, _)| *name == normalized)
        .map_or(DEFAULT_SEVERITY.1, |(_, number)| *number);
    (normalized.to_owned(), number)
}

/// Settle a level name and a declared severity number into one agreed pair.
///
/// The number wins the band when the two disagree. `{"level":"info",
/// "severity_number":21}` used to store `("info", 21)`, which puts a fatal line under
/// the info badge and behind the info filter — the one place a log store must not be
/// quietly wrong. Numbers and names agree in every real payload, so this only fires on
/// a sender that is already confused, and it fails towards being seen.
pub fn resolve_severity(level: Option<String>, declared: Option<i64>) -> (String, i64) {
    match (level, declared) {
        (_, Some(number)) => (band(number).to_owned(), number),
        (Some(level), None) => severity(&level),
        (None, None) => (DEFAULT_SEVERITY.0.to_owned(), DEFAULT_SEVERITY.1),
    }
}

/// The band an OTel severity number falls in. Numbers inside a band (`info2` is 10)
/// round down to the band, which is what a filter and a badge want.
pub fn band(number: i64) -> &'static str {
    SEVERITIES
        .iter()
        .rev()
        .find(|(_, base)| number >= *base)
        .map_or(DEFAULT_SEVERITY.0, |(name, _)| *name)
}

/// A timestamp in any of the forms the two doors carry: RFC3339, or a number of
/// seconds, milliseconds, microseconds or nanoseconds since the epoch.
///
/// The unit is read off the magnitude. A value under 1e11 is seconds until the year
/// 5138, which outlasts the format; the same test one step up separates each unit
/// from the next. Anything unreadable falls back to arrival time, because a log line
/// with a broken clock is still a log line.
pub fn normalize_ts(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => crate::db::normalize_timestamp(text),
        Value::Number(number) => {
            let seconds = number.as_f64()?;
            let nanos = if seconds.abs() < 1e11 {
                seconds * 1e9
            } else if seconds.abs() < 1e14 {
                seconds * 1e6
            } else if seconds.abs() < 1e17 {
                seconds * 1e3
            } else {
                seconds
            };
            let when = OffsetDateTime::from_unix_timestamp_nanos(nanos as i128).ok()?;
            crate::db::normalize_timestamp(&when.format(&Rfc3339).ok()?)
        }
        _ => None,
    }
}

/// The code origin an attribute map carries, under either generation of OTel names.
///
/// The 2024 rename left both spellings in the field at once — an SDK pinned a year ago
/// still sends `code.filepath` — so a store that read only the new names would show
/// the file for some services and nothing for others.
pub fn code_origin(attributes: &Map<String, Value>) -> (Option<String>, Option<i64>, Option<String>) {
    let text = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| attributes.get(*key))
            .and_then(scalar_text)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let file = text(&["code.file.path", "code.filepath"]);
    let function = text(&["code.function.name", "code.function"]);
    let line = ["code.line.number", "code.lineno"]
        .iter()
        .find_map(|key| attributes.get(*key))
        .and_then(scalar_number)
        .filter(|line| *line > 0);
    (file, line, function)
}

/// The plain value of an attribute, whether it arrived bare or wrapped in Sentry's
/// `{"value": …, "type": …}`.
fn scalar(value: &Value) -> &Value {
    match value {
        Value::Object(map) => map.get("value").unwrap_or(value),
        other => other,
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match scalar(value) {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn scalar_number(value: &Value) -> Option<i64> {
    match scalar(value) {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|n| n as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Flatten Sentry's `{key: {value, type}}` attribute map to plain JSON values.
fn flatten(attributes: Option<&Value>) -> Map<String, Value> {
    let Some(Value::Object(map)) = attributes else { return Map::new() };
    map.iter().map(|(key, value)| (key.clone(), scalar(value).clone())).collect()
}

/// Parse a `log` envelope item.
///
/// The payload is `{"items": [ … ]}` and each item is a log record: `timestamp`,
/// `level`, `body`, `attributes`, and optionally `severity_number` and `trace_id`.
/// Parsing is lenient in the same way the event path is — `message` is taken as a
/// synonym for `body`, a missing level is `info`, an unreadable timestamp is arrival
/// time — because a dropped line is worse than an imperfect one.
pub fn parse_envelope_item(payload: &[u8]) -> Vec<Record> {
    let Ok(value) = serde_json::from_slice::<Value>(payload) else { return Vec::new() };
    let items = match value.get("items").and_then(Value::as_array) {
        Some(items) => items.clone(),
        // A bare array, and a bare single record, both turn up in the wild.
        None => match value {
            Value::Array(items) => items,
            Value::Object(_) => vec![value],
            _ => return Vec::new(),
        },
    };
    // Capped like the NDJSON door. The splitter already holds an item to 1 MiB, but a
    // megabyte of `{"body":""}` is still tens of thousands of records and one very
    // long transaction. Truncated rather than refused: an envelope must never fail
    // over an item, so the excess is dropped and said out loud.
    if items.len() > MAX_BATCH {
        tracing::warn!(dropped = items.len() - MAX_BATCH, "bugs: log item over the batch cap");
    }
    items.iter().take(MAX_BATCH).filter_map(record_from_sentry).collect()
}

fn record_from_sentry(item: &Value) -> Option<Record> {
    let object = item.as_object()?;
    let attributes = flatten(object.get("attributes"));
    let message = object
        .get("body")
        .or_else(|| object.get("message"))
        .and_then(scalar_text)
        .or_else(|| attributes.get("sentry.message.template").and_then(scalar_text))
        .unwrap_or_default();

    // The captured 2.68 envelope puts the number in `sentry.severity_number` rather
    // than beside `level`, which is where the protocol document shows it. Both are
    // read; a number outside 1–24 is not an OTel severity and is ignored.
    let declared = object
        .get("severity_number")
        .or_else(|| attributes.get("sentry.severity_number"))
        .and_then(scalar_number)
        .filter(|number| (1..=24).contains(number));
    let level = object
        .get("level")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| attributes.get("sentry.severity_text").and_then(scalar_text));
    let (severity_text, severity_number) = resolve_severity(level, declared);

    let (code_file, code_line, code_function) = code_origin(&attributes);
    Some(Record {
        ts: normalize_ts(object.get("timestamp")).unwrap_or_else(now),
        severity_text,
        severity_number,
        message,
        code_file,
        code_line,
        code_function,
        trace_id: object.get("trace_id").and_then(Value::as_str).map(str::to_owned),
        attributes,
    })
}

/// The most log lines one batch may carry.
///
/// A per-line length cap alone bounds nothing: 100 MiB of `0\n` is 50 million lines,
/// each one cheap, and the batch built out of them is not. Ten thousand is more than
/// any real flush and small enough that the transaction behind it is uninteresting.
pub const MAX_BATCH: usize = 10_000;

/// How many rejected line numbers are named in the answer. The count is always
/// exact; the list is a debugging aid, and echoing a caller-controlled number of them
/// is how a small request buys a large response.
pub const MAX_REPORTED_REJECTS: usize = 20;

/// What one NDJSON body turned into.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub records: Vec<Record>,
    /// How many lines were not readable as a JSON object. Exact.
    pub rejected: usize,
    /// The first few of their line numbers, for a person reading the answer.
    pub rejected_lines: Vec<usize>,
    /// The body carried more than [`MAX_BATCH`] lines. Nothing in it is stored: the
    /// caller answers 413 and the sender splits the batch.
    pub too_many: bool,
}

impl Parsed {
    /// The `rejected` half of the answer: a count that is always right, and a few
    /// line numbers to look at.
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({ "count": self.rejected, "lines": self.rejected_lines })
    }
}

/// Parse the NDJSON door's body: one JSON object per line.
///
/// `ts`, `level` and `message` are read under the spellings a shell script or a
/// journald shipper actually writes; every other key becomes an attribute, which is
/// how the `code.*` keys arrive through this door.
///
/// Stops the moment the line count passes `max_records`, without building the rest:
/// the point of the cap is the memory it does not spend.
pub fn parse_ndjson(body: &[u8], max_line: usize, max_records: usize) -> Parsed {
    let mut parsed = Parsed::default();
    for (number, line) in body.split(|byte| *byte == b'\n').enumerate() {
        let line = trim(line);
        if line.is_empty() {
            continue;
        }
        if parsed.records.len() + parsed.rejected >= max_records {
            parsed.too_many = true;
            parsed.records.clear();
            parsed.rejected_lines.clear();
            return parsed;
        }
        if line.len() > max_line {
            parsed.reject(number + 1);
            continue;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(Value::Object(object)) => parsed.records.push(record_from_ndjson(&object)),
            _ => parsed.reject(number + 1),
        }
    }
    parsed
}

impl Parsed {
    fn reject(&mut self, line: usize) {
        self.rejected += 1;
        if self.rejected_lines.len() < MAX_REPORTED_REJECTS {
            self.rejected_lines.push(line);
        }
    }
}

fn trim(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(line.len());
    let end = line.iter().rposition(|byte| !byte.is_ascii_whitespace()).map_or(start, |at| at + 1);
    &line[start..end]
}

/// The keys the NDJSON door reads by name. Everything else is an attribute.
const TS_KEYS: [&str; 3] = ["ts", "timestamp", "time"];
const LEVEL_KEYS: [&str; 4] = ["level", "severity_text", "severity", "levelname"];
const MESSAGE_KEYS: [&str; 4] = ["message", "msg", "body", "text"];

fn record_from_ndjson(object: &Map<String, Value>) -> Record {
    let first = |keys: &[&str]| keys.iter().find_map(|key| object.get(*key));

    let mut attributes = flatten(object.get("attributes"));
    for (key, value) in object {
        if TS_KEYS.contains(&key.as_str())
            || LEVEL_KEYS.contains(&key.as_str())
            || MESSAGE_KEYS.contains(&key.as_str())
            || key == "attributes"
            || key == "trace_id"
            || key == "severity_number"
        {
            continue;
        }
        attributes.insert(key.clone(), value.clone());
    }

    let declared = object
        .get("severity_number")
        .and_then(scalar_number)
        .filter(|number| (1..=24).contains(number));
    let (severity_text, severity_number) = resolve_severity(first(&LEVEL_KEYS).and_then(scalar_text), declared);

    let (code_file, code_line, code_function) = code_origin(&attributes);
    Record {
        ts: normalize_ts(first(&TS_KEYS)).unwrap_or_else(now),
        severity_text,
        severity_number,
        message: first(&MESSAGE_KEYS).and_then(scalar_text).unwrap_or_default(),
        code_file,
        code_line,
        code_function,
        trace_id: object.get("trace_id").and_then(Value::as_str).map(str::to_owned),
        attributes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bed() -> (Db, i64) {
        let db = Db::in_memory().unwrap();
        crate::bugs::index::migrate(&db).unwrap();
        migrate(&db).unwrap();
        let project =
            crate::bugs::index::create_project(&db, "demo", &"a".repeat(32), None).unwrap();
        (db, project.id)
    }

    /// The production incident of 2026-08-20: a database whose `bugs_logs` predates
    /// `dedupe_key` must still migrate, because SCHEMA's unique index names that
    /// column and `CREATE TABLE IF NOT EXISTS` will not add it.
    #[test]
    fn migrate_adds_dedupe_key_to_an_old_table() {
        let db = Db::in_memory().unwrap();
        crate::bugs::index::migrate(&db).unwrap();
        db.with(|conn| {
            conn.execute_batch(
                "CREATE TABLE bugs_logs (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    project_id      INTEGER NOT NULL REFERENCES bugs_projects(id),
                    ts              TEXT NOT NULL,
                    severity_text   TEXT NOT NULL,
                    severity_number INTEGER NOT NULL,
                    message         TEXT NOT NULL,
                    attributes      TEXT NOT NULL DEFAULT '{}',
                    code_file       TEXT,
                    code_line       INTEGER,
                    code_function   TEXT,
                    trace_id        TEXT,
                    source          TEXT NOT NULL,
                    archive_key     TEXT,
                    received_at     TEXT NOT NULL
                );",
            )?;
            Ok(())
        })
        .unwrap();
        migrate(&db).unwrap();
        let has_column = db
            .with(|conn| {
                let mut statement = conn.prepare("PRAGMA table_info(bugs_logs)")?;
                let names = statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(names.contains(&"dedupe_key".to_owned()))
            })
            .unwrap();
        assert!(has_column);
    }

    fn record(message: &str, ts: &str) -> Record {
        Record {
            ts: ts.to_owned(),
            severity_text: "info".to_owned(),
            severity_number: 9,
            message: message.to_owned(),
            attributes: Map::new(),
            code_file: None,
            code_line: None,
            code_function: None,
            trace_id: None,
        }
    }

    #[test]
    fn both_generations_of_otel_code_attributes_normalize_to_one_shape() {
        let new = flatten(Some(&json!({
            "code.file.path": {"value": "src/app.py", "type": "string"},
            "code.line.number": {"value": 41, "type": "integer"},
            "code.function.name": {"value": "handle", "type": "string"},
        })));
        assert_eq!(
            code_origin(&new),
            (Some("src/app.py".to_owned()), Some(41), Some("handle".to_owned()))
        );

        // The pre-2024 names, which SDKs pinned a year ago still send.
        let old = flatten(Some(&json!({
            "code.filepath": "src/app.py",
            "code.lineno": 41,
            "code.function": "handle",
        })));
        assert_eq!(code_origin(&old), code_origin(&new));

        // A line number that arrived as text, and one that means "unknown".
        let odd = flatten(Some(&json!({"code.filepath": "a.rs", "code.lineno": "7"})));
        assert_eq!(code_origin(&odd).1, Some(7));
        let zero = flatten(Some(&json!({"code.filepath": "a.rs", "code.lineno": 0})));
        assert_eq!(code_origin(&zero).1, None, "line 0 is not a line");
        assert_eq!(code_origin(&Map::new()), (None, None, None));
    }

    #[test]
    fn a_level_maps_to_its_otel_band_however_it_is_spelled() {
        assert_eq!(severity("WARNING"), ("warn".to_owned(), 13));
        assert_eq!(severity("warn"), ("warn".to_owned(), 13));
        assert_eq!(severity("critical"), ("fatal".to_owned(), 21));
        assert_eq!(severity(" Error "), ("error".to_owned(), 17));
        // An unknown level is somebody's custom one, not a reason to lose the line.
        assert_eq!(severity("shouty"), ("info".to_owned(), 9));
        assert_eq!(band(10), "info");
        assert_eq!(band(24), "fatal");
        assert_eq!(band(1), "trace");
    }

    #[test]
    fn a_timestamp_arrives_in_four_units_and_one_string() {
        let seconds = normalize_ts(Some(&json!(1_755_561_600.0))).expect("seconds");
        assert!(seconds.starts_with("2025-08-19T00:00:00"), "{seconds}");
        let millis = normalize_ts(Some(&json!(1_755_561_600_000i64))).expect("millis");
        assert_eq!(millis, seconds);
        let micros = normalize_ts(Some(&json!(1_755_561_600_000_000i64))).expect("micros");
        assert_eq!(micros, seconds);
        let nanos = normalize_ts(Some(&json!(1_755_561_600_000_000_000i64))).expect("nanos");
        assert_eq!(nanos, seconds);
        assert_eq!(normalize_ts(Some(&json!("2025-08-19T00:00:00Z"))), Some(seconds));
        assert_eq!(normalize_ts(Some(&json!("not a time"))), None);
        assert_eq!(normalize_ts(None), None);
    }

    #[test]
    fn a_sentry_log_item_parses_into_records() {
        let payload = json!({"items": [{
            "timestamp": 1_755_561_600.0,
            "trace_id": "5b8efff798038103d269b633813fc60c",
            "level": "warn",
            "body": "cache miss for user 12",
            "severity_number": 13,
            "attributes": {
                "sentry.sdk.name": {"value": "sentry.javascript.node", "type": "string"},
                "code.file.path": {"value": "src/cache.ts", "type": "string"},
                "code.line.number": {"value": 88, "type": "integer"},
            },
        }]})
        .to_string();

        let records = parse_envelope_item(payload.as_bytes());
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.message, "cache miss for user 12");
        assert_eq!(record.severity_text, "warn");
        assert_eq!(record.severity_number, 13);
        assert_eq!(record.code_file.as_deref(), Some("src/cache.ts"));
        assert_eq!(record.code_line, Some(88));
        assert_eq!(record.trace_id.as_deref(), Some("5b8efff798038103d269b633813fc60c"));
        // The attribute map is flattened, so the archive and the UI read one shape.
        assert_eq!(record.attributes["sentry.sdk.name"], json!("sentry.javascript.node"));
    }

    #[test]
    fn a_log_item_that_says_almost_nothing_still_lands() {
        let records = parse_envelope_item(br#"{"items":[{"body":"hello"}]}"#);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].severity_text, "info");
        assert_eq!(records[0].severity_number, 9);
        assert!(!records[0].ts.is_empty(), "an absent timestamp becomes arrival time");

        assert!(parse_envelope_item(b"not json").is_empty());
        assert!(parse_envelope_item(br#"{"items":[]}"#).is_empty());
    }

    #[test]
    fn ndjson_reads_the_spellings_a_shell_script_writes() {
        let body = concat!(
            r#"{"ts":"2026-08-19T04:05:06Z","level":"error","message":"disk full","code.filepath":"bin/backup.sh","code.lineno":12}"#,
            "\n",
            "   \n",
            r#"{"time":1755561600,"severity":"warning","msg":"slow","host":"celld"}"#,
            "\n",
            "this is not json\n",
        );
        let parsed = parse_ndjson(body.as_bytes(), 1024 * 1024, MAX_BATCH);
        assert_eq!(parsed.records.len(), 2, "blank lines are skipped, bad ones counted");
        assert_eq!(parsed.rejected, 1);
        assert_eq!(parsed.rejected_lines, vec![4]);

        assert_eq!(parsed.records[0].severity_text, "error");
        assert_eq!(parsed.records[0].code_file.as_deref(), Some("bin/backup.sh"));
        assert_eq!(parsed.records[0].code_line, Some(12));
        assert_eq!(parsed.records[1].severity_text, "warn");
        // Everything that is not a named field is an attribute.
        assert_eq!(parsed.records[1].attributes["host"], json!("celld"));
        assert!(!parsed.records[1].attributes.contains_key("msg"), "the message is not an attribute");
    }

    #[test]
    fn an_over_long_ndjson_line_is_rejected_and_the_rest_survive() {
        let long = format!("{{\"message\":\"{}\"}}", "x".repeat(2048));
        let body = format!("{{\"message\":\"fine\"}}\n{long}\n{{\"message\":\"also fine\"}}\n");
        let parsed = parse_ndjson(body.as_bytes(), 1024, MAX_BATCH);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.rejected, 1);
        assert_eq!(parsed.rejected_lines, vec![2]);
    }

    #[test]
    fn a_batch_over_the_line_cap_is_refused_whole_and_costs_nothing_to_refuse() {
        // The shape the review found: a per-line length cap passes every one of
        // these, and 100 MiB of them is fifty million lines.
        let flood = "0\n".repeat(MAX_BATCH + 500);
        let parsed = parse_ndjson(flood.as_bytes(), 1024 * 1024, MAX_BATCH);
        assert!(parsed.too_many, "the batch is over the cap");
        assert!(parsed.records.is_empty(), "nothing is built");
        assert!(parsed.rejected_lines.is_empty(), "and nothing is echoed back");

        // The same with valid lines, which is the more expensive half: one giant
        // transaction on the shared connection.
        let valid = "{\"message\":\"x\"}\n".repeat(MAX_BATCH + 1);
        let parsed = parse_ndjson(valid.as_bytes(), 1024 * 1024, MAX_BATCH);
        assert!(parsed.too_many);
        assert!(parsed.records.is_empty());

        // Exactly at the cap is fine.
        let full = "{\"message\":\"x\"}\n".repeat(MAX_BATCH);
        let parsed = parse_ndjson(full.as_bytes(), 1024 * 1024, MAX_BATCH);
        assert!(!parsed.too_many);
        assert_eq!(parsed.records.len(), MAX_BATCH);
    }

    #[test]
    fn the_rejected_line_numbers_are_a_sample_and_the_count_is_exact() {
        let body = "nope\n".repeat(100);
        let parsed = parse_ndjson(body.as_bytes(), 1024 * 1024, MAX_BATCH);
        assert_eq!(parsed.rejected, 100, "the count is exact");
        assert_eq!(
            parsed.rejected_lines.len(),
            MAX_REPORTED_REJECTS,
            "the list is capped, so a small request cannot buy a large answer"
        );
        assert_eq!(parsed.report()["count"], 100);
    }

    #[test]
    fn a_declared_severity_number_decides_the_band_when_the_level_contradicts_it() {
        // The one a log store must not get wrong: a fatal hiding under the info
        // badge, and behind the info filter.
        assert_eq!(
            resolve_severity(Some("info".to_owned()), Some(21)),
            ("fatal".to_owned(), 21)
        );
        // Agreement is the normal case and is unchanged.
        assert_eq!(resolve_severity(Some("warn".to_owned()), Some(13)), ("warn".to_owned(), 13));
        // A number inside a band still reads as its band.
        assert_eq!(resolve_severity(Some("info".to_owned()), Some(10)), ("info".to_owned(), 10));
        assert_eq!(resolve_severity(Some("error".to_owned()), None), ("error".to_owned(), 17));
        assert_eq!(resolve_severity(None, None), ("info".to_owned(), 9));

        // And end to end, through both doors.
        let records = parse_envelope_item(br#"{"items":[{"level":"info","severity_number":21,"body":"boom"}]}"#);
        assert_eq!(records[0].severity_text, "fatal");
        let parsed = parse_ndjson(
            br#"{"level":"info","severity_number":21,"message":"boom"}"#,
            1024,
            MAX_BATCH,
        );
        assert_eq!(parsed.records[0].severity_text, "fatal");
    }

    #[test]
    fn the_same_batch_indexed_twice_is_one_set_of_rows() {
        let (db, project) = bed();
        let rows = [
            record("first line", "2026-08-19T04:00:00.000000Z"),
            record("second line", "2026-08-19T04:00:01.000000Z"),
        ];
        // The same origin twice is what a sweep does: re-reading one envelope out of
        // the bucket and digesting it again.
        assert_eq!(insert(&db, project, &rows, source::ENVELOPE, "a", "envelope-1#0", &now()).unwrap(), 2);
        assert_eq!(
            insert(&db, project, &rows, source::ENVELOPE, "b", "envelope-1#0", &now()).unwrap(),
            0,
            "a re-digest files nothing new"
        );
        assert_eq!(count(&db, project).unwrap(), 2);

        // A different origin is a different batch, even with identical lines.
        assert_eq!(insert(&db, project, &rows, source::ENVELOPE, "c", "envelope-2#0", &now()).unwrap(), 2);
        assert_eq!(count(&db, project).unwrap(), 4);
    }

    #[test]
    fn a_search_box_splits_into_terms_and_a_file_filter() {
        assert_eq!(
            parse_search("timeout file:src/app.py retry"),
            (Some("timeout retry".to_owned()), Some("src/app.py".to_owned()))
        );
        assert_eq!(parse_search("file:x"), (None, Some("x".to_owned())));
        assert_eq!(parse_search("  "), (None, None));
        // A search box is not an FTS5 program: every term is quoted so nothing in it
        // can be a syntax error.
        assert_eq!(fts_match("a AND b"), Some("\"a\" AND \"AND\" AND \"b\"".to_owned()));
        assert_eq!(fts_match("\"\""), None);
        assert_eq!(fts_match("   "), None);
        // And the chain is capped: FTS5 refuses an expression tree deeper than
        // SQLITE_MAX_EXPR_DEPTH, which a pasted paragraph would otherwise reach.
        let many = (0..500).map(|n| format!("w{n}")).collect::<Vec<_>>().join(" ");
        assert_eq!(fts_match(&many).unwrap().matches(" AND ").count(), MAX_SEARCH_TERMS - 1);
    }

    #[test]
    fn search_finds_by_message_level_and_file() {
        let (db, project) = bed();
        let mut with_file = record("connection refused", "2026-08-19T04:05:06.000000Z");
        with_file.severity_text = "error".to_owned();
        with_file.severity_number = 17;
        with_file.code_file = Some("src/net/socket.rs".to_owned());
        with_file.code_line = Some(41);
        let rows = [record("started up", "2026-08-19T04:00:00.000000Z"), with_file];
        insert(&db, project, &rows, source::NDJSON, "projects/1/logs/x.ndjson", "batch-a", &now())
            .unwrap();

        let all = search(&db, project, &Query { limit: 50, ..Query::default() }).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].message, "connection refused", "newest first");

        let found = search(
            &db,
            project,
            &Query { text: Some("refused".to_owned()), limit: 50, ..Query::default() },
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code_file.as_deref(), Some("src/net/socket.rs"));

        let by_level = search(
            &db,
            project,
            &Query { level: Some("error".to_owned()), limit: 50, ..Query::default() },
        )
        .unwrap();
        assert_eq!(by_level.len(), 1);

        let by_file = search(
            &db,
            project,
            &Query { file: Some("net/socket".to_owned()), limit: 50, ..Query::default() },
        )
        .unwrap();
        assert_eq!(by_file.len(), 1);
        assert!(
            search(&db, project, &Query { file: Some("nope".to_owned()), limit: 50, ..Query::default() })
                .unwrap()
                .is_empty()
        );

        // A query with no results is an empty list, never an error, whatever is typed.
        for hostile in ["\"", "AND", "*", "((", "NEAR(", "a OR"] {
            let query = Query { text: Some(hostile.to_owned()), limit: 50, ..Query::default() };
            assert!(search(&db, project, &query).is_ok(), "{hostile} must not fail the search");
        }
    }

    #[test]
    fn the_fts_index_forgets_a_row_when_the_row_goes() {
        let (db, project) = bed();
        let rows = [record("ephemeral message", "2026-08-19T04:00:00.000000Z")];
        insert(&db, project, &rows, source::NDJSON, "k", "batch-a", &now()).unwrap();
        let query = Query { text: Some("ephemeral".to_owned()), limit: 10, ..Query::default() };
        assert_eq!(search(&db, project, &query).unwrap().len(), 1);

        db.with(|conn| conn.execute("DELETE FROM bugs_logs", [])).unwrap();
        assert!(
            search(&db, project, &query).unwrap().is_empty(),
            "an external-content index that keeps deleted rows returns ghosts"
        );
    }

    #[test]
    fn the_prune_takes_what_is_past_retention_and_leaves_the_rest() {
        let (db, project) = bed();
        let old = days_ago(45).expect("a date");
        let recent = days_ago(2).expect("a date");
        let rows = [record("ancient history", &old), record("this morning", &recent)];
        insert(&db, project, &rows, source::NDJSON, "k", "batch-a", &now()).unwrap();
        assert_eq!(count(&db, project).unwrap(), 2);

        // The default window is 30 days.
        assert_eq!(prune(&db).unwrap(), 1);
        let left = search(&db, project, &Query { limit: 10, ..Query::default() }).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, "this morning");
        assert_eq!(prune(&db).unwrap(), 0, "a second run has nothing to do");

        // And the row's search index went with it.
        let query = Query { text: Some("ancient".to_owned()), limit: 10, ..Query::default() };
        assert!(search(&db, project, &query).unwrap().is_empty());
    }
}
