//! The drain: pull buffered rows off the public ingester and replay them here.
//!
//! nashcode accepts no inbound connection. Everything the public internet sends lands
//! in a buffer on a box we do not trust, and this task goes and gets it. The protocol
//! is `ingester/README.md`, "The drainer contract" — three bearer-authed routes over
//! plain HTTP. Nothing below knows what serves them, and nothing below may learn: the
//! design's hedge is that five hundred lines of axum could replace the celld edge
//! without this file changing.
//!
//! Two rules carry the whole thing:
//!
//! 1. **A row is replayed into the same door it arrived at.** `kind: "envelope"` goes
//!    through the envelope pipeline with its streaming caps and its byte-budget queue;
//!    `kind: "logs"` goes through the NDJSON path. Not a copy of those paths — those
//!    paths.
//! 2. **An ack follows the digest, never leads it.** Everything acked is gone from the
//!    edge, so the ack is the one irreversible step. A queue that answers "busy" ends
//!    the project's cycle with no ack and the rows come back next time. Delivery is
//!    at-least-once and the dedupe on `event_id` and `dedupe_key` is what makes the
//!    redelivery cost nothing.
//!
//! The exception to rule 2 is a row that can never work: a body that will not
//! decompress, an envelope that will not split, a `kind` from a future version of the
//! edge. Deferring those for ever would wedge the project behind one bad row, so they
//! are acked and counted. The count is the thing to watch; it should be zero.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::bugs::{AcceptError, Bugs, envelope, index, ingest, logs};
use crate::config::Drain as DrainConfig;
use crate::db::{Db, DbResult, now};

/// What one drain request may ask for. The edge's own default, taken deliberately:
/// a cell serves one request at a time, so every byte in a drain answer is a byte the
/// project spends not accepting envelopes. Small batches and a loop beat one big one.
pub const DRAIN_MAX_BYTES: usize = 2 * 1024 * 1024;

/// How many batches one project may take in one cycle. The loop already stops on
/// `X-Ingest-Remaining: 0`; this is the second stop, for an edge that answers wrongly.
pub const MAX_BATCHES: usize = 64;

/// How long one control request may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---- the transport ----------------------------------------------------------------

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One HTTP answer, reduced to what the drainer reads.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub status: u16,
    /// `X-Ingest-Last-Seq`: the cursor for the next call.
    pub last_seq: Option<i64>,
    /// `X-Ingest-Remaining`: rows left after that cursor.
    pub remaining: Option<i64>,
    pub body: Vec<u8>,
}

/// A way to make one request to the ingester's control plane.
///
/// The contract is plain HTTP; how the bytes get there is the transport's business.
/// Production dials an iroh EndpointId, the tests dial loopback TCP, and the loop
/// above cannot tell the difference. That is the point.
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// `path` starts with `/` and carries its own query string. The bearer token is
    /// the transport's to add — it is part of dialling, not part of the protocol the
    /// loop reasons about.
    fn send<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        body: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<Answer, String>>;

    /// What to call this in a log line. Never the token.
    fn describe(&self) -> String;
}

/// The transport for an `http://host:port` target: ordinary TCP, ordinary HTTP.
///
/// Not a test fixture. A drainer and an ingester on the same private network have no
/// use for a hole-punched QUIC session, and the tests reach the real celld node this
/// way, which is what makes the contract testable at all.
pub struct HttpTransport {
    base: String,
    token: String,
    client: reqwest::Client,
}

/// Hand-written, because [`Transport`] requires `Debug` and the struct holds the one
/// secret in the system. A derived `Debug` puts the drain token in the first
/// `?transport` anybody ever writes, and a token in a log line is a token that has
/// leaked.
impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    pub fn new(base: &str, token: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { base: base.trim_end_matches('/').to_owned(), token: token.to_owned(), client })
    }
}

impl Transport for HttpTransport {
    fn send<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        body: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<Answer, String>> {
        Box::pin(async move {
            let url = format!("{}{path}", self.base);
            let verb = reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|error| error.to_string())?;
            let mut request =
                self.client.request(verb, &url).bearer_auth(&self.token);
            if let Some(body) = body {
                request = request.header("content-type", "application/json").body(body);
            }
            let response = request.send().await.map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            // Read the two headers out before the body consumes the response.
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let last_seq = header("x-ingest-last-seq");
            let remaining = header("x-ingest-remaining");
            let body = response.bytes().await.map_err(|error| error.to_string())?.to_vec();
            Ok(answer_from(status, last_seq, remaining, body))
        })
    }

    fn describe(&self) -> String {
        self.base.clone()
    }
}

/// Assemble an [`Answer`] out of what a transport read off the wire.
pub fn answer_from(
    status: u16,
    last_seq: Option<String>,
    remaining: Option<String>,
    body: Vec<u8>,
) -> Answer {
    Answer {
        status,
        last_seq: last_seq.and_then(|value| value.trim().parse().ok()),
        remaining: remaining.and_then(|value| value.trim().parse().ok()),
        body,
    }
}

/// Build the transport a configuration asks for. Async because binding an iroh
/// endpoint is, and doing it lazily on the first tick would hide a bad key until the
/// first drain rather than at startup.
pub async fn transport_for(config: &DrainConfig) -> Result<Arc<dyn Transport>, String> {
    if config.is_url() {
        return Ok(Arc::new(HttpTransport::new(&config.target, &config.token)?));
    }
    super::iroh::transport(config).await
}

// ---- one drained row ---------------------------------------------------------------

/// One line of a drain answer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub seq: i64,
    #[serde(default)]
    pub received_at: i64,
    pub kind: String,
    #[serde(default)]
    pub content_encoding: Option<String>,
    /// The last `X-Forwarded-For` entry the edge could parse as an address, or null.
    /// Data, never markup: everything else in that header came from the client.
    #[serde(default)]
    pub remote_ip: Option<String>,
    #[serde(default)]
    pub bytes: i64,
    /// Base64 of the bytes exactly as they arrived — still compressed when
    /// `content_encoding` says so.
    pub body: String,
}

/// What happened to one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The digest took it. Safe to ack.
    Accepted,
    /// It can never work. Acked anyway, and counted, so one bad row cannot wedge the
    /// project behind it for ever.
    Poison(String),
    /// Not now. No ack, and the rest of the batch waits with it.
    Defer(String),
}

/// What one cycle did. Returned rather than only logged so a test can assert on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cycle {
    /// Rows the digest took.
    pub replayed: usize,
    /// Rows acked, poison included.
    pub acked: usize,
    /// Rows that can never work.
    pub poison: usize,
    /// Projects whose cycle ended early because the digest was not ready.
    pub deferred: usize,
    /// Transport or protocol failures.
    pub errors: usize,
    /// The registry was pushed this cycle.
    pub registry_pushed: bool,
}

// ---- the drainer --------------------------------------------------------------------

/// The tokio task, and everything a test needs to drive one cycle of it by hand.
pub struct Drainer {
    bugs: Bugs,
    db: Db,
    transport: Arc<dyn Transport>,
    max_bytes: usize,
    /// The last reachability we logged. A dead ingester is one line per state change,
    /// not one line every thirty seconds for a week.
    reach: std::sync::Mutex<Reach>,
    /// The last complaint logged about each project, and how many cycles running it has
    /// been starved. Both are per process: they decide what gets logged and when to
    /// reach for the cursor rewind, and neither is worth a table.
    gripes: std::sync::Mutex<std::collections::HashMap<i64, (Gripe, u32)>>,
    poison: AtomicU64,
}

/// Why the ingester is not answering. The *kind* is what a state change is measured
/// on, never the message: two unreachable ticks whose `std::io` strings differ by a
/// port number are one state, and comparing the text made every tick a change and put
/// a Down/Up pair in the log every thirty seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Unknown,
    Up,
    Down(Down),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Down {
    Token,
    Unreachable,
}

/// The last thing worth saying about one project. Same discipline as [`Reach`]: a
/// complaint is logged when it starts, not once per cycle for as long as it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gripe {
    Fine,
    /// The edge answered something other than 200 or 404.
    Refused(u16),
    /// The edge answered with lines that will not parse.
    Unreadable,
    /// The edge says rows are waiting and hands back none of them.
    Starved,
}

/// A failure that ends the whole cycle rather than one project's share of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fatal {
    /// Every control route answers 404 without the token, so a 404 here is almost
    /// certainly the token and almost certainly not the routing.
    Token,
    /// The ingester is not answering.
    Unreachable(String),
}

impl Drainer {
    pub fn new(bugs: Bugs, db: Db, transport: Arc<dyn Transport>) -> Self {
        Self {
            bugs,
            db,
            transport,
            max_bytes: DRAIN_MAX_BYTES,
            reach: std::sync::Mutex::new(Reach::Unknown),
            gripes: std::sync::Mutex::new(std::collections::HashMap::new()),
            poison: AtomicU64::new(0),
        }
    }

    /// How many rows this process has given up on. Should be zero.
    pub fn poison_count(&self) -> u64 {
        self.poison.load(Ordering::Relaxed)
    }

    /// Push the registry if it moved, then drain every active project.
    pub async fn cycle(&self) -> Cycle {
        let mut report = Cycle::default();
        let registry_ok = self.push_registry(&mut report).await;

        let projects = match index::registry(&self.db) {
            Ok(projects) => projects,
            Err(error) => {
                tracing::warn!(%error, "drain: cannot list the projects");
                report.errors += 1;
                return report;
            }
        };
        for (id, _, active) in projects {
            if !active {
                continue;
            }
            match self.drain_project(id, &mut report).await {
                Ok(()) => {}
                Err(fatal) => {
                    self.note(fatal);
                    report.errors += 1;
                    return report;
                }
            }
        }
        // `note_up` is the other half of the once-per-state-change rule, so it has to
        // be as strict as `note`. It used to fire whenever the projects drained, which
        // for a registry PUT that fails every cycle meant a Down line and an Up line
        // every thirty seconds for ever — the noise the latch exists to prevent.
        if registry_ok && report.errors == 0 {
            self.note_up();
        }
        report
    }

    /// Run for ever, one cycle per interval.
    pub async fn run(self, interval: Duration) {
        let mut every = tokio::time::interval(interval);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            let report = self.cycle().await;
            if report.replayed > 0 || report.poison > 0 || report.registry_pushed {
                tracing::info!(
                    replayed = report.replayed,
                    acked = report.acked,
                    poison = report.poison,
                    registry = report.registry_pushed,
                    "drain: a cycle brought something back"
                );
            }
        }
    }

    // ---- the registry ---------------------------------------------------------------

    /// `PUT` the whole `(project_id, key, active)` set, whenever it differs from the
    /// set we last pushed.
    ///
    /// Replaces, never merges: a project that has been revoked here has to stop
    /// authenticating there, and a merge would leave it working for ever. The hash of
    /// the last accepted push lives in SQLite, so a restart does not re-push a set
    /// nothing changed about.
    async fn push_registry(&self, report: &mut Cycle) -> bool {
        let projects = match index::registry(&self.db) {
            Ok(projects) => projects,
            Err(error) => {
                tracing::warn!(%error, "drain: cannot read the project set");
                report.errors += 1;
                return false;
            }
        };
        let payload = registry_payload(&projects);
        let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
        match self.state("registry_hash") {
            Ok(Some(seen)) if seen == hash => return true,
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "drain: cannot read the registry push state"),
        }

        // The edge refuses an empty set without `?allow_empty=1`, and it is right to.
        // Emptying it takes every project on the fleet offline, and an SDK reads the
        // resulting 404 as a verdict rather than as weather — it destroys events. An
        // empty set here is far more likely to be a bug than an intention, so the
        // automatic path never opens that door. An operator with a real reason can.
        if projects.is_empty() {
            tracing::warn!(
                "drain: nashcode has no projects; refusing to empty the ingester's registry. \
                 PUT it by hand with ?allow_empty=1 if that is really the intent"
            );
            return true;
        }

        match self
            .transport
            .send("PUT", "/_nashcode/registry", Some(payload.into_bytes()))
            .await
        {
            Ok(answer) if answer.status == 200 => {
                if let Err(error) = self.set_state("registry_hash", &hash) {
                    tracing::warn!(%error, "drain: cannot record the registry push");
                }
                report.registry_pushed = true;
                tracing::info!(projects = projects.len(), "drain: pushed the project registry");
                true
            }
            Ok(answer) if answer.status == 404 => {
                report.errors += 1;
                self.note(Fatal::Token);
                false
            }
            Ok(answer) => {
                report.errors += 1;
                tracing::warn!(
                    status = answer.status,
                    detail = %String::from_utf8_lossy(&answer.body),
                    "drain: the ingester refused the registry"
                );
                false
            }
            Err(error) => {
                report.errors += 1;
                self.note(Fatal::Unreachable(error));
                false
            }
        }
    }

    // ---- one project ----------------------------------------------------------------

    async fn drain_project(&self, project_id: i64, report: &mut Cycle) -> Result<(), Fatal> {
        let mut cursor = self.cursor(project_id);
        for _ in 0..MAX_BATCHES {
            let path = format!(
                "/_nashcode/drain/{project_id}?after={cursor}&max_bytes={}",
                self.max_bytes
            );
            let answer = match self.transport.send("GET", &path, None).await {
                Ok(answer) => answer,
                Err(error) => return Err(Fatal::Unreachable(error)),
            };
            match answer.status {
                200 => {}
                404 => return Err(Fatal::Token),
                status => {
                    self.gripe(project_id, Gripe::Refused(status), || {
                        format!(
                            "drain: project {project_id} is answering {status}: {}",
                            String::from_utf8_lossy(&answer.body)
                        )
                    });
                    report.errors += 1;
                    return Ok(());
                }
            }

            let batch = parse_rows(&answer.body);
            let remaining = answer.remaining.unwrap_or_else(|| {
                // The header is how the loop knows to come back inside one cycle.
                // Without it every drain is one batch per interval, which turns a
                // backlog into an afternoon. Worth saying out loud once.
                self.gripe(project_id, Gripe::Unreadable, || {
                    format!(
                        "drain: project {project_id} answered without X-Ingest-Remaining; \
                         draining one batch per cycle until it comes back"
                    )
                });
                0
            });

            if batch.rows.is_empty() {
                return self
                    .nothing_usable(project_id, &batch, &answer, remaining, cursor, report)
                    .await;
            }
            if batch.dropped > 0 {
                self.gripe(project_id, Gripe::Unreadable, || {
                    format!(
                        "drain: dropped {} unreadable line(s) from project {project_id}",
                        batch.dropped
                    )
                });
            }

            // `acked_to` walks up only over rows the digest is finished with. The
            // first row it cannot take stops the batch where it is, which is what
            // keeps the ack behind the digest rather than ahead of it.
            let mut acked_to: Option<i64> = None;
            let mut taken = 0usize;
            let mut deferred = false;
            for row in &batch.rows {
                match self.replay(project_id, row).await {
                    Outcome::Accepted => {
                        report.replayed += 1;
                        taken += 1;
                        acked_to = Some(row.seq);
                    }
                    Outcome::Poison(why) => {
                        tracing::warn!(
                            project_id,
                            seq = row.seq,
                            kind = row.kind,
                            why,
                            "drain: acking a row nothing here can ever digest"
                        );
                        self.poison.fetch_add(1, Ordering::Relaxed);
                        report.poison += 1;
                        taken += 1;
                        acked_to = Some(row.seq);
                    }
                    Outcome::Defer(why) => {
                        tracing::debug!(
                            project_id,
                            seq = row.seq,
                            why,
                            "drain: leaving the rest of the batch on the edge"
                        );
                        report.deferred += 1;
                        deferred = true;
                        break;
                    }
                }
            }

            if let Some(up_to) = acked_to {
                match self.ack(project_id, up_to).await {
                    Ok(true) => {
                        self.set_cursor(project_id, up_to);
                        cursor = up_to;
                        report.acked += taken;
                        // Rows moved, so whatever we were complaining about is over.
                        if batch.dropped == 0 {
                            self.gripe(project_id, Gripe::Fine, String::new);
                        }
                    }
                    // A failed ack is not a lost row. The cursor stays where it was,
                    // the same rows come back next cycle, and the digest throws the
                    // duplicates away.
                    Ok(false) => return Ok(()),
                    Err(fatal) => return Err(fatal),
                }
            }

            if deferred || remaining <= 0 {
                return Ok(());
            }
        }
        tracing::warn!(project_id, "drain: stopped at the batch limit; more is waiting");
        Ok(())
    }

    /// A 200 that yielded no row this loop can act on. Two very different things.
    ///
    /// **Bytes but no rows** is a batch of lines none of which parse. Left alone that
    /// wedges the project for ever: the lowest unacked line is unreadable, so nothing is
    /// acked, so the cursor never moves, so the next cycle fetches the same line again.
    /// `X-Ingest-Last-Seq` is the way out — it names the highest seq the edge just
    /// served, whether or not we could read it, so acking to it steps over the whole
    /// bad batch. The lines are counted as poison, because that is what they are.
    ///
    /// **No bytes, and yet rows are said to be waiting** is the shape of a cursor that
    /// is ahead of the buffer: a replacement VPS whose cell was rebuilt from an older
    /// snapshot hands out sequence numbers we have already passed, and a strictly
    /// monotone cursor then starves for ever in silence. One occurrence is logged; a
    /// second in a row rewinds the cursor to what the edge says it last served and lets
    /// the dedupe absorb the replay. Rewinding is safe by construction — that is the
    /// whole point of at-least-once — and starving is not.
    async fn nothing_usable(
        &self,
        project_id: i64,
        batch: &Batch,
        answer: &Answer,
        remaining: i64,
        cursor: i64,
        report: &mut Cycle,
    ) -> Result<(), Fatal> {
        if batch.dropped > 0 {
            let last = answer.last_seq.unwrap_or(cursor);
            self.gripe(project_id, Gripe::Unreadable, || {
                format!(
                    "drain: every line project {project_id} served after {cursor} is \
                     unreadable ({} of them); acking past them to {last} rather than \
                     asking for them again for ever",
                    batch.dropped
                )
            });
            if last <= cursor {
                // No cursor to move to. Nothing to do but say so and come back.
                report.errors += 1;
                return Ok(());
            }
            self.poison.fetch_add(batch.dropped as u64, Ordering::Relaxed);
            report.poison += batch.dropped;
            return match self.ack(project_id, last).await {
                Ok(true) => {
                    self.set_cursor(project_id, last);
                    report.acked += batch.dropped;
                    Ok(())
                }
                Ok(false) => Ok(()),
                Err(fatal) => Err(fatal),
            };
        }

        if remaining > 0 {
            let strikes = self.strike(project_id);
            self.gripe(project_id, Gripe::Starved, || {
                format!(
                    "drain: project {project_id} says {remaining} row(s) are waiting \
                     after {cursor} and serves none of them. A cursor ahead of the \
                     buffer is what a rebuilt edge looks like"
                )
            });
            if strikes >= 2
                && let Some(last) = answer.last_seq
                && last < cursor
            {
                tracing::warn!(
                    project_id,
                    from = cursor,
                    to = last,
                    "drain: rewinding the ack cursor; dedupe absorbs whatever comes back twice"
                );
                self.rewind_cursor(project_id, last);
                self.clear_strikes(project_id);
            }
            report.errors += 1;
        }
        Ok(())
    }

    /// `POST /_nashcode/ack`. `false` means "not now" and leaves the cursor alone.
    async fn ack(&self, project_id: i64, up_to: i64) -> Result<bool, Fatal> {
        // `up_to` has to be a JSON number: the edge refuses a string, because `"5"`
        // coerces to something plausible in JavaScript and that endpoint deletes rows
        // for a living.
        let body = serde_json::json!({ "up_to": up_to }).to_string().into_bytes();
        let path = format!("/_nashcode/ack/{project_id}");
        match self.transport.send("POST", &path, Some(body)).await {
            Ok(answer) if answer.status == 200 => Ok(true),
            Ok(answer) if answer.status == 404 => Err(Fatal::Token),
            Ok(answer) => {
                tracing::warn!(
                    status = answer.status,
                    project_id,
                    up_to,
                    detail = %String::from_utf8_lossy(&answer.body),
                    "drain: the ingester refused an ack"
                );
                Ok(false)
            }
            Err(error) => Err(Fatal::Unreachable(error)),
        }
    }

    // ---- replay ----------------------------------------------------------------------

    /// Put one row through the door it arrived at.
    pub async fn replay(&self, project_id: i64, row: &Row) -> Outcome {
        let raw = match BASE64.decode(row.body.as_bytes()) {
            Ok(raw) => raw,
            Err(error) => return Outcome::Poison(format!("the body is not base64: {error}")),
        };
        // The edge caps a body at 2 MiB compressed, which is a tenth of what the
        // tailnet door takes. Checking anyway: this side must not depend on a cap
        // enforced by a box we do not trust.
        if raw.len() > ingest::MAX_COMPRESSED {
            return Outcome::Poison(format!("{} bytes is over the compressed cap", raw.len()));
        }
        let body = match ingest::decompress(row.content_encoding.as_deref(), raw) {
            Ok(body) => body,
            Err(error) => return Outcome::Poison(error.to_string()),
        };

        match row.kind.as_str() {
            "envelope" => self.replay_envelope(project_id, body).await,
            "logs" => self.replay_logs(project_id, row.seq, body).await,
            other => Outcome::Poison(format!("no door takes a row of kind {other}")),
        }
    }

    async fn replay_envelope(&self, project_id: i64, body: Vec<u8>) -> Outcome {
        // Split first, for the same reason the live door does: an envelope that will
        // not split is a 400 there and is poison here, and finding that out before the
        // bucket write keeps the bucket free of objects nothing will ever read.
        if let Err(error) = envelope::split(&body) {
            return Outcome::Poison(error.to_string());
        }
        match self.bugs.accept(project_id, body).await {
            Ok(()) => Outcome::Accepted,
            Err(AcceptError::Busy) => Outcome::Defer("the digest queue is full".to_owned()),
            Err(AcceptError::Store(detail)) => Outcome::Defer(detail),
        }
    }

    async fn replay_logs(&self, project_id: i64, seq: i64, body: Vec<u8>) -> Outcome {
        let parsed = logs::parse_ndjson(&body, envelope::MAX_ITEM, logs::MAX_BATCH);
        if parsed.too_many {
            return Outcome::Poison(format!(
                "more than {} log lines in one batch",
                logs::MAX_BATCH
            ));
        }
        // The same source the direct NDJSON door records, so a drained batch and a
        // posted one are the same row. Anything else would make the edge visible in
        // the data, and the edge is a delivery detail.
        //
        // The origin is not cosmetic. A log row's dedupe key is built from it, and the
        // default — a freshly minted archive key — is different on every delivery, so
        // an at-least-once redelivery filed every line a second time and acceptance
        // fact 3 was false for the log door. `drain/<project>/<seq>` is the one name
        // the edge promises never repeats and never rewinds.
        let origin = drain_origin(project_id, seq);
        match self
            .bugs
            .accept_logs_with_origin(
                project_id,
                &parsed.records,
                logs::source::NDJSON,
                Some(&origin),
            )
            .await
        {
            Ok(_) => Outcome::Accepted,
            Err(detail) => Outcome::Defer(detail),
        }
    }

    // ---- state -----------------------------------------------------------------------

    /// The highest seq we have acked for a project.
    pub fn cursor(&self, project_id: i64) -> i64 {
        self.db
            .with(|conn| {
                conn.query_row(
                    "SELECT acked_seq FROM bugs_drain_cursor WHERE project_id = ?1",
                    params![project_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            })
            .unwrap_or_default()
            .unwrap_or(0)
    }

    /// Move the cursor backwards, which nothing else here is allowed to do.
    ///
    /// Only [`Self::nothing_usable`] calls it, and only when the edge has said twice in
    /// a row that it is holding rows we are asking past. Replaying is free — dedupe
    /// eats it — and starving is not.
    fn rewind_cursor(&self, project_id: i64, seq: i64) {
        let written = self.db.with(|conn| {
            conn.execute(
                "INSERT INTO bugs_drain_cursor (project_id, acked_seq, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(project_id) DO UPDATE SET acked_seq = ?2, updated_at = ?3",
                params![project_id, seq, now()],
            )
        });
        if let Err(error) = written {
            tracing::warn!(%error, project_id, seq, "drain: cannot rewind the ack cursor");
        }
    }

    /// One more consecutive starved cycle for this project; the running total.
    fn strike(&self, project_id: i64) -> u32 {
        let mut gripes = self.gripes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = gripes.entry(project_id).or_insert((Gripe::Fine, 0));
        entry.1 = entry.1.saturating_add(1);
        entry.1
    }

    fn clear_strikes(&self, project_id: i64) {
        let mut gripes = self.gripes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = gripes.get_mut(&project_id) {
            entry.1 = 0;
        }
    }

    /// Say something about a project, but only when it is news.
    ///
    /// The message closure is not called unless the complaint has changed, so building
    /// it is free on the thousandth identical cycle.
    fn gripe(&self, project_id: i64, next: Gripe, message: impl FnOnce() -> String) {
        let changed = {
            let mut gripes =
                self.gripes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = gripes.entry(project_id).or_insert((Gripe::Fine, 0));
            let changed = entry.0 != next;
            entry.0 = next;
            if next != Gripe::Starved {
                entry.1 = 0;
            }
            changed
        };
        if changed && next != Gripe::Fine {
            tracing::warn!("{}", message());
        }
    }

    fn set_cursor(&self, project_id: i64, seq: i64) {
        let written = self.db.with(|conn| {
            conn.execute(
                "INSERT INTO bugs_drain_cursor (project_id, acked_seq, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(project_id) DO UPDATE SET acked_seq = ?2, updated_at = ?3
                 WHERE bugs_drain_cursor.acked_seq < ?2",
                params![project_id, seq, now()],
            )
        });
        if let Err(error) = written {
            tracing::warn!(%error, project_id, seq, "drain: cannot record the ack cursor");
        }
    }

    fn state(&self, key: &str) -> DbResult<Option<String>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT value FROM bugs_drain_state WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })
    }

    fn set_state(&self, key: &str, value: &str) -> DbResult<()> {
        self.db.with(|conn| {
            conn.execute(
                "INSERT INTO bugs_drain_state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
                params![key, value],
            )?;
            Ok(())
        })
    }

    // ---- logging -----------------------------------------------------------------------

    /// One line per state change. A drainer that logged every failed tick would write
    /// three thousand identical lines a day while the VPS was down, and the one line
    /// that mattered would be the first.
    fn note(&self, fatal: Fatal) {
        let (next, message) = match &fatal {
            Fatal::Token => (
                Reach::Down(Down::Token),
                format!(
                    "drain: {} answers 404 on every control route. That is what it says \
                     to a stranger, so this is almost certainly the drain token and \
                     almost certainly not the routing",
                    self.transport.describe()
                ),
            ),
            Fatal::Unreachable(detail) => (
                Reach::Down(Down::Unreachable),
                format!("drain: cannot reach {}: {detail}", self.transport.describe()),
            ),
        };
        let mut reach = self.reach.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *reach != next {
            tracing::warn!("{message}");
            *reach = next;
        }
    }

    fn note_up(&self) {
        let mut reach = self.reach.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*reach, Reach::Down(_)) {
            tracing::info!("drain: {} is answering again", self.transport.describe());
        }
        *reach = Reach::Up;
    }
}

/// The stable identity of a drained log batch, which its rows' dedupe keys are built
/// from. `seq` rises for ever within a project and is never reused, so the same batch
/// delivered twice lands on the same key both times.
pub fn drain_origin(project_id: i64, seq: i64) -> String {
    format!("drain/{project_id}/{seq}")
}

/// The registry body, canonical so the same set always hashes the same way.
///
/// `project_id` is a string because the edge stores it as one — it is the cell name,
/// not an integer it does arithmetic with.
fn registry_payload(projects: &[(i64, String, bool)]) -> String {
    let entries: Vec<serde_json::Value> = projects
        .iter()
        .map(|(id, key, active)| {
            serde_json::json!({ "project_id": id.to_string(), "key": key, "active": active })
        })
        .collect();
    serde_json::json!({ "projects": entries }).to_string()
}

/// What one drain answer contained.
#[derive(Debug, Default, Clone)]
pub struct Batch {
    pub rows: Vec<Row>,
    /// Lines that were there and would not parse. The count matters as much as the
    /// rows: a batch of nothing but these is what wedges a project, and the caller
    /// needs to know the difference between that and an empty answer.
    pub dropped: usize,
}

/// NDJSON in, rows out. A line that will not parse is dropped rather than taking the
/// batch with it: the rows around it are real events, and refusing all of them over one
/// would lose more than it protected.
///
/// The dropping is silent here on purpose. This runs once per batch per cycle, and a
/// warning inside it is a warning every thirty seconds for as long as the bad line
/// sits there. The caller owns the complaint, and complains once.
pub fn parse_rows(body: &[u8]) -> Batch {
    let mut batch = Batch::default();
    for line in body.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Row>(line) {
            Ok(row) => batch.rows.push(row),
            Err(_) => batch.dropped += 1,
        }
    }
    batch
}

/// The drainer's own tables. Owned here rather than in `index.rs` for the same reason
/// the log store owns its own: a table nothing else reads belongs to the thing that
/// reads it.
pub fn migrate(db: &Db) -> DbResult<()> {
    db.with(|conn| {
        conn.execute_batch(SCHEMA)?;
        Ok(())
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bugs_drain_cursor (
    project_id INTEGER PRIMARY KEY,
    acked_seq  INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS bugs_drain_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::config::Config;

    /// A transport backed by a list of rows in memory.
    ///
    /// Not a stand-in for the ingester — `viewer/tests/bugs_drain.rs` drives the real
    /// celld node over the real protocol, and that is what proves the HTTP shapes.
    /// This is here for the two states a real edge cannot be made to hold still in:
    /// a digest queue that is exactly full, and an ack that is refused.
    #[derive(Debug, Default)]
    struct Fake {
        rows: Mutex<Vec<Row>>,
        /// Requests seen, as `METHOD path`.
        seen: Mutex<Vec<String>>,
        /// Status the ack route answers.
        ack_status: Mutex<u16>,
        /// Status every route answers, when set. 404 is the wrong-token case.
        force: Mutex<Option<u16>>,
        /// A drain answer served verbatim: body, `X-Ingest-Last-Seq`,
        /// `X-Ingest-Remaining`. For the two shapes of "nothing this loop can use".
        raw: Mutex<Option<(Vec<u8>, i64, i64)>>,
    }

    impl Fake {
        fn new(rows: Vec<Row>) -> Arc<Self> {
            Arc::new(Self {
                rows: Mutex::new(rows),
                ack_status: Mutex::new(200),
                ..Self::default()
            })
        }

        fn rows(&self) -> Vec<Row> {
            self.rows.lock().unwrap().clone()
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Transport for Fake {
        fn send<'a>(
            &'a self,
            method: &'a str,
            path: &'a str,
            body: Option<Vec<u8>>,
        ) -> BoxFuture<'a, Result<Answer, String>> {
            self.seen.lock().unwrap().push(format!("{method} {path}"));
            let forced = *self.force.lock().unwrap();
            Box::pin(async move {
                if let Some(status) = forced {
                    return Ok(Answer { status, body: b"{\"detail\":\"not found\"}".to_vec(), ..Answer::default() });
                }
                if path.starts_with("/_nashcode/registry") {
                    return Ok(Answer { status: 200, ..Answer::default() });
                }
                if path.starts_with("/_nashcode/ack/") {
                    let status = *self.ack_status.lock().unwrap();
                    if status == 200 {
                        let up_to = serde_json::from_slice::<serde_json::Value>(
                            &body.unwrap_or_default(),
                        )
                        .ok()
                        .and_then(|value| value.get("up_to").and_then(serde_json::Value::as_i64))
                        .unwrap_or(0);
                        self.rows.lock().unwrap().retain(|row| row.seq > up_to);
                    }
                    return Ok(Answer { status, ..Answer::default() });
                }
                if let Some((body, last, remaining)) = self.raw.lock().unwrap().clone() {
                    return Ok(Answer {
                        status: 200,
                        last_seq: Some(last),
                        remaining: Some(remaining),
                        body,
                    });
                }
                let after: i64 = path
                    .split("after=")
                    .nth(1)
                    .and_then(|rest| rest.split('&').next())
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let rows = self.rows.lock().unwrap();
                let batch: Vec<&Row> = rows.iter().filter(|row| row.seq > after).collect();
                let last = batch.last().map(|row| row.seq).unwrap_or(after);
                let mut body = Vec::new();
                for row in &batch {
                    body.extend_from_slice(serde_json::to_string(row).unwrap().as_bytes());
                    body.push(b'\n');
                }
                Ok(Answer { status: 200, last_seq: Some(last), remaining: Some(0), body })
            })
        }

        fn describe(&self) -> String {
            "the fake".to_owned()
        }
    }

    const ENVELOPE: &str =
        "{}\n{\"type\":\"event\"}\n{\"event_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"level\":\"error\"}\n";

    fn row(seq: i64, kind: &str, body: &[u8]) -> Row {
        Row {
            seq,
            received_at: 0,
            kind: kind.to_owned(),
            content_encoding: None,
            remote_ip: None,
            bytes: body.len() as i64,
            body: BASE64.encode(body),
        }
    }

    fn drainer(rows: Vec<Row>) -> (Drainer, Arc<Fake>, Bugs, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            bugs_bucket: Some(format!("file://{}", dir.path().join("bucket").display())),
            bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
            ..Config::from_env()
        };
        let db = Db::in_memory().unwrap();
        let bugs = Bugs::new(&config, db.clone()).unwrap();
        bugs.create_project("demo", None).unwrap();
        let fake = Fake::new(rows);
        let drainer = Drainer::new(bugs.clone(), db, fake.clone() as Arc<dyn Transport>);
        (drainer, fake, bugs, dir)
    }

    #[tokio::test]
    async fn a_full_queue_defers_the_ack_and_the_rows_come_back() {
        let (drainer, fake, bugs, _dir) = drainer(vec![
            row(1, "envelope", ENVELOPE.as_bytes()),
            row(2, "envelope", ENVELOPE.as_bytes()),
        ]);

        // Hold the whole byte budget. This is the state a real edge cannot be asked
        // to hold still in, and it is the one the SPEC rule is about.
        let held = bugs
            .inner
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(crate::bugs::QUEUE_BYTES).unwrap())
            .expect("the budget starts empty");
        let report = drainer.cycle().await;
        assert_eq!(report.replayed, 0, "nothing was digested");
        assert_eq!(report.acked, 0, "so nothing was acked");
        assert_eq!(report.deferred, 1);
        assert_eq!(fake.rows().len(), 2, "both rows are still on the edge");
        assert_eq!(drainer.cursor(1), 0, "and the cursor never moved");
        assert!(!fake.seen().iter().any(|seen| seen.starts_with("POST /_nashcode/ack")));

        // Give the memory back and the same rows go through.
        drop(held);
        let report = drainer.cycle().await;
        assert_eq!(report.replayed, 2);
        assert_eq!(report.acked, 2);
        assert!(fake.rows().is_empty(), "and the edge dropped them");
        assert_eq!(drainer.cursor(1), 2);
    }

    #[tokio::test]
    async fn an_ack_the_edge_refuses_leaves_the_cursor_where_it_was() {
        let (drainer, fake, _bugs, _dir) = drainer(vec![row(1, "envelope", ENVELOPE.as_bytes())]);
        *fake.ack_status.lock().unwrap() = 500;

        let report = drainer.cycle().await;
        assert_eq!(report.replayed, 1, "the digest took it");
        assert_eq!(report.acked, 0, "the edge did not");
        assert_eq!(drainer.cursor(1), 0, "so the row comes back and dedupe eats it");
        assert_eq!(fake.rows().len(), 1);
    }

    #[tokio::test]
    async fn a_row_nothing_can_digest_is_acked_and_counted() {
        let (drainer, fake, _bugs, _dir) = drainer(vec![
            row(1, "envelope", b"not an envelope at all"),
            row(2, "envelope", ENVELOPE.as_bytes()),
            row(3, "quiche", b"{}"),
        ]);

        let report = drainer.cycle().await;
        assert_eq!(report.poison, 2, "the bad envelope and the unknown kind");
        assert_eq!(report.replayed, 1);
        assert_eq!(report.acked, 3, "all three left the edge");
        assert_eq!(drainer.poison_count(), 2);
        assert!(fake.rows().is_empty(), "one bad row cannot wedge the project behind it");
    }

    #[tokio::test]
    async fn a_body_that_will_not_decompress_is_poison_rather_than_a_retry_for_ever() {
        let mut bad = row(1, "envelope", b"this was never gzip");
        bad.content_encoding = Some("gzip".to_owned());
        let (drainer, fake, _bugs, _dir) = drainer(vec![bad]);

        let report = drainer.cycle().await;
        assert_eq!(report.poison, 1);
        assert!(fake.rows().is_empty());
    }

    #[tokio::test]
    async fn the_registry_is_pushed_once_and_not_again_until_it_changes() {
        let (drainer, fake, bugs, _dir) = drainer(Vec::new());

        assert!(drainer.cycle().await.registry_pushed);
        assert!(!drainer.cycle().await.registry_pushed, "nothing changed");

        bugs.create_project("second", None).unwrap();
        assert!(drainer.cycle().await.registry_pushed, "a new project is a change");

        let pushes =
            fake.seen().iter().filter(|seen| seen.starts_with("PUT /_nashcode/registry")).count();
        assert_eq!(pushes, 2);
    }

    #[tokio::test]
    async fn an_empty_project_set_is_never_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            bugs_bucket: Some(format!("file://{}", dir.path().join("bucket").display())),
            bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
            ..Config::from_env()
        };
        let db = Db::in_memory().unwrap();
        let bugs = Bugs::new(&config, db.clone()).unwrap();
        let fake = Fake::new(Vec::new());
        let drainer = Drainer::new(bugs, db, fake.clone() as Arc<dyn Transport>);

        let report = drainer.cycle().await;
        assert!(!report.registry_pushed);
        assert!(
            fake.seen().is_empty(),
            "emptying the registry takes every project offline; it never happens by itself"
        );
    }

    #[tokio::test]
    async fn a_404_on_every_route_is_reported_as_the_token() {
        let (drainer, fake, _bugs, _dir) = drainer(vec![row(1, "envelope", ENVELOPE.as_bytes())]);
        *fake.force.lock().unwrap() = Some(404);

        let report = drainer.cycle().await;
        assert_eq!(report.replayed, 0);
        assert!(report.errors > 0);
    }

    fn log_query() -> logs::Query {
        logs::Query { text: None, level: None, file: None, limit: 100, offset: 0 }
    }

    /// Blocker 1, without a celld node. A drained log batch delivered twice must file
    /// each line once. It did not: `accept_logs` named the batch by a freshly minted
    /// archive key, so the dedupe key was different on every delivery.
    #[tokio::test]
    async fn a_redelivered_log_batch_files_each_line_once() {
        let batch = b"{\"level\":\"error\",\"message\":\"one\"}\n{\"level\":\"info\",\"message\":\"two\"}\n";
        let (drainer, fake, bugs, _dir) = drainer(vec![row(1, "logs", batch)]);
        // Refuse the ack, which is exactly the redelivery window the protocol promises
        // is safe: the rows stay on the edge and come back next cycle.
        *fake.ack_status.lock().unwrap() = 500;

        assert_eq!(drainer.cycle().await.replayed, 1);
        assert_eq!(bugs.logs(1, &log_query()).unwrap().len(), 2, "two lines, filed once");

        assert_eq!(drainer.cycle().await.replayed, 1, "the same batch again");
        assert_eq!(
            bugs.logs(1, &log_query()).unwrap().len(),
            2,
            "still two: a drained batch dedupes on its seq, not on a fresh archive key"
        );
    }

    #[test]
    fn a_drained_batch_is_named_by_something_that_never_repeats() {
        assert_eq!(drain_origin(7, 42), "drain/7/42");
        assert_ne!(drain_origin(7, 42), drain_origin(7, 43));
        assert_ne!(drain_origin(7, 42), drain_origin(8, 42));
    }

    /// Blocker 4. The lowest unacked line is unreadable, so nothing parses, so nothing
    /// is acked, so the cursor never moves and the next cycle asks for it again — for
    /// ever. `X-Ingest-Last-Seq` is the way past it.
    #[tokio::test]
    async fn a_batch_of_unreadable_lines_is_acked_past_rather_than_asked_for_for_ever() {
        let (drainer, fake, _bugs, _dir) = drainer(Vec::new());
        *fake.raw.lock().unwrap() =
            Some((b"{ this is not json\n{ nor is this\n".to_vec(), 9, 0));

        let report = drainer.cycle().await;
        assert_eq!(report.poison, 2, "both lines counted, not silently dropped");
        assert_eq!(report.acked, 2);
        assert_eq!(drainer.cursor(1), 9, "the cursor stepped over the whole bad batch");
        assert_eq!(drainer.poison_count(), 2);
    }

    /// And it does not ack into thin air when the header gives it nowhere to go.
    #[tokio::test]
    async fn an_unreadable_batch_with_no_cursor_to_move_to_acks_nothing() {
        let (drainer, fake, _bugs, _dir) = drainer(Vec::new());
        *fake.raw.lock().unwrap() = Some((b"{ this is not json\n".to_vec(), 0, 0));

        let report = drainer.cycle().await;
        assert_eq!(report.acked, 0);
        assert_eq!(drainer.cursor(1), 0);
        assert!(report.errors > 0, "and it is reported rather than passed over");
    }

    /// Blocker 6. A replacement edge rebuilt from an older snapshot hands out sequence
    /// numbers we have already passed. A strictly monotone cursor then starves in
    /// silence for ever. Two consecutive cycles of it rewind the cursor.
    #[tokio::test]
    async fn an_edge_that_serves_none_of_the_rows_it_says_it_has_rewinds_the_cursor() {
        let (drainer, fake, _bugs, _dir) = drainer(Vec::new());
        drainer.set_cursor(1, 500);
        // Nothing in the body, and yet seven rows are said to be waiting.
        *fake.raw.lock().unwrap() = Some((Vec::new(), 12, 7));

        drainer.cycle().await;
        assert_eq!(drainer.cursor(1), 500, "one occurrence is a complaint, not an action");

        drainer.cycle().await;
        assert_eq!(drainer.cursor(1), 12, "the second in a row rewinds; dedupe absorbs the replay");
    }

    #[tokio::test]
    async fn an_idle_edge_is_not_mistaken_for_a_starved_one() {
        let (drainer, fake, _bugs, _dir) = drainer(Vec::new());
        drainer.set_cursor(1, 500);
        *fake.raw.lock().unwrap() = Some((Vec::new(), 500, 0));

        for _ in 0..4 {
            let report = drainer.cycle().await;
            assert_eq!(report.errors, 0, "an empty buffer is the normal case");
        }
        assert_eq!(drainer.cursor(1), 500);
    }

    /// The drain token is the one secret in the system, and `Transport` requires
    /// `Debug`. One `?transport` in a future log line must not leak it.
    #[test]
    fn no_debug_output_anywhere_carries_the_drain_token() {
        let secret = "swordfishswordfishswordfishswor";
        let transport = HttpTransport::new("http://127.0.0.1:1", secret).unwrap();
        let printed = format!("{transport:?}");
        assert!(!printed.contains(secret), "{printed}");
        assert!(printed.contains("<redacted>"));

        let config = crate::config::Drain {
            target: "http://127.0.0.1:1".to_owned(),
            token: secret.to_owned(),
            key_path: std::path::PathBuf::from("/tmp/k"),
            interval: Duration::from_secs(30),
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains(secret), "{printed}");
    }

    #[test]
    fn a_drain_line_that_will_not_parse_does_not_take_the_batch_with_it() {
        let body = b"{\"seq\":1,\"kind\":\"envelope\",\"body\":\"e30=\"}\n{ this is not json\n\n{\"seq\":3,\"kind\":\"logs\",\"body\":\"e30=\"}\n";
        let batch = parse_rows(body);
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.dropped, 1, "and the bad one is counted, not forgotten");
        assert_eq!(batch.rows[0].seq, 1);
        assert_eq!(batch.rows[1].seq, 3);
    }

    #[test]
    fn the_registry_body_is_what_the_edge_validates() {
        let payload = registry_payload(&[(1, "0".repeat(32), true), (2, "f".repeat(32), false)]);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let projects = parsed["projects"].as_array().unwrap();
        assert_eq!(projects[0]["project_id"], "1", "a string: it is a cell name, not a number");
        assert_eq!(projects[0]["active"], true);
        assert_eq!(projects[1]["active"], false);
        assert_eq!(projects[1]["key"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn the_cursor_survives_a_new_drainer_over_the_same_database() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            bugs_bucket: Some(format!("file://{}", dir.path().join("bucket").display())),
            ..Config::from_env()
        };
        let db = Db::open(&dir.path().join("nashcode.db")).unwrap();
        let bugs = Bugs::new(&config, db.clone()).unwrap();
        let fake = Fake::new(Vec::new());

        let first = Drainer::new(bugs.clone(), db.clone(), fake.clone() as Arc<dyn Transport>);
        first.set_cursor(7, 42);
        drop(first);

        let second = Drainer::new(bugs, db, fake as Arc<dyn Transport>);
        assert_eq!(second.cursor(7), 42);
        // And it never walks backwards, whatever order the answers arrive in.
        second.set_cursor(7, 9);
        assert_eq!(second.cursor(7), 42);
    }
}
