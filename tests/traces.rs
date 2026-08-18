//! Traces end to end: commit attribution, idempotent batches, the hook binary's
//! never-fail contract, and the session page.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::{get, post_json, request, simple_bed, stacked_fixture};
use topcoat::router::Method;

fn hook_binary() -> &'static str {
    env!("CARGO_BIN_EXE_nashgit")
}

#[tokio::test]
async fn head_moves_between_events_attribute_the_commit_to_the_session() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let initial = bed.remote_tip("demo", "main");

    // Event 1: the session starts at the current tip.
    let (status, body) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-attr", "agent": "claude-code",
            "events": [{ "seq": 1, "kind": "prompt", "payload": {"prompt": "add a feature"}, "head": initial }],
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The agent commits (and pushes, so the mirror can expand the range).
    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("src/new.txt", "made by the agent\n");
    let produced = work.commit_all("agent commit");
    work.push("main");
    bed.mirrors.refresh_now("demo").await;

    // Event 2 carries the new HEAD: the commit belongs to the session now.
    let (status, body) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-attr", "agent": "claude-code",
            "events": [{ "seq": 2, "kind": "stop", "payload": {}, "head": produced }],
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let outcome: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(outcome["commits"][0], produced, "{outcome}");

    // Read back as JSON.
    let (status, body) = request_json(&bed.router, "/demo/traces/sess-attr").await;
    assert_eq!(status, 200);
    let session: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(session["commits"][0], produced);
    assert_eq!(session["events"].as_array().expect("events").len(), 2);

    // And from the commit side.
    let (status, body) =
        get(&bed.router, &format!("/demo/commits/{produced}/trace")).await;
    assert_eq!(status, 200);
    let by_commit: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(by_commit["sessions"][0], "sess-attr");
}

async fn request_json(router: &topcoat::router::Router, path: &str) -> (u16, String) {
    let request = topcoat::router::request::Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("accept", "application/json")
        .body(topcoat::router::Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = topcoat::router::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn the_same_batch_posted_twice_stores_one_copy() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let batch = serde_json::json!({
        "session": "sess-idem", "agent": "backfill",
        "events": [
            { "seq": 1, "kind": "prompt", "payload": {"prompt": "hi"} },
            { "seq": 2, "kind": "stop", "payload": {} },
        ],
    });

    let (status, body) = post_json(&bed.router, "/demo/traces/events", batch.clone()).await;
    assert_eq!(status, 200, "{body}");
    let first: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(first["stored"], 2);

    let (status, body) = post_json(&bed.router, "/demo/traces/events", batch).await;
    assert_eq!(status, 200);
    let second: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(second["stored"], 0);
    assert_eq!(second["duplicates"], 2);

    let (_, body) = request_json(&bed.router, "/demo/traces/sess-idem").await;
    let session: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(session["events"].as_array().expect("events").len(), 2, "one copy only");
}

#[tokio::test]
async fn the_session_page_renders_events_and_commits() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let initial = bed.remote_tip("demo", "main");

    post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-page", "agent": "claude-code",
            "events": [
                { "seq": 1, "kind": "prompt", "payload": {"prompt": "rename the widget"}, "head": initial },
            ],
        }),
    )
    .await;
    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("src/w.txt", "widget\n");
    let produced = work.commit_all("rename widget");
    work.push("main");
    bed.mirrors.refresh_now("demo").await;
    post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-page", "agent": "claude-code",
            "events": [{ "seq": 2, "kind": "stop", "payload": {}, "head": produced }],
        }),
    )
    .await;

    // The list page shows the session; the session page shows events and the commit.
    let (status, index) = get(&bed.router, "/demo/traces").await;
    assert_eq!(status, 200);
    assert!(index.contains("sess-page"), "{index}");

    let (status, page) = get(&bed.router, "/demo/traces/sess-page").await;
    assert_eq!(status, 200);
    assert!(page.contains("rename the widget"), "prompt missing: {page}");
    assert!(page.contains("prompt") && page.contains("stop"), "event kinds missing");
    let short: String = produced.chars().take(8).collect();
    assert!(page.contains(&short), "commit missing from the page");

    // The branch page's commit list links to the trace.
    let (_, branch_page) = get(&bed.router, "/demo/main").await;
    assert!(
        branch_page.contains("/demo/traces/sess-page"),
        "trace link missing from the branch page"
    );
}

#[tokio::test]
async fn transcripts_come_back_verbatim() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let raw = "{\"type\":\"user\"}\nnot even json\n{\"type\":\"assistant\"}\n";
    let (status, body) = request(
        &bed.router,
        Method::POST,
        "/demo/traces/sess-t/transcript",
        Some(("application/octet-stream", raw.to_owned())),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, back) = get(&bed.router, "/demo/traces/sess-t/transcript").await;
    assert_eq!(status, 200);
    assert_eq!(back, raw, "bytes must be verbatim");
}

#[tokio::test]
async fn bad_trace_posts_are_client_errors() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    // Path traversal in the session id.
    let (status, _) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({ "session": "../../etc", "events": [{ "kind": "x" }] }),
    )
    .await;
    assert_eq!(status, 400);
    // Empty batch.
    let (status, _) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({ "session": "ok", "events": [] }),
    )
    .await;
    assert_eq!(status, 400);
    // Unknown repo.
    let (status, _) = post_json(
        &bed.router,
        "/ghost/traces/events",
        serde_json::json!({ "session": "ok", "events": [{ "kind": "x" }] }),
    )
    .await;
    assert_eq!(status, 404);
}

// ---- the hook binary's never-fail contract ---------------------------------------

fn run_hook(stdin: &str, envs: &[(&str, &str)]) -> std::process::ExitStatus {
    use std::io::Write;
    let mut child = std::process::Command::new(hook_binary())
        .arg("hook")
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("hook spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stdin.as_bytes())
        .expect("stdin writes");
    child.wait().expect("hook exits")
}

#[test]
fn the_hook_exits_zero_with_the_server_down() {
    let status = run_hook(
        "{\"session_id\":\"s\",\"hook_event_name\":\"Stop\",\"cwd\":\"/\"}",
        &[("NASHGIT_URL", "http://127.0.0.1:9"), ("NASHGIT_REPO", "demo")],
    );
    assert_eq!(status.code(), Some(0), "a dead server must not fail the agent's turn");
}

#[test]
fn the_hook_exits_zero_on_garbage_stdin() {
    let status = run_hook("this is not json {{{", &[("NASHGIT_URL", "http://127.0.0.1:9")]);
    assert_eq!(status.code(), Some(0), "garbage input must not fail the agent's turn");
}

#[tokio::test]
async fn the_hook_records_an_event_against_a_live_server() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    // A real listener on an ephemeral port, driven by the same router.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    // Router is not Clone; a second router over the same shared App state serves.
    let router = nashgit::web::router(bed.app.clone());
    let server = tokio::spawn(async move {
        let _ = topcoat::serve_until(listener, router, async {
            let _ = stop_rx.await;
        })
        .await;
    });

    // The hook runs from inside a clone whose origin names the repo.
    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    let payload = serde_json::json!({
        "session_id": "sess-hook",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "cwd": work.dir.to_string_lossy(),
    })
    .to_string();
    let status = tokio::task::spawn_blocking({
        let url = format!("http://{addr}");
        move || run_hook(&payload, &[("NASHGIT_URL", url.as_str())])
    })
    .await
    .expect("join");
    assert_eq!(status.code(), Some(0));

    // Give the server a moment, then the event is there with the repo inferred from
    // the git remote and the HEAD attached.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, body) = request_json(&bed.router, "/demo/traces/sess-hook").await;
    assert_eq!(status, 200, "{body}");
    let session: serde_json::Value = serde_json::from_str(&body).expect("json");
    let events = session["events"].as_array().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "PostToolUse");
    assert!(events[0]["head"].as_str().is_some_and(|h| h.len() >= 7), "HEAD attached");

    let _ = stop_tx.send(());
    let _ = server.await;
}
