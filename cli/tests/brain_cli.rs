//! `nashcode brain` end to end: the real binary against a real `/brain` answer
//! served from a loopback listener this test owns. No network, no host, no viewer.
//!
//! Two contracts are load-bearing here and both are asserted. The result is the
//! *digest*, not the stanza the viewer sent. And every failure path exits 0 with
//! an `ok: true` envelope: this command runs from a session-start hook, so a
//! dead viewer must cost one `status` field, not the session.
//!
//! `fixtures/brain.json` is not invented. It was captured from the viewer's own
//! `GET /brain?repo=demo` over a `testbed_with` fixture carrying stacked branches,
//! two plans, seven comments, three architecture submissions, a real index run and
//! two real CI runs. Only the clock-dependent values were pinned afterwards, so
//! the humanised ages are deterministic; every key, nesting and type is the
//! viewer's. Regenerate it the same way if the stanza changes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("fixtures/brain.json");

/// How long a test waits for the CLI to speak before it calls the run a failure.
/// Without this a bug that stops the CLI connecting hangs the suite instead of
/// failing it.
const PATIENCE: Duration = Duration::from_secs(20);

/// Answer one request on a loopback port; hand the whole request head back.
fn one_shot_server(status: &'static str, body: &'static str) -> (u16, Server) {
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
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&head).to_string()
    });
    (port, Server(Some(handle)))
}

/// A one-shot server whose `head()` gives up instead of blocking forever.
///
/// `JoinHandle::join` has no timeout, so a CLI that never connects used to hang
/// the whole suite. Polling `is_finished` turns that into a failed test.
struct Server(Option<std::thread::JoinHandle<String>>);

impl Server {
    fn head(mut self) -> String {
        let handle = self.0.take().expect("head() called once");
        let deadline = Instant::now() + PATIENCE;
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "the CLI never sent a request");
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.join().expect("the server thread panicked")
    }

    /// Wait for the request without reading it.
    fn served(self) {
        let _ = self.head();
    }
}

/// A listener that accepts and then says nothing, ever. Proves the client gives
/// up on its own timeout rather than hanging a session-start hook forever.
fn black_hole() -> (u16, std::sync::mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            // Hold the connection open and answer nothing.
            held.push(stream);
            let _ = tx.send(());
        }
    });
    (port, rx)
}

/// A port with nothing behind it: bound to learn the number, then released.
fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

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

fn first_line(head: &str) -> String {
    head.lines().next().unwrap_or_default().to_string()
}

/// The one JSON value the run wrote to stdout.
fn envelope(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value ({e})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// True when no ancestor of `dir` is a git or jj working copy.
///
/// `brain` walks up from the cwd looking for one, and on a machine whose TMPDIR
/// happens to sit inside a checkout the no-repository tests would silently test
/// the opposite thing.
fn has_no_repo_above(dir: &std::path::Path) -> bool {
    dir.ancestors().all(|p| !p.join(".git").exists() && !p.join(".jj").exists())
}

#[test]
fn the_result_is_the_digest_and_the_viewer_is_asked_for_json() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, dir.path(), &["--json", "brain", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let v = envelope(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["exit_code"], 0);
    let result = &v["result"];
    assert_eq!(result["status"], "ok");
    assert_eq!(result["generated_at"], "2026-08-19T12:00:00Z");

    let repo = &result["repos"][0];
    assert_eq!(repo["name"], "demo");
    assert_eq!(repo["default_branch"], "main");
    // The digest, not the stanza: the aggregate's cards and plan summaries are gone.
    assert!(repo.get("cards").is_none(), "the digest passed the stanza through");
    assert!(repo.get("activity").is_none());
    assert!(repo["plans"][0].get("summary").is_none());

    // The default branch leads, whatever order the stanza listed them in.
    assert_eq!(repo["branches"][0]["branch"], "main");
    assert_eq!(repo["branches"][0]["tip"], "f26f8ed");
    assert_eq!(repo["branches"][0]["ci"], "passed");
    assert_eq!(repo["branches"][0]["ahead"], 0);
    let part1 = repo["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["branch"] == "part-1")
        .expect("part-1");
    assert_eq!(part1["ahead"], 1);
    assert_eq!(part1["ci"], "failed");

    assert_eq!(repo["code"]["indexed"], true);
    assert_eq!(repo["code"]["files"], 5);
    assert_eq!(repo["code"]["chunks"], 5);
    assert_eq!(repo["code"]["age"], "3 days ago");
    assert_eq!(repo["code"]["last_indexed_at"], "2026-08-16T12:00:00Z");

    assert_eq!(repo["plans"][0]["path"], "plans/api.md");
    assert_eq!(repo["plans"][0]["open_comments"], 7);
    assert_eq!(repo["plans"][1]["path"], "plans/board.md");
    assert_eq!(repo["plans"][1]["open_comments"], 0);

    assert_eq!(repo["architecture"]["latest_author"], "local");
    assert_eq!(repo["architecture"]["submissions"], 3);
    assert_eq!(repo["architecture"]["latest_at"], "2026-08-19T10:00:00Z");

    let recent = repo["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 5, "only the last five of nine events survive");
    assert_eq!(recent[4]["type"], "ci_run");
    assert_eq!(recent[4]["branch"], "part-1");
    assert_eq!(recent[4]["status"], "failed");
    // A comment row has no status, so the key is absent rather than empty.
    assert_eq!(recent[0]["type"], "comment");
    assert!(recent[0].get("status").is_none(), "{}", recent[0]);

    let head = server.head();
    assert_eq!(first_line(&head), "GET /brain?repo=demo HTTP/1.1");
    assert!(
        head.to_lowercase().contains("accept: application/json"),
        "the viewer content-negotiates /brain: {head}"
    );
}

#[test]
fn the_digest_is_the_only_thing_on_stdout_and_the_stanza_never_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, dir.path(), &["brain", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Exactly one JSON value, and none of the aggregate's bulk inside it.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("\"cards\""), "{text}");
    assert!(!text.contains("embedded_chunks"), "{text}");
    assert!(!text.contains("\"activity\""), "{text}");

    let repo = &envelope(&out)["result"]["repos"][0];
    assert_eq!(repo["name"], "demo");
    assert_eq!(repo["code"]["files"], 5);
    assert_eq!(repo["recent"].as_array().unwrap().len(), 5);

    server.served();
}

#[test]
fn without_an_argument_the_origin_remote_names_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("widget");
    std::fs::create_dir_all(&repo).unwrap();
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

    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["--json", "brain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(first_line(&server.head()), "GET /brain?repo=widget HTTP/1.1");
}

#[test]
fn outside_a_repository_it_digests_the_whole_brain() {
    let dir = tempfile::tempdir().unwrap();
    assert!(has_no_repo_above(dir.path()), "TMPDIR sits inside a checkout");
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, dir.path(), &["--json", "brain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // No filter on the wire, and the same digest shape as a single repository.
    assert_eq!(first_line(&server.head()), "GET /brain HTTP/1.1");
    let v = envelope(&out);
    assert_eq!(v["result"]["repos"].as_array().unwrap().len(), 1);
}

#[test]
fn a_dead_viewer_is_a_status_and_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), Some(dead_port()));

    for args in [&["brain", "demo"][..], &["--json", "brain", "demo"][..]] {
        let out = nashcode(&config, dir.path(), args);
        assert_eq!(out.status.code(), Some(0), "a session-start hook must survive this");
        let v = envelope(&out);
        // The command succeeded; the viewer is what is down.
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["result"]["status"], "unavailable");
        let why = v["result"]["error"].as_str().unwrap();
        assert!(why.contains("viewer unreachable"), "{v}");

        // Bounded, because this lands at the top of every session: one line's
        // worth of reason, not a stack of them.
        assert_eq!(why.lines().count(), 1, "the reason grew lines: {why:?}");
        assert!(why.len() < 400, "the reason grew to {} bytes: {why:?}", why.len());
        assert!(
            out.stdout.len() < 2048,
            "a dead viewer cost {} bytes of context",
            out.stdout.len()
        );
    }
}

#[test]
fn a_viewer_that_answers_with_an_error_status_is_reported_not_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server("503 Service Unavailable", "nope");
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, dir.path(), &["--json", "brain", "demo"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["status"], "unavailable");
    assert!(v["result"]["error"].as_str().unwrap().contains("503"), "{v}");
    server.served();
}

#[test]
fn an_unconfigured_profile_still_exits_zero() {
    let dir = tempfile::tempdir().unwrap();

    assert!(has_no_repo_above(dir.path()), "TMPDIR sits inside a checkout");

    // A profile with no viewer URL.
    let config = write_config(dir.path(), None);
    let out = nashcode(&config, dir.path(), &["brain"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["ok"], true);
    let why = v["result"]["error"].as_str().unwrap();
    assert!(why.contains("no viewer URL"), "{why}");
    assert!(why.contains("nashcode setup --viewer"), "{why}");

    // No profile store at all.
    let missing = dir.path().join("absent.toml");
    let out = nashcode(&missing, dir.path(), &["--json", "brain"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["status"], "unavailable");
}

#[test]
fn a_viewer_that_answers_html_is_reported_rather_than_half_read() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server("200 OK", "<!doctype html><title>brain</title>");
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, dir.path(), &["--json", "brain", "demo"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["ok"], true, "the command succeeded; the answer was junk: {v}");
    assert_eq!(v["result"]["status"], "unavailable");
    assert!(v["result"]["error"].as_str().unwrap().contains("not JSON"), "{v}");
    server.served();
}

#[test]
fn a_viewer_that_accepts_and_never_answers_times_out_rather_than_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let (port, connected) = black_hole();
    let config = write_config(dir.path(), Some(port));

    let started = Instant::now();
    let out = nashcode(&config, dir.path(), &["--json", "brain", "demo"]);
    let elapsed = started.elapsed();

    // Connected, so this is the client's own timeout firing, not a refused socket.
    connected
        .recv_timeout(PATIENCE)
        .expect("the CLI never opened a connection");

    assert_eq!(out.status.code(), Some(0), "a hung viewer must not fail the session");
    assert!(
        elapsed < Duration::from_secs(30),
        "waited {elapsed:?}; a session-start hook cannot block that long"
    );
    let v = envelope(&out);
    assert_eq!(v["ok"], true, "a hung viewer is not a failed command: {v}");
    assert_eq!(v["result"]["status"], "unavailable");
    assert!(v["result"]["error"].as_str().unwrap().contains("unreachable"), "{v}");
}

#[test]
fn a_repo_the_viewer_has_never_heard_of_is_named_in_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = one_shot_server("200 OK", r#"{"generated_at":"2026-08-19T12:00:00Z","repos":[]}"#);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, dir.path(), &["brain", "typo-repo"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["result"]["status"], "unavailable");
    assert_eq!(
        v["result"]["error"],
        "no repository named typo-repo on the viewer"
    );
    server.served();

    // With no name asked for, an empty viewer is an empty viewer, not a typo.
    let (port, server) = one_shot_server("200 OK", r#"{"generated_at":"2026-08-19T12:00:00Z","repos":[]}"#);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, dir.path(), &["brain"]);
    assert_eq!(out.status.code(), Some(0));
    let v = envelope(&out);
    assert_eq!(v["result"]["status"], "ok");
    assert_eq!(v["result"]["repos"].as_array().unwrap().len(), 0);
    server.served();
}
