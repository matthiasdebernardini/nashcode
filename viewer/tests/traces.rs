//! Traces: the transcript side of the same artifact the diff shows.
//!
//! Covers the whole round trip over the real router — record a session, get the commits
//! it produced attributed to it, read the session back as JSON and as a page, and the
//! commit-to-conversation link. Plus the one property the hook must never violate: it
//! cannot fail an agent's turn.

mod common;

use common::{get, get_json, post_json, redirect, request, simple_bed, stacked_fixture};
use topcoat::router::{Method, Router};

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

    // And the session still holds exactly four events, not eight.
    let (_, body) = get_json(&bed.router, "/demo/traces/sess-1").await;
    let session: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(session["events"].as_array().expect("events").len(), 4);
}

#[tokio::test]
async fn the_session_page_renders_the_conversation_and_its_commits() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    let (status, body) = get(&bed.router, "/demo/agent/sess-1").await;
    assert_eq!(status, 200);
    // The prompt and the tool calls read as themselves, not as raw hook names.
    assert!(body.contains("add a retry note"), "the prompt renders");
    assert!(body.contains("plans/api.md"), "the edit names its file");
    assert!(body.contains("git commit -am plan"), "the command renders");
    // And the commit it produced is shown inline.
    assert!(body.contains("committed"), "the commit is marked where it happened");
    assert!(body.contains(&after[..8]), "the sha is shown");

    // The index lists the session under its first prompt, with its counts.
    let (status, body) = get(&bed.router, "/demo/agent").await;
    assert_eq!(status, 200);
    assert!(body.contains("sess-1"));
    assert!(body.contains("add a retry note"), "the first prompt titles the session");
    assert!(body.contains("4 events"));
    assert!(body.contains("1 commits"));
}

/// Two tabs became one. The old URLs keep working for anyone who bookmarked them.
#[tokio::test]
async fn the_old_pages_redirect_into_the_agent_tab() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    for (from, to) in [
        ("/demo/traces", "/demo/agent"),
        ("/demo/traces/sess-1", "/demo/agent/sess-1"),
        ("/demo/prompts", "/demo/agent"),
        ("/demo/prompts?q=retry", "/demo/agent?q=retry"),
    ] {
        let (status, location) = redirect(&bed.router, from).await;
        assert_eq!(status, 301, "{from} moved permanently");
        assert_eq!(location.as_deref(), Some(to), "{from} points at the Agent tab");
    }

    // A repo nobody configured is still a 404, not a redirect into nothing.
    let (status, _) = redirect(&bed.router, "/ghost/traces").await;
    assert_eq!(status, 404);
}

/// The pages moved; the API did not. Agents push to and poll these paths.
#[tokio::test]
async fn the_old_json_apis_keep_their_paths() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    let (status, sessions) = get_json(&bed.router, "/demo/traces").await;
    assert_eq!(status, 200);
    let (status, session) = get_json(&bed.router, "/demo/traces/sess-1").await;
    assert_eq!(status, 200);
    let (status, prompts) = get_json(&bed.router, "/demo/prompts?q=retry").await;
    assert_eq!(status, 200);

    // And `/agent` answers the prompt search with byte-identical JSON.
    let (status, same) = get_json(&bed.router, "/demo/agent?q=retry").await;
    assert_eq!(status, 200);
    assert_eq!(same, prompts, "one search, one answer, two URLs");

    let sessions: serde_json::Value = serde_json::from_str(&sessions).expect("json");
    assert_eq!(sessions[0]["session"], "sess-1");
    let session: serde_json::Value = serde_json::from_str(&session).expect("json");
    assert_eq!(session["events"].as_array().expect("events").len(), 4);
}

/// A backfilled Claude Code transcript is a different shape from a live hook event, and
/// it has to read as the same conversation.
#[tokio::test]
async fn a_raw_transcript_session_reads_as_a_conversation() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let (status, body) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-raw",
            "agent": "claude-code",
            "events": [
                {"seq": 1, "kind": "user", "payload": {
                    "type": "user",
                    "prompt": "rename the project",
                    "message": {"role": "user", "content": "rename the project"}
                }},
                {"seq": 2, "kind": "assistant", "payload": {
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "weigh the rename options"},
                        {"type": "text", "text": "Renaming it now."},
                        {"type": "tool_use", "id": "toolu_1", "name": "Edit",
                         "input": {"file_path": "/repo/src/web.rs",
                                   "old_string": "old", "new_string": "new"}}
                    ]}
                }},
                {"seq": 3, "kind": "user", "payload": {
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "done"}
                    ]},
                    "toolUseResult": {
                        "filePath": "/repo/src/web.rs",
                        "structuredPatch": [{
                            "oldStart": 12, "oldLines": 1, "newStart": 12, "newLines": 1,
                            "lines": ["-old", "+new"]
                        }]
                    }
                }},
                {"seq": 4, "kind": "user", "payload": {
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_2",
                         "content": "String to replace not found", "is_error": true}
                    ]}
                }}
            ]
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = get(&bed.router, "/demo/agent/sess-raw").await;
    assert_eq!(status, 200);
    assert!(body.contains("rename the project"), "the person's words render");
    assert!(body.contains("Renaming it now."), "so do the agent's");
    assert!(body.contains("weigh the rename options"), "thinking is present");
    assert!(body.contains("Thinking</summary>"), "and folded away");
    assert!(body.contains("/repo/src/web.rs"), "the edit names its file");

    // The file change renders as a Pierre diff, mounted the way the branch page does.
    assert!(body.contains("nashcode-diff-mount"), "the diff has a mount");
    assert!(body.contains("nashcode-diff-data"), "and a payload for the client");
    assert!(body.contains("@@ -12,1 +12,1 @@"), "built from the transcript's own patch");

    // A failed call is open and styled as an error; a successful one stays folded.
    assert!(body.contains("String to replace not found"));
    assert!(
        body.contains(r#"<details class="mt-1 flash flash-error" open="open">"#),
        "an error result renders open and styled as an error"
    );
    assert!(
        body.contains(r#"<details class="mt-1"><summary class="color-fg-muted text-small"><i class="ph ph-arrow-elbow-down-right"#),
        "a result that worked stays folded"
    );
}

/// A backfilled transcript opens with harness state lines that carry no conversation.
/// A row for one of those repeats its type name and says nothing else, so the page
/// leaves it out. The stored events are untouched.
#[tokio::test]
async fn bookkeeping_transcript_lines_are_dropped_from_the_page() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let (status, body) = post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-noise",
            "agent": "claude-code",
            "events": [
                {"seq": 1, "kind": "file-history-snapshot", "payload": {
                    "type": "file-history-snapshot",
                    "messageId": "msg-1",
                    "snapshot": {"trackedFileBackups": {}}
                }},
                {"seq": 2, "kind": "user", "payload": {
                    "type": "user",
                    "message": {"role": "user", "content": "hey can you go through this project"}
                }},
                {"seq": 3, "kind": "queue-operation", "payload": {"type": "queue-operation"}}
            ]
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = get(&bed.router, "/demo/agent/sess-noise").await;
    assert_eq!(status, 200);
    assert!(body.contains("hey can you go through this project"), "the prompt renders");
    assert!(!body.contains("file-history-snapshot"), "the state line leaves no row");
    assert!(!body.contains("queue-operation"), "nor does the queue line");

    // Only the page filters. Agents polling the API still see every event.
    let (status, body) = get_json(&bed.router, "/demo/traces/sess-noise").await;
    assert_eq!(status, 200);
    let session: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(session["events"].as_array().expect("events").len(), 3);
}

#[tokio::test]
async fn traces_read_back_as_json_when_asked() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    let (status, body) = get_json(&bed.router, "/demo/traces").await;
    assert_eq!(status, 200);
    let sessions: serde_json::Value = serde_json::from_str(&body).expect("json list");
    assert_eq!(sessions[0]["session"], "sess-1");
    assert_eq!(sessions[0]["events"], 4);
    assert_eq!(sessions[0]["commits"], 1);

    let (status, body) = get_json(&bed.router, "/demo/traces/sess-1").await;
    assert_eq!(status, 200);
    let session: serde_json::Value = serde_json::from_str(&body).expect("json session");
    assert_eq!(session["session"], "sess-1");
    assert_eq!(session["events"].as_array().expect("events").len(), 4);
    assert_eq!(session["commits"][0], after);
}

/// Post a transcript for one session. Returns the status and body.
async fn put_transcript(router: &Router, path: &str, body: &str) -> (u16, String) {
    request(router, Method::POST, path, Some(("application/x-ndjson", body.to_owned()))).await
}

/// Session ids come from harnesses, not from us. Two that differ only in a character a
/// filename scrubber would flatten are still two sessions, and neither may eat the
/// other's transcript.
#[tokio::test]
async fn lookalike_session_ids_keep_separate_transcripts() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let dotted = "{\"session\":\"a.b\"}\n";
    let scored = "{\"session\":\"a_b\"}\n";
    let (status, body) = put_transcript(&bed.router, "/demo/traces/a.b/transcript", dotted).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = put_transcript(&bed.router, "/demo/traces/a_b/transcript", scored).await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = get(&bed.router, "/demo/traces/a.b/transcript").await;
    assert_eq!(status, 200);
    assert_eq!(body, dotted, "`a_b` must not have overwritten `a.b`");
    let (status, body) = get(&bed.router, "/demo/traces/a_b/transcript").await;
    assert_eq!(status, 200);
    assert_eq!(body, scored);
}

/// A transcript is written once. Overwriting is a decision, not a default.
#[tokio::test]
async fn a_second_transcript_needs_replace() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let first = "{\"role\":\"user\",\"text\":\"first run\"}\n";
    let second = "{\"role\":\"user\",\"text\":\"second run\"}\n";
    let (status, body) = put_transcript(&bed.router, "/demo/traces/sess-1/transcript", first).await;
    assert_eq!(status, 200, "{body}");

    let (status, body) =
        put_transcript(&bed.router, "/demo/traces/sess-1/transcript", second).await;
    assert_eq!(status, 409, "a second upload is refused: {body}");
    assert!(body.contains("replace"), "the refusal says how to override it: {body}");
    let (_, stored) = get(&bed.router, "/demo/traces/sess-1/transcript").await;
    assert_eq!(stored, first, "the refused upload changed nothing");

    let (status, body) =
        put_transcript(&bed.router, "/demo/traces/sess-1/transcript?replace=1", second).await;
    assert_eq!(status, 200, "{body}");
    let (_, stored) = get(&bed.router, "/demo/traces/sess-1/transcript").await;
    assert_eq!(stored, second, "?replace=1 overwrites");
}

/// Two writers on the same database allocating `seq` at once. The in-process mutex does
/// not cover this — each connection is its own writer, the way two nashcode processes
/// would be — so the transaction has to.
#[test]
fn concurrent_seq_allocation_loses_no_event() {
    use nashcode::db::{Db, NewTraceEvent};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trace-race.db");
    Db::open(&path).expect("schema");

    let writers = 8;
    let each = 20;
    std::thread::scope(|scope| {
        for _ in 0..writers {
            scope.spawn(|| {
                let db = Db::open(&path).expect("open");
                for _ in 0..each {
                    db.add_trace_event(&NewTraceEvent {
                        repo: "demo".to_owned(),
                        session: "race".to_owned(),
                        seq: None,
                        kind: "event".to_owned(),
                        payload: "{}".to_owned(),
                        head: None,
                        agent: None,
                    })
                    .expect("stored");
                }
            });
        }
    });

    let db = Db::open(&path).expect("open");
    let events = db.trace_events("demo", "race").expect("events");
    assert_eq!(events.len(), writers * each, "every event was kept");
    let numbers: std::collections::BTreeSet<i64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(numbers.len(), writers * each, "no two events share a seq");
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
    let (status, _) = get(&bed.router, "/demo/agent/never-happened").await;
    assert_eq!(status, 404);
    let (status, _) = get_json(&bed.router, "/demo/traces/never-happened").await;
    assert_eq!(status, 404);
}

/// The hook runs inside somebody's agent loop. Whatever happens, it must not be the
/// reason their turn failed.
mod hook {
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn run_hook(stdin: &str, url: &str) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nashcode-viewer"))
            .arg("hook")
            .env("NASHCODE_URL", url)
            .env("NASHCODE_REPO", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("nashcode runs");
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

/// End to end over a real listener: the hook infers the repo from the clone's origin
/// remote, attaches HEAD, and the server records the event.
#[tokio::test]
async fn the_hook_records_an_event_against_a_live_server() {
    use std::io::Write;
    use std::process::Stdio;

    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let router = nashcode::web::router(bed.app.clone());
    let server = tokio::spawn(async move {
        let _ = topcoat::serve_until(listener, router, async {
            let _ = stop_rx.await;
        })
        .await;
    });

    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    let payload = serde_json::json!({
        "session_id": "sess-live",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "cwd": work.dir.to_string_lossy(),
    })
    .to_string();
    let url = format!("http://{addr}");
    let status = tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_nashcode-viewer"))
            .arg("hook")
            .env("NASHCODE_URL", url)
            .env_remove("NASHCODE_REPO")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("hook spawns");
        child.stdin.as_mut().expect("stdin").write_all(payload.as_bytes()).expect("write");
        child.wait().expect("hook exits")
    })
    .await
    .expect("join");
    assert!(status.success());

    let (status, body) = get(&bed.router, "/demo/agent/sess-live").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("PostToolUse"), "the event is recorded: {body}");

    let _ = stop_tx.send(());
    let _ = server.await;
}

/// Prompts are the most re-readable part of a trace, so they get their own page.
#[tokio::test]
async fn prompts_are_listed_searchable_and_linked_to_their_session() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");
    let after = bed.remote_tip("demo", "part-1");
    post_json(&bed.router, "/demo/traces/events", session_events(&before, &after)).await;

    // A second session, so search has something to exclude.
    post_json(
        &bed.router,
        "/demo/traces/events",
        serde_json::json!({
            "session": "sess-2",
            "agent": "claude-code",
            "events": [{
                "seq": 1,
                "kind": "UserPromptSubmit",
                "head": before,
                "payload": {"prompt": "rewrite the board column ordering"}
            }]
        }),
    )
    .await;

    let (status, body) = get(&bed.router, "/demo/agent").await;
    assert_eq!(status, 200);
    assert!(body.contains("add a retry note"), "the first prompt titles its session");
    assert!(body.contains("rewrite the board column ordering"), "so does the second");
    assert!(body.contains("/demo/agent/sess-1"), "each session links to its conversation");

    // Substring search narrows the list.
    let (status, body) = get(&bed.router, "/demo/agent?q=retry").await;
    assert_eq!(status, 200);
    assert!(body.contains("add a retry note"));
    assert!(!body.contains("rewrite the board column ordering"), "search excludes the rest");
    assert!(!body.contains("sess-2"), "and the session it belongs to");

    // And the same URL is an API.
    let (status, body) = get_json(&bed.router, "/demo/prompts?q=retry").await;
    assert_eq!(status, 200);
    let prompts: serde_json::Value = serde_json::from_str(&body).expect("json");
    let prompts = prompts.as_array().expect("array");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["session"], "sess-1");
    assert_eq!(prompts[0]["text"], "add a retry note");

    // Narrowing by session works too.
    let (_, body) = get_json(&bed.router, "/demo/prompts?session=sess-2").await;
    let prompts: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(prompts.as_array().expect("array").len(), 1);
    assert_eq!(prompts[0]["text"], "rewrite the board column ordering");
}
