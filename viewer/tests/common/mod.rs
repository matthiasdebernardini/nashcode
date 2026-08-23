//! Shared plumbing for the integration tests: fixture repos built with real `git` in
//! tempdirs, an app + router wired exactly like `main.rs`, and a request helper that
//! drives the router without a listener.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use nashcode::brain::Brain;
use nashcode::bugs::Bugs;
use nashcode::ci::CiQueue;
use nashcode::code::{EmbedError, Embedder, Embeddings, Indexer};
use nashcode::config::Config;
use nashcode::db::Db;
use nashcode::docs::DocIndexCache;
use nashcode::hooks::Webhooks;
use nashcode::mirror::Mirrors;
use nashcode::ops::{Actor, Ops};
use nashcode::upstream::Upstreams;
use nashcode::web::{self, App};
use topcoat::router::{Body, Method, Router, request::Request, to_bytes};

/// Run git in `dir`, panicking loudly on failure — a broken fixture is a broken test.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {dir:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A working clone of a bare fixture remote, for building history.
pub struct Work {
    pub dir: PathBuf,
    _keep: tempfile::TempDir,
}

impl Work {
    /// Clone the remote into a fresh workdir.
    pub fn clone_from(remote: &Path) -> Self {
        let keep = tempfile::tempdir().expect("tempdir");
        let dir = keep.path().join("work");
        let output = Command::new("git")
            .arg("clone")
            .arg(remote)
            .arg(&dir)
            .output()
            .expect("git clone");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Self { dir, _keep: keep }
    }

    pub fn write(&self, path: &str, content: &str) {
        let full = self.dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full, content).expect("write");
    }

    /// Write raw bytes, for fixtures that need a file git treats as binary.
    pub fn write_bytes(&self, path: &str, content: &[u8]) {
        let full = self.dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full, content).expect("write");
    }

    pub fn commit_all(&self, message: &str) -> String {
        git(&self.dir, &["add", "-A"]);
        git(&self.dir, &["commit", "-m", message]);
        git(&self.dir, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    pub fn checkout_new(&self, branch: &str) {
        git(&self.dir, &["checkout", "-b", branch]);
    }

    pub fn checkout(&self, branch: &str) {
        git(&self.dir, &["checkout", branch]);
    }

    pub fn push(&self, branch: &str) {
        git(&self.dir, &["push", "origin", branch]);
    }

    pub fn push_all(&self) {
        git(&self.dir, &["push", "origin", "--all"]);
    }
}

/// Create a bare remote named `<name>.git` under `root` with `main` as HEAD.
pub fn make_remote(root: &Path, name: &str) -> PathBuf {
    let bare = root.join(format!("{name}.git"));
    let output = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("--initial-branch=main")
        .arg(&bare)
        .output()
        .expect("git init --bare");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    bare
}

/// The standard stacked fixture: main + part-1 <- part-2, plus independent feature-x.
pub fn stacked_fixture(root: &Path, name: &str) -> Work {
    let remote = make_remote(root, name);
    let work = Work::clone_from(&remote);
    work.write("README.md", "# demo\n");
    work.write("src/app.txt", "one\n");
    work.commit_all("initial");
    work.push("main");

    work.checkout_new("part-1");
    work.write("src/app.txt", "one\ntwo\n");
    work.commit_all("part 1");
    work.push("part-1");

    work.checkout_new("part-2");
    work.write("src/extra.txt", "three\n");
    work.commit_all("part 2");
    work.push("part-2");

    work.checkout("main");
    work.checkout_new("feature-x");
    work.write("docs/notes.txt", "independent\n");
    work.commit_all("feature x");
    work.push("feature-x");
    work.checkout("main");
    work
}

/// Everything a test needs, wired exactly like `main.rs` minus the listener.
pub struct TestBed {
    pub root: tempfile::TempDir,
    pub config: Arc<Config>,
    pub db: Db,
    pub mirrors: Mirrors,
    pub app: App,
    pub router: Router,
    pub bugs: Bugs,
    pub embeddings: Embeddings,
    pub indexer: Indexer,
    pub upstreams: Upstreams,
}

impl TestBed {
    pub fn remote_root(&self) -> PathBuf {
        self.root.path().join("remotes")
    }

    pub fn actor(&self) -> Actor {
        Actor { login: "tester@example.invalid".into(), name: "Tester".into() }
    }

    /// The tip of a branch on the *remote* (the truth the viewer must agree with).
    pub fn remote_tip(&self, repo: &str, branch: &str) -> String {
        let bare = self.remote_root().join(format!("{repo}.git"));
        git(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).trim().to_owned()
    }

    /// Bring the mirror up to date and index the default branch, the way the CI
    /// worker does when a merge lands.
    pub async fn index(&self, repo: &str) -> nashcode::db::CodeRun {
        self.mirrors.refresh_now(repo).await;
        self.indexer.index_default_branch(repo).await.expect("the index run starts")
    }
}

/// Build a testbed over already-created remotes under `<root>/remotes`.
pub fn testbed_with(root: tempfile::TempDir, repos: &[&str], webhooks: BTreeMap<String, Vec<String>>) -> TestBed {
    let remotes = root.path().join("remotes");
    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: repos.iter().map(|r| (*r).to_owned()).collect(),
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        people_path: root.path().join("people.json"),
        webhooks,
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:1".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: None,
        bugs_s3_endpoint: None,
        bugs_ingest_url: "http://127.0.0.1:0".to_owned(),
        bugs_drain: None,
        pushover: None,
        public_url: "http://127.0.0.1:0".to_owned(),
        bugs_self_dsn: None,
    });
    testbed_from_config(root, config)
}

pub fn testbed_from_config(root: tempfile::TempDir, config: Arc<Config>) -> TestBed {
    testbed_build(root, config, false)
}

/// Like `testbed_from_config`, but with the tip observer wired the way `main.rs`
/// wires it: every newly seen branch tip enqueues a CI run and fires the `push`
/// webhook. Kept opt-in because the phantom queued runs would block merge tests.
pub fn observed_bed(build: impl FnOnce(&Path) -> Work, webhooks: BTreeMap<String, Vec<String>>) -> TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    build(&remotes);
    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: ["demo"].into_iter().collect(),
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        people_path: root.path().join("people.json"),
        webhooks,
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:1".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: None,
        bugs_s3_endpoint: None,
        bugs_ingest_url: "http://127.0.0.1:0".to_owned(),
        bugs_drain: None,
        pushover: None,
        public_url: "http://127.0.0.1:0".to_owned(),
        bugs_self_dsn: None,
    });
    testbed_build(root, config, true)
}

fn testbed_build(root: tempfile::TempDir, config: Arc<Config>, observe: bool) -> TestBed {
    let db = Db::open(&config.db_path).expect("db opens");
    let hooks = Webhooks::new(config.webhooks.clone());
    let (ci, _ci_rx) = CiQueue::new(db.clone());
    let mut mirrors = Mirrors::new(config.clone(), db.clone());
    if observe {
        let ci = ci.clone();
        let hooks = hooks.clone();
        mirrors = mirrors.with_observer(Arc::new(move |tip: nashcode::mirror::NewTip| {
            ci.enqueue(&tip.repo, &tip.branch, &tip.commit);
            hooks.send(
                nashcode::hooks::PUSH,
                serde_json::json!({
                    "repo": tip.repo,
                    "branch": tip.branch,
                    "commit": tip.commit,
                }),
            );
        }));
    }
    let ops = Ops {
        config: config.clone(),
        db: db.clone(),
        mirrors: mirrors.clone(),
        hooks: hooks.clone(),
    };
    // Every bed gets a working embedder, so the semantic path is exercised without a
    // model download. Tests that want the "no model" behaviour build an empty slot.
    let embeddings = Embeddings::with(Arc::new(WordBag::default()));
    let indexer = Indexer {
        config: config.clone(),
        db: db.clone(),
        mirrors: mirrors.clone(),
        embeddings: embeddings.clone(),
    };
    let bugs = Bugs::new(&config, db.clone()).expect("the bugs schema applies");
    let upstreams = Upstreams::new(config.clone());
    let app = App {
        config: config.clone(),
        db: db.clone(),
        mirrors: mirrors.clone(),
        upstreams: upstreams.clone(),
        docs: DocIndexCache::new(),
        ci,
        ops,
        brain: Brain::new(upstreams.clone()),
        bugs: bugs.clone(),
        embeddings: embeddings.clone(),
    };
    let router = web::router(app.clone());
    TestBed { root, config, db, mirrors, app, router, bugs, embeddings, indexer, upstreams }
}

/// A deterministic stand-in for the real encoder.
///
/// Each vector counts how often a fixed vocabulary appears in the text, so text that
/// talks about the same things scores high and unrelated text scores low. No model, no
/// download, no network — which is what makes the whole pipeline testable.
#[derive(Debug, Clone)]
pub struct WordBag {
    vocabulary: Vec<String>,
}

impl Default for WordBag {
    fn default() -> Self {
        Self::new(&[
            "retry", "backoff", "connect", "socket", "parse", "config", "render",
            "widget", "draw", "sleep", "error", "log",
        ])
    }
}

impl WordBag {
    pub fn new(words: &[&str]) -> Self {
        Self { vocabulary: words.iter().map(|w| (*w).to_owned()).collect() }
    }
}

impl Embedder for WordBag {
    fn model(&self) -> &str {
        "word-bag-test"
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                let lower = text.to_ascii_lowercase();
                self.vocabulary
                    .iter()
                    .map(|word| lower.matches(word.as_str()).count() as f32)
                    .collect()
            })
            .collect())
    }
}

/// One remote named `demo` built by `build`, then a testbed over it.
pub fn simple_bed(build: impl FnOnce(&Path) -> Work) -> TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    build(&remotes);
    testbed_with(root, &["demo"], BTreeMap::new())
}

/// Drive the router with a request; returns (status, body).
pub async fn request(
    router: &Router,
    method: Method,
    path: &str,
    body: Option<(&str, String)>,
) -> (u16, String) {
    let mut builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some((content_type, payload)) => {
            builder = builder.header("content-type", content_type);
            builder.body(Body::from(payload)).expect("request builds")
        }
        None => builder.body(Body::empty()).expect("request builds"),
    };
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn get(router: &Router, path: &str) -> (u16, String) {
    request(router, Method::GET, path, None).await
}

/// GET with extra request headers.
pub async fn get_from(router: &Router, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut builder = Request::builder().method(Method::GET).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// GET with `Accept: application/json`, for endpoints that serve both a page and JSON.
pub async fn get_json(router: &Router, path: &str) -> (u16, String) {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("accept", "application/json")
        .body(Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// GET without following the answer: the status and wherever it points.
pub async fn redirect(router: &Router, path: &str) -> (u16, Option<String>) {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (status, location)
}

pub async fn post_json(router: &Router, path: &str, payload: serde_json::Value) -> (u16, String) {
    request(router, Method::POST, path, Some(("application/json", payload.to_string()))).await
}

/// The same, with extra request headers — the Tailscale identity, say.
pub async fn post_json_from(
    router: &Router,
    path: &str,
    payload: serde_json::Value,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(payload.to_string())).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// POST a browser form, without following the redirect: the status and where it points.
pub async fn post_form(
    router: &Router,
    path: &str,
    fields: &[(&str, &str)],
) -> (u16, Option<String>, String) {
    post_form_from(router, path, fields, &[]).await
}

/// The same, with extra request headers — `sec-fetch-site`, say, which is how a
/// browser tells the server that a POST came from another page.
pub async fn post_form_from(
    router: &Router,
    path: &str,
    fields: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> (u16, Option<String>, String) {
    let payload = serde_urlencoded::to_string(fields).expect("form encodes");
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(payload)).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.expect("body reads");
    (status, location, String::from_utf8_lossy(&bytes).into_owned())
}

/// A tiny HTTP listener that records every request body and answers with a canned
/// (status, body) pair. Enough HTTP for webhooks and the Anthropic stub.
pub struct StubServer {
    pub url: String,
    pub received: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// A stub that answers each request with the next body in the list, repeating the
/// last one once the list runs out. Enough to drive a multi-turn tool conversation.
pub async fn spawn_stub_seq(status_line: &'static str, bodies: Vec<String>) -> StubServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let bodies = Arc::new(bodies);
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let tx = tx.clone();
            let bodies = bodies.clone();
            let served = served.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (headers_end, content_length) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = find_headers_end(&buf) {
                        let headers = String::from_utf8_lossy(&buf[..pos]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        break (pos + 4, length);
                    }
                };
                while buf.len() < headers_end + content_length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let _ = tx.send(String::from_utf8_lossy(&buf[headers_end..]).into_owned());
                let index = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = bodies[index.min(bodies.len() - 1)].clone();
                let reply = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    StubServer { url: format!("http://{addr}"), received: rx }
}

pub async fn spawn_stub(status_line: &'static str, response_body: String) -> StubServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let tx = tx.clone();
            let response_body = response_body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read headers, then exactly content-length body bytes.
                let (headers_end, content_length) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = find_headers_end(&buf) {
                        let headers = String::from_utf8_lossy(&buf[..pos]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        break (pos + 4, length);
                    }
                };
                while buf.len() < headers_end + content_length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[headers_end..]).into_owned();
                let _ = tx.send(body);
                let reply = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len(),
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    StubServer { url: format!("http://{addr}"), received: rx }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// A read-only static file server over one directory, counting what it was asked for.
///
/// This is enough HTTP for git's dumb protocol, which is all an upstream mirror needs:
/// after `git update-server-info` a bare repo is a directory of files, and `git clone
/// --mirror http://...` asks for `info/refs`, `HEAD`, and objects. Nothing here has to
/// be smart, and nothing here needs the network — it binds loopback on a port the
/// kernel picks.
///
/// The counter is the point of the whole thing: it is how a test proves that a pinned
/// dependency already on disk stopped asking.
pub struct FileServer {
    pub url: String,
    requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl FileServer {
    /// How many requests have been served since the listener came up.
    pub fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Serve `root` over HTTP on 127.0.0.1, on a port the kernel picks.
pub async fn serve_dir(root: PathBuf) -> FileServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let root = root.clone();
            let counter = counter.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if find_headers_end(&buf).is_some() {
                        break;
                    }
                }
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let head = String::from_utf8_lossy(&buf).into_owned();
                let mut parts = head.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let target = parts.next().unwrap_or_default().to_owned();
                let reply = static_reply(&root, &method, &target);
                let _ = socket.write_all(&reply).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    FileServer { url: format!("http://{addr}"), requests }
}

/// One response: the file, or a 404. A target that is not a plain relative path is a
/// 404 too — this server sits in a test, but it still does not serve `/etc/passwd`.
fn static_reply(root: &Path, method: &str, target: &str) -> Vec<u8> {
    let path = target.split(['?', '#']).next().unwrap_or_default().trim_start_matches('/');
    let plain = !path.is_empty()
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    let body = if plain { std::fs::read(root.join(path)).ok() } else { None };

    let (status, body) = match body {
        Some(body) => ("200 OK", body),
        None => ("404 Not Found", Vec::new()),
    };
    let mut reply = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if method != "HEAD" {
        reply.extend_from_slice(&body);
    }
    reply
}

// ---- upstream fixtures -------------------------------------------------------------

/// A directory of bare repos, served over the dumb protocol: the upstreams a stack test
/// declares.
///
/// Every repo here is a real bare repo published with `git update-server-info`, so the
/// fetches under test are real fetches and nothing touches the network. The server's
/// request counter is what proves a pinned dependency stops asking once it has what it
/// was pinned to — and that a browse never asks at all.
pub struct Origin {
    pub keep: tempfile::TempDir,
    pub server: FileServer,
}

impl Origin {
    pub async fn new() -> Self {
        let keep = tempfile::tempdir().expect("tempdir");
        let server = serve_dir(keep.path().to_path_buf()).await;
        Self { keep, server }
    }

    /// A new upstream with one commit on `main`, published for dumb clients.
    pub fn repo(&self, name: &str) -> Work {
        let bare = make_remote(self.keep.path(), name);
        let work = Work::clone_from(&bare);
        work.write("README.md", &format!("# {name}\n"));
        work.commit_all("initial");
        work.push("main");
        self.publish(name);
        work
    }

    /// `git update-server-info` is what makes a bare repo readable without a git
    /// server, so every push to an upstream is followed by one.
    pub fn publish(&self, name: &str) {
        git(&self.bare(name), &["update-server-info"]);
    }

    pub fn bare(&self, name: &str) -> PathBuf {
        self.keep.path().join(format!("{name}.git"))
    }

    pub fn url(&self, name: &str) -> String {
        format!("{}/{name}.git", self.server.url)
    }

    pub fn tip(&self, name: &str, branch: &str) -> String {
        git(&self.bare(name), &["rev-parse", &format!("refs/heads/{branch}")]).trim().to_owned()
    }

    pub fn requests(&self) -> usize {
        self.server.requests()
    }

    /// Take an upstream off the air without stopping the server, so the next fetch
    /// fails the way an upstream that moved or died does.
    pub fn unpublish(&self, name: &str) {
        let bare = self.bare(name);
        std::fs::rename(&bare, bare.with_extension("gone")).expect("rename");
    }
}

/// A testbed whose repos each declare the manifest given. An empty manifest means the
/// repo has no `.nashcode/stack.toml` at all.
pub fn bed_declaring(manifests: &[(&str, &str)]) -> TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    for (name, manifest) in manifests {
        let bare = make_remote(&remotes, name);
        let work = Work::clone_from(&bare);
        work.write("README.md", &format!("# {name}\n"));
        if !manifest.is_empty() {
            work.write(".nashcode/stack.toml", manifest);
        }
        work.commit_all("initial");
        work.push("main");
    }
    let names: Vec<&str> = manifests.iter().map(|(name, _)| *name).collect();
    testbed_with(root, &names, BTreeMap::new())
}
