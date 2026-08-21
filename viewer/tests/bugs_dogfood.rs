//! Dogfooding: nashcode's own errors arrive in nashcode's own project (goal fact 19),
//! and the loop that would make is closed.
//!
//! Everything here is real. A real listener on loopback, the real Sentry Rust SDK
//! pointed at a DSN this viewer minted, the real envelope door, the real digest. That
//! is the point of dogfooding: an instance that watched itself through a private
//! channel would never exercise the door every other service uses.
//!
//! The SDK's client is process-global, so these tests would fight over it if they
//! shared a process. `cargo nextest run` gives each test its own, which is why the
//! repo's rule about nextest is load-bearing here rather than a preference.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::TestBed;
use nashcode::bugs::selfreport;
use nashcode::config::Config;

/// A viewer with error tracking on, served on a real loopback port.
struct Live {
    bed: TestBed,
    /// The DSN of the project nashcode reports itself to.
    dsn: String,
    /// The origin the viewer is actually listening on.
    base: String,
    project_id: i64,
    project_key: String,
    _stop: tokio::sync::oneshot::Sender<()>,
    _bucket: tempfile::TempDir,
}

async fn live() -> Live {
    let root = tempfile::tempdir().expect("tempdir");
    let bucket = tempfile::tempdir().expect("bucket");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");

    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: ["demo"].into_iter().collect(),
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
        bugs_ingest_url: "http://127.0.0.1:0".to_owned(),
        bugs_drain: None,
        pushover: None,
        public_url: "http://127.0.0.1:0".to_owned(),
        bugs_self_dsn: None,
    });
    let bed = common::testbed_from_config(root, config);
    let project = bed.bugs.create_project("nashcode", None).expect("project");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = nashcode::web::router(bed.app.clone());
    tokio::spawn(async move {
        let _ = topcoat::serve_until(listener, router, async {
            let _ = stopped.await;
        })
        .await;
    });

    // The connection string an SDK would be handed, pointed at the port we just took.
    Live {
        bed,
        dsn: format!("http://{}@{addr}/{}", project.key, project.id),
        base: format!("http://{addr}"),
        project_id: project.id,
        project_key: project.key,
        _stop: stop,
        _bucket: bucket,
    }
}

/// Wire the SDK and the tracing layer exactly as `main.rs` does.
fn report_through(dsn: &str) -> sentry::ClientInitGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let guard = selfreport::init(Some(dsn), None).expect("the SDK starts");
    tracing_subscriber::registry().with(selfreport::layer()).init();
    guard
}

/// Every log the process emits, with the guard's answer at the moment it was emitted.
type Witnessed = Arc<std::sync::Mutex<Vec<(String, bool)>>>;

/// A layer that only remembers. It sits beside the real one, so what it records is what
/// the real one saw — the same event, on the same task, at the same instant.
struct Witness(Witnessed);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Witness {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        self.0
            .lock()
            .expect("witness")
            .push((event.metadata().target().to_owned(), selfreport::in_pipeline()));
    }
}

/// The same wiring, plus a witness. Used to prove the guard is *live* on a real code
/// path rather than merely correct as a rule.
fn report_through_witnessed(dsn: &str) -> (sentry::ClientInitGuard, Witnessed) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let guard = selfreport::init(Some(dsn), None).expect("the SDK starts");
    let seen: Witnessed = Arc::new(std::sync::Mutex::new(Vec::new()));
    tracing_subscriber::registry()
        .with(selfreport::layer())
        .with(Witness(seen.clone()))
        .init();
    (guard, seen)
}

/// Push whatever the SDK is holding, then let the viewer digest it.
async fn settle(live: &Live, guard: &sentry::ClientInitGuard, expect: u64) {
    guard.flush(Some(Duration::from_secs(5)));
    if expect > 0 {
        // Bounded: if the event never arrives this fails as a timeout rather than
        // hanging the suite.
        let _ = tokio::time::timeout(Duration::from_secs(10), live.bed.bugs.digested(expect)).await;
    } else {
        // Nothing is expected, so there is nothing to wait for. Give the transport a
        // moment to prove it: an assertion that passes because we looked too early is
        // not an assertion.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

fn issue_titles(live: &Live) -> Vec<String> {
    live.bed
        .bugs
        .issues(live.project_id, None)
        .expect("issues")
        .into_iter()
        .map(|issue| issue.title)
        .collect()
}

// ---- fact 19 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_error_from_a_request_handler_lands_in_nashcodes_own_project() {
    let live = live().await;
    let guard = report_through(&live.dsn);

    // What a handler failing looks like from here: an error on a target that is not
    // the error-tracking pipeline.
    tracing::error!(target: "nashcode::web::pages", "the mirror is gone");
    settle(&live, &guard, 1).await;

    let titles = issue_titles(&live);
    assert_eq!(titles.len(), 1, "one issue, from {titles:?}");
    assert!(titles[0].contains("the mirror is gone"), "{}", titles[0]);

    // And it is marked as nashcode talking about itself.
    let issues = live.bed.bugs.issues(live.project_id, None).expect("issues");
    let event = live.bed.bugs.latest_event(issues[0].id).expect("event").expect("an event");
    let raw = live.bed.bugs.payload(&event.object_key).await.expect("the payload");
    let payload: serde_json::Value = serde_json::from_slice(&raw).expect("JSON");
    assert_eq!(payload["tags"][selfreport::SELF_TAG], serde_json::json!("1"));
}

// ---- the loop, closed ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn nothing_the_error_tracking_pipeline_says_is_ever_reported_back_into_it() {
    let live = live().await;
    let guard = report_through(&live.dsn);

    // Every one of these is a real line this code base logs on a bad day. Reporting
    // any of them posts an envelope to this same instance, whose digest logs the next
    // one — a loop with an HTTP request in it.
    tracing::error!(target: "nashcode::bugs::digest", "cannot digest envelope");
    tracing::error!(target: "nashcode::bugs", "the bucket will not open");
    tracing::error!(target: "nashcode::web::bugs", "cannot store an envelope");
    tracing::error!(target: "nashcode::bugs::pushover", "Pushover refused a notification");

    // And the transitive case the names cannot catch: a failure inside git, logged by
    // git, while the pipeline is the thing that called it.
    selfreport::quietly(async {
        tracing::error!(target: "nashcode::git", "git exited 128");
    })
    .await;

    settle(&live, &guard, 0).await;
    assert_eq!(issue_titles(&live), Vec::<String>::new(), "the loop stayed closed");

    // The guard is a rule about the pipeline, not a mute button. Ordinary errors still
    // report, in the same process, immediately afterwards.
    tracing::error!(target: "nashcode::ops", "cannot push the branch");
    settle(&live, &guard, 1).await;
    let titles = issue_titles(&live);
    assert_eq!(titles.len(), 1, "from {titles:?}");
    assert!(titles[0].contains("cannot push the branch"), "{}", titles[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ingest_door_runs_inside_the_guard() {
    let live = live().await;
    let guard = report_through(&live.dsn);

    // Post an envelope the way an SDK does, and watch what the door reports about
    // itself while it handles it: nothing. This is the property the door's own
    // `quietly` wrapper exists for, and it holds for everything the door calls.
    let event = serde_json::json!({
        "event_id": "b".repeat(32),
        "exception": {"values": [{"type": "ValueError", "value": "from an SDK"}]},
    });
    let body = format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n");
    let url = format!(
        "{}/api/{}/envelope/?sentry_key={}",
        live.base, live.project_id, live.project_key
    );
    let answer = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/x-sentry-envelope")
        .body(body)
        .send()
        .await
        .expect("the door answers");
    assert_eq!(answer.status(), 200);

    settle(&live, &guard, 1).await;
    // Exactly the one event that was posted. Nothing the door said about handling it
    // became a second one.
    let titles = issue_titles(&live);
    assert_eq!(titles.len(), 1, "from {titles:?}");
    assert!(titles[0].contains("from an SDK"), "{}", titles[0]);
}

#[tokio::test]
async fn with_no_self_dsn_the_sdk_never_starts() {
    assert!(selfreport::init(None, None).is_none());
    assert!(selfreport::init(Some("   "), None).is_none());
}

/// The guard, proved against a failure nobody wrote a branch for.
///
/// Every other test here states the rule and checks it. This one breaks the bucket
/// underneath a live ingest — the object store's directory simply stops existing, which
/// is what a detached volume looks like — and then asks what the shipped wiring did
/// while a real error was being logged by real code on a real request.
///
/// Two things have to be true together, and neither is interesting alone. The door must
/// genuinely have failed and said so at ERROR, or the test proves nothing but that
/// nothing happened. And the guard must have been *live* at that instant, on that task,
/// which is the property `quietly` exists for and the one a rule-shaped test cannot
/// reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_bucket_that_vanishes_mid_flight_is_reported_to_nobody() {
    let live = live().await;
    let (guard, seen) = report_through_witnessed(&live.dsn);

    // The volume goes away. Nothing tells the viewer; it finds out by writing.
    std::fs::remove_dir_all(live._bucket.path()).expect("the bucket goes");
    // And it cannot come back by accident: `object_store`'s local backend creates
    // parents, so leaving a file where the directory was is what keeps it broken.
    std::fs::write(live._bucket.path(), b"not a directory").expect("block the path");

    let event = serde_json::json!({
        "event_id": "c".repeat(32),
        "exception": {"values": [{"type": "ValueError", "value": "during an outage"}]},
    });
    let url = format!(
        "{}/api/{}/envelope/?sentry_key={}",
        live.base, live.project_id, live.project_key
    );
    let answer = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/x-sentry-envelope")
        .body(format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n"))
        .send()
        .await
        .expect("the door answers");
    // 502, not 500 and not a hang: the payload has nowhere durable to go and the sender
    // is told so in the one way a Sentry SDK knows how to obey.
    assert_eq!(answer.status(), 502, "the failure has to be real for this test to mean anything");

    settle(&live, &guard, 0).await;

    let seen = seen.lock().expect("witness").clone();
    let from_the_door: Vec<&(String, bool)> =
        seen.iter().filter(|(target, _)| target.starts_with("nashcode::")).collect();
    assert!(
        !from_the_door.is_empty(),
        "the door logged nothing, so this test watched an outage nobody noticed: {seen:?}"
    );
    assert!(
        from_the_door.iter().all(|(_, guarded)| *guarded),
        "a line was logged from the ingest path with the guard down: {from_the_door:?}"
    );
}
