//! The drain, against the real public ingester.
//!
//! No mock stands in for the edge. This test brings up what `ingester/test.sh` brings
//! up — a MinIO container for the fleet bucket, a real `celld deploy` of the Worker in
//! `ingester/`, and a real celld node on loopback — then posts envelopes and log
//! batches at it the way the internet would, and drains them into a real viewer with a
//! real object store behind it. What comes out is compared against the same bytes
//! posted straight at the tailnet door.
//!
//! **It skips when the machine cannot run it.** docker, celld 0.2+, and esbuild are
//! not things every checkout has, and a red suite for a missing container teaches
//! nobody anything. The skip prints why, on stderr; `cargo nextest run -p nashcode
//! --no-capture -E 'test(the_public_ingester)'` shows it. Set
//! `NASHCODE_REQUIRE_CELLD=1` and the missing prerequisite is a failure instead, which
//! is what a machine that is supposed to have them should do.
//!
//! One test function, not eight. Every function nextest runs gets its own process, so
//! a node per fact would mean eight MinIO containers and eight `celld deploy` runs for
//! about three minutes of setup. The facts are sections, and each one says what it is.
//! The two states a real edge cannot be made to hold still in — a digest queue that is
//! exactly full, and an ack the edge refuses — are unit tests in `bugs/drain.rs`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nashcode::bugs::drain::{Answer, Drainer, HttpTransport, Transport};
use nashcode::config::Config;
use nashcode::db::Db;
use topcoat::router::{Body, Method, Router, request::Request, to_bytes};

// ---- the node ------------------------------------------------------------------------

/// Why this machine cannot run the test, if it cannot.
fn missing_prerequisite() -> Option<String> {
    if !which("docker") {
        return Some("no docker: the fleet bucket is a MinIO container and celld has no \
                     filesystem or in-memory bucket mode"
            .to_owned());
    }
    if !Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return Some("docker is installed but not running".to_owned());
    }
    if !which("esbuild") {
        return Some("no esbuild on PATH: `celld deploy` bundles the Worker with it".to_owned());
    }
    let celld = celld_binary();
    let version = Command::new(&celld).arg("--version").output().ok().map(|out| {
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    });
    match version {
        None => Some(format!(
            "no celld at {}: install it with `curl -fsSL https://celld.dev/install.sh | sh`, \
             or point CELLD at a binary",
            celld.display()
        )),
        Some(line) => {
            let number = line.split_whitespace().last().unwrap_or("");
            let (major, minor) = {
                let mut parts = number.split('.');
                (
                    parts.next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0),
                    parts.next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0),
                )
            };
            if major == 0 && minor < 2 {
                Some(format!(
                    "celld {number} at {} is too old; the ingester needs 0.2.0 or newer. \
                     Set CELLD to a newer binary",
                    celld.display()
                ))
            } else {
                None
            }
        }
    }
}

fn which(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn celld_binary() -> PathBuf {
    std::env::var("CELLD").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("celld"))
}

fn ingester_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ingester")
}

/// A port nothing is listening on. Racy in principle, the same way every test harness
/// that does this is racy, and it has never mattered on loopback.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// A live ingester: one MinIO container, one deployed Worker, one celld node.
///
/// Built in place rather than with struct-update syntax: the type has a `Drop`, and a
/// `Drop` type cannot be moved out of. Everything is `Option` until it exists, so a
/// panic anywhere in `start` still takes the container down on the way out.
struct Node {
    url: String,
    token: String,
    container: String,
    celld: Option<Child>,
    _watch: tempfile::TempDir,
}

impl Node {
    async fn start() -> Self {
        let run_id = format!("{:x}", uuid_like());
        let container = format!("nashcode-drain-test-{run_id}");
        let bucket = format!("drain-test-{run_id}");
        let minio_port = free_port();
        let node_port = free_port();
        let token = format!("{:x}{:x}", uuid_like(), uuid_like());

        let endpoint = format!("http://127.0.0.1:{minio_port}");
        let env: Vec<(&str, String)> = vec![
            ("AWS_ACCESS_KEY_ID", "draintest".to_owned()),
            ("AWS_SECRET_ACCESS_KEY", "draintest123".to_owned()),
            ("AWS_REGION", "us-east-1".to_owned()),
            ("S3_ENDPOINT", endpoint.clone()),
        ];

        let publish = format!("{minio_port}:9000");
        run(
            Command::new("docker").args([
                "run",
                "-d",
                "--name",
                container.as_str(),
                "-p",
                publish.as_str(),
                "-e",
                "MINIO_ROOT_USER=draintest",
                "-e",
                "MINIO_ROOT_PASSWORD=draintest123",
                "minio/minio:latest",
                "server",
                "/data",
            ]),
            "docker run minio",
        );

        // From here on the container exists, so the guard has to.
        let mut node = Node {
            url: format!("http://127.0.0.1:{node_port}"),
            token,
            container,
            celld: None,
            _watch: tempfile::tempdir().expect("tempdir"),
        };

        let client = reqwest::Client::new();
        let health = format!("{endpoint}/minio/health/live");
        wait_for(Duration::from_secs(90), "MinIO", || {
            let client = client.clone();
            let health = health.clone();
            async move {
                client.get(&health).send().await.map(|r| r.status().is_success()).unwrap_or(false)
            }
        })
        .await;

        run(
            Command::new("docker").args([
                "exec",
                node.container.as_str(),
                "mc",
                "alias",
                "set",
                "local",
                "http://127.0.0.1:9000",
                "draintest",
                "draintest123",
            ]),
            "mc alias set",
        );
        let make_bucket = format!("local/{bucket}");
        run(
            Command::new("docker").args([
                "exec",
                node.container.as_str(),
                "mc",
                "mb",
                make_bucket.as_str(),
            ]),
            "mc mb",
        );

        let mut deploy = Command::new(celld_binary());
        deploy.arg("deploy").arg(ingester_dir()).arg("--bucket").arg(format!("s3://{bucket}"));
        for (key, value) in &env {
            deploy.env(key, value);
        }
        run(&mut deploy, "celld deploy");

        // A minute of lease so a loaded machine does not make the node self-fence, and
        // a 200 ms registry TTL so a pushed set is visible almost at once.
        let mut serve = Command::new(celld_binary());
        serve
            .arg("--bucket")
            .arg(format!("s3://{bucket}"))
            .arg("--listen")
            .arg(format!("127.0.0.1:{node_port}"))
            .env("CELLD_TTL_MS", "60000")
            .env("CELLD_IDLE_EVICT_S", "1")
            .env("CELLD_VAR_DRAIN_TOKEN", &node.token)
            .env("CELLD_VAR_REGISTRY_TTL_MS", "200")
            .env("CELLD_WATCH", node._watch.path())
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in &env {
            serve.env(key, value);
        }
        node.celld = Some(serve.spawn().expect("celld starts"));

        let probe = format!("{}/api/999/envelope/", node.url);
        wait_for(Duration::from_secs(90), "the celld node", || {
            let client = client.clone();
            let probe = probe.clone();
            async move {
                client
                    .post(&probe)
                    .body("{}")
                    .send()
                    .await
                    .map(|r| r.status().as_u16() == 404)
                    .unwrap_or(false)
            }
        })
        .await;
        node
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(child) = self.celld.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("docker")
            .args(["rm", "-f", self.container.as_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn run(command: &mut Command, what: &str) {
    let output = command.output().unwrap_or_else(|error| panic!("{what}: {error}"));
    assert!(
        output.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn wait_for<F, Fut>(limit: Duration, what: &str, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if ready().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{what} never came up");
}

fn uuid_like() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.finish()
}

// ---- the viewer half -------------------------------------------------------------------

/// A viewer with error tracking on and a `file://` bucket, over a database on disk so
/// the restart half of the test can reopen it.
fn viewer_bed() -> common::TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");
    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: vec!["demo".to_owned()],
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        webhooks: BTreeMap::new(),
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:1".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: Some(format!("file://{}", root.path().join("bucket").display())),
        bugs_s3_endpoint: None,
        bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
        bugs_drain: None,
    });
    common::testbed_from_config(root, config)
}

/// Everything in the hot window, newest first.
fn log_query() -> nashcode::bugs::LogQuery {
    nashcode::bugs::LogQuery { text: None, level: None, file: None, limit: 100, offset: 0 }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bugs").join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {path:?}: {error}"))
}

/// Post straight at the tailnet door, through the real router.
async fn post_direct(
    router: &Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, String) {
    let mut builder = Request::builder().method(Method::POST).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body)).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A transport that does everything the real one does and then throws the ack away.
///
/// Not a stand-in for the edge: every byte still goes to the real celld node. It exists
/// to produce the one condition the protocol promises is safe — the same rows delivered
/// twice — without waiting for a network to lose a packet.
#[derive(Debug)]
struct NoAck(HttpTransport);

impl Transport for NoAck {
    fn send<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        body: Option<Vec<u8>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Answer, String>> + Send + 'a>> {
        if path.starts_with("/_nashcode/ack/") {
            return Box::pin(async { Err("the ack never left the building".to_owned()) });
        }
        self.0.send(method, path, body)
    }

    fn describe(&self) -> String {
        self.0.describe()
    }
}

// ---- the test ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_public_ingester_buffers_and_the_drain_brings_it_home() {
    if let Some(reason) = missing_prerequisite() {
        let message = format!(
            "\n\
             ==========================================================================\n\
             SKIPPED: bugs_drain — {reason}\n\
             Nothing about the drain was proven on this machine. Set \
             NASHCODE_REQUIRE_CELLD=1 to make this a failure.\n\
             ==========================================================================\n"
        );
        eprintln!("{message}");
        println!("{message}");
        assert!(
            std::env::var("NASHCODE_REQUIRE_CELLD").is_err(),
            "NASHCODE_REQUIRE_CELLD is set: {reason}"
        );
        return;
    }

    let node = Node::start().await;
    let client = reqwest::Client::new();
    let bed = viewer_bed();
    let bugs = bed.bugs.clone();

    // `public` is reached through the edge; `direct` is the control, reached through
    // the tailnet door with the same bytes. Everything the drain does to the first
    // must produce what the second already has.
    let public = bugs.create_project("public", Some("demo")).expect("project");
    let direct = bugs.create_project("direct", Some("demo")).expect("project");

    let transport = Arc::new(
        HttpTransport::new(&node.url, &node.token).expect("transport"),
    ) as Arc<dyn Transport>;
    let drainer = Drainer::new(bugs.clone(), bed.db.clone(), transport.clone());

    // ---- fact: the registry push is what makes the edge answer at all ----------------
    let envelope = fixture("python-exception.envelope");
    let auth = format!("Sentry sentry_key={}, sentry_version=7", public.key);
    let public_url = format!("{}/api/{}/envelope/", node.url, public.id);

    let before = client
        .post(&public_url)
        .header("x-sentry-auth", &auth)
        .body(envelope.clone())
        .send()
        .await
        .expect("post");
    assert_eq!(before.status().as_u16(), 404, "an unregistered project does not exist yet");

    let report = drainer.cycle().await;
    assert!(report.registry_pushed, "the first cycle pushes the set");
    assert_eq!(report.errors, 0);

    let accepted = client
        .post(&public_url)
        .header("x-sentry-auth", &auth)
        .body(envelope.clone())
        .send()
        .await
        .expect("post");
    assert_eq!(accepted.status().as_u16(), 200, "the edge takes it once the registry knows");

    // ---- fact: the same envelope, both ways, is the same issue ------------------------
    let direct_auth = format!("Sentry sentry_key={}, sentry_version=7", direct.key);
    let (status, _) = post_direct(
        &bed.router,
        &format!("/api/{}/envelope/", direct.id),
        &[("x-sentry-auth", direct_auth.as_str())],
        envelope.clone(),
    )
    .await;
    assert_eq!(status, 200);
    bugs.digested(1).await;

    let report = drainer.cycle().await;
    assert_eq!(report.replayed, 1, "one row came off the edge");
    assert_eq!(report.acked, 1);
    assert_eq!(report.poison, 0);
    bugs.digested(2).await;

    let drained = bugs.issues(public.id, None).expect("issues");
    let posted = bugs.issues(direct.id, None).expect("issues");
    assert_eq!(drained.len(), 1, "one issue out of the drain");
    assert_eq!(posted.len(), 1, "one issue out of the direct post");
    assert_eq!(drained[0].grouping_key, posted[0].grouping_key, "grouped the same way");
    assert_eq!(drained[0].title, posted[0].title);
    assert_eq!(drained[0].culprit, posted[0].culprit);
    assert_eq!(drained[0].events, 1);
    assert_eq!(
        bugs.events(drained[0].id, 10).expect("events").len(),
        1,
        "one event, not one per delivery attempt"
    );

    // ---- fact: a logs row lands in the log store -------------------------------------
    let batch = "{\"level\":\"error\",\"message\":\"the drain carried this\"}\n\
                 {\"level\":\"info\",\"message\":\"and this\"}\n";
    let logged = client
        .post(format!("{}/api/{}/logs", node.url, public.id))
        .header("x-sentry-auth", &auth)
        .body(batch)
        .send()
        .await
        .expect("post");
    assert_eq!(logged.status().as_u16(), 200);

    let report = drainer.cycle().await;
    assert_eq!(report.replayed, 1);
    let rows = bugs.logs(public.id, &log_query()).expect("logs");
    assert_eq!(rows.len(), 2, "both lines of the batch");
    assert!(rows.iter().any(|row| row.message.contains("the drain carried this")));

    // ---- fact: draining twice without an ack duplicates nothing -----------------------
    let again = client
        .post(&public_url)
        .header("x-sentry-auth", &auth)
        .body(envelope.clone())
        .send()
        .await
        .expect("post");
    assert_eq!(again.status().as_u16(), 200);

    let cursor_before = drainer.cursor(public.id);
    let deaf = Drainer::new(
        bugs.clone(),
        bed.db.clone(),
        Arc::new(NoAck(HttpTransport::new(&node.url, &node.token).expect("transport")))
            as Arc<dyn Transport>,
    );
    let report = deaf.cycle().await;
    assert_eq!(report.replayed, 1, "the digest took it");
    assert_eq!(report.acked, 0, "the ack never landed");
    assert_eq!(drainer.cursor(public.id), cursor_before, "so the cursor did not move");

    let report = drainer.cycle().await;
    assert_eq!(report.replayed, 1, "the same row again, which is what at-least-once means");
    assert_eq!(report.acked, 1);
    bugs.digested(4).await;

    let drained = bugs.issues(public.id, None).expect("issues");
    assert_eq!(drained.len(), 1, "still one issue");
    assert_eq!(
        bugs.events(drained[0].id, 10).expect("events").len(),
        1,
        "and still one event: the same event_id twice is one occurrence"
    );
    let rows = bugs.logs(public.id, &log_query()).expect("logs");
    assert_eq!(rows.len(), 2, "and the log rows did not double either");

    // ---- fact: a poison row is acked rather than left to wedge the queue --------------
    let garbage = client
        .post(&public_url)
        .header("x-sentry-auth", &auth)
        .body("this was never an envelope".as_bytes().to_vec())
        .send()
        .await
        .expect("post");
    assert_eq!(garbage.status().as_u16(), 200, "the edge does not parse; it buffers");

    let report = drainer.cycle().await;
    assert_eq!(report.poison, 1);
    assert_eq!(report.acked, 1, "acked anyway, or every later row waits behind it");
    assert_eq!(drainer.poison_count(), 1);

    // ---- fact: the ack cursor survives a restart --------------------------------------
    let cursor = drainer.cursor(public.id);
    assert!(cursor > 0, "something has been acked by now");
    drop(drainer);

    let reopened = Db::open(&bed.config.db_path).expect("reopen the database");
    let restarted = Drainer::new(bugs.clone(), reopened, transport.clone());
    assert_eq!(restarted.cursor(public.id), cursor, "the cursor is on disk, not in memory");
    let report = restarted.cycle().await;
    assert_eq!(report.replayed, 0, "and a restart replays nothing that was already acked");

    // ---- fact: revoking a project and pushing the registry closes the edge ------------
    assert!(bugs.set_project_active(public.id, false).expect("revoke"));
    let report = restarted.cycle().await;
    assert!(report.registry_pushed, "the set changed, so it was pushed");

    // The isolate caches the registry for CELLD_VAR_REGISTRY_TTL_MS; give it that long.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let closed = client
        .post(&public_url)
        .header("x-sentry-auth", &auth)
        .body(envelope.clone())
        .send()
        .await
        .expect("post");
    assert_eq!(closed.status().as_u16(), 404, "a revoked key is absent, not merely wrong");

    // And the tailnet door says the same thing about the same project.
    let (status, _) = post_direct(
        &bed.router,
        &format!("/api/{}/envelope/", public.id),
        &[("x-sentry-auth", auth.as_str())],
        envelope,
    )
    .await;
    assert_eq!(status, 404);
}
