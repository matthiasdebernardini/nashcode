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
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!("active = \"test\"\n\n[profiles.test]\nurl = \"{url}\"\ntoken = \"tt\"\n"),
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
    let dir = tempfile::tempdir().unwrap();
    // No viewer, so `brain` answers from configuration alone and asks nothing.
    let config = write_config(dir.path(), "http://127.0.0.1:1");

    // Agents have `--json` memorised from the clap surface. It has to keep
    // working — including in the place a value flag would have swallowed the
    // repository name that follows it.
    let out = nashcode(&config, dir.path(), &["brain", "--json", "somerepo"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["ok"], true);
    // The positional survived: the command that ran named the repository.
    assert!(
        v["command"].as_str().unwrap().contains("somerepo"),
        "{v}"
    );
    assert!(v["result"]["error"].as_str().unwrap().contains("no viewer URL"));

    // And before the command, which is where it used to be typed.
    let out = nashcode(&config, dir.path(), &["--json", "brain", "somerepo"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(envelope(&out)["ok"], true);
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
