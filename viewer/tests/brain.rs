//! `/brain` aggregates a two-repo fixture; `/brain/ask` is tested against a stub HTTP
//! server standing in for the Anthropic API — no real API calls.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{
    Work, get, post_json, spawn_stub, stacked_fixture, testbed_from_config, testbed_with,
};
use nashcode::config::Config;

fn two_repo_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    let work = stacked_fixture(&remotes, "demo");
    work.write("plans/api.md", "---\nbranch: part-1\n---\n# API plan\n\nShip it.\n");
    work.write("tasks/one.md", "---\nstatus: doing\nbranch: part-1\n---\n# One\n\nBody.\n");
    work.commit_all("docs");
    work.push("main");

    let second = common::make_remote(&remotes, "other");
    let other = Work::clone_from(&second);
    other.write("README.md", "# other\n");
    other.commit_all("initial");
    other.push("main");
    root
}

#[tokio::test]
async fn brain_aggregates_two_repos_into_the_documented_shape() {
    let root = two_repo_root();
    let bed = testbed_with(root, &["demo", "other"], BTreeMap::new());

    let (status, body) = get(&bed.router, "/brain").await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(brain["generated_at"].as_str().is_some());
    let repos = brain["repos"].as_array().expect("repos array");
    assert_eq!(repos.len(), 2);

    let demo = repos.iter().find(|r| r["name"] == "demo").expect("demo present");
    assert_eq!(demo["default_branch"], "main");
    let branches = demo["branches"].as_array().expect("branches");
    let part1 = branches.iter().find(|b| b["branch"] == "part-1").expect("part-1");
    assert_eq!(part1["parent"], "main");
    assert_eq!(part1["ahead"], 1);
    assert!(part1["ci"].as_str().is_some());

    let plans = demo["plans"].as_array().expect("plans");
    assert!(plans.iter().any(|p| p["path"] == "plans/api.md" && p["refs"]["branch"] == "part-1"));
    assert!(demo["cards"]["doing"].as_array().is_some_and(|cards| {
        cards.iter().any(|c| c["path"] == "tasks/one.md")
    }));
    assert!(demo["activity"].as_array().is_some());
    assert!(demo["open_comment_counts"].is_object());

    // ?repo= filters to one.
    let (_, filtered) = get(&bed.router, "/brain?repo=other").await;
    let brain: serde_json::Value = serde_json::from_str(&filtered).expect("json");
    assert_eq!(brain["repos"].as_array().expect("repos").len(), 1);
}

fn asking_bed(root: tempfile::TempDir, stub_url: &str, key: Option<&str>) -> common::TestBed {
    let remotes = root.path().join("remotes");
    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: vec!["demo".to_owned(), "other".to_owned()],
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        webhooks: BTreeMap::new(),
        anthropic_key: key.map(str::to_owned),
        anthropic_url: stub_url.to_owned(),
        brain_model: "claude-opus-5".to_owned(),
    });
    testbed_from_config(root, config)
}

#[tokio::test]
async fn ask_sends_one_request_and_returns_answer_and_model() {
    let response = serde_json::json!({
        "content": [
            { "type": "text", "text": "Pick up part-2; " },
            { "type": "tool_use", "id": "x", "name": "ignored", "input": {} },
            { "type": "text", "text": "its parent just merged." },
        ],
        "stop_reason": "end_turn",
        "model": "claude-opus-5-20260115",
    });
    let mut stub = spawn_stub("HTTP/1.1 200 OK", response.to_string()).await;
    let bed = asking_bed(two_repo_root(), &stub.url, Some("test-key"));

    let (status, body) = post_json(
        &bed.router,
        "/brain/ask",
        serde_json::json!({ "question": "what next?", "repo": "demo" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["answer"], "Pick up part-2; its parent just merged.");
    assert_eq!(answer["model"], "claude-opus-5-20260115");

    // Exactly the documented request shape went upstream.
    let sent = stub.received.recv().await.expect("request captured");
    let sent: serde_json::Value = serde_json::from_str(&sent).expect("json");
    assert_eq!(sent["model"], "claude-opus-5");
    assert_eq!(sent["max_tokens"], 16000);
    assert!(sent["system"].as_str().is_some());
    let user = sent["messages"][0]["content"].as_str().expect("one user message");
    assert!(user.contains("what next?"));
    assert!(user.contains("plans/api.md"), "plan text missing from the prompt");
    assert!(sent.get("temperature").is_none() && sent.get("thinking").is_none());
}

#[tokio::test]
async fn a_refusal_becomes_a_502_with_the_explanation() {
    let response = serde_json::json!({
        "content": [{ "type": "text", "text": "I can't help with that." }],
        "stop_reason": "refusal",
        "model": "claude-opus-5",
    });
    let stub = spawn_stub("HTTP/1.1 200 OK", response.to_string()).await;
    let bed = asking_bed(two_repo_root(), &stub.url, Some("test-key"));

    let (status, body) =
        post_json(&bed.router, "/brain/ask", serde_json::json!({ "question": "?" })).await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("refused"));
    assert!(body.contains("can't help"));
}

#[tokio::test]
async fn a_429_passes_through_with_the_apis_message() {
    let response = serde_json::json!({
        "type": "error",
        "error": { "type": "rate_limit_error", "message": "Number of requests exceeded." },
    });
    let stub = spawn_stub("HTTP/1.1 429 Too Many Requests", response.to_string()).await;
    let bed = asking_bed(two_repo_root(), &stub.url, Some("test-key"));

    let (status, body) =
        post_json(&bed.router, "/brain/ask", serde_json::json!({ "question": "?" })).await;
    assert_eq!(status, 429, "{body}");
    assert!(body.contains("Number of requests exceeded."));
}

#[tokio::test]
async fn without_a_key_the_route_is_a_404() {
    let bed = asking_bed(two_repo_root(), "http://127.0.0.1:1", None);
    let (status, _) =
        post_json(&bed.router, "/brain/ask", serde_json::json!({ "question": "?" })).await;
    assert_eq!(status, 404);
}
