//! `nashcode comments` end to end: the real binary, a real HTTP round trip,
//! and a canned viewer answer — served from a listener on 127.0.0.1 that this
//! test owns, so nothing here needs a network or a host.

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

#[test]
fn json_mode_passes_the_viewers_answer_through_untouched() {
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

    let got: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let want: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(got, want, "--json must be a byte-faithful passthrough");

    let request_line = server.join().unwrap();
    assert_eq!(
        request_line,
        "GET /demo/comments?file=plans%2Frewrite-the-parser.md&branch=main&since=2026-08-01T00%3A00%3A00Z HTTP/1.1"
    );
}

#[test]
fn human_mode_renders_author_line_and_indented_body() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server(FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(
        &config,
        dir.path(),
        &["comments", "plans/rewrite-the-parser.md", "--repo", "demo"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("#1 rob line 12"), "{text}");
    assert!(text.contains("    This step assumes"), "{text}");
    assert!(text.contains("    Ship step 1 first"), "{text}");
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
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no viewer URL"), "{err}");
    assert!(err.contains("nashcode setup --viewer"), "{err}");

    // And in --json mode, stdout still carries exactly one JSON value.
    let out = nashcode(
        &config,
        dir.path(),
        &["--json", "comments", "plans/x.md", "--repo", "demo"],
    );
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["error"].as_str().unwrap().contains("no viewer URL"));
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
