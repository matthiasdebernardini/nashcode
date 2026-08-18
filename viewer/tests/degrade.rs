//! Degrade, never 500: with the git server dead, every page still renders from the
//! existing mirror behind a stale banner.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{get, stacked_fixture, testbed_from_config, testbed_with};
use nashcode::config::Config;

#[tokio::test]
async fn every_page_survives_a_dead_git_server_on_an_existing_mirror() {
    // Live first: build the fixture and let the mirror clone it.
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    let work = stacked_fixture(&remotes, "demo");
    work.write("plans/api.md", "# Plan\n\nText.\n");
    work.write("tasks/one.md", "---\nstatus: todo\n---\n# One\n");
    work.commit_all("docs");
    work.push("main");

    let live = testbed_with(root, &["demo"], BTreeMap::new());
    live.mirrors.refresh("demo").await;
    assert!(live.config.mirror_path("demo").exists(), "mirror cloned while live");

    // Now the same mirrors and database, but DGIT_URL points at a dead host.
    let dead_config = Arc::new(Config {
        dgit_url: "http://127.0.0.1:9/dead".to_owned(),
        git_token: String::new(),
        repos: vec!["demo".to_owned()],
        mirrors: live.config.mirrors.clone(),
        bind: "127.0.0.1:0".to_owned(),
        db_path: live.root.path().join("nashcode-dead.db"),
        ci_logs: live.root.path().join("ci-logs"),
        traces: live.root.path().join("traces"),
        webhooks: BTreeMap::new(),
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:9".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
    });
    let dead = testbed_from_config(tempfile::tempdir().expect("tempdir"), dead_config);

    for path in [
        "/",
        "/demo",
        "/demo/stacks",
        "/demo/plans",
        "/demo/plans/api.md",
        "/demo/board",
        "/demo/ci",
        "/demo/part-1",
        "/demo/tasks/one.md",
    ] {
        let (status, body) = get(&dead.router, path).await;
        assert_eq!(status, 200, "{path} -> {status}");
        // The repo pages carry the stale banner; the index marks the repo stale.
        assert!(
            body.contains("last mirrored state") || body.contains(">stale<"),
            "{path} should say it is stale"
        );
    }

    // The brain also answers from the stale mirror.
    let (status, body) = get(&dead.router, "/brain").await;
    assert_eq!(status, 200);
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(brain["repos"][0]["stale"], true);

    // A repo that never cloned shows the error card, still not a 500.
    let missing_config = Arc::new(Config {
        repos: vec!["ghost".to_owned()],
        mirrors: live.root.path().join("empty-mirrors"),
        db_path: live.root.path().join("nashcode-ghost.db"),
        ..(*dead.config).clone()
    });
    let ghost = testbed_from_config(tempfile::tempdir().expect("tempdir"), missing_config);
    let (status, body) = get(&ghost.router, "/ghost").await;
    assert_eq!(status, 200);
    assert!(body.contains("has no mirror yet"));
}
