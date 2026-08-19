//! `nashcode comments` end to end: the real binary, a real HTTP round trip,
//! and a canned viewer answer — served from a listener on 127.0.0.1 that this
//! test owns, so nothing here needs a network or a host.
//!
//! The result is a bounded list, so an agent gets `{items, count, total,
//! truncated, fields}` and a `--select` it can paste back. The rows inside it
//! are the viewer's own, untouched.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

const FIXTURE: &str = include_str!("fixtures/comments.json");

/// Serve `body` to exactly one request on a loopback port; report the request
/// line ("GET /path HTTP/1.1") back to the test.
fn one_shot_server(body: &'static str) -> (u16, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut head = Vec::new();
        loop {
            let n = stream.read(&mut buf).unwrap();
            head.extend_from_slice(&buf[..n]);
            if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let request_line = String::from_utf8_lossy(&head)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        request_line
    });
    (port, handle)
}

/// A profile store whose viewer is the local listener.
fn write_config(dir: &std::path::Path, viewer: Option<u16>) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let viewer_line = viewer
        .map(|p| format!("viewer_url = \"http://127.0.0.1:{p}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        &path,
        format!(
            "active = \"test\"\n\n[profiles.test]\nurl = \"http://127.0.0.1:1\"\ntoken = \"tt\"\n{viewer_line}"
        ),
    )
    .unwrap();
    path
}

fn nashcode(config: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(args)
        .env("NASHCODE_CONFIG", config)
        .current_dir(cwd)
        .output()
        .unwrap()
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

#[test]
fn the_rows_are_a_bounded_list_of_the_viewers_own_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server(FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(
        &config,
        dir.path(),
        &[
            "--json",
            "comments",
            "plans/rewrite-the-parser.md",
            "--repo",
            "demo",
            "--branch",
            "main",
            "--since",
            "2026-08-01T00:00:00Z",
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let result = &envelope(&out)["result"];
    let want: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    // The wrapper is the CLI's business; the rows inside it are the viewer's,
    // passed through key for key.
    assert_eq!(result["items"], want["comments"]);
    assert_eq!(result["count"], 2);
    assert_eq!(result["total"], 2);
    assert_eq!(result["truncated"], false);
    // The row schema comes back as --select paths an agent can paste unedited.
    let fields: Vec<&str> = result["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.as_str())
        .collect();
    assert!(fields.contains(&"items.id") && fields.contains(&"items.body"), "{fields:?}");

    let request_line = server.join().unwrap();
    assert_eq!(
        request_line,
        "GET /demo/comments?file=plans%2Frewrite-the-parser.md&branch=main&since=2026-08-01T00%3A00%3A00Z HTTP/1.1"
    );
}

#[test]
fn the_next_action_polls_from_the_newest_comment_returned() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server(FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(
        &config,
        dir.path(),
        &["comments", "plans/rewrite-the-parser.md", "--repo", "demo"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // The whole point of the polling loop: the next call asks only for what it
    // has not already seen.
    let v = envelope(&out);
    let actions: Vec<&str> = v["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["command"].as_str())
        .collect();
    assert!(
        actions.contains(
            &"nashcode comments plans/rewrite-the-parser.md --repo=demo --since=2026-08-17T14:05:30Z"
        ),
        "{actions:?}"
    );
    server.join().unwrap();
}

#[test]
fn a_profile_without_a_viewer_url_fails_with_a_clear_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), None);

    let out = nashcode(
        &config,
        dir.path(),
        &["comments", "plans/x.md", "--repo", "demo"],
    );
    assert_eq!(out.status.code(), Some(3), "the viewer is a resource, missing");
    let v = envelope(&out);
    let why = v["error"]["message"].as_str().unwrap();
    assert!(why.contains("no viewer URL"), "{why}");
    assert!(why.contains("nashcode setup --viewer"), "{why}");
    assert!(v["fix"].as_str().unwrap().contains("--viewer"), "{v}");
}

#[test]
fn without_dash_dash_repo_the_origin_remote_names_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("widget");
    std::fs::create_dir_all(&repo).unwrap();
    // A real git repo whose origin points at the profile's server.
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["remote", "add", "origin", "https://example-host/widget.git"],
    ] {
        let ok = Command::new("git")
            .args(&args)
            .current_dir(&repo)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    let (port, server) = one_shot_server(FIXTURE);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["--json", "comments", "plans/x.md"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let request_line = server.join().unwrap();
    assert!(
        request_line.starts_with("GET /widget/comments?file=plans%2Fx.md"),
        "{request_line}"
    );
}

#[test]
fn a_bounded_list_says_so_when_it_is_showing_less_than_it_has() {
    // The viewer answers with more rows than it says it holds — the shape a
    // paged endpoint takes. `truncated` is what stops an agent concluding it
    // has seen everything.
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server(FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(
        &config,
        dir.path(),
        &["comments", "plans/rewrite-the-parser.md", "--repo", "demo"],
    );
    let result = &envelope(&out)["result"];
    // This fixture is complete, so it is not truncated — and says which.
    assert_eq!(result["truncated"], false);
    assert_eq!(result["count"], result["total"]);
    assert!(result.get("guidance").is_none(), "{result}");
    server.join().unwrap();
}

#[test]
fn select_projects_the_rows_by_the_paths_the_list_advertised() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server(FIXTURE);
    let config = write_config(dir.path(), Some(port));

    // The `fields` a bounded list publishes are meant to be pasted back
    // unedited. This is that round trip, through the real binary.
    let out = nashcode(
        &config,
        dir.path(),
        &[
            "comments",
            "plans/rewrite-the-parser.md",
            "--repo",
            "demo",
            "--select=items.id,items.body",
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result = &envelope(&out)["result"];
    let first = &result["items"][0];
    assert_eq!(first["id"], 1);
    assert!(first["body"].as_str().unwrap().starts_with("This step"));
    // Everything not asked for is gone, metadata included.
    assert!(first.get("author").is_none(), "{first}");
    assert!(result.get("count").is_none(), "{result}");
    server.join().unwrap();
}
