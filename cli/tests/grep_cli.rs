//! `nashcode grep` end to end: the real binary, a real `rg` over a real checkout, and
//! a real `/code/find` answer served from a loopback listener this test owns. No
//! network, no host, no viewer.
//!
//! Five contracts are load-bearing and all five are asserted. An unknown rg flag is
//! never an error and never changes the meaning of the search. The output is grep's,
//! so anything that parses grep parses this. The text layer comes from the working
//! tree when there is one and from the index when there is not. Every narrowing goes
//! to both halves. And the exit codes are grep's — 0 with hits, 1 without — with 2
//! kept for a usage mistake and for having neither a checkout nor an index.
//!
//! `fixtures/code_find.json` is the viewer's own answer to `GET /demo/code/find?q=
//! backoff` over the `viewer/tests/code_find.rs` fixture repo. Regenerate it with
//!
//! ```text
//! cargo nextest run -p nashcode --test code_find --run-ignored only --no-capture \
//!   -E 'test(print_the_cli_fixture)'
//! ```
//!
//! then pin `indexed_at` and `age_seconds`, which are the only edited values, so the
//! humanised age stays deterministic.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("fixtures/code_find.json");

/// An answer from an indexed repository that holds nothing for this query.
const NO_HITS: &str = r#"{"repo":"demo","query":"zzz","indexed":true,
  "commit":"59f7f0626796a45aea0de8fe97f94d5fb034d2de","age_seconds":259200,
  "hits":[],"counts":{"definition":0,"reference":0,"text":0,"semantic":0},
  "truncated":false,"semantic_available":true,
  "hint":"indexed at 59f7f06, but nothing matches"}"#;

/// An indexed repository whose index has nothing to add to this particular search.
const EMPTY_INDEX: &str = r#"{"repo":"demo","query":"retry","indexed":true,
  "commit":"59f7f0626796a45aea0de8fe97f94d5fb034d2de","age_seconds":259200,
  "hits":[],"counts":{"definition":0,"reference":0,"text":0,"semantic":0},
  "truncated":false,"semantic_available":true}"#;

/// An answer whose only layer is semantic: the index found no exact match either.
const ONLY_SEMANTIC: &str = r#"{"repo":"demo","query":"pause","indexed":true,
  "commit":"59f7f0626796a45aea0de8fe97f94d5fb034d2de","age_seconds":259200,
  "hits":[{"layer":"semantic","path":"src/net.rs","line":4,
           "text":"fn backoff(attempt: u32) {","name":"backoff","end_line":6,"score":0.7}],
  "counts":{"definition":0,"reference":0,"text":0,"semantic":1},
  "truncated":false,"semantic_available":true}"#;

/// A capped answer: the server had more than its row budget allowed.
const TRUNCATED: &str = r#"{"repo":"demo","query":"backoff","indexed":true,
  "commit":"59f7f0626796a45aea0de8fe97f94d5fb034d2de","age_seconds":259200,
  "hits":[],"counts":{"definition":0,"reference":0,"text":0,"semantic":0},
  "truncated":true,"semantic_available":true}"#;

/// How long a test waits for the CLI to speak before it calls the run a failure.
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

    fn served(self) {
        let _ = self.head();
    }
}

/// A listener that accepts and then says nothing, ever.
fn black_hole() -> (u16, std::sync::mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            held.push(stream);
            let _ = tx.send(());
        }
    });
    (port, rx)
}

/// A port with nothing behind it.
fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn write_config(dir: &Path, viewer: Option<u16>) -> std::path::PathBuf {
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

/// A working tree holding the same files the fixture repository holds, so the local
/// half and the index half describe one codebase.
fn checkout(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("work");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(
        repo.join("src/net.rs"),
        "\
use std::time::Duration;

/// Sleep, then try again.
fn backoff(attempt: u32) {
    sleep(Duration::from_secs(attempt as u64));
}

pub fn retry(attempts: u32) {
    for attempt in 0..attempts {
        backoff(attempt);
        connect();
    }
}
",
    )
    .unwrap();
    std::fs::write(repo.join("docs/notes.md"), "# notes\n\nretry is documented here.\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["remote", "add", "origin", "https://example-host/demo.git"],
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

fn nashcode(config: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    nashcode_with_rg(config, cwd, args, None)
}

/// The same, with `rg` pointed somewhere else — at nothing, for the fallback tests.
fn nashcode_with_rg(
    config: &Path,
    cwd: &Path,
    args: &[&str],
    rg: Option<&str>,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nashcode"));
    command.args(args).env("NASHCODE_CONFIG", config).current_dir(cwd);
    if let Some(rg) = rg {
        command.env("NASHCODE_RG_BIN", rg);
    }
    command.output().unwrap()
}

fn lines(out: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect()
}

fn errors(out: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stderr).lines().map(str::to_owned).collect()
}

fn first_line(head: &str) -> String {
    head.lines().next().unwrap_or_default().to_string()
}

/// True when no ancestor of `dir` is a git or jj working copy.
fn has_no_repo_above(dir: &Path) -> bool {
    dir.ancestors().all(|p| !p.join(".git").exists() && !p.join(".jj").exists())
}

/// These tests drive the real ripgrep, which is the whole point of the local half.
fn require_rg() {
    let found = Command::new("rg").arg("--version").output().is_ok();
    assert!(found, "these tests need ripgrep (rg) on PATH");
}

#[test]
fn unknown_rg_flags_are_accepted_in_silence() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    // Every one of these is a flag this command does not model.
    let out = nashcode(
        &config,
        &repo,
        &["grep", "--no-heading", "-S", "--color=never", "--hidden", "backoff"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stderr).is_empty(),
        "an unknown flag must not even warn: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(lines(&out).iter().any(|l| l.starts_with("src/net.rs:")), "{:?}", lines(&out));
    server.served();
}

#[test]
fn an_unknown_flag_that_takes_a_value_does_not_become_the_pattern() {
    require_rg();
    for spelling in [
        vec!["grep", "--color", "never", "backoff"],
        vec!["grep", "--max-count", "3", "backoff"],
        vec!["grep", "-m", "3", "backoff"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = checkout(dir.path());
        let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
        let config = write_config(dir.path(), Some(port));

        let out = nashcode(&config, &repo, &spelling);
        assert_eq!(out.status.code(), Some(0), "{spelling:?}: {:?}", lines(&out));
        // The pattern reached both halves: the request asked about `backoff`, and the
        // local pass found it. Before the flag table, both asked about `never` or `3`.
        assert_eq!(
            first_line(&server.head()),
            "GET /demo/code/find?q=backoff&limit=100 HTTP/1.1",
            "{spelling:?}"
        );
        assert!(
            lines(&out).iter().any(|l| l.starts_with("src/net.rs:4:")),
            "{spelling:?}: {:?}",
            lines(&out)
        );
    }
}

#[test]
fn a_flag_that_inverts_the_search_is_forwarded_to_rg_not_dropped() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));

    // `-v` inverts the match. Dropping it would answer the opposite question.
    let out = nashcode(&config, &repo, &["grep", "-v", "backoff", "docs/notes.md"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", lines(&out));
    let shown = lines(&out);
    assert!(shown.iter().any(|l| l.starts_with("docs/notes.md:1:")), "{shown:?}");
    assert!(
        !shown.iter().any(|l| l.contains("backoff(attempt)")),
        "-v was dropped: {shown:?}"
    );
    server.served();
}

#[test]
fn a_pattern_that_begins_with_a_dash_survives_the_double_dash() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    std::fs::write(repo.join("src/flags.rs"), "// -Zthreads=8 is a rustc flag\n").unwrap();
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));

    // clap eats the first `--`, so this only works if grep reads its own argv.
    let out = nashcode(&config, &repo, &["grep", "--", "-Zthreads"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", errors(&out));
    assert!(
        lines(&out).iter().any(|l| l.starts_with("src/flags.rs:1:")),
        "{:?}",
        lines(&out)
    );
    assert_eq!(
        first_line(&server.head()),
        "GET /demo/code/find?q=-Zthreads&limit=100 HTTP/1.1"
    );
}

#[test]
fn the_output_is_grep_shaped_with_the_definitions_block_first() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "backoff"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let shown = lines(&out);

    assert_eq!(shown[0], "# index: 59f7f06 (3 days ago)");
    assert_eq!(shown[1], "# definitions:");
    assert_eq!(shown[2], "src/net.rs:4:fn backoff(attempt: u32) { # fn, 1 ref, 1 caller");
    // Then the text hits, from the working tree, with nothing appended to them.
    assert_eq!(shown[3], "src/net.rs:4:fn backoff(attempt: u32) {");
    assert_eq!(shown[4], "src/net.rs:10:        backoff(attempt);");
    assert_eq!(shown.len(), 5, "{shown:?}");

    // Anything that parses grep parses this: every line is a comment or path:line:text,
    // and a definition strips back to raw content at the last ` # `.
    for line in &shown {
        if line.starts_with('#') {
            continue;
        }
        let content = line.rsplit_once(" # ").map(|(raw, _)| raw).unwrap_or(line);
        let (path, rest) = content.split_once(':').expect("a path");
        let (number, _) = rest.split_once(':').expect("a line number");
        assert_eq!(path, "src/net.rs");
        assert!(number.parse::<u32>().is_ok(), "not a line number: {line}");
    }

    assert_eq!(first_line(&server.head()), "GET /demo/code/find?q=backoff&limit=100 HTTP/1.1");
}

#[test]
fn context_flags_use_greps_dash_form_on_both_sides_of_a_hit() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "-C1", "backoff"]);
    assert_eq!(out.status.code(), Some(0));
    let shown = lines(&out);
    assert!(shown.iter().any(|l| l == "src/net.rs-3-/// Sleep, then try again."), "{shown:?}");
    assert!(shown.iter().any(|l| l == "src/net.rs:4:fn backoff(attempt: u32) {"), "{shown:?}");
    assert!(shown.iter().any(|l| l.starts_with("src/net.rs-5-")), "{shown:?}");
    server.served();

    // -A and -B ask for one side each.
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "-A", "1", "-B", "0", "backoff"]);
    let shown = lines(&out);
    assert!(shown.iter().any(|l| l.starts_with("src/net.rs-5-")), "after: {shown:?}");
    assert!(!shown.iter().any(|l| l.starts_with("src/net.rs-3-")), "before: {shown:?}");
    server.served();
}

#[test]
fn a_type_or_glob_narrows_the_local_pass_and_the_request_together() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());

    // Without -t, `retry` is in the markdown too.
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "retry"]);
    assert!(lines(&out).iter().any(|l| l.starts_with("docs/notes.md:")), "{:?}", lines(&out));
    server.served();

    // With it, neither half is allowed to answer with markdown.
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "-t", "rust", "retry"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!lines(&out).iter().any(|l| l.starts_with("docs/notes.md:")), "{:?}", lines(&out));
    let head = first_line(&server.head());
    assert!(head.contains("types=rust"), "the narrowing must reach the index: {head}");

    // A glob and a path argument travel the same way.
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "-g", "*.md", "retry", "docs"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!lines(&out).iter().any(|l| l.starts_with("src/")), "{:?}", lines(&out));
    let head = first_line(&server.head());
    assert!(head.contains("globs=%2A.md"), "{head}");
    assert!(head.contains("paths=docs"), "{head}");
}

#[test]
fn ignore_case_reaches_the_index_as_well_as_the_tree() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "-i", "BACKOFF"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", lines(&out));
    assert!(lines(&out).iter().any(|l| l.starts_with("src/net.rs:4:")), "{:?}", lines(&out));
    let head = first_line(&server.head());
    assert!(head.contains("case=insensitive"), "{head}");
}

#[test]
fn files_only_puts_paths_on_stdout_and_every_comment_on_stderr() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "-l", "backoff"]);
    assert_eq!(out.status.code(), Some(0));
    // stdout is a pure path list, so `nashcode grep -l x | xargs sed -i` is safe.
    assert_eq!(lines(&out), ["src/net.rs"]);
    let notes = errors(&out);
    assert_eq!(notes[0], "# index: 59f7f06 (3 days ago)");
    assert!(notes.contains(&"# definitions:".to_string()), "{notes:?}");
    server.served();

    // `--json` answers with files rather than text rows that carry no text.
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "-l", "--json", "backoff"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["files"], serde_json::json!(["src/net.rs"]));
    assert!(value.get("text").is_none(), "{value}");
    server.served();
}

#[test]
fn a_dead_viewer_degrades_to_plain_rg_with_one_comment_line() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let config = write_config(dir.path(), Some(dead_port()));

    let out = nashcode(&config, &repo, &["grep", "backoff"]);
    // Exit per rg: the local pass found something.
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let shown = lines(&out);
    assert!(shown[0].starts_with("# index unreachable"), "{shown:?}");
    assert_eq!(shown.iter().filter(|l| l.starts_with('#')).count(), 1, "one line, no more");
    assert_eq!(shown[1], "src/net.rs:4:fn backoff(attempt: u32) {");
    assert_eq!(shown[2], "src/net.rs:10:        backoff(attempt);");
}

#[test]
fn a_viewer_that_accepts_and_never_answers_times_out_rather_than_hanging() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, connected) = black_hole();
    let config = write_config(dir.path(), Some(port));

    let started = Instant::now();
    let out = nashcode(&config, &repo, &["grep", "backoff"]);
    let elapsed = started.elapsed();

    connected.recv_timeout(PATIENCE).expect("the CLI never opened a connection");
    assert_eq!(out.status.code(), Some(0), "the local half still answered");
    assert!(elapsed < Duration::from_secs(15), "waited {elapsed:?}; the client gives up at 10s");
    assert!(lines(&out)[0].starts_with("# index unreachable"), "{:?}", lines(&out));
}

#[test]
fn without_rg_the_text_layer_comes_from_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let missing = dir.path().join("no-such-rg");
    let out = nashcode_with_rg(
        &config,
        &repo,
        &["grep", "backoff"],
        Some(missing.to_str().unwrap()),
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let shown = lines(&out);
    // The failure is stated, not hidden behind an answer that looks local.
    assert!(shown[1].starts_with("# local rg failed:"), "{shown:?}");
    // The same two text lines, this time out of the index rather than the tree.
    assert!(shown.contains(&"src/net.rs:4:fn backoff(attempt: u32) {".to_string()), "{shown:?}");
    assert!(shown.contains(&"src/net.rs:10:        backoff(attempt);".to_string()), "{shown:?}");
    server.served();

    // And `--json` says which half answered.
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode_with_rg(
        &config,
        &repo,
        &["grep", "--json", "backoff"],
        Some(missing.to_str().unwrap()),
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["index"]["source"], "index");
    assert!(value["notes"][0].as_str().unwrap().contains("local rg failed"), "{value}");
    server.served();
}

#[test]
fn an_rg_that_refuses_the_arguments_says_so_rather_than_answering_in_silence() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    // rg has no type called `cobol`, and refuses the whole run over it.
    let out = nashcode(&config, &repo, &["grep", "-t", "cobol", "backoff"]);
    let shown = lines(&out);
    assert!(
        shown.iter().any(|l| l.starts_with("# local rg failed:")),
        "a silent index-only answer is the bug: {shown:?}"
    );
    server.served();
}

#[test]
fn the_semantic_block_prints_only_when_the_text_pass_found_nothing() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());

    // rg finds nothing for this word, and neither did the index's text pass.
    let (port, server) = one_shot_server("200 OK", ONLY_SEMANTIC);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "pause"]);
    assert_eq!(out.status.code(), Some(0), "a semantic hit is a hit");
    let shown = lines(&out);
    assert_eq!(shown[1], "# semantic (no exact match):");
    assert_eq!(shown[2], "src/net.rs:4:fn backoff(attempt: u32) {");
    server.served();

    // With text hits, the same semantic layer stays out of the way.
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));
    let out = nashcode(&config, &repo, &["grep", "backoff"]);
    assert!(!lines(&out).iter().any(|l| l.contains("semantic")), "{:?}", lines(&out));
    server.served();
}

#[test]
fn a_capped_index_answer_never_prints_as_complete() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", TRUNCATED);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "backoff"]);
    assert!(
        lines(&out).iter().any(|l| l.starts_with("# index answer truncated")),
        "{:?}",
        lines(&out)
    );
    server.served();
}

#[test]
fn a_live_index_with_no_local_hit_still_exits_one() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", NO_HITS);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "zzz_nothing_here"]);
    assert_eq!(out.status.code(), Some(1), "grep's own no-match code");
    // The header still prints, and so does the server's word on the empty answer: an
    // agent must be able to tell "not there" from "no index".
    assert_eq!(lines(&out)[0], "# index: 59f7f06 (3 days ago)");
    assert!(lines(&out).iter().any(|l| l.contains("nothing matches")), "{:?}", lines(&out));
    server.served();
}

#[test]
fn no_checkout_and_no_index_is_the_only_search_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(has_no_repo_above(dir.path()), "TMPDIR sits inside a checkout");
    let config = write_config(dir.path(), Some(dead_port()));

    let out = nashcode(&config, dir.path(), &["grep", "backoff"]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(out.stdout.is_empty(), "nothing to print: {:?}", lines(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("checkout"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");

    // Same fact in `--json`, and still exit 2.
    let out = nashcode(&config, dir.path(), &["--json", "grep", "backoff"]);
    assert_eq!(out.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["error"].as_str().unwrap().contains("checkout"), "{value}");
    assert!(value["fix"].is_string(), "{value}");
}

#[test]
fn a_missing_pattern_is_a_usage_error_that_stays_machine_readable() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), Some(dead_port()));

    // Human: help on stderr, so a pipeline reading stdout gets nothing to misparse.
    let out = nashcode(&config, dir.path(), &["grep"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "{:?}", lines(&out));
    assert!(errors(&out).iter().any(|l| l.contains("no pattern")), "{:?}", errors(&out));

    // Machine: one envelope on stdout, never help text.
    let out = nashcode(&config, dir.path(), &["grep", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["error"].as_str().unwrap().contains("no pattern"), "{value}");
    assert_eq!(value["fix"], "nashcode grep --help");

    // And `--help` is help, on stdout, exit 0.
    let out = nashcode(&config, dir.path(), &["grep", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ripgrep"), "{:?}", lines(&out));
}

#[test]
fn json_mode_carries_the_same_layers_as_the_text() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    // `--json` typed after the subcommand, which is where an agent types it.
    let out = nashcode(&config, &repo, &["grep", "--json", "--color=never", "backoff"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(value["ok"], true);
    assert_eq!(value["pattern"], "backoff");
    assert_eq!(value["repo"], "demo");
    assert_eq!(value["index"]["commit_short"], "59f7f06");
    assert_eq!(value["index"]["age"], "3 days ago");
    assert_eq!(value["index"]["reachable"], true);
    assert_eq!(value["index"]["source"], "rg");

    let definitions = value["definitions"].as_array().unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0]["name"], "backoff");
    assert_eq!(definitions[0]["kind"], "function");
    assert_eq!(definitions[0]["references"], 1);
    assert_eq!(definitions[0]["callers"], 1);

    let text = value["text"].as_array().unwrap();
    assert_eq!(text.len(), 2);
    assert_eq!(text[0]["line"], 4);
    assert_eq!(text[0]["context"], false);
    // Semantic is empty for the same reason the text output omits the block.
    assert!(value["semantic"].as_array().unwrap().is_empty());
    assert_eq!(value["hits"], 3);
    // The flags it did not model are named rather than hidden.
    assert_eq!(value["ignored_flags"], serde_json::json!(["--color=never"]));

    server.served();
}

#[test]
fn a_named_repository_overrides_the_remote() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("200 OK", FIXTURE);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "--repo", "other", "backoff"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(first_line(&server.head()), "GET /other/code/find?q=backoff&limit=100 HTTP/1.1");
}

#[test]
fn a_repository_the_viewer_has_never_heard_of_degrades_like_a_dead_one() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("404 Not Found", "not found");
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "--repo", "typo-repo", "backoff"]);
    assert_eq!(out.status.code(), Some(0), "the local half still answered");
    let shown = lines(&out);
    assert!(shown[0].starts_with("# index unreachable"), "{shown:?}");
    assert!(shown[0].contains("404"), "{shown:?}");
    assert_eq!(shown[1], "src/net.rs:4:fn backoff(attempt: u32) {");
    server.served();
}

#[test]
fn a_viewer_that_answers_with_an_error_status_degrades_like_a_dead_one() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    let (port, server) = one_shot_server("503 Service Unavailable", "nope");
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "backoff"]);
    assert_eq!(out.status.code(), Some(0));
    let shown = lines(&out);
    assert!(shown[0].starts_with("# index unreachable"), "{shown:?}");
    assert!(shown[0].contains("503"), "{shown:?}");
    assert_eq!(shown[1], "src/net.rs:4:fn backoff(attempt: u32) {");
    server.served();
}

#[test]
fn a_path_argument_that_leaves_the_repository_is_named_not_searched() {
    require_rg();
    let dir = tempfile::tempdir().unwrap();
    let repo = checkout(dir.path());
    std::fs::write(dir.path().join("outside.txt"), "backoff lives here too\n").unwrap();
    let (port, server) = one_shot_server("200 OK", EMPTY_INDEX);
    let config = write_config(dir.path(), Some(port));

    let out = nashcode(&config, &repo, &["grep", "backoff", "../outside.txt", "src"]);
    let shown = lines(&out);
    assert!(
        shown.iter().any(|l| l.starts_with("# path outside the repository")),
        "{shown:?}"
    );
    // Named in a comment, and in no hit line: it was not searched.
    assert!(
        !shown.iter().any(|l| !l.starts_with('#') && l.contains("outside.txt")),
        "{shown:?}"
    );
    assert!(shown.iter().any(|l| l.starts_with("src/net.rs:4:")), "{shown:?}");
    server.served();
}

#[test]
fn a_local_search_never_waits_on_a_pipe_that_will_not_answer() {
    require_rg();
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let repo = checkout(dir.path());
        // A FIFO nobody writes to: rg blocks on it forever, and this command must not.
        let fifo = repo.join("src/pipe");
        let made = Command::new("mkfifo").arg(&fifo).output().unwrap().status.success();
        assert!(made, "mkfifo");
        let config = write_config(dir.path(), Some(dead_port()));

        let started = Instant::now();
        let out = nashcode(&config, &repo, &["grep", "backoff", "src/pipe"]);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(15), "waited {elapsed:?}; the child is killed at 10s");
        // Nothing to search and nothing to ask: the one error.
        assert_eq!(out.status.code(), Some(2), "{:?} {:?}", lines(&out), errors(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("did not finish"),
            "{:?}",
            errors(&out)
        );
    }
}
