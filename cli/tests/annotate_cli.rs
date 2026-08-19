//! `nashcode annotate` end to end: the real binary, a fake plannotator on PATH,
//! and a loopback listener standing in for the viewer. No UI, no network.
//!
//! The fake plannotator is the point of most of this. It records the argv it was
//! handed, refuses to overwrite a result file that already exists (plannotator's
//! own rule), and prints the decision on stdout the way the real one does — so a
//! test can prove that copy never reaches the user's terminal.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Serve one request on a loopback port, then report what arrived.
fn one_shot_server(status: &'static str, body: &'static str) -> (u16, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut got = Vec::new();
        // Read the head, then whatever content-length promised.
        loop {
            let n = stream.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got);
            if n == 0 {
                break;
            }
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let want: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if rest.len() >= want {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&got).into_owned();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        text
    });
    (port, handle)
}

/// A git repository on branch `review/x`, with `origin` naming it `widget` and
/// one plan in it.
fn repo(root: &Path) -> PathBuf {
    let repo = root.join("widget");
    std::fs::create_dir_all(repo.join("plans")).unwrap();
    std::fs::write(repo.join("plans").join("x.md"), "# x\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "review/x"],
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
    repo
}

/// A fake plannotator that publishes `decision` and exits `code`.
fn plannotator(root: &Path, decision: &str, code: i32) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" > "{argv}"
rf=""
while [ $# -gt 0 ]; do
  case "$1" in --result-file) rf="$2"; shift 2 ;; *) shift ;; esac
done
[ -n "$rf" ] || exit 2
[ -e "$rf" ] && exit 2
printf '%s\n' '{decision}' > "$rf"
printf '%s\n' '{decision}'
exit {code}
"#,
        argv = root.join("argv.txt").display(),
    );
    let path = bin.join("plannotator");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn config(dir: &Path, viewer: Option<u16>) -> PathBuf {
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

fn nashcode(config: &Path, bin: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(args)
        .env("NASHCODE_CONFIG", config)
        .env("PATH", path)
        .current_dir(cwd)
        .output()
        .unwrap()
}

#[test]
fn plannotator_is_launched_gated_with_a_result_file_that_does_not_exist_yet() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo(dir.path());
    let bin = plannotator(dir.path(), r#"{"decision":"dismissed"}"#, 0);
    let cfg = config(dir.path(), None);

    let out = nashcode(&cfg, &bin, &work, &["annotate", "plans/x.md"]);
    // The shim exits 2 if the result file already exists, so success is the
    // no-overwrite contract holding.
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    assert!(argv.contains("annotate plans/x.md"), "{argv}");
    assert!(argv.contains("--gate"), "{argv}");
    assert!(argv.contains("--json"), "{argv}");
    assert!(argv.contains("--result-file "), "{argv}");
    assert!(argv.contains("decision.json"), "{argv}");

    // Dismissed says so and posts nothing, and plannotator's own copy of the
    // decision record never reaches our stdout.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("dismissed, nothing posted"), "{text}");
    assert!(!text.contains("\"decision\""), "{text}");
}

#[test]
fn json_mode_launches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo(dir.path());
    let bin = plannotator(dir.path(), r#"{"decision":"approved"}"#, 0);
    let cfg = config(dir.path(), None);

    let out = nashcode(&cfg, &bin, &work, &["--json", "annotate", "plans/x.md"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !dir.path().join("argv.txt").exists(),
        "--json must not open an editor"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["file"], "plans/x.md");
    assert!(v["plannotator"].as_str().unwrap().ends_with("plannotator"));
}

#[test]
fn an_annotated_decision_becomes_one_whole_file_comment() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo(dir.path());
    let bin = plannotator(
        dir.path(),
        r#"{"decision":"annotated","feedback":"step 2 is wrong"}"#,
        0,
    );
    let (port, server) = one_shot_server("201 Created", r#"{"id":7}"#);
    let cfg = config(dir.path(), Some(port));

    let out = nashcode(&cfg, &bin, &work, &["annotate", "plans/x.md"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("posted #7"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /widget/comments HTTP/1.1"),
        "{request}"
    );
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let sent: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({
            "branch": "review/x",
            "file": "plans/x.md",
            "body": "step 2 is wrong",
        })
    );
}

#[test]
fn a_refused_post_prints_the_feedback_and_then_fails() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo(dir.path());
    let bin = plannotator(
        dir.path(),
        r#"{"decision":"annotated","feedback":"step 2 is wrong"}"#,
        0,
    );
    let (port, server) = one_shot_server("400 Bad Request", "unknown branch review/x");
    let cfg = config(dir.path(), Some(port));

    let out = nashcode(&cfg, &bin, &work, &["annotate", "plans/x.md"]);
    assert!(!out.status.success(), "a refused post must not report success");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("step 2 is wrong"), "feedback was lost: {text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("400"), "{err}");
    server.join().unwrap();
}

#[test]
fn with_nowhere_to_post_the_feedback_is_printed_and_the_command_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo(dir.path());
    let bin = plannotator(
        dir.path(),
        r#"{"decision":"approved","feedback":"good plan"}"#,
        0,
    );
    // A profile with no viewer URL: local-only review, nothing to post to.
    let cfg = config(dir.path(), None);

    let out = nashcode(&cfg, &bin, &work, &["annotate", "plans/x.md"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Approved.\n\ngood plan"), "{text}");
    assert!(text.contains("not posted:"), "{text}");
    assert!(text.contains("no viewer URL"), "{text}");
}

#[test]
fn a_decision_published_before_a_nonzero_exit_is_still_posted() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo(dir.path());
    // plannotator published, then tripped on the way out.
    let bin = plannotator(
        dir.path(),
        r#"{"decision":"annotated","feedback":"do not lose this"}"#,
        2,
    );
    let (port, server) = one_shot_server("201 Created", r#"{"id":9}"#);
    let cfg = config(dir.path(), Some(port));

    let out = nashcode(&cfg, &bin, &work, &["annotate", "plans/x.md"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("exited 2"));
    let request = server.join().unwrap();
    assert!(request.contains("do not lose this"), "{request}");
}
