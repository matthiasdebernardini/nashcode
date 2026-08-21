//! Cron monitors, end to end: real `check_in` envelope items through the real router,
//! a real object store in a tempdir, and the shipped sweep deciding what is late.
//!
//! The property under test throughout is who is allowed to say what. A client says
//! "I started", "I finished" and "I failed"; only this server ever says "you never
//! came" or "you never finished". A tracker that took a client's word for those would
//! be a tracker that is silent exactly when the client is dead, which is the one
//! moment cron monitoring exists for.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::TestBed;
use nashcode::bugs::crons::{self, status};
use nashcode::bugs::pushover::{self, Notifier};
use nashcode::config::{Config, Pushover};
use topcoat::router::{Body, Method, Router, request::Request, to_bytes};

// ---- the bed -------------------------------------------------------------------------

/// Error tracking on. `notify` turns Pushover on, pointed at an address nothing
/// answers: every test here reads the *queue*, which the notifier writes and the sender
/// drains, and a notification that was decided is the fact under test. Whether it left
/// the box is `bugs_pushover.rs`'s job.
fn bed(notify: bool) -> (TestBed, tempfile::TempDir) {
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
        bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
        bugs_drain: None,
        pushover: notify.then(|| Pushover {
            token: "app-token".to_owned(),
            user: "user-key".to_owned(),
            api_url: "http://127.0.0.1:1".to_owned(),
        }),
        public_url: "https://nashcode.example".to_owned(),
        bugs_self_dsn: None,
    });
    (common::testbed_from_config(root, config), bucket)
}

struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", self.body))
    }
}

async fn send(router: &Router, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Answer {
    let mut builder = Request::builder().method(Method::POST).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body)).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024).await.expect("body reads");
    Answer { status, body: String::from_utf8_lossy(&bytes).into_owned() }
}

/// One envelope carrying one `check_in` item, posted the way an SDK posts it.
async fn post_check_in(bed: &TestBed, id: i64, key: &str, payload: serde_json::Value) -> Answer {
    let auth = format!("Sentry sentry_version=7, sentry_client=sentry.python/2.35.0, sentry_key={key}");
    let body = format!("{{}}\n{{\"type\":\"check_in\"}}\n{payload}\n").into_bytes();
    send(
        &bed.router,
        &format!("/api/{id}/envelope/"),
        &[("x-sentry-auth", auth.as_str()), ("content-type", "application/x-sentry-envelope")],
        body,
    )
    .await
}

fn check_in(slug: &str, run: usize, status: &str) -> serde_json::Value {
    serde_json::json!({
        "check_in_id": format!("{run:032x}"),
        "monitor_slug": slug,
        "status": status,
    })
}

fn with_config(mut payload: serde_json::Value, config: serde_json::Value) -> serde_json::Value {
    payload["monitor_config"] = config;
    payload
}

fn crontab(pattern: &str) -> serde_json::Value {
    serde_json::json!({"schedule": {"type": "crontab", "value": pattern}})
}

/// Now, as the sweep counts it.
fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

// ---- storing check-ins ----------------------------------------------------------------

#[tokio::test]
async fn a_check_in_is_stored_and_answered_like_any_other_envelope() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");

    let answer = post_check_in(&bed, project.id, &project.key, check_in("nightly", 1, "ok")).await;
    assert_eq!(answer.status, 200);
    assert!(answer.json().get("id").is_some(), "the envelope is acknowledged the same way");
    bed.bugs.digested(1).await;

    let stored = bed.bugs.checkins(project.id, 10).expect("check-ins");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].slug, "nightly");
    assert_eq!(stored[0].status, status::OK);
    assert!(!stored[0].coerced);
}

#[tokio::test]
async fn a_check_in_with_no_config_declares_no_monitor_and_one_with_a_config_does() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");

    // A bare check-in for a slug nothing has declared. Stored, and that is all: with no
    // schedule there is nothing to be late against, so a monitor invented here would sit
    // on the page saying "ok" for ever.
    post_check_in(&bed, project.id, &project.key, check_in("nightly", 1, "ok")).await;
    bed.bugs.digested(1).await;
    assert!(bed.bugs.monitors(project.id).expect("monitors").is_empty());
    assert_eq!(bed.bugs.checkins(project.id, 10).expect("check-ins").len(), 1);

    // The same slug, now carrying a schedule. The monitor appears and adopts it.
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("nightly", 2, "ok"), crontab("0 2 * * *")),
    )
    .await;
    bed.bugs.digested(2).await;

    let monitors = bed.bugs.monitors(project.id).expect("monitors");
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0].slug, "nightly");
    assert_eq!(monitors[0].schedule, "0 2 * * *");
    assert_eq!(monitors[0].status, status::OK);
    assert!(monitors[0].next_checkin_latest.is_some(), "a schedule means a deadline");
}

#[tokio::test]
async fn a_schedule_that_will_not_parse_declares_nothing() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("nightly", 1, "ok"), crontab("every other tuesday")),
    )
    .await;
    bed.bugs.digested(1).await;

    assert!(
        bed.bugs.monitors(project.id).expect("monitors").is_empty(),
        "a monitor that cannot be late is not a monitor"
    );
    assert_eq!(bed.bugs.checkins(project.id, 10).expect("check-ins").len(), 1, "still evidence");
}

// ---- who is allowed to say what -------------------------------------------------------

#[tokio::test]
async fn a_client_claiming_missed_is_not_believed() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");

    for (run, claimed) in ["missed", "timeout"].iter().enumerate() {
        post_check_in(
            &bed,
            project.id,
            &project.key,
            with_config(check_in("nightly", run + 1, claimed), crontab("0 2 * * *")),
        )
        .await;
    }
    bed.bugs.digested(2).await;

    let stored = bed.bugs.checkins(project.id, 10).expect("check-ins");
    assert!(!stored.is_empty());
    for row in &stored {
        assert_eq!(row.status, status::ERROR, "a client cannot claim {}", row.status);
        assert!(row.coerced, "and the row records that it was not believed");
    }
    let monitors = bed.bugs.monitors(project.id).expect("monitors");
    assert_eq!(monitors[0].status, status::ERROR);
}

#[tokio::test]
async fn only_the_sweep_says_missed() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("nightly", 1, "ok"), crontab("0 2 * * *")),
    )
    .await;
    bed.bugs.digested(1).await;
    assert_eq!(bed.bugs.monitors(project.id).expect("monitors")[0].status, status::OK);

    // Nothing is due yet, so a sweep now decides nothing.
    assert_eq!(bed.bugs.sweep_crons().expect("sweep").total(), 0);

    // A week on, a daily job that never checked in again is plainly missing.
    let swept =
        crons::sweep_at(&bed.db, &Notifier::off(), now_unix() + 7 * 86_400).expect("sweep");
    assert_eq!(swept.missed, 1);
    assert_eq!(swept.timed_out, 0);
    assert_eq!(bed.bugs.monitors(project.id).expect("monitors")[0].status, status::MISSED);
}

#[tokio::test]
async fn a_run_that_starts_and_never_finishes_times_out() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");
    let config = serde_json::json!({
        "schedule": {"type": "interval", "value": 1, "unit": "hour"},
        "max_runtime": 5,
    });
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("import", 1, "in_progress"), config),
    )
    .await;
    bed.bugs.digested(1).await;

    let monitor = &bed.bugs.monitors(project.id).expect("monitors")[0];
    assert_eq!(monitor.status, status::IN_PROGRESS);
    assert!(monitor.timeout_at.is_some());

    // Six minutes on, the five-minute run has overrun.
    let swept = crons::sweep_at(&bed.db, &Notifier::off(), now_unix() + 6 * 60).expect("sweep");
    assert_eq!(swept, crons::Swept { missed: 0, timed_out: 1 });
    assert_eq!(bed.bugs.monitors(project.id).expect("monitors")[0].status, status::TIMEOUT);
}

// ---- notifications ---------------------------------------------------------------------

#[tokio::test]
async fn an_incident_rings_once_and_so_does_the_recovery() {
    let (bed, _bucket) = bed(true);
    let project = bed.bugs.create_project("api", None).expect("project");

    // It breaks.
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("nightly", 1, "error"), crontab("0 2 * * *")),
    )
    .await;
    bed.bugs.digested(1).await;
    let queued = pushover::history(&bed.db, 20).expect("history");
    assert_eq!(queued.len(), 1, "the incident is one message");
    assert_eq!(queued[0].reason, "cron");
    assert!(queued[0].title.contains("nightly"), "{}", queued[0].title);

    // It stays broken, loudly. Still one incident, so still one message.
    for run in 2..=4 {
        post_check_in(&bed, project.id, &project.key, check_in("nightly", run, "error")).await;
    }
    bed.bugs.digested(4).await;
    // And it goes missing on top of being broken.
    crons::sweep_at(&bed.db, bed.bugs.notifier(), now_unix() + 7 * 86_400).expect("sweep");
    assert_eq!(
        pushover::history(&bed.db, 20).expect("history").len(),
        1,
        "one thing being broken is one message, however many ways it says so"
    );

    // It recovers.
    post_check_in(&bed, project.id, &project.key, check_in("nightly", 9, "ok")).await;
    bed.bugs.digested(5).await;
    let queued = pushover::history(&bed.db, 20).expect("history");
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0].reason, "cron-recovery");
    assert_eq!(
        queued[0].url.as_deref(),
        Some(format!("https://nashcode.example/bugs/{}/crons", project.name).as_str()),
        "the link has to work from a phone"
    );

    // A second `ok` is not a second recovery.
    post_check_in(&bed, project.id, &project.key, check_in("nightly", 10, "ok")).await;
    bed.bugs.digested(6).await;
    assert_eq!(pushover::history(&bed.db, 20).expect("history").len(), 2);
}

#[tokio::test]
async fn a_missed_monitor_rings_through_the_same_one_door() {
    let (bed, _bucket) = bed(true);
    let project = bed.bugs.create_project("api", None).expect("project");
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("nightly", 1, "ok"), crontab("0 2 * * *")),
    )
    .await;
    bed.bugs.digested(1).await;
    assert!(pushover::history(&bed.db, 20).expect("history").is_empty(), "healthy is not news");

    crons::sweep_at(&bed.db, bed.bugs.notifier(), now_unix() + 7 * 86_400).expect("sweep");
    let queued = pushover::history(&bed.db, 20).expect("history");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].reason, "cron");
    assert!(queued[0].message.contains("missed"), "{}", queued[0].message);

    // Sweeping again does not ring again: the incident is already open.
    crons::sweep_at(&bed.db, bed.bugs.notifier(), now_unix() + 14 * 86_400).expect("sweep");
    assert_eq!(pushover::history(&bed.db, 20).expect("history").len(), 1);
}

// ---- the page --------------------------------------------------------------------------

#[tokio::test]
async fn the_crons_page_serves_a_person_and_a_reader() {
    let (bed, _bucket) = bed(false);
    let project = bed.bugs.create_project("api", None).expect("project");
    post_check_in(
        &bed,
        project.id,
        &project.key,
        with_config(check_in("nightly", 1, "error"), crontab("0 2 * * *")),
    )
    .await;
    bed.bugs.digested(1).await;

    let (status, body) = common::get(&bed.router, "/bugs/api/crons").await;
    assert_eq!(status, 200);
    assert!(body.contains("nightly"), "the monitor is on the page");
    assert!(body.contains("0 2 * * *"), "and so is its schedule");

    let (status, body) = common::get_json(&bed.router, "/bugs/api/crons").await;
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(json["monitors"].as_array().expect("monitors").len(), 1);
    assert_eq!(json["monitors"][0]["slug"], serde_json::json!("nightly"));
    assert_eq!(json["monitors"][0]["status"], serde_json::json!("error"));
    assert_eq!(json["incidents"].as_array().expect("incidents").len(), 1);
    assert_eq!(json["check_ins"].as_array().expect("check-ins").len(), 1);

    // A project that does not exist is a 404, the same as every other page here.
    let (status, _) = common::get(&bed.router, "/bugs/nope/crons").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn with_no_bucket_the_crons_page_is_not_there() {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");
    let bed = common::testbed_with(root, &["demo"], BTreeMap::new());
    let (status, _) = common::get(&bed.router, "/bugs/api/crons").await;
    assert_eq!(status, 404);
}
