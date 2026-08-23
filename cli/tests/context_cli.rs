//! `nashcode context` end to end: the real binary, a real HTTP round trip, and a
//! canned viewer answer served from a listener on 127.0.0.1 that this test owns.
//! Nothing here needs a network or a host.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// What the CLI actually sent: the request line and the body.
struct Sent {
    line: String,
    body: String,
}

/// Serve one canned answer to exactly one request on a loopback port, and hand the
/// request back to the test.
fn one_shot(status: &'static str, body: &'static str) -> (u16, std::thread::JoinHandle<Sent>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut raw = Vec::new();
        let mut head_end = None;
        let mut length = 0usize;
        loop {
            let n = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
            if head_end.is_none()
                && let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n")
            {
                head_end = Some(at + 4);
                let head = String::from_utf8_lossy(&raw[..at]).to_lowercase();
                length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
            }
            match head_end {
                Some(at) if raw.len() >= at + length => break,
                _ if n == 0 => break,
                _ => {}
            }
        }
        let at = head_end.unwrap_or(raw.len());
        let sent = Sent {
            line: String::from_utf8_lossy(&raw).lines().next().unwrap_or_default().to_string(),
            body: String::from_utf8_lossy(&raw[at..]).to_string(),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        sent
    });
    (port, handle)
}

fn write_config(dir: &std::path::Path, viewer: u16) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "active = \"test\"\n\n[profiles.test]\nurl = \"http://127.0.0.1:1\"\n\
             token = \"tt\"\nviewer_url = \"http://127.0.0.1:{viewer}\"\n"
        ),
    )
    .unwrap();
    path
}

fn nashcode(
    config: &std::path::Path,
    cwd: &std::path::Path,
    args: &[&str],
    stdin: Option<&str>,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(args)
        .env("NASHCODE_CONFIG", config)
        .current_dir(cwd)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(text) = stdin {
        child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn envelope(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value ({e})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

const FILED: &str = r#"{"ok":true,"id":"2026-06-13-0905-re-invoice-18f2a0b1",
    "path":"context/email/2026/06/2026-06-13-0905-re-invoice-18f2a0b1.md",
    "commit":"0123456789012345678901234567890123456789"}"#;

#[test]
fn a_put_sends_the_flags_as_the_body_and_the_text_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot("201 Created", FILED);
    let config = write_config(dir.path(), port);

    let out = nashcode(
        &config,
        dir.path(),
        &[
            "context", "put", "email", "--repo", "demo", "--title", "Re: invoice", "--at",
            "2026-06-13T09:05:00Z", "--source", "18f2a",
        ],
        Some("The invoice is paid.\n"),
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let sent = server.join().unwrap();
    assert_eq!(sent.line, "POST /demo/context/email HTTP/1.1", "{}", sent.line);
    let body: serde_json::Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(body["title"], "Re: invoice");
    assert_eq!(body["at"], "2026-06-13T09:05:00Z");
    assert_eq!(body["source"], "18f2a");
    assert_eq!(body["text"], "The invoice is paid.\n");

    let result = &envelope(&out)["result"];
    assert_eq!(result["id"], "2026-06-13-0905-re-invoice-18f2a0b1");
    assert_eq!(result["kind"], "email");
    assert_eq!(result["repo"], "demo");
    // Always present, so a pusher branches on a field rather than on its absence.
    assert_eq!(result["existing"], false);
}

#[test]
fn a_repeat_the_viewer_already_has_comes_back_as_existing_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot(
        "200 OK",
        r#"{"ok":true,"existing":true,"id":"2026-06-13-0905-re-invoice-18f2a0b1",
            "path":"context/email/2026/06/2026-06-13-0905-re-invoice-18f2a0b1.md"}"#,
    );
    let config = write_config(dir.path(), port);

    let out = nashcode(
        &config,
        dir.path(),
        &["context", "put", "email", "--repo", "demo", "--title", "Re: invoice", "--source", "18f2a"],
        Some("The invoice is paid.\n"),
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    server.join().unwrap();
    assert_eq!(envelope(&out)["result"]["existing"], true);
}

#[test]
fn a_kind_that_is_not_one_of_the_four_never_reaches_the_viewer() {
    let dir = tempfile::tempdir().unwrap();
    // A port nothing listens on: reaching it at all would be the failure.
    let config = write_config(dir.path(), 1);

    let out = nashcode(
        &config,
        dir.path(),
        &["context", "put", "voicemail", "--repo", "demo", "--title", "x"],
        Some("words"),
    );
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let error = &envelope(&out)["error"];
    assert_eq!(error["code"], "USAGE");
    assert!(
        error["message"].as_str().unwrap().contains("meeting, email, chat, note"),
        "{error}"
    );
}

#[test]
fn an_email_without_a_title_is_refused_before_anything_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), 1);
    let out = nashcode(
        &config,
        dir.path(),
        &["context", "put", "email", "--repo", "demo"],
        Some("words"),
    );
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn ls_carries_the_kind_and_the_cursor_and_hands_the_next_one_back() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot(
        "200 OK",
        r#"{"items":[{"kind":"email","id":"a","ingested_at":"2026-06-13T09:06:00.000000Z"}],
            "next_since":"2026-06-13T09:06:00.000000Z|email/a"}"#,
    );
    let config = write_config(dir.path(), port);

    let out = nashcode(
        &config,
        dir.path(),
        &[
            "context", "ls", "--repo", "demo", "--kind", "email", "--since",
            "2026-06-13T09:05:00.000000Z|email/z",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let sent = server.join().unwrap();
    assert_eq!(
        sent.line,
        "GET /demo/context?kind=email&since=2026-06-13T09%3A05%3A00.000000Z%7Cemail%2Fz HTTP/1.1",
        "{}",
        sent.line
    );

    let value = envelope(&out);
    assert_eq!(value["result"]["next_since"], "2026-06-13T09:06:00.000000Z|email/a");
    // The next action is the poll, with the cursor already in it.
    let actions = value["next_actions"].as_array().cloned().unwrap_or_default();
    assert!(
        actions.iter().any(|a| a["command"]
            .as_str()
            .is_some_and(|c| c.contains("--since=2026-06-13T09:06:00.000000Z|email/a"))),
        "{value}"
    );
}

#[test]
fn an_id_the_viewer_does_not_have_exits_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot("404 Not Found", r#"{"error":"no such item"}"#);
    let config = write_config(dir.path(), port);

    let out = nashcode(
        &config,
        dir.path(),
        &["context", "get", "email", "nobody-filed-this", "--repo", "demo"],
        None,
    );
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    let sent = server.join().unwrap();
    assert_eq!(sent.line, "GET /demo/context/email/nobody-filed-this HTTP/1.1");
    assert_eq!(envelope(&out)["error"]["code"], "NOT_FOUND");
}

#[test]
fn a_meeting_travels_as_the_extensions_own_json_and_ignores_the_flags() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot("201 Created", FILED);
    let config = write_config(dir.path(), port);

    let transcript = dir.path().join("meeting.json");
    std::fs::write(
        &transcript,
        r#"{"title":"Weekly sync","started_at":"2026-06-12T15:00:00Z",
            "ended_at":"2026-06-12T15:30:00Z","speakers":[{"id":"S1","name":"Rob"}],
            "segments":[{"speaker":"S1","start_ms":0,"end_ms":900,"text":"Morning."}]}"#,
    )
    .unwrap();

    let out = nashcode(
        &config,
        dir.path(),
        &[
            "context",
            "put",
            "meeting",
            transcript.to_str().unwrap(),
            "--repo",
            "demo",
            "--title",
            "ignored",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let sent = server.join().unwrap();
    assert_eq!(sent.line, "POST /demo/context/meeting HTTP/1.1");
    let body: serde_json::Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(body["title"], "Weekly sync", "the flag did not overwrite the transcript");
    assert_eq!(body["started_at"], "2026-06-12T15:00:00Z");
    assert!(body.get("text").is_none(), "{body}");
}
