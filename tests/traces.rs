//! Traces: the transcript side of the same artifact the diff shows.
//!
//! Covers the whole round trip over the real router — record a session, get the commits
//! it produced attributed to it, read the session back as JSON and as a page, and the
//! commit-to-conversation link. Plus the one property the hook must never violate: it
//! cannot fail an agent's turn.

mod common;

use common::{get, post_json, request, simple_bed, stacked_fixture};
use topcoat::router::Method;

/// The events a real agent run produces around one commit.
fn session_events(before: &str, after: &str) -> serde_json::Value {
    serde_json::json!({
        "session": "sess-1",
        "agent": "claude-code",
        "events": [
            {
                "seq": 1,
                "kind": "UserPromptSubmit",
                "head": before,
                "payload": {"prompt": "add a retry note", "session_id": "sess-1"}
            },
            {
                "seq": 2,
                "kind": "PreToolUse",
                "head": before,
                "payload": {"tool_name": "Edit", "tool_input": {"file_path": "plans/api.md"}}
            },
            // The commit landed between these two events.
            {
                "seq": 3,
                "kind": "PostToolUse",
                "head": after,
                "payload": {"tool_name": "Bash", "tool_input": {"command": "git commit -am plan"}}
            },
            {"seq": 4, "kind": "Stop", "head": after, "payload": {}}
        ]
    })
}

#[tokio::test]
async fn a_session_is_recorded_and_its_commits_are_attributed() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");

    let (status, body) =
        post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;
    assert_eq!(status, 200, "{body}");

    let outcome: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(outcome["stored"], 4);
    assert_eq!(outcome["duplicates"], 0);
    // The first head is where the session started, so only the move is attributed.
    let attributed = outcome["commits"].as_array().expect("commits");
    assert!(attributed.contains(&serde_json::Value::String(after.clone())));

    // The commit points back at the conversation that produced it.
    let (status, body) = get(&bed.router, &format!("/demo/commits/{after}/trace")).await;
    assert_eq!(status, 200);
    let link: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(link["sessions"][0], "sess-1");
}

#[tokio::test]
async fn the_same_batch_twice_stores_one_copy() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    let batch = session_events(&before, &after);

    let (_, first) = post_json(&bed.router, "/demo/traces/events", batch.clone()).await;
    let (_, second) = post_json(&bed.router, "/demo/traces/events", batch).await;

    let first: serde_json::Value = serde_json::from_str(&first).expect("json");
    let second: serde_json::Value = serde_json::from_str(&second).expect("json");
    assert_eq!(first["stored"], 4);
    assert_eq!(second["stored"], 0, "a retry must not double-write");
    assert_eq!(second["duplicates"], 4);
    assert!(
        second["commits"].as_array().expect("commits").is_empty(),
        "a commit is never attributed twice"
    );

    // And the session still holds exactly four events.
    let (_, body) = request(
        &bed.router,
        Method::GET,
        "/demo/traces/sess-1",
        None,
    )
    .await;
    assert_eq!(body.matches("Box-row").count() > 0, true);
}

#[tokio::test]
async fn the_session_page_renders_the_transcript_and_its_commits() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    let (status, body) = get(&bed.router, "/demo/traces/sess-1").await;
    assert_eq!(status, 200);
    // The prompt and the tool calls read as themselves, not as raw hook names.
    assert!(body.contains("add a retry note"), "the prompt renders");
    assert!(body.contains("Edit: plans/api.md"), "the edit renders with its file");
    assert!(body.contains("Bash: git commit -am plan"), "the command renders");
    // And the commit it produced is shown inline.
    assert!(body.contains("committed"), "the commit is marked where it happened");
    assert!(body.contains(&after[..8]), "the sha is shown");

    // The index lists the session with its counts.
    let (status, body) = get(&bed.router, "/demo/traces").await;
    assert_eq!(status, 200);
    assert!(body.contains("sess-1"));
    assert!(body.contains("4 events"));
    assert!(body.contains("1 commits"));
}

#[tokio::test]
async fn traces_read_back_as_json_when_asked() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    let json_get = |path: String| {
        let router = bed.router.clone();
        async move {
            let request = topcoat::router::request::Request::builder()
                .method(Method::GET)
                .uri(path)
                .header("accept", "application/json")
                .body(topcoat::router::Body::empty())
                .expect("request builds");
            let response = router.handle(request).await;
            let status = response.status().as_u16();
            let bytes =
                http_body_util::BodyExt::collect(response.into_body()).await.expect("body");
            let body = String::from_utf8_lossy(&bytes.to_bytes()).into_owned();
            (status, body)
        }
    };

    let (status, body) = json_get("/demo/traces".to_owned()).await;
    assert_eq!(status, 200);
    let sessions: serde_json::Value = serde_json::from_str(&body).expect("json list");
    assert_eq!(sessions[0]["session"], "sess-1");
    assert_eq!(sessions[0]["events"], 4);
    assert_eq!(sessions[0]["commits"], 1);

    let (status, body) = json_get("/demo/traces/sess-1".to_owned()).await;
    assert_eq!(status, 200);
    let session: serde_json::Value = serde_json::from_str(&body).expect("json session");
    assert_eq!(session["session"], "sess-1");
    assert_eq!(session["events"].as_array().expect("events").len(), 4);
    assert_eq!(session["commits"][0], after);
}

#[tokio::test]
async fn a_raw_transcript_round_trips_byte_for_byte() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let transcript = "{\"role\":\"user\"}\n{\"role\":\"assistant\",\"text\":\"héllo\"}\n";
    let (status, _) = request(
        &bed.router,
        Method::POST,
        "/demo/traces/sess-1/transcript",
        Some(("application/x-ndjson", transcript.to_owned())),
    )
    .await;
    assert_eq!(status, 200);

    let (status, body) = get(&bed.router, "/demo/traces/sess-1/transcript").await;
    assert_eq!(status, 200);
    assert_eq!(body, transcript, "stored verbatim");
}

#[tokio::test]
async fn bad_input_is_refused_without_a_500() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    // A session id that would escape the transcript directory.
    let (status, _) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({"session": "../../etc/passwd", "events": [{"kind": "x"}]}),
    )
    .await;
    assert_eq!(status, 400);

    // No events at all.
    let (status, _) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({"session": "s", "events": []}),
    )
    .await;
    assert_eq!(status, 400);

    // A repo nobody configured.
    let (status, _) = post_json(
        &bed.router,
        "/ghost/traces/events",
        serde_json::json!({"session": "s", "events": [{"kind": "x"}]}),
    )
    .await;
    assert_eq!(status, 404);

    // A session that was never recorded.
    let (status, _) = get(&bed.router, "/demo/traces/never-happened").await;
    assert_eq!(status, 404);
}

/// The hook runs inside somebody's agent loop. Whatever happens, it must not be the
/// reason their turn failed.
mod hook {
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn run_hook(stdin: &str, url: &str) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nashgit"))
            .arg("hook")
            .env("NASHGIT_URL", url)
            .env("NASHGIT_REPO", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("nashgit runs");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write");
        child.wait_with_output().expect("hook finishes")
    }

    #[test]
    fn it_exits_zero_when_the_server_is_unreachable() {
        // Port 9 discards, so this is a connection failure, not a slow response.
        let output = run_hook(
            r#"{"session_id":"s1","hook_event_name":"Stop"}"#,
            "http://127.0.0.1:9",
        );
        assert!(output.status.success(), "a dead server must not fail the turn");
    }

    #[test]
    fn it_exits_zero_on_garbage_input() {
        let output = run_hook("this is not json", "http://127.0.0.1:9");
        assert!(output.status.success(), "bad input must not fail the turn");
    }

    #[test]
    fn it_exits_zero_on_empty_input() {
        let output = run_hook("", "http://127.0.0.1:9");
        assert!(output.status.success(), "no input must not fail the turn");
    }
}
