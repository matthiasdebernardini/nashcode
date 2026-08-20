//! Notifications, against a real listener.
//!
//! Goal facts 11, 12 and 13. The stub is a genuine TCP server speaking HTTP/1.1 on
//! loopback, and the sender is the shipped one pointed at it by configuration — the
//! repo rule is real HTTP, and nothing here fakes any of nashcode's own code.
//!
//! The property under test throughout is what does *not* go out. A tracker that pushes
//! per event is a tracker whose notifications get muted inside a week, and then the one
//! that mattered is muted too.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::TestBed;
use nashcode::bugs::pushover::{self, Sender, Tick};
use nashcode::config::{Config, Pushover};

// ---- the stub ----------------------------------------------------------------------

/// One answer the stub will give.
#[derive(Clone)]
struct Reply {
    status: &'static str,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn ok() -> Self {
        Self {
            status: "HTTP/1.1 200 OK",
            headers: vec![
                ("x-limit-app-limit".to_owned(), "10000".to_owned()),
                ("x-limit-app-remaining".to_owned(), "9997".to_owned()),
            ],
            body: r#"{"status":1,"request":"abc"}"#.to_owned(),
        }
    }

    fn status(status: &'static str) -> Self {
        Self { status, headers: Vec::new(), body: r#"{"status":0}"#.to_owned() }
    }

    fn header(mut self, name: &str, value: String) -> Self {
        self.headers.push((name.to_owned(), value));
        self
    }
}

#[derive(Default)]
struct StubState {
    /// Consumed one per request. When it runs out the last one repeats.
    replies: Vec<Reply>,
    received: Vec<String>,
}

struct Stub {
    url: String,
    state: Arc<Mutex<StubState>>,
}

impl Stub {
    /// Every request gets `replies[i]`, and the last entry repeats for ever.
    async fn spawn(replies: Vec<Reply>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(StubState { replies, received: Vec::new() }));
        let held = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let state = held.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (headers_end, length) = loop {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..at]).to_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (at + 4, length);
                        }
                    };
                    while buf.len() < headers_end + length {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                    }
                    let reply = {
                        let mut state = state.lock().expect("stub state");
                        state.received.push(String::from_utf8_lossy(&buf[headers_end..]).into_owned());
                        let index = (state.received.len() - 1).min(state.replies.len().saturating_sub(1));
                        state.replies.get(index).cloned().unwrap_or_else(Reply::ok)
                    };
                    let mut head = format!(
                        "{}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (name, value) in &reply.headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(reply.body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url: format!("http://{addr}"), state }
    }

    fn requests(&self) -> Vec<String> {
        self.state.lock().expect("stub state").received.clone()
    }

    fn count(&self) -> usize {
        self.state.lock().expect("stub state").received.len()
    }
}

/// One field out of a form-encoded body.
fn field(body: &str, name: &str) -> Option<String> {
    serde_urlencoded::from_str::<Vec<(String, String)>>(body)
        .ok()?
        .into_iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

// ---- the bed -----------------------------------------------------------------------

/// Error tracking on, notifications pointed at `api_url`.
fn bed(api_url: Option<&str>) -> (TestBed, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    let bucket = tempfile::tempdir().expect("bucket");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");
    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: vec!["demo".to_owned()],
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        webhooks: BTreeMap::new(),
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:1".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: Some(format!("file://{}", bucket.path().display())),
        bugs_s3_endpoint: None,
        bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
        bugs_drain: None,
        pushover: api_url.map(|url| Pushover {
            token: "app-token".to_owned(),
            user: "user-key".to_owned(),
            api_url: url.to_owned(),
        }),
        public_url: "https://nashcode.example".to_owned(),
        bugs_self_dsn: None,
    });
    (common::testbed_from_config(root, config), bucket)
}

fn sender(bed: &TestBed) -> Sender {
    Sender::new(
        bed.db.clone(),
        bed.config.pushover.clone().expect("configured"),
        &bed.config.public_url,
    )
}

/// One envelope carrying one exception. Type and value decide the grouping.
fn envelope_typed(event_id: &str, ty: &str, value: &str) -> Vec<u8> {
    let event = serde_json::json!({
        "event_id": event_id,
        "platform": "python",
        "level": "error",
        "exception": {"values": [{"type": ty, "value": value}]},
        "tags": {"environment": "prod", "release": "9f3c1a2"},
    });
    format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes()
}

fn envelope(event_id: &str, value: &str) -> Vec<u8> {
    envelope_typed(event_id, "ValueError", value)
}

fn event_id(n: usize) -> String {
    format!("{n:032x}")
}

/// Push `count` events into the digest and wait for it to finish them.
async fn send_events(bed: &TestBed, project_id: i64, from: usize, count: usize, value: &str) {
    for n in from..from + count {
        bed.bugs.accept(project_id, envelope(&event_id(n), value)).await.expect("accepted");
    }
    bed.bugs.digested((from + count - 1) as u64).await;
}

// ---- fact 11: a regression rings once, a repeat never -------------------------------

#[tokio::test]
async fn a_new_issue_rings_once_a_repeat_never_and_a_regression_rings_again() {
    let stub = Stub::spawn(vec![Reply::ok()]).await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    send_events(&bed, project.id, 1, 1, "boom").await;
    let queued = pushover::history(&bed.db, 20).expect("history");
    assert_eq!(queued.len(), 1, "a new issue is one message");
    assert_eq!(queued[0].reason, "new");

    // The same error again is not news.
    send_events(&bed, project.id, 2, 1, "boom").await;
    assert_eq!(pushover::history(&bed.db, 20).expect("history").len(), 1);

    // Resolve it, then break it again.
    let issues = bed.bugs.issues(project.id, None).expect("issues");
    assert_eq!(issues.len(), 1);
    bed.bugs.set_state(project.id, issues[0].id, "resolved", "tester").expect("resolved");

    send_events(&bed, project.id, 3, 1, "boom").await;
    let queued = pushover::history(&bed.db, 20).expect("history");
    assert_eq!(queued.len(), 2, "the regression is the second message");
    assert_eq!(queued[0].reason, "regression");

    // And a second identical event on the reopened issue sends nothing.
    send_events(&bed, project.id, 4, 1, "boom").await;
    assert_eq!(pushover::history(&bed.db, 20).expect("history").len(), 2);

    // Drain what is queued and check it really went out.
    for _ in 0..3 {
        sender(&bed).tick().await;
    }
    assert_eq!(stub.count(), 2, "two messages for four events");
}

// ---- fact 12: the ladder, not the flood ---------------------------------------------

#[tokio::test]
async fn a_thousand_events_ring_four_times_and_never_a_thousand() {
    let stub = Stub::spawn(vec![Reply::ok()]).await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    send_events(&bed, project.id, 1, 1000, "boom").await;

    let queued = pushover::history(&bed.db, 100).expect("history");
    let reasons: Vec<&str> = queued.iter().rev().map(|row| row.reason.as_str()).collect();
    assert_eq!(reasons, vec!["new", "ladder", "ladder", "ladder"], "one issue, four messages");

    // The rungs are the ones the goal doc names.
    let rungs: Vec<String> = queued
        .iter()
        .rev()
        .filter(|row| row.reason == "ladder")
        .map(|row| row.message.lines().next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        rungs,
        vec!["10 events and still open.", "100 events and still open.", "1000 events and still open."]
    );

    let issues = bed.bugs.issues(project.id, None).expect("issues");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].events, 1000);
}

// ---- fact 13: the payload, and what an answer means ---------------------------------

#[tokio::test]
async fn the_payload_fits_and_its_link_opens_the_issue() {
    let stub = Stub::spawn(vec![Reply::ok()]).await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    // A value long enough that a careless implementation would blow both caps.
    let long = "b".repeat(4000);
    send_events(&bed, project.id, 1, 1, &long).await;
    assert_eq!(sender(&bed).tick().await, Tick::Sent(1));

    let sent = stub.requests();
    assert_eq!(sent.len(), 1);
    let title = field(&sent[0], "title").expect("a title");
    let message = field(&sent[0], "message").expect("a message");
    assert!(title.starts_with("api: "), "{title}");
    assert!(title.chars().count() <= 250, "title is {} characters", title.chars().count());
    assert!(message.chars().count() <= 1024, "message is {} characters", message.chars().count());
    assert!(!message.trim().is_empty());
    assert!(message.contains("New issue."), "{message}");
    assert!(message.contains("environment=prod"), "{message}");

    let issues = bed.bugs.issues(project.id, None).expect("issues");
    assert_eq!(
        field(&sent[0], "url").as_deref(),
        Some(format!("https://nashcode.example/bugs/api/issues/{}", issues[0].id).as_str())
    );
    assert_eq!(field(&sent[0], "url_title").as_deref(), Some("Open in nashcode"));
    assert_eq!(field(&sent[0], "priority").as_deref(), Some("0"));
    assert_eq!(field(&sent[0], "token").as_deref(), Some("app-token"));
    assert_eq!(field(&sent[0], "user").as_deref(), Some("user-key"));

    // And the link the message carries is a page that exists.
    let (status, _) = common::get(&bed.router, &format!("/bugs/api/issues/{}", issues[0].id)).await;
    assert_eq!(status, 200, "the issue page the notification links to");
}

#[tokio::test]
async fn a_fatal_issue_raises_the_priority() {
    let stub = Stub::spawn(vec![Reply::ok()]).await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    let event = serde_json::json!({
        "event_id": event_id(1),
        "level": "fatal",
        "exception": {"values": [{"type": "Panic", "value": "the disk is gone"}]},
    });
    let body = format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes();
    bed.bugs.accept(project.id, body).await.expect("accepted");
    bed.bugs.digested(1).await;

    assert_eq!(sender(&bed).tick().await, Tick::Sent(1));
    assert_eq!(field(&stub.requests()[0], "priority").as_deref(), Some("1"));
}

#[tokio::test]
async fn a_429_parks_the_whole_queue_until_reset_and_asks_nothing_more() {
    // Reset an hour out, so the park is still in force when the next tick runs.
    let reset = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs()
        + 3600)
        .to_string();
    let stub = Stub::spawn(vec![
        Reply::status("HTTP/1.1 429 Too Many Requests")
            .header("x-limit-app-reset", reset)
            .header("x-limit-app-remaining", "0".to_owned()),
    ])
    .await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    send_events(&bed, project.id, 1, 1, "boom").await;
    send_events(&bed, project.id, 2, 1, "other").await;
    assert_eq!(pushover::history(&bed.db, 10).expect("history").len(), 2);

    let sender = sender(&bed);
    let first = sender.tick().await;
    assert!(matches!(first, Tick::Parked { .. }), "{first:?}");
    assert_eq!(stub.count(), 1, "one request, and it was answered 429");

    // Every later turn is a no-op: no retry of the 429'd message, and nothing behind
    // it goes either. A 4xx is never retried, and 429 is the one that comes back.
    for _ in 0..5 {
        assert!(matches!(sender.tick().await, Tick::Parked { .. }));
    }
    assert_eq!(stub.count(), 1, "the queue is parked, not retrying");

    // Both messages are still there, unsent and unfailed: nothing was lost.
    assert_eq!(pushover::pending_count(&bed.db).expect("pending"), 2);
    let budget = bed.bugs.push_budget().expect("budget");
    assert_eq!(budget.remaining, Some(0));
    assert!(budget.parked_until.is_some());
    assert_eq!(budget.pending, 2);
}

#[tokio::test]
async fn a_4xx_is_final_and_a_5xx_comes_back() {
    let stub = Stub::spawn(vec![
        Reply::status("HTTP/1.1 400 Bad Request"),
        Reply::status("HTTP/1.1 503 Service Unavailable"),
    ])
    .await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    send_events(&bed, project.id, 1, 1, "boom").await;
    send_events(&bed, project.id, 2, 1, "other").await;

    let sender = sender(&bed);
    // The first message is judged and refused. It never comes back.
    assert_eq!(sender.tick().await, Tick::Failed(1));
    // The second is not judged at all, so it does.
    let second = sender.tick().await;
    assert!(matches!(second, Tick::Deferred { id: 2, seconds: 5 }), "{second:?}");
    assert_eq!(stub.count(), 2);

    // Neither the failed one nor the deferred one is asked about again right now.
    assert_eq!(sender.tick().await, Tick::Idle);
    assert_eq!(stub.count(), 2);

    let history = pushover::history(&bed.db, 10).expect("history");
    let refused = history.iter().find(|row| row.id == 1).expect("the refused one");
    assert!(refused.failed_at.is_some());
    assert!(refused.last_error.as_deref().unwrap_or_default().contains("400"));
    let deferred = history.iter().find(|row| row.id == 2).expect("the deferred one");
    assert!(deferred.failed_at.is_none() && deferred.sent_at.is_none());
    assert_eq!(deferred.attempts, 1);
}

#[tokio::test]
async fn the_hourly_cap_sends_one_notice_and_then_holds_the_rest() {
    let stub = Stub::spawn(vec![Reply::ok()]).await;
    let (bed, _bucket) = bed(Some(&stub.url));

    // Twenty-five distinct issues, so twenty-five "new issue" messages. The *type*
    // is what varies: grouping parameterizes the value, so `boom 1` and `boom 2`
    // would be one issue.
    let project = bed.bugs.create_project("api", None).expect("project");
    for n in 1..=25 {
        bed.bugs
            .accept(project.id, envelope_typed(&event_id(n), &format!("Error{n}"), "boom"))
            .await
            .expect("accepted");
    }
    bed.bugs.digested(25).await;
    assert_eq!(pushover::pending_count(&bed.db).expect("pending"), 25);

    let sender = sender(&bed);
    for _ in 0..pushover::HOURLY_CAP {
        assert!(matches!(sender.tick().await, Tick::Sent(_)));
    }
    assert_eq!(stub.count(), pushover::HOURLY_CAP as usize);

    // The twenty-first turn does not send the twenty-first message. It sends one
    // notice about the five that are waiting, and parks.
    let tripped = sender.tick().await;
    assert_eq!(tripped, Tick::Suppressed { pending: 5 });
    assert_eq!(stub.count(), pushover::HOURLY_CAP as usize + 1);
    let notice = stub.requests().last().cloned().expect("the notice");
    let message = field(&notice, "message").expect("a message");
    assert!(message.contains("5 pending"), "{message}");
    assert!(message.contains("held"), "{message}");

    // And exactly one notice: a second trip inside the same hour is silent.
    for _ in 0..4 {
        assert!(matches!(sender.tick().await, Tick::Parked { .. }));
    }
    assert_eq!(stub.count(), pushover::HOURLY_CAP as usize + 1, "one notice, not one per turn");
    assert_eq!(pushover::pending_count(&bed.db).expect("pending"), 5, "the rest are held, not lost");
}

// ---- off is off ---------------------------------------------------------------------

#[tokio::test]
async fn with_no_credentials_nothing_is_queued_and_everything_else_works() {
    let (bed, _bucket) = bed(None);
    let project = bed.bugs.create_project("api", None).expect("project");
    send_events(&bed, project.id, 1, 3, "boom").await;

    assert_eq!(pushover::pending_count(&bed.db).expect("pending"), 0);
    assert!(!bed.bugs.notifies());

    // The issue is still filed and still on the page.
    let issues = bed.bugs.issues(project.id, None).expect("issues");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].events, 3);
    let (status, body) = common::get_json(&bed.router, "/bugs").await;
    assert_eq!(status, 200);
    let listed: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(listed["pushover"]["on"], serde_json::json!(false));
    assert_eq!(listed["projects"].as_array().expect("a list").len(), 1);
}

#[tokio::test]
async fn a_log_line_never_notifies() {
    let stub = Stub::spawn(vec![Reply::ok()]).await;
    let (bed, _bucket) = bed(Some(&stub.url));
    let project = bed.bugs.create_project("api", None).expect("project");

    let records: Vec<nashcode::bugs::LogRecord> = (0..50)
        .map(|n| nashcode::bugs::LogRecord {
            ts: format!("2026-08-20T00:00:{n:02}.000000Z"),
            severity_text: "fatal".to_owned(),
            severity_number: 21,
            message: format!("the disk is gone, attempt {n}"),
            attributes: serde_json::Map::new(),
            code_file: None,
            code_line: None,
            code_function: None,
            trace_id: None,
        })
        .collect();
    bed.bugs
        .accept_logs(project.id, &records, nashcode::bugs::logs::source::NDJSON)
        .await
        .expect("stored");

    assert_eq!(pushover::pending_count(&bed.db).expect("pending"), 0);
    assert_eq!(sender(&bed).tick().await, Tick::Idle);
    assert_eq!(stub.count(), 0, "logs never push");
}
