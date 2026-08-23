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
    match responds(Command::new("docker").arg("info"), Duration::from_secs(5)) {
        Some(true) => {}
        Some(false) => return Some("docker is installed but not running".to_owned()),
        None => {
            return Some(
                "docker is installed but not responding; `docker info` did not return \
                 within five seconds"
                    .to_owned(),
            );
        }
    }
    if !which("esbuild") {
        return Some("no esbuild on PATH: `celld deploy` bundles the Worker with it".to_owned());
    }
    let celld = celld_binary();
    let Some(spoken) = Command::new(&celld).arg("--version").output().ok().map(|out| {
        // Both streams: `celld --version` prints an allocator warning of its own
        // before the version, and which stream it lands on is not ours to assume.
        format!(
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }) else {
        return Some(format!(
            "no celld at {}: install it with `curl -fsSL https://celld.dev/install.sh | sh`, \
             or point CELLD at a binary",
            celld.display()
        ));
    };
    match parse_version(&spoken) {
        None => Some(format!(
            "cannot read a version out of `{} --version`: {}",
            celld.display(),
            spoken.trim()
        )),
        Some((0, minor)) if minor < 2 => Some(format!(
            "celld 0.{minor} at {} is too old; the ingester needs 0.2.0 or newer. \
             Set CELLD to a newer binary",
            celld.display()
        )),
        Some(_) => None,
    }
}

/// The first `major.minor.patch` in a blob of output.
///
/// Taking the last whitespace-separated word is what the first version of this did, and
/// it read `value.` out of the tail of celld's own allocator warning and called the
/// binary too old. A version is three numbers with dots between them and nothing else,
/// so ask for exactly that: the timestamp in a log line has colons in it and cannot be
/// mistaken for one.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|token| {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let major = parts[0].parse::<u32>().ok()?;
        let minor = parts[1].parse::<u32>().ok()?;
        parts[2].parse::<u32>().ok()?;
        Some((major, minor))
    })
}

/// Run a probe with a deadline. `Some(ok)` if it finished, `None` if it did not.
///
/// The deadline is the whole point. `docker info` against a wedged daemon never returns
/// — and this box has had a wedged daemon — so the version of this that just called
/// `status()` held its nextest slot until the run was killed by hand. A prerequisite
/// check that can hang is worse than no prerequisite check: it turns "this machine
/// cannot run the test" into "the suite never finishes".
fn responds(command: &mut Command, within: Duration) -> Option<bool> {
    let Ok(mut child) = command.stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return Some(false);
    };
    let deadline = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => {}
            Err(_) => return Some(false),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
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
        repos: ["demo"].into_iter().collect(),
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        people_path: root.path().join("people.json"),
        webhooks: BTreeMap::new(),
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:1".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: Some(format!("file://{}", root.path().join("bucket").display())),
        bugs_s3_endpoint: None,
        bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
        bugs_drain: None,
        pushover: None,
        public_url: "http://127.0.0.1:0".to_owned(),
        bugs_self_dsn: None,
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
    //
    // Both kinds have to be inside the redelivery window. The first version of this
    // posted the log batch, drained it, acked it, and only then opened the window — so
    // the log rows were long gone from the edge and the assertion that they had not
    // doubled was asserting nothing. It passed for a build in which every redelivered
    // log line was filed a second time.
    let again = client
        .post(&public_url)
        .header("x-sentry-auth", &auth)
        .body(envelope.clone())
        .send()
        .await
        .expect("post");
    assert_eq!(again.status().as_u16(), 200);

    let second_batch = "{\"level\":\"warn\",\"message\":\"redelivered once\"}\n\
                        {\"level\":\"warn\",\"message\":\"redelivered twice\"}\n";
    let logged_again = client
        .post(format!("{}/api/{}/logs", node.url, public.id))
        .header("x-sentry-auth", &auth)
        .body(second_batch)
        .send()
        .await
        .expect("post");
    assert_eq!(logged_again.status().as_u16(), 200);

    let cursor_before = drainer.cursor(public.id);
    let deaf = Drainer::new(
        bugs.clone(),
        bed.db.clone(),
        Arc::new(NoAck(HttpTransport::new(&node.url, &node.token).expect("transport")))
            as Arc<dyn Transport>,
    );
    let report = deaf.cycle().await;
    assert_eq!(report.replayed, 2, "the envelope and the log batch both went through");
    assert_eq!(report.acked, 0, "and neither ack landed");
    assert_eq!(drainer.cursor(public.id), cursor_before, "so the cursor did not move");

    let landed = bugs.logs(public.id, &log_query()).expect("logs");
    assert_eq!(landed.len(), 4, "two from the first batch, two from the second");

    let report = drainer.cycle().await;
    assert_eq!(report.replayed, 2, "the same rows again, which is what at-least-once means");
    assert_eq!(report.acked, 2);
    bugs.digested(4).await;

    let drained = bugs.issues(public.id, None).expect("issues");
    assert_eq!(drained.len(), 1, "still one issue");
    assert_eq!(
        bugs.events(drained[0].id, 10).expect("events").len(),
        1,
        "and still one event: the same event_id twice is one occurrence"
    );
    let rows = bugs.logs(public.id, &log_query()).expect("logs");
    assert_eq!(
        rows.len(),
        4,
        "and the log rows did not double: a drained batch dedupes on its seq, not on a \
         fresh archive key"
    );
    assert_eq!(
        rows.iter().filter(|row| row.message.starts_with("redelivered")).count(),
        2,
        "one row per line, however many times the line was delivered"
    );

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

/// The prerequisite check cost a whole suite run once, so it gets a test of its own.
#[test]
fn a_version_is_three_numbers_and_never_the_tail_of_a_log_line() {
    assert_eq!(parse_version("celld 0.2.1\n"), Some((0, 2)));
    assert_eq!(parse_version("celld 1.0.3"), Some((1, 0)));
    assert_eq!(parse_version("nothing numeric here"), None);

    // What this binary really says. The last word of it is `value.`, which the first
    // version of the check read as a version number and called 0.2.1 too old.
    let real = "2026-08-19T22:49:46.331627Z  WARN celld::memory: the allocator will not \
                run a background thread, so freed pages return only when a thread \
                allocates again error=`name` or `mib` specifies an unknown/invalid \
                value.\ncelld 0.2.1\n";
    assert_eq!(parse_version(real), Some((0, 2)));
}
