//! The log store, end to end: both doors, the search, the code-origin links, and the
//! prune.
//!
//! Same conventions as `bugs.rs`: a real `file://` object store in a tempdir, real
//! HTTP through `Router::handle`, a real git fixture repo behind the blob links.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::TestBed;
use nashcode::config::Config;
use topcoat::router::{Body, HeaderMap, Method, Router, request::Request, to_bytes};

fn bugs_bed() -> (TestBed, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");

    let bucket = root.path().join("bucket");
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
        bugs_bucket: Some(format!("file://{}", bucket.display())),
        bugs_s3_endpoint: None,
        bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
        bugs_drain: None,
    });
    let bed = common::testbed_from_config(root, config);
    (bed, bucket)
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bugs").join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {path:?}: {error}"))
}

struct Answer {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", self.body))
    }
}

async fn send(
    router: &Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Answer {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body)).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 128 * 1024 * 1024).await.expect("body reads");
    Answer { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

async fn get(bed: &TestBed, path: &str, headers: &[(&str, &str)]) -> Answer {
    send(&bed.router, Method::GET, path, headers, Vec::new()).await
}

const JSON: (&str, &str) = ("accept", "application/json");

/// A project, optionally declaring a repo. Hands back `(id, key)`.
async fn project(bed: &TestBed, name: &str, repo: Option<&str>) -> (i64, String) {
    let mut body = serde_json::json!({ "name": name });
    if let Some(repo) = repo {
        body["repo"] = serde_json::json!(repo);
    }
    let answer = send(
        &bed.router,
        Method::POST,
        "/bugs",
        &[("content-type", "application/json")],
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(answer.status, 201, "{}", answer.body);
    let created = answer.json();
    (created["id"].as_i64().expect("an id"), created["key"].as_str().expect("a key").to_owned())
}

/// The envelope door, the way every SDK uses it.
async fn post_envelope(bed: &TestBed, id: i64, key: &str, body: Vec<u8>) -> Answer {
    let auth = format!("Sentry sentry_version=7, sentry_client=sentry.python/2.68.0, sentry_key={key}");
    send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[("x-sentry-auth", auth.as_str()), ("content-type", "application/x-sentry-envelope")],
        body,
    )
    .await
}

/// The NDJSON door, the way a curl or a log shipper uses it.
async fn post_ndjson(bed: &TestBed, id: i64, key: &str, lines: &str) -> Answer {
    send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/logs?sentry_key={key}"),
        &[("content-type", "application/x-ndjson")],
        lines.as_bytes().to_vec(),
    )
    .await
}

fn walk(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found
}

// ---- fact 14: both doors, the search, and the prune --------------------------------

#[tokio::test]
async fn a_sentry_log_item_and_a_curl_ndjson_post_both_land_and_are_searchable() {
    let (bed, bucket) = bugs_bed();
    let (id, key) = project(&bed, "api", None).await;

    // Door one: a real envelope captured off sentry-python 2.68 with `enable_logs`.
    let answer = post_envelope(&bed, id, &key, fixture("sentry-logs.envelope")).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    bed.bugs.digested(1).await;

    // Door two: the shape a cron job or a journald shipper writes.
    let lines = concat!(
        r#"{"ts":"2026-08-19T04:05:06Z","level":"error","message":"backup failed: disk full","code.file.path":"src/app.txt","code.line.number":2,"code.function.name":"main"}"#,
        "\n",
        r#"{"ts":"2026-08-19T04:05:07Z","level":"info","message":"backup retried","host":"celld"}"#,
        "\n",
    );
    let answer = post_ndjson(&bed, id, &key, lines).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.json()["accepted"], 2);
    assert_eq!(answer.json()["rejected"]["count"], 0);

    // Four lines in total: two from the SDK, two from curl.
    let page = get(&bed, "/bugs/api/logs", &[JSON]).await.json();
    let rows = page["logs"].as_array().expect("logs");
    assert_eq!(rows.len(), 4, "{rows:?}");
    // Newest first, across both doors. The SDK's own clock stamped the captured
    // fixture, so which line is first is the fixture's business — the ordering is not.
    let stamps: Vec<&str> = rows.iter().map(|row| row["ts"].as_str().expect("a ts")).collect();
    let mut sorted = stamps.clone();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(stamps, sorted, "newest first");
    let position = |message: &str| {
        rows.iter().position(|row| row["message"] == message).unwrap_or_else(|| panic!("{message}"))
    };
    assert!(position("backup retried") < position("backup failed: disk full"));

    // The SDK's two lines carry their level and their trace, normalized to the OTel
    // model whichever way the SDK spelled it.
    let sdk: Vec<&serde_json::Value> =
        rows.iter().filter(|row| row["source"] == "envelope").collect();
    assert_eq!(sdk.len(), 2);
    let warned = sdk.iter().find(|row| row["severity_text"] == "warn").expect("the warning");
    assert_eq!(warned["message"], "cache miss for user 12");
    assert_eq!(warned["severity_number"], 13);
    assert_eq!(warned["trace_id"].as_str().expect("a trace id").len(), 32);
    assert_eq!(
        warned["attributes"]["sentry.sdk.name"], "sentry.python",
        "the {{value, type}} wrapper is flattened away"
    );

    // FTS finds them by word, through both doors.
    let found = get(&bed, "/bugs/api/logs?q=cache", &[JSON]).await.json();
    assert_eq!(found["logs"].as_array().expect("logs").len(), 1);
    let found = get(&bed, "/bugs/api/logs?q=disk", &[JSON]).await.json();
    assert_eq!(found["logs"].as_array().expect("logs").len(), 1);
    assert_eq!(found["logs"][0]["message"], "backup failed: disk full");

    // The level filter and the `file:` token narrow it further.
    let errors = get(&bed, "/bugs/api/logs?level=error", &[JSON]).await.json();
    assert_eq!(errors["logs"].as_array().expect("logs").len(), 1);
    let by_file = get(&bed, "/bugs/api/logs?q=file%3Asrc%2Fapp.txt", &[JSON]).await.json();
    assert_eq!(by_file["logs"].as_array().expect("logs").len(), 1);
    assert_eq!(by_file["logs"][0]["code_line"], 2);
    assert_eq!(get(&bed, "/bugs/api/logs?level=nonsense", &[JSON]).await.status, 400);

    // Every batch is archived as one NDJSON object, and the page renders too.
    let archives: Vec<String> =
        walk(&bucket).into_iter().filter(|path| path.contains("/logs/")).collect();
    assert_eq!(archives.len(), 2, "one object per accepted batch: {archives:?}");
    let archived = std::fs::read_to_string(&archives[0]).expect("read");
    assert!(archived.lines().all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
    assert_eq!(get(&bed, "/bugs/api/logs", &[]).await.status, 200);

    // The prune takes hot rows past retention and leaves the archive object alone.
    let old = nashcode::bugs::logs::days_ago(60).expect("a date");
    let line = serde_json::json!({"ts": old, "level": "info", "message": "last month"});
    assert_eq!(post_ndjson(&bed, id, &key, &format!("{line}\n")).await.status, 200);
    assert_eq!(bed.bugs.log_count(id).unwrap(), 5);

    let objects_before = walk(&bucket).len();
    assert_eq!(bed.bugs.prune_logs().unwrap(), 1, "one row is past the 30-day default");
    assert_eq!(bed.bugs.log_count(id).unwrap(), 4);
    assert!(
        get(&bed, "/bugs/api/logs?q=month", &[JSON]).await.json()["logs"]
            .as_array()
            .expect("logs")
            .is_empty(),
        "and the search index forgot it too"
    );
    assert_eq!(walk(&bucket).len(), objects_before, "the archive object stays");
}

// ---- code origin: both generations of OTel names -----------------------------------

#[tokio::test]
async fn a_code_origin_is_indexed_under_either_generation_of_otel_names() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api", None).await;

    let lines = concat!(
        r#"{"level":"info","message":"new names","code.file.path":"src/new.rs","code.line.number":11,"code.function.name":"fresh"}"#,
        "\n",
        // What an SDK pinned before the 2024 rename still sends.
        r#"{"level":"info","message":"old names","code.filepath":"src/old.rs","code.lineno":22,"code.function":"legacy"}"#,
        "\n",
    );
    assert_eq!(post_ndjson(&bed, id, &key, lines).await.json()["accepted"], 2);

    let rows = get(&bed, "/bugs/api/logs", &[JSON]).await.json();
    let rows = rows["logs"].as_array().expect("logs").clone();
    let new = rows.iter().find(|row| row["message"] == "new names").expect("the new one");
    let old = rows.iter().find(|row| row["message"] == "old names").expect("the old one");
    assert_eq!((&new["code_file"], &new["code_line"], &new["code_function"]),
               (&serde_json::json!("src/new.rs"), &serde_json::json!(11), &serde_json::json!("fresh")));
    assert_eq!((&old["code_file"], &old["code_line"], &old["code_function"]),
               (&serde_json::json!("src/old.rs"), &serde_json::json!(22), &serde_json::json!("legacy")));

    // Both spellings answer the same `file:` filter, because both normalized.
    for wanted in ["src/new.rs", "src/old.rs"] {
        let found = get(
            &bed,
            &format!("/bugs/api/logs?q=file%3A{}", wanted.replace('/', "%2F")),
            &[JSON],
        )
        .await
        .json();
        assert_eq!(found["logs"].as_array().expect("logs").len(), 1, "{wanted}");
    }
}

// ---- code origin: the link, and the refusal to make a dead one ---------------------

#[tokio::test]
async fn a_resolvable_code_origin_links_into_the_repo_and_an_unresolvable_one_does_not() {
    let (bed, _bucket) = bugs_bed();
    // The mirror has to exist before a path can be resolved against it.
    bed.mirrors.refresh_now("demo").await;
    let (id, key) = project(&bed, "api", Some("demo")).await;

    // `src/app.txt` is in the fixture repo. The other three are not, or could never be.
    let lines = concat!(
        r#"{"level":"info","message":"real file","code.file.path":"src/app.txt","code.line.number":2}"#,
        "\n",
        r#"{"level":"info","message":"missing file","code.file.path":"src/never_existed.rs","code.line.number":9}"#,
        "\n",
        r#"{"level":"info","message":"absolute path","code.file.path":"/usr/lib/python3/logging.py","code.line.number":9}"#,
        "\n",
        r#"{"level":"info","message":"a directory","code.file.path":"src","code.line.number":1}"#,
        "\n",
    );
    assert_eq!(post_ndjson(&bed, id, &key, lines).await.json()["accepted"], 4);

    let page = get(&bed, "/bugs/api/logs", &[]).await;
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("href=\"/demo/blob/src/app.txt#L2\""),
        "a path that resolves links to its line"
    );
    // Everything else is shown, and never linked: a dead link is worse than text.
    for shown in ["src/never_existed.rs:9", "/usr/lib/python3/logging.py:9", "src:1"] {
        assert!(page.body.contains(shown), "{shown} is still shown");
    }
    for dead in ["/demo/blob/src/never_existed.rs", "/demo/blob/usr/lib", "/demo/blob/src#L1"] {
        assert!(!page.body.contains(dead), "{dead} must not be a link");
    }

    // A project with no declared repo has nowhere to link to at all.
    let (other, other_key) = project(&bed, "plain", None).await;
    assert_eq!(
        post_ndjson(
            &bed,
            other,
            &other_key,
            "{\"level\":\"info\",\"message\":\"real file\",\"code.file.path\":\"src/app.txt\",\"code.line.number\":2}\n",
        )
        .await
        .json()["accepted"],
        1
    );
    let page = get(&bed, "/bugs/plain/logs", &[]).await;
    assert!(page.body.contains("src/app.txt:2"), "still shown");
    assert!(!page.body.contains("/blob/src/app.txt"), "but never linked");
}

// ---- the NDJSON door's auth and its manners ---------------------------------------

#[tokio::test]
async fn the_ndjson_door_takes_the_same_key_as_the_envelope_route() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api", None).await;
    let line = "{\"level\":\"info\",\"message\":\"hello\"}\n";

    // No key at all, and the wrong key.
    let none = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/logs"),
        &[],
        line.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(none.status, 403);
    assert_eq!(none.header("x-sentry-error"), Some("no sentry_key"));
    assert_eq!(post_ndjson(&bed, id, &"f".repeat(32), line).await.status, 403);
    assert_eq!(post_ndjson(&bed, id + 999, &key, line).await.status, 404);

    // The header form, which is what a shipper with a config file uses.
    let auth = format!("Sentry sentry_version=7, sentry_key={key}");
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/logs"),
        &[("x-sentry-auth", auth.as_str())],
        line.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.header("access-control-allow-origin"), Some("*"));

    // The trailing-slash spelling reaches the same door.
    assert_eq!(
        send(
            &bed.router,
            Method::POST,
            &format!("/api/{id}/logs/?sentry_key={key}"),
            &[],
            line.as_bytes().to_vec()
        )
        .await
        .status,
        200
    );

    // One unreadable line is counted, and the readable ones beside it still land.
    let mixed = format!("{line}not json at all\n{line}");
    let answer = post_ndjson(&bed, id, &key, &mixed).await;
    assert_eq!(answer.json()["accepted"], 2);
    assert_eq!(answer.json()["rejected"]["count"], 1);
    assert_eq!(answer.json()["rejected"]["lines"], serde_json::json!([2]));

    // And an oversized body is refused on the same cap the envelopes get.
    let huge = "x".repeat(21 * 1024 * 1024);
    assert_eq!(post_ndjson(&bed, id, &key, &huge).await.status, 413);
}

#[tokio::test]
async fn with_no_bucket_the_log_routes_answer_404() {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");
    let bed = common::testbed_with(root, &["demo"], BTreeMap::new());

    assert_eq!(get(&bed, "/bugs/anything/logs", &[]).await.status, 404);
    assert_eq!(
        send(&bed.router, Method::POST, "/api/1/logs", &[], b"{}\n".to_vec()).await.status,
        404
    );
}

// ---- the review's two blockers, through the router --------------------------------

#[tokio::test]
async fn a_batch_with_too_many_lines_is_413_and_nothing_in_it_is_stored() {
    let (bed, bucket) = bugs_bed();
    let (id, key) = project(&bed, "api", None).await;

    // The cheap flood: every line passes the per-line length cap, and there are far
    // too many of them. Before the cap this built a Vec of every rejected line
    // number and echoed the lot back.
    let flood = "0\n".repeat(nashcode::bugs::logs::MAX_BATCH + 100);
    let answer = post_ndjson(&bed, id, &key, &flood).await;
    assert_eq!(answer.status, 413, "{}", answer.body);
    assert!(answer.body.len() < 4096, "the refusal is small: {} bytes", answer.body.len());

    // The expensive flood: valid lines, which would have been one enormous
    // transaction on the connection every page load shares.
    let valid = "{\"message\":\"x\"}\n".repeat(nashcode::bugs::logs::MAX_BATCH + 1);
    assert_eq!(post_ndjson(&bed, id, &key, &valid).await.status, 413);

    // Neither one left anything behind.
    assert_eq!(bed.bugs.log_count(id).unwrap(), 0);
    assert!(!walk(&bucket).iter().any(|path| path.contains("/logs/")), "no archive object");

    // A batch at the cap still works, so the limit is a limit and not a wall.
    let full = "{\"message\":\"fine\"}\n".repeat(nashcode::bugs::logs::MAX_BATCH);
    let answer = post_ndjson(&bed, id, &key, &full).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.json()["accepted"], nashcode::bugs::logs::MAX_BATCH);
}

#[tokio::test]
async fn many_bad_lines_produce_a_small_answer() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api", None).await;

    let bad = "nope\n".repeat(500);
    let answer = post_ndjson(&bed, id, &key, &bad).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.json()["rejected"]["count"], 500, "the count is exact");
    assert_eq!(
        answer.json()["rejected"]["lines"].as_array().expect("a list").len(),
        nashcode::bugs::logs::MAX_REPORTED_REJECTS,
        "the sample is capped"
    );
    assert!(answer.body.len() < 1024, "a small request buys a small answer");
}

#[tokio::test]
async fn a_sweep_files_each_log_line_once_however_often_it_runs() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api", None).await;

    assert_eq!(post_envelope(&bed, id, &key, fixture("sentry-logs.envelope")).await.status, 200);
    bed.bugs.digested(1).await;
    assert_eq!(bed.bugs.log_count(id).unwrap(), 2);

    // `sweep(true)` is the reindex primitive: it re-reads every stored envelope and
    // digests it again. Events have always deduped on their id; log rows did not,
    // so every pass used to double the store.
    assert_eq!(bed.bugs.sweep(true).await, 1);
    bed.bugs.digested(2).await;
    assert_eq!(bed.bugs.log_count(id).unwrap(), 2, "a re-digest files nothing new");

    assert_eq!(bed.bugs.sweep(true).await, 1);
    bed.bugs.digested(3).await;
    assert_eq!(bed.bugs.log_count(id).unwrap(), 2);

    // And the search still finds one of each, not three.
    let found = get(&bed, "/bugs/api/logs?q=cache", &[JSON]).await.json();
    assert_eq!(found["logs"].as_array().expect("logs").len(), 1);
}
