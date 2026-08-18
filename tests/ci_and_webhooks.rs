//! The CI runner against real fixture repos, and webhook delivery to a local listener.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{Work, get, observed_bed, post_json, request, simple_bed, spawn_stub, stacked_fixture};
use nashgit::ci::{CiWorker, Job};
use nashgit::db::status;
use nashgit::hooks::Webhooks;

/// Add an executable `.nashgit/ci` to the fixture's main branch.
fn with_ci_script(root: &std::path::Path, script: &str) -> Work {
    let work = stacked_fixture(root, "demo");
    work.write(".nashgit/ci", script);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = work.dir.join(".nashgit/ci");
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    work.commit_all("add ci script");
    work.push("main");
    work
}

fn worker(bed: &common::TestBed, hooks: Webhooks, timeout: Duration) -> CiWorker {
    CiWorker { config: bed.config.clone(), db: bed.db.clone(), hooks, timeout }
}

async fn run_tip(bed: &common::TestBed, hooks: Webhooks, timeout: Duration, branch: &str) -> i64 {
    bed.mirrors.refresh("demo").await;
    let tip = bed.mirrors.repo("demo").tip(branch).await.expect("tip");
    let run_id = bed.app.ci.enqueue("demo", branch, &tip).expect("enqueued");
    let job = Job { run_id, repo: "demo".into(), branch: branch.into(), commit: tip };
    worker(bed, hooks, timeout).run_job(&job).await;
    run_id
}

#[tokio::test]
async fn a_green_script_records_passed_with_its_log_and_env() {
    let bed = simple_bed(|root| {
        with_ci_script(root, "#!/bin/sh\necho building $NASHGIT_REPO@$NASHGIT_BRANCH:$NASHGIT_COMMIT\nexit 0\n")
    });
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_secs(60), "main").await;

    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::PASSED);
    let log = std::fs::read_to_string(run.log_path.expect("log path")).expect("log file");
    assert!(log.contains("building demo@main:"), "{log}");
    assert!(log.contains(&tip), "commit env var missing: {log}");
}

#[tokio::test]
async fn a_red_script_records_failed_and_the_dot_blocks_merge() {
    let bed = simple_bed(|root| with_ci_script(root, "#!/bin/sh\necho boom >&2\nexit 3\n"));
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_secs(60), "main").await;

    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::FAILED);
    // stderr was merged into the captured log.
    let log = std::fs::read_to_string(run.log_path.expect("log path")).expect("log file");
    assert!(log.contains("boom"));
    assert!(status::blocks_merge(Some(&run.status)));
}

#[tokio::test]
async fn a_repo_without_a_ci_script_is_skipped() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_secs(60), "main").await;
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::SKIPPED);
    assert!(!status::blocks_merge(Some(&run.status)));
}

#[tokio::test]
async fn a_hung_script_times_out_and_records_it() {
    let bed = simple_bed(|root| with_ci_script(root, "#!/bin/sh\nsleep 30\n"));
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_millis(300), "main").await;
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::TIMEOUT);
}

#[tokio::test]
async fn the_rerun_endpoint_enqueues_the_tip_again() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let (status_code, body) = post_json(&bed.router, "/demo/main/ci/rerun", serde_json::json!({})).await;
    assert_eq!(status_code, 200, "{body}");
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run recorded");
    assert_eq!(run.status, status::QUEUED);

    // The CI pages render.
    let (code, _) = get(&bed.router, "/demo/ci").await;
    assert_eq!(code, 200);
    let (code, page) = get(&bed.router, "/demo/main/ci").await;
    assert_eq!(code, 200);
    assert!(page.contains("Re-run"));
}

#[tokio::test]
async fn push_and_ci_finished_webhooks_hit_a_local_listener() {
    // Listener first, so its URL can go into the config.
    let mut stub = spawn_stub("HTTP/1.1 200 OK", "{}".to_owned()).await;
    let mut hooks_map = BTreeMap::new();
    hooks_map.insert("push".to_owned(), vec![stub.url.clone()]);
    hooks_map.insert("ci_finished".to_owned(), vec![stub.url.clone()]);

    let bed = observed_bed(
        |root| with_ci_script(root, "#!/bin/sh\necho ok\n"),
        hooks_map.clone(),
    );

    // First refresh sees every branch tip for the first time -> push webhooks.
    bed.mirrors.refresh("demo").await;
    let push_payload = tokio::time::timeout(Duration::from_secs(5), stub.received.recv())
        .await
        .expect("push webhook delivered")
        .expect("channel open");
    let parsed: serde_json::Value = serde_json::from_str(&push_payload).expect("json");
    assert_eq!(parsed["event"], "push");
    assert_eq!(parsed["repo"], "demo");
    assert!(parsed["commit"].as_str().is_some());

    // Drain the remaining push events (one per branch).
    for _ in 0..3 {
        let _ = tokio::time::timeout(Duration::from_secs(5), stub.received.recv()).await;
    }

    // A finished CI run fires ci_finished with the status and a log tail.
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run_id = bed.db.enqueue_run("demo", "main", &tip).expect("run");
    let job = Job { run_id, repo: "demo".into(), branch: "main".into(), commit: tip };
    worker(&bed, Webhooks::new(hooks_map), Duration::from_secs(60)).run_job(&job).await;

    let ci_payload = tokio::time::timeout(Duration::from_secs(5), stub.received.recv())
        .await
        .expect("ci webhook delivered")
        .expect("channel open");
    let parsed: serde_json::Value = serde_json::from_str(&ci_payload).expect("json");
    assert_eq!(parsed["event"], "ci_finished");
    assert_eq!(parsed["status"], "passed");
    assert!(parsed["log_tail"].as_str().unwrap_or_default().contains("ok"));
}

#[tokio::test]
async fn form_posts_redirect_back_instead_of_returning_json() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let (code, _) = request(
        &bed.router,
        topcoat::router::Method::POST,
        "/demo/main/ci/rerun",
        Some(("application/x-www-form-urlencoded", String::new())),
    )
    .await;
    assert_eq!(code, 303, "form posts answer See Other");
}
