//! The agent-side client. The same binary is the server and the client, so there is
//! one thing to install.
//!
//! `nashcode-viewer hook` has one hard rule: **it must never fail an agent's turn.** An
//! unreachable server, a malformed payload, or no configured repo all exit 0 quietly.
//! Errors go to stderr only when `NASHCODE_DEBUG` is set.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Where the viewer lives, from the client's point of view.
pub fn base_url() -> String {
    std::env::var("NASHCODE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8090".to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn debug(message: &str) {
    if std::env::var("NASHCODE_DEBUG").is_ok() {
        eprintln!("nashcode: {message}");
    }
}

/// The repo name: `NASHCODE_REPO`, else the basename of the git remote at `dir`.
pub fn infer_repo(dir: &Path) -> Option<String> {
    if let Ok(repo) = std::env::var("NASHCODE_REPO")
        && !repo.trim().is_empty()
    {
        return Some(repo.trim().to_owned());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    let name = last.trim_end_matches(".git");
    (!name.is_empty()).then(|| name.to_owned())
}

fn head_of(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|sha| !sha.is_empty())
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client builds")
}

/// `nashcode-viewer hook` — read one agent-harness hook payload from stdin, record it, exit 0.
pub async fn hook() -> i32 {
    // Nothing below may propagate a failure.
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        debug("cannot read stdin");
        return 0;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) else {
        debug("stdin is not JSON; dropping the event");
        return 0;
    };

    let session = payload["session_id"]
        .as_str()
        .or_else(|| payload["sessionId"].as_str())
        .unwrap_or("unknown-session")
        .to_owned();
    let kind = payload["hook_event_name"]
        .as_str()
        .or_else(|| payload["hookEventName"].as_str())
        .unwrap_or("event")
        .to_owned();
    let cwd = payload["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let Some(repo) = infer_repo(&cwd) else {
        debug("no repo: set NASHCODE_REPO or run inside a clone with an origin remote");
        return 0;
    };
    let head = head_of(&cwd);

    let body = serde_json::json!({
        "session": session,
        "agent": "claude-code",
        "events": [{ "kind": kind, "payload": payload, "head": head }],
    });
    let url = format!("{}/{repo}/traces/events", base_url());
    match client(Duration::from_secs(3)).post(&url).json(&body).send().await {
        Ok(response) if response.status().is_success() => debug("event recorded"),
        Ok(response) => debug(&format!("server said {}", response.status())),
        Err(error) => debug(&format!("cannot reach {url}: {error}")),
    }
    0
}

/// The prompts page shows any event whose payload carries a top-level `prompt`. The
/// live hook provides that; harness transcripts bury the user's words under
/// `message.content` (a string, or text blocks). Lift them out so backfilled sessions
/// have prompts too. Lines starting with `<` are harness markup (command output,
/// system reminders), not something a person asked for.
fn transcript_prompt(value: &serde_json::Value) -> Option<String> {
    if value["type"].as_str() != Some("user") || value.get("prompt").is_some() {
        return None;
    }
    let text = match &value["message"]["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .find(|block| block["type"].as_str() == Some("text"))
            .and_then(|block| block["text"].as_str())
            .map(str::to_owned)?,
        _ => return None,
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    Some(trimmed.to_owned())
}

/// `nashcode-viewer trace push <file>` — backfill a whole transcript for a session.
///
/// `replace` passes `?replace=1`, which the server needs before it will overwrite a
/// transcript the session already has.
pub async fn trace_push(
    file: &str,
    session: Option<String>,
    repo: Option<String>,
    replace: bool,
) -> i32 {
    let Ok(raw) = std::fs::read_to_string(file) else {
        eprintln!("cannot read {file}");
        return 1;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(repo) = repo.or_else(|| infer_repo(&cwd)) else {
        eprintln!("no repo: pass --repo, set NASHCODE_REPO, or run inside a clone");
        return 1;
    };

    // JSONL lines become events; the session id comes from the flag, the lines, or
    // the file name.
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut found_session: Option<String> = None;
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => serde_json::json!({ "text": trimmed }),
        };
        if let Some(prompt) = transcript_prompt(&value) {
            value["prompt"] = serde_json::Value::String(prompt);
        }
        if found_session.is_none() {
            found_session = value["sessionId"]
                .as_str()
                .or_else(|| value["session_id"].as_str())
                .map(str::to_owned);
        }
        let kind = value["type"].as_str().unwrap_or("line").to_owned();
        events.push(serde_json::json!({
            "seq": (index + 1) as i64,
            "kind": kind,
            "payload": value,
        }));
    }
    let session = session
        .or(found_session)
        .or_else(|| {
            Path::new(file).file_stem().map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown-session".to_owned());
    if events.is_empty() {
        eprintln!("{file} contains no events");
        return 1;
    }

    let base = base_url();
    let http = client(Duration::from_secs(60));

    // Events in idempotent chunks, then the raw transcript verbatim. Chunks are
    // bounded by serialized size, not event count: a large transcript's tool
    // results can put 2000 events far past the server's request-body limit.
    const CHUNK_BYTES: usize = 800 * 1024;
    let mut chunks: Vec<Vec<serde_json::Value>> = vec![Vec::new()];
    let mut chunk_bytes = 0usize;
    for event in events {
        let len = event.to_string().len();
        let last = chunks.last_mut().expect("chunks starts non-empty");
        if !last.is_empty() && chunk_bytes + len > CHUNK_BYTES {
            chunks.push(vec![event]);
            chunk_bytes = len;
        } else {
            last.push(event);
            chunk_bytes += len;
        }
    }

    let mut stored = 0u64;
    let mut duplicates = 0u64;
    for chunk in &chunks {
        let body = serde_json::json!({ "session": session, "agent": "backfill", "events": chunk });
        let url = format!("{base}/{repo}/traces/events");
        match http.post(&url).json(&body).send().await {
            Ok(response) if response.status().is_success() => {
                if let Ok(outcome) = response.json::<serde_json::Value>().await {
                    stored += outcome["stored"].as_u64().unwrap_or(0);
                    duplicates += outcome["duplicates"].as_u64().unwrap_or(0);
                }
            }
            Ok(response) => {
                eprintln!("server rejected events: {}", response.status());
                return 1;
            }
            Err(error) => {
                eprintln!("cannot reach {url}: {error}");
                return 1;
            }
        }
    }

    let suffix = if replace { "?replace=1" } else { "" };
    let url = format!("{base}/{repo}/traces/{session}/transcript{suffix}");
    match http.post(&url).body(raw).send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) if response.status() == reqwest::StatusCode::CONFLICT => {
            eprintln!(
                "session {session} already has a transcript; re-run with --replace to overwrite it"
            );
            return 1;
        }
        Ok(response) => {
            eprintln!("server rejected the transcript: {}", response.status());
            return 1;
        }
        Err(error) => {
            eprintln!("cannot reach {url}: {error}");
            return 1;
        }
    }

    println!("session {session}: {stored} events stored, {duplicates} already known");
    0
}

/// `nashcode-viewer trace list` — sessions for the repo, newest first.
pub async fn trace_list(repo: Option<String>) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(repo) = repo.or_else(|| infer_repo(&cwd)) else {
        eprintln!("no repo: pass --repo, set NASHCODE_REPO, or run inside a clone");
        return 1;
    };
    let url = format!("{}/{repo}/traces", base_url());
    let response = client(Duration::from_secs(10))
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            let sessions: Vec<serde_json::Value> = response.json().await.unwrap_or_default();
            if sessions.is_empty() {
                println!("no traces for {repo}");
                return 0;
            }
            for row in sessions {
                println!(
                    "{}  {:>4} events  {:>3} commits  {}",
                    row["session"].as_str().unwrap_or("?"),
                    row["events"],
                    row["commits"],
                    row["last_event_at"].as_str().unwrap_or(""),
                );
            }
            0
        }
        Ok(response) => {
            eprintln!("server said {}", response.status());
            1
        }
        Err(error) => {
            eprintln!("cannot reach {url}: {error}");
            1
        }
    }
}

/// `nashcode-viewer trace show <session>` — one session's events.
pub async fn trace_show(session: &str, repo: Option<String>) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(repo) = repo.or_else(|| infer_repo(&cwd)) else {
        eprintln!("no repo: pass --repo, set NASHCODE_REPO, or run inside a clone");
        return 1;
    };
    let url = format!("{}/{repo}/traces/{session}", base_url());
    let response = client(Duration::from_secs(10))
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            for event in body["events"].as_array().into_iter().flatten() {
                let payload: serde_json::Value = event["payload"]
                    .as_str()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| event["payload"].clone());
                let summary = crate::traces::summarize(
                    event["kind"].as_str().unwrap_or("event"),
                    &payload,
                );
                println!(
                    "#{:<4} {:<20} {}",
                    event["seq"],
                    event["kind"].as_str().unwrap_or("event"),
                    summary
                );
            }
            let commits: Vec<&str> = body["commits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str())
                .collect();
            if !commits.is_empty() {
                println!("commits: {}", commits.join(" "));
            }
            0
        }
        Ok(response) => {
            eprintln!("server said {}", response.status());
            1
        }
        Err(error) => {
            eprintln!("cannot reach {url}: {error}");
            1
        }
    }
}

/// `nashcode doctor` — what is configured, what is missing, is the server up.
pub async fn doctor() -> i32 {
    let base = base_url();
    println!("NASHCODE_URL      {base}");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match infer_repo(&cwd) {
        Some(repo) => println!("repo             {repo}"),
        None => println!("repo             (none: set NASHCODE_REPO or run inside a clone)"),
    }
    match head_of(&cwd) {
        Some(head) => println!("HEAD             {head}"),
        None => println!("HEAD             (not a git repository)"),
    }

    match client(Duration::from_secs(5)).get(format!("{base}/")).send().await {
        Ok(response) if response.status().is_success() => {
            println!("server           reachable");
            0
        }
        Ok(response) => {
            println!("server           answered {}", response.status());
            1
        }
        Err(error) => {
            println!("server           unreachable: {error}");
            1
        }
    }
}

/// Parse `--repo x` / `--session y` style flags from a flat argument list.
pub fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

pub const USAGE: &str = "\
nashcode-viewer — stacked-branch viewer and agent-trace client

USAGE:
  nashcode-viewer [serve]                   run the server (the default)
  nashcode-viewer hook                      record one hook payload from stdin; always exits 0
  nashcode-viewer trace push <file> [--session s] [--repo r] [--replace]
                                           backfill a transcript (JSONL) for a session;
                                           --replace overwrites one already stored
  nashcode-viewer trace list [--repo r]     sessions, newest first
  nashcode-viewer trace show <session> [--repo r]
                                           one session's events
  nashcode-viewer doctor                    config summary + server reachability

The client half reads NASHCODE_URL (default http://127.0.0.1:8090) and infers the
repo from the git remote of the working directory, falling back to NASHCODE_REPO.";
