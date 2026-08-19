//! The contract that is the same on every command: the command tree audits
//! clean, a stray `--json` is ignored rather than eaten, and each failure class
//! comes back as its own exit code.
//!
//! An agent branches on the exit code, so these four codes are as much a public
//! interface as any output field. Nothing here needs a network: the two codes
//! that need a server get one on 127.0.0.1 for a single request.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

/// Answer requests on a loopback port with `status` and an empty body, and
/// report every request line seen.
fn recording_server(status: &'static str, body: &'static str) -> (u16, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let mut head = Vec::new();
            while let Ok(n) = stream.read(&mut buf) {
                head.extend_from_slice(&buf[..n]);
                if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let line = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            sink.lock().unwrap().push(line);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (port, Requests(seen))
}

struct Requests(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl Requests {
    /// The request lines seen so far, once the CLI has had a chance to send one.
    fn lines(&self) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let seen = self.0.lock().unwrap().clone();
            if !seen.is_empty() || std::time::Instant::now() > deadline {
                return seen;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// Answer one request on a loopback port with `status`, then stop.
fn one_shot_server(status: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // dgit is asked twice by some paths (probe, then act), so keep
        // answering until the client goes away.
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let mut head = Vec::new();
            while let Ok(n) = stream.read(&mut buf) {
                head.extend_from_slice(&buf[..n]);
                if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = stream.write_all(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
    port
}

fn write_config(dir: &Path, url: &str) -> std::path::PathBuf {
    write_config_with_viewer(dir, url, None)
}

fn write_config_with_viewer(dir: &Path, url: &str, viewer: Option<u16>) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let viewer_line = viewer
        .map(|p| format!("viewer_url = \"http://127.0.0.1:{p}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        &path,
        format!(
            "active = \"test\"\n\n[profiles.test]\nurl = \"{url}\"\ntoken = \"tt\"\n{viewer_line}"
        ),
    )
    .unwrap();
    path
}

fn nashcode(config: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
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
fn the_command_tree_audits_clean() {
    let report = nashcode_cli::cli::build().audit();
    assert!(
        report.is_clean(),
        "the surface has dead links or dead ends: {:#?}",
        report.findings
    );
}

#[test]
fn a_stray_json_flag_is_ignored_and_never_eats_the_positional() {
    // Agents have `--json` memorised from the clap surface. It has to keep
    // working — including in the place a value flag would have swallowed the
    // repository name that follows it. The proof is the request that goes out:
    // `?repo=somerepo` means the positional survived being next to the flag.
    let empty = r#"{"generated_at":"2026-08-19T12:00:00Z","repos":[]}"#;

    for args in [
        &["brain", "--json", "somerepo"][..],
        &["--json", "brain", "somerepo"][..],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (port, seen) = recording_server("200 OK", empty);
        let config = write_config_with_viewer(dir.path(), "http://127.0.0.1:1", Some(port));

        let out = nashcode(&config, dir.path(), args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert_eq!(envelope(&out)["ok"], true, "{args:?}");

        let lines = seen.lines();
        assert_eq!(
            lines.first().map(String::as_str),
            Some("GET /brain?repo=somerepo HTTP/1.1"),
            "{args:?} sent {lines:?}"
        );
    }
}

#[test]
fn a_transport_failure_on_a_mutating_command_is_an_upstream_failure() {
    // A dead box. Nothing answers, so there is no status code to read — and
    // this used to fall through every pattern and exit 1, which tells an agent
    // to give up rather than to check the deployment.
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &format!("http://127.0.0.1:{}", dead_port()));

    for args in [
        &["rm", "widget", "--yes"][..],
        &["new", "widget"][..],
        &["gc", "widget"][..],
        &["desc", "widget", "--desc", "x"][..],
        &["ls"][..],
    ] {
        let out = nashcode(&config, dir.path(), args);
        assert_eq!(
            out.status.code(),
            Some(5),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let v = envelope(&out);
        assert_eq!(v["error"]["code"], "API", "{args:?}");
        assert_eq!(v["fix"], "nashcode doctor", "{args:?}");
    }
}

/// A port with nothing behind it: bound to learn the number, then released.
fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[test]
fn an_auth_failure_is_not_told_to_run_a_command_that_would_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let port = one_shot_server("401 Unauthorized");
    let config = write_config(dir.path(), &format!("http://127.0.0.1:{port}"));

    // `nashcode ls` reads the index anonymously, so it succeeds while the push
    // token is dead. Sending an agent there after a 401 teaches it the opposite
    // of what happened.
    let v = envelope(&nashcode(&config, dir.path(), &["gc", "widget"]));
    let fix = v["fix"].as_str().unwrap();
    assert!(fix.contains("token"), "{fix}");
    assert_ne!(fix, "nashcode ls");
}

#[test]
fn select_and_compact_reach_the_result_through_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "http://127.0.0.1:1");
    std::fs::write(
        &config,
        "active = \"test\"\n\n[profiles.test]\nurl = \"https://a.example\"\ntoken = \"tt\"\n",
    )
    .unwrap();

    // --select projects to the named fields and drops the rest.
    let v = envelope(&nashcode(&config, dir.path(), &["profiles", "--select=active"]));
    assert_eq!(v["result"]["active"], "test");
    assert!(v["result"].get("profiles").is_none(), "{v}");

    // --compact drops the null fields a row is padded with.
    let v = envelope(&nashcode(&config, dir.path(), &["profiles", "--compact"]));
    let row = &v["result"]["profiles"][0];
    assert_eq!(row["name"], "test");
    assert!(row.get("viewer_url").is_none(), "null survived --compact: {row}");
    assert!(row.get("bucket").is_none(), "null survived --compact: {row}");

    // --quiet empties the trail on a successful envelope.
    let v = envelope(&nashcode(&config, dir.path(), &["profiles", "--quiet"]));
    assert_eq!(v["next_actions"].as_array().unwrap().len(), 0, "{v}");
}

#[test]
fn quiet_does_not_reach_an_error_envelope() {
    // Pinned because AGENTS.md has to describe it truthfully, not because it is
    // right: `--quiet` is documented as "omit next_actions", and on the failure
    // path agcli 0.15.0 keeps them. Recorded in cli/NOTES.md as an agcli gap;
    // when it is fixed this test is what says so.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.toml");

    let v = envelope(&nashcode(&missing, dir.path(), &["ls", "--quiet"]));
    assert_eq!(v["ok"], false);
    assert!(
        !v["next_actions"].as_array().unwrap().is_empty(),
        "agcli started honouring --quiet on errors — update AGENTS.md and NOTES.md: {v}"
    );
}

#[test]
fn a_bad_invocation_exits_two() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "http://127.0.0.1:1");

    // No confirmation prompt exists any more, so a delete without --yes is a
    // usage mistake, and the fix is the line that would have worked.
    let out = nashcode(&config, dir.path(), &["rm", "widget"]);
    assert_eq!(out.status.code(), Some(2));
    let v = envelope(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["error"]["code"], "USAGE");
    assert_eq!(v["fix"], "nashcode rm widget --yes");
}

#[test]
fn a_missing_profile_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.toml");

    let out = nashcode(&missing, dir.path(), &["ls"]);
    assert_eq!(out.status.code(), Some(3));
    let v = envelope(&out);
    assert_eq!(v["error"]["code"], "NOT_FOUND");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("no active profile"),
        "{v}"
    );
    assert!(v["fix"].as_str().unwrap().starts_with("nashcode "), "{v}");
}

#[test]
fn a_rejected_token_exits_four() {
    let dir = tempfile::tempdir().unwrap();
    let port = one_shot_server("401 Unauthorized");
    let config = write_config(dir.path(), &format!("http://127.0.0.1:{port}"));

    let out = nashcode(&config, dir.path(), &["gc", "widget"]);
    assert_eq!(out.status.code(), Some(4), "{}", String::from_utf8_lossy(&out.stdout));
    let v = envelope(&out);
    assert_eq!(v["error"]["code"], "AUTH");
    assert!(v["error"]["message"].as_str().unwrap().contains("401"), "{v}");
}

#[test]
fn an_upstream_failure_exits_five() {
    let dir = tempfile::tempdir().unwrap();
    let port = one_shot_server("503 Service Unavailable");
    let config = write_config(dir.path(), &format!("http://127.0.0.1:{port}"));

    let out = nashcode(&config, dir.path(), &["gc", "widget"]);
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = envelope(&out);
    assert_eq!(v["error"]["code"], "API");
    assert!(v["error"]["message"].as_str().unwrap().contains("503"), "{v}");
}
