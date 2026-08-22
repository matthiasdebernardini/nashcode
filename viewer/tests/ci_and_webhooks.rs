//! The CI runner against real fixture repos, and webhook delivery to a local listener.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{Work, get, observed_bed, post_json, request, simple_bed, spawn_stub, stacked_fixture};
use nashcode::ci::{CiWorker, Job};
use nashcode::db::status;
use nashcode::hooks::Webhooks;

/// The push token the token tests hand the server. A blank one would prove nothing:
/// "the job saw no `GIT_TOKEN`" has to mean "it was withheld", not "there was none".
const TOKEN: &str = "s3cr3t-push-token";

/// The ordinary opt-in: CI on, no token.
const OPT_IN: &str = "enabled = true\n";

/// Write an executable `.nashcode/ci` — and, when `opt_in` is `Some`, the
/// `.nashcode/ci.toml` beside it — on the branch that is checked out, then push it.
fn add_ci(work: &Work, branch: &str, script: &str, opt_in: Option<&str>) {
    work.write(".nashcode/ci", script);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = work.dir.join(".nashcode/ci");
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    if let Some(body) = opt_in {
        work.write(".nashcode/ci.toml", body);
    }
    work.commit_all("add ci script");
    work.push(branch);
}

/// Add an executable `.nashcode/ci` to the fixture's main branch, with the opt-in that
/// lets it run.
fn with_ci_script(root: &std::path::Path, script: &str) -> Work {
    with_ci(root, script, OPT_IN)
}

/// The same, with the opt-in file spelled out.
fn with_ci(root: &std::path::Path, script: &str, opt_in: &str) -> Work {
    let work = stacked_fixture(root, "demo");
    add_ci(&work, "main", script, Some(opt_in));
    work
}

/// Run a branch tip against a server that actually holds a push token, and return the
/// job's log.
async fn run_tip_log(bed: &common::TestBed, branch: &str) -> String {
    bed.mirrors.refresh("demo").await;
    let tip = bed.mirrors.repo("demo").tip(branch).await.expect("tip");
    let run_id = bed.app.ci.enqueue("demo", branch, &tip).expect("enqueued");
    let job = Job { run_id, repo: "demo".into(), branch: branch.into(), commit: tip.clone() };
    let mut config = (*bed.config).clone();
    config.git_token = TOKEN.to_owned();
    CiWorker {
        config: std::sync::Arc::new(config),
        db: bed.db.clone(),
        hooks: Webhooks::new(BTreeMap::new()),
        timeout: Duration::from_secs(60),
        indexer: Some(bed.indexer.clone()),
        queue: Some(bed.app.ci.clone()),
    }
    .run_job(&job)
    .await;
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    std::fs::read_to_string(run.log_path.expect("log path")).expect("log file")
}

fn worker(bed: &common::TestBed, hooks: Webhooks, timeout: Duration) -> CiWorker {
    CiWorker {
        config: bed.config.clone(),
        db: bed.db.clone(),
        hooks,
        timeout,
        indexer: Some(bed.indexer.clone()),
        queue: Some(bed.app.ci.clone()),
    }
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
        with_ci_script(root, "#!/bin/sh\necho building $NASHCODE_REPO@$NASHCODE_BRANCH:$NASHCODE_COMMIT\nexit 0\n")
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

/// The other way to have nothing to run: opted in, but no script to run.
#[tokio::test]
async fn an_opted_in_repo_with_no_script_is_still_skipped() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        work.write(".nashcode/ci.toml", OPT_IN);
        work.commit_all("opt in without a script");
        work.push("main");
        work
    });
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_secs(60), "main").await;
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::SKIPPED);
    assert!(run.log_path.is_none(), "no script is not a reason worth a log file");
}

/// Invariant 5: pushing a branch is not permission to run code on the box. The opt-in
/// lives on the default branch, so a branch that brings its own script runs nothing.
#[tokio::test]
async fn a_branch_script_without_the_default_branch_opt_in_is_skipped() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        work.checkout("part-1");
        add_ci(&work, "part-1", "#!/bin/sh\necho pwned\n", None);
        work
    });
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_secs(60), "part-1").await;

    let tip = bed.mirrors.repo("demo").tip("part-1").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::SKIPPED);
    // Nothing ran, so nothing is wrong: a repo that never opted in still merges.
    assert!(!status::blocks_merge(Some(&run.status)));
    let log = std::fs::read_to_string(run.log_path.expect("log path")).expect("log file");
    assert_eq!(log, "ci not enabled on default branch");
}

/// The second half of the invariant: the opt-in alone buys no token, and a branch
/// cannot vote itself one by shipping its own `ci.toml`.
#[tokio::test]
async fn a_branch_cannot_grant_itself_the_git_token() {
    let bed = simple_bed(|root| {
        let script = "#!/bin/sh\necho token=${GIT_TOKEN:-unset}\n";
        let work = with_ci(root, script, OPT_IN);
        work.checkout("part-1");
        add_ci(&work, "part-1", script, Some("enabled = true\ngit_token = true\n"));
        work
    });
    let log = run_tip_log(&bed, "part-1").await;
    assert!(log.contains("token=unset"), "{log}");
    assert!(!log.contains(TOKEN), "the push token reached the job: {log}");
}

/// And the door does open, for the one repo whose default branch asked.
#[tokio::test]
async fn git_token_true_on_the_default_branch_hands_the_job_the_token() {
    let bed = simple_bed(|root| {
        with_ci(
            root,
            "#!/bin/sh\necho token=${GIT_TOKEN:-unset}\n",
            "enabled = true\ngit_token = true\n",
        )
    });
    let log = run_tip_log(&bed, "main").await;
    assert!(log.contains(&format!("token={TOKEN}")), "{log}");
}

#[tokio::test]
async fn a_hung_script_times_out_but_keeps_its_partial_output() {
    let bed = simple_bed(|root| {
        with_ci_script(root, "#!/bin/sh\necho progress before the hang\nsleep 60\n")
    });
    // Generous enough that the echo always lands first, even under a fully
    // parallel workspace test run.
    run_tip(&bed, Webhooks::new(BTreeMap::new()), Duration::from_secs(3), "main").await;
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.latest_run("demo", &tip).expect("query").expect("run exists");
    assert_eq!(run.status, status::TIMEOUT);
    let log = std::fs::read_to_string(run.log_path.expect("log path")).expect("log file");
    assert!(log.contains("progress before the hang"), "partial output kept: {log}");
    assert!(log.contains("output above is partial"), "timeout marker present: {log}");
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
