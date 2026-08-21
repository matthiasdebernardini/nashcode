//! Error tracking, end to end: a project gets a DSN, real Sentry envelopes go in
//! through the real router, and issues come out.
//!
//! The bucket is a `file://` object store in a tempdir, which is a real
//! `object_store` backend and not a stub — the same code path an S3 bucket takes.
//! The envelopes are the vendored fixtures under `fixtures/bugs/`; see the
//! `ATTRIBUTION.md` beside them.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::TestBed;
use nashcode::config::Config;
use topcoat::router::{Body, HeaderMap, Method, Router, request::Request, to_bytes};

/// A testbed with error tracking on, plus the directory its bucket lives in.
fn bugs_bed() -> (TestBed, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");

    let bucket = root.path().join("bucket");
    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: ["demo"].into_iter().collect(),
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
        pushover: None,
        public_url: "http://127.0.0.1:0".to_owned(),
        bugs_self_dsn: None,
    });
    let bed = common::testbed_from_config(root, config);
    (bed, bucket)
}

/// A testbed with no bucket, which is the shipped default.
fn off_bed() -> TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    common::stacked_fixture(&remotes, "demo");
    common::testbed_with(root, &["demo"], BTreeMap::new())
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

/// POST an envelope with header auth, the way every server-side SDK does.
async fn post_envelope(bed: &TestBed, id: i64, key: &str, body: Vec<u8>) -> Answer {
    let auth = format!("Sentry sentry_version=7, sentry_client=sentry.python/2.35.0, sentry_key={key}");
    send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[("x-sentry-auth", auth.as_str()), ("content-type", "application/x-sentry-envelope")],
        body,
    )
    .await
}

/// Create a project through the JSON API and hand back `(id, key, name)`.
async fn project(bed: &TestBed, name: &str) -> (i64, String) {
    let answer = send(
        &bed.router,
        Method::POST,
        "/bugs",
        &[("content-type", "application/json")],
        serde_json::json!({ "name": name }).to_string().into_bytes(),
    )
    .await;
    assert_eq!(answer.status, 201, "{}", answer.body);
    let created = answer.json();
    (created["id"].as_i64().expect("an id"), created["key"].as_str().expect("a key").to_owned())
}

async fn get(bed: &TestBed, path: &str, headers: &[(&str, &str)]) -> Answer {
    send(&bed.router, Method::GET, path, headers, Vec::new()).await
}

const JSON: (&str, &str) = ("accept", "application/json");

// ---- fact 17: the feature is off without a bucket ---------------------------------

#[tokio::test]
async fn with_no_bucket_the_pages_and_the_ingest_route_answer_404() {
    let bed = off_bed();
    assert!(!bed.bugs.enabled());

    for path in ["/bugs", "/bugs/anything", "/bugs/anything/issues/1"] {
        assert_eq!(get(&bed, path, &[]).await.status, 404, "{path}");
    }
    let answer = post_envelope(&bed, 1, &"a".repeat(32), fixture("python-exception.envelope")).await;
    assert_eq!(answer.status, 404);
    let answer = send(&bed.router, Method::OPTIONS, "/api/1/envelope/", &[], Vec::new()).await;
    assert_eq!(answer.status, 404);

    // The rest of the viewer is untouched by the feature being off.
    assert_eq!(get(&bed, "/demo", &[]).await.status, 200);
}

// ---- fact 1: a project shows a DSN -----------------------------------------------

#[tokio::test]
async fn a_project_created_in_the_ui_shows_a_dsn_and_an_sdk_snippet() {
    let (bed, _bucket) = bugs_bed();

    // The browser form path, which is what a person actually uses.
    let (status, location, _) =
        common::post_form(&bed.router, "/bugs", &[("name", "checkout"), ("repo", "demo")]).await;
    assert_eq!(status, 303);
    assert_eq!(location.as_deref(), Some("/bugs/checkout"));

    let page = get(&bed, "/bugs/checkout", &[]).await;
    assert_eq!(page.status, 200);
    let json = get(&bed, "/bugs/checkout", &[JSON]).await.json();
    let dsn = json["dsn"].as_str().expect("a dsn");
    let key = json["project"]["key"].as_str().expect("a key");
    let id = json["project"]["id"].as_i64().expect("an id");

    assert_eq!(dsn, format!("https://{key}@bugs.example.invalid/{id}"));
    assert_eq!(key.len(), 32);
    assert!(page.body.contains(dsn), "the page shows the DSN");
    assert!(page.body.contains("sentry_sdk.init"), "the page shows a copy-paste snippet");
    // The declared repo cross-links back to the code.
    assert!(page.body.contains("href=\"/demo\""), "the repo cross-link is there");

    // An unknown repo would render a dead link on every issue.
    let bad = send(
        &bed.router,
        Method::POST,
        "/bugs",
        &[("content-type", "application/json")],
        serde_json::json!({"name": "other", "repo": "nope"}).to_string().into_bytes(),
    )
    .await;
    assert_eq!(bad.status, 400);

    let listed = get(&bed, "/bugs", &[JSON]).await.json();
    assert_eq!(listed["projects"].as_array().expect("a list").len(), 1);
    // The notification state travels with the list: whether anything can get out is
    // part of the state of the feature.
    assert_eq!(listed["pushover"]["on"], serde_json::json!(false));
}

// ---- fact 2: the response shape ---------------------------------------------------

#[tokio::test]
async fn the_envelope_response_is_200_json_carrying_the_event_id() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let answer = post_envelope(&bed, id, &key, fixture("python-exception.envelope")).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.header("content-type"),
        Some("application/json; charset=utf-8"),
        "an empty or non-JSON body puts some SDKs in a retry loop"
    );
    assert_eq!(answer.json()["id"], "4df262ddba6f4cd6a1104f818353c7b6");
}

// ---- fact 6 (server half): the suppression header ---------------------------------

#[tokio::test]
async fn every_200_tells_the_sdk_to_stop_sending_what_we_do_not_store() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    let answer = post_envelope(&bed, id, &key, fixture("python-exception.envelope")).await;

    let limits = answer.header("x-sentry-rate-limits").expect("the header rides every 200");
    assert_eq!(
        limits,
        "86400:transaction;span;profile;profile_chunk;replay;trace_metric:project"
    );
    // The categories we *want* must never appear, and the list must never be empty:
    // an empty category list means "everything", which would silence errors too.
    for wanted in ["error", "default", "log_item", "monitor", "session"] {
        let categories = limits.split(':').nth(1).expect("a category list");
        assert!(
            !categories.split(';').any(|category| category == wanted),
            "{wanted} must not be rate limited"
        );
    }
}

// ---- fact 3: unknown item types ---------------------------------------------------

#[tokio::test]
async fn an_unknown_item_type_ingests_and_the_event_beside_it_is_processed() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let answer = post_envelope(&bed, id, &key, fixture("unknown-item.envelope")).await;
    assert_eq!(answer.status, 200, "an unfamiliar item type must never fail the envelope");
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 1, "the event behind the unknown items was still processed");
    assert!(
        issues[0]["title"].as_str().expect("a title").starts_with("RuntimeError:"),
        "{:?}",
        issues[0]["title"]
    );
}

// ---- fact 4: compression and the size caps ----------------------------------------

#[tokio::test]
async fn a_gzip_body_ingests_the_same_as_a_plain_one() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let auth = format!("Sentry sentry_version=7, sentry_key={key}");
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[("x-sentry-auth", auth.as_str()), ("content-encoding", "gzip")],
        fixture("python-exception.envelope.gz"),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.json()["id"], "4df262ddba6f4cd6a1104f818353c7b6");
    bed.bugs.digested(1).await;
    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    assert_eq!(issues["issues"].as_array().expect("issues").len(), 1);
}

#[tokio::test]
async fn a_body_over_the_cap_is_413_and_is_never_buffered_whole() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // Over the 20 MiB compressed cap.
    let huge = vec![b'x'; 21 * 1024 * 1024];
    let answer = post_envelope(&bed, id, &key, huge).await;
    assert_eq!(answer.status, 413);
    assert!(answer.header("access-control-allow-origin").is_some(), "a browser must see it");

    // Over the 1 MiB per-item cap, declared in the item header, so the reader knows
    // before it reads the payload.
    let mut oversized = Vec::from(&b"{}\n"[..]);
    oversized.extend_from_slice(
        format!("{{\"type\":\"event\",\"length\":{}}}\n", 2 * 1024 * 1024).as_bytes(),
    );
    oversized.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
    oversized.push(b'\n');
    assert_eq!(post_envelope(&bed, id, &key, oversized).await.status, 413);

    // Nothing landed.
    assert!(get(&bed, "/bugs/api", &[JSON]).await.json()["issues"]
        .as_array()
        .expect("issues")
        .is_empty());
}

// ---- fact 5: auth -----------------------------------------------------------------

#[tokio::test]
async fn a_wrong_key_is_403_and_an_unknown_project_is_404() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    let envelope = fixture("python-exception.envelope");

    let wrong = post_envelope(&bed, id, &"f".repeat(32), envelope.clone()).await;
    assert_eq!(wrong.status, 403);
    assert_eq!(wrong.header("x-sentry-error"), Some("wrong key for this project"));

    assert_eq!(post_envelope(&bed, id + 999, &key, envelope.clone()).await.status, 404);
    assert_eq!(post_envelope(&bed, 0, &key, envelope.clone()).await.status, 404);

    // No auth anywhere at all — not in a header, not in the query, not in the
    // envelope's own `dsn` header.
    let none = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[],
        envelope.clone(),
    )
    .await;
    assert_eq!(none.status, 403);

    // A `dsn` in the envelope header is the third accepted source.
    let self_authed = format!(
        "{{\"event_id\":\"{}\",\"dsn\":\"https://{key}@bugs.example.invalid/{id}\"}}\n\
         {{\"type\":\"event\"}}\n{{\"event_id\":\"{}\",\"platform\":\"python\",\
         \"exception\":{{\"values\":[{{\"type\":\"E\",\"value\":\"v\"}}]}}}}\n",
        "b".repeat(32),
        "b".repeat(32),
    );
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[],
        self_authed.into_bytes(),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
}

#[tokio::test]
async fn an_unauthenticated_body_is_refused_before_it_is_read_or_expanded() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // No header, no query: the only key left is the envelope's own `dsn`, which is
    // the first line. A body of 25 MiB — past the 20 MiB compressed cap — with no
    // `dsn` there must come back 403, not 413: a 413 would mean the cap was reached,
    // which would mean the whole body had been pulled before anyone checked it.
    let mut huge = Vec::from(&b"{\"event_id\":\"aaaa\"}\n"[..]);
    huge.extend(std::iter::repeat_n(b'x', 25 * 1024 * 1024));
    let answer =
        send(&bed.router, Method::POST, &format!("/api/{id}/envelope/"), &[], huge).await;
    assert_eq!(answer.status, 403, "auth comes before the size cap");
    assert_eq!(answer.header("x-sentry-error"), Some("no sentry_key"));

    // A compressed body cannot be read without expanding it, so that door is narrow:
    // 64 KiB compressed. This one is small compressed and enormous expanded — the
    // classic bomb — and it is refused on the compressed cap, so it is never expanded.
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &vec![b'x'; 200 * 1024 * 1024]).expect("gzip");
    let bomb = encoder.finish().expect("gzip");
    assert!(bomb.len() > 64 * 1024, "the point is that it is over the unauthed cap");
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[("content-encoding", "gzip")],
        bomb.clone(),
    )
    .await;
    assert_eq!(answer.status, 413, "an unauthenticated compressed body gets a small budget");

    // With a key in the header the same body is judged on the real caps instead, and
    // this one is a bomb, so it stops at the decompressed cap rather than the small one.
    let auth = format!("Sentry sentry_version=7, sentry_key={key}");
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[("x-sentry-auth", auth.as_str()), ("content-encoding", "gzip")],
        bomb,
    )
    .await;
    assert_eq!(answer.status, 413);
    assert_eq!(answer.header("x-sentry-error"), Some("the decompressed body"));

    // And a small compressed envelope authenticated only by its own `dsn` still works.
    let event = serde_json::json!({
        "event_id": "e".repeat(32),
        "platform": "python",
        "exception": {"values": [{"type": "E", "value": "v"}]},
    });
    let plain = format!(
        "{{\"event_id\":\"{}\",\"dsn\":\"https://{key}@bugs.example.invalid/{id}\"}}\n\
         {{\"type\":\"event\"}}\n{event}\n",
        "e".repeat(32),
    );
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, plain.as_bytes()).expect("gzip");
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/"),
        &[("content-encoding", "gzip")],
        encoder.finish().expect("gzip"),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
}

#[tokio::test]
async fn the_startup_sweep_re_digests_an_envelope_the_digest_never_finished() {
    let (bed, _bucket) = bugs_bed();
    let (id, _key) = project(&bed, "api").await;

    // What a kill -9 between the two writes leaves behind: the object is in the
    // bucket, the row is recorded, and `digested_at` was never stamped. Without the
    // sweep nothing would ever look at it again.
    bed.bugs.store(id, fixture("python-exception.envelope")).await.expect("stored");
    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    assert!(issues["issues"].as_array().expect("issues").is_empty(), "nothing digested yet");

    // What `main` does on the way up.
    assert_eq!(bed.bugs.sweep(false).await, 1);
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 1, "the sweep re-read the object and indexed it");
    assert!(issues[0]["title"].as_str().expect("a title").starts_with("RuntimeError:"));

    // A second sweep has nothing left to do: the row is stamped now.
    assert_eq!(bed.bugs.sweep(false).await, 0);

    // `all` takes every envelope, stamped or not — the primitive a reindex needs.
    // Re-digesting the same event twice is one occurrence, not two.
    assert_eq!(bed.bugs.sweep(true).await, 1);
    bed.bugs.digested(2).await;
    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    assert_eq!(issues["issues"][0]["events"], 1);
}

// ---- fact 7: browser CORS ---------------------------------------------------------

#[tokio::test]
async fn a_preflight_returns_relays_header_set_and_query_auth_works() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "web").await;

    let preflight = send(
        &bed.router,
        Method::OPTIONS,
        &format!("/api/{id}/envelope/"),
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(preflight.status, 200);
    assert_eq!(preflight.header("access-control-allow-origin"), Some("*"));
    assert_eq!(preflight.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(preflight.header("access-control-max-age"), Some("3600"));

    let allowed: Vec<&str> = preflight
        .header("access-control-allow-headers")
        .expect("the allow list")
        .split(',')
        .map(str::trim)
        .collect();
    assert_eq!(allowed.len(), 11, "Relay's list is eleven headers: {allowed:?}");
    for header in [
        "x-sentry-auth",
        "x-requested-with",
        "x-forwarded-for",
        "origin",
        "referer",
        "accept",
        "content-type",
        "authentication",
        "authorization",
        "content-encoding",
        "transfer-encoding",
    ] {
        assert!(allowed.contains(&header), "{header} is missing from {allowed:?}");
    }

    // The browser SDK's real request: cross-site, no custom headers, auth in the
    // query string, `text/plain` body. It must not be treated as a forgery.
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/?sentry_version=7&sentry_key={key}&sentry_client=sentry.javascript.browser%2F9.41.0"),
        &[
            ("origin", "https://app.example.com"),
            ("sec-fetch-site", "cross-site"),
            ("content-type", "text/plain;charset=UTF-8"),
        ],
        fixture("python-exception.envelope"),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.header("access-control-allow-origin"), Some("*"));

    // Backoff breaks silently unless the SDK can read these three off the response.
    let exposed: Vec<&str> = answer
        .header("access-control-expose-headers")
        .expect("the expose list")
        .split(',')
        .map(str::trim)
        .collect();
    for header in ["x-sentry-error", "x-sentry-rate-limits", "retry-after"] {
        assert!(exposed.contains(&header), "{header} is not exposed");
    }
}

// ---- fact 8: the storage split ----------------------------------------------------

#[tokio::test]
async fn the_payload_lives_in_the_bucket_and_the_row_only_points_at_it() {
    let (bed, bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    post_envelope(&bed, id, &key, fixture("python-exception.envelope")).await;
    bed.bugs.digested(1).await;

    // The raw envelope and the event payload are both objects on disk.
    let objects = walk(&bucket);
    assert!(
        objects.iter().any(|path| path.contains("/envelopes/")),
        "the raw envelope is kept: {objects:?}"
    );
    let event_object = objects
        .iter()
        .find(|path| path.contains("/events/"))
        .expect("the event payload is an object");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(event_object).expect("read")).expect("json");
    assert_eq!(stored["event_id"], "4df262ddba6f4cd6a1104f818353c7b6");
    assert!(stored["breadcrumbs"].is_object() || stored["breadcrumbs"].is_array());

    // The SQLite row holds index fields and a pointer, no payload.
    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue_id = issues["issues"][0]["id"].as_i64().expect("an issue id");
    let detail = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(detail["event"]["event_id"], "4df262ddba6f4cd6a1104f818353c7b6");
    assert!(detail["event"]["object_key"].as_str().expect("a key").contains("/events/"));
    assert!(detail["event"].get("payload").is_none(), "the row carries no payload");

    // The detail page renders from the bucket object, not from the row.
    let page = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[]).await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("capture_exception add_full_stack"), "the exception value");
    assert!(page.body.contains("probe_capture_exception.py"), "a stack frame");
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

// ---- fact 10: grouping ------------------------------------------------------------

fn uuid_envelope(event_id: &str, session: &str) -> Vec<u8> {
    let event = serde_json::json!({
        "event_id": event_id,
        "timestamp": "2026-08-19T04:05:06Z",
        "platform": "python",
        "exception": {"values": [{
            "type": "KeyError",
            "value": format!("no session {session}"),
        }]},
    })
    .to_string();
    format!("{{\"event_id\":\"{event_id}\"}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes()
}

#[tokio::test]
async fn two_events_differing_only_by_a_uuid_are_one_issue() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    post_envelope(&bed, id, &key, uuid_envelope(&"a".repeat(32), "3f0e2c1a-9b7d-4f21-a8c3-5d6e7f809a1b")).await;
    post_envelope(&bed, id, &key, uuid_envelope(&"b".repeat(32), "91a2b3c4-d5e6-4718-9a0b-1c2d3e4f5061")).await;
    bed.bugs.digested(2).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 1, "the uuid must not split the issue");
    assert_eq!(issues[0]["events"], 2);
    assert_eq!(issues[0]["grouping_key"], "KeyError: no session <uuid>");
    assert_eq!(issues[0]["mechanism"], "nashcode-v1");
}

#[tokio::test]
async fn an_explicit_fingerprint_decides_the_grouping() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // The fixture's own fingerprint, and a numeric timestamp beside it.
    post_envelope(&bed, id, &key, fixture("custom-fingerprint.envelope")).await;
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue = &issues["issues"][0];
    assert_eq!(
        issue["grouping_key"],
        "celery ⋄ SoftTimeLimitExceeded ⋄ sentry.tasks.store.process_event"
    );

    // A second event with a different exception but the same fingerprint joins it.
    let same = serde_json::json!({
        "event_id": "c".repeat(32),
        "timestamp": 1726558446.0,
        "platform": "python",
        "fingerprint": ["celery", "SoftTimeLimitExceeded", "sentry.tasks.store.process_event"],
        "exception": {"values": [{"type": "ValueError", "value": "something else"}]},
    })
    .to_string();
    let body = format!("{{}}\n{{\"type\":\"event\"}}\n{same}\n").into_bytes();
    post_envelope(&bed, id, &key, body).await;
    bed.bugs.digested(2).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    assert_eq!(issues["issues"].as_array().expect("issues").len(), 1);
    assert_eq!(issues["issues"][0]["events"], 2);
}

#[tokio::test]
async fn the_default_sentinel_extends_the_computed_key() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let event = |tenant: &str, event_id: &str| {
        let payload = serde_json::json!({
            "event_id": event_id,
            "timestamp": "2026-08-19T04:05:06Z",
            "platform": "python",
            "fingerprint": ["{{ default }}", tenant],
            "exception": {"values": [{"type": "KeyError", "value": "boom"}]},
        })
        .to_string();
        format!("{{}}\n{{\"type\":\"event\"}}\n{payload}\n").into_bytes()
    };

    post_envelope(&bed, id, &key, event("tenant-7", &"a".repeat(32))).await;
    post_envelope(&bed, id, &key, event("tenant-7", &"b".repeat(32))).await;
    post_envelope(&bed, id, &key, event("tenant-8", &"c".repeat(32))).await;
    bed.bugs.digested(3).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 2, "the extension splits the two tenants");
    for issue in issues {
        assert!(
            issue["grouping_key"].as_str().expect("a key").starts_with("KeyError: boom ⋄ tenant-"),
            "{issue:?}"
        );
    }
}

// ---- fact 11 (without the push): regressions --------------------------------------

#[tokio::test]
async fn resolving_an_issue_and_sending_it_again_reopens_it_as_a_regression() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    let boom = |event_id: &str| uuid_envelope(event_id, "3f0e2c1a-9b7d-4f21-a8c3-5d6e7f809a1b");

    post_envelope(&bed, id, &key, boom(&"a".repeat(32))).await;
    bed.bugs.digested(1).await;
    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue_id = issues["issues"][0]["id"].as_i64().expect("an issue id");
    assert_eq!(issues["issues"][0]["state"], "unresolved");
    assert_eq!(issues["issues"][0]["regression"], false);

    // Resolve it from the UI, which stamps the Tailscale identity.
    let (status, location, _) = common::post_form_from(
        &bed.router,
        &format!("/bugs/api/issues/{issue_id}/state"),
        &[("state", "resolved")],
        &[
            ("tailscale-user-login", "matthias@example.invalid"),
            ("tailscale-user-name", "Matthias"),
        ],
    )
    .await;
    assert_eq!(status, 303);
    assert_eq!(location.as_deref(), Some(&format!("/bugs/api/issues/{issue_id}")[..]));

    let resolved = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(resolved["issue"]["state"], "resolved");
    assert_eq!(resolved["issue"]["actor"], "matthias@example.invalid");

    // The same error again reopens it, flagged.
    post_envelope(&bed, id, &key, boom(&"b".repeat(32))).await;
    bed.bugs.digested(2).await;
    let reopened = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(reopened["issue"]["state"], "unresolved");
    assert_eq!(reopened["issue"]["regression"], true);
    assert_eq!(reopened["issue"]["events"], 2);

    // A second identical event changes nothing but the count.
    post_envelope(&bed, id, &key, boom(&"c".repeat(32))).await;
    bed.bugs.digested(3).await;
    let again = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(again["issue"]["state"], "unresolved");
    assert_eq!(again["issue"]["events"], 3);

    // And the state filter agrees with all of it.
    let listed = get(&bed, "/bugs/api?state=resolved", &[JSON]).await.json();
    assert!(listed["issues"].as_array().expect("issues").is_empty());
    let listed = get(&bed, "/bugs/api?state=unresolved", &[JSON]).await.json();
    assert_eq!(listed["issues"].as_array().expect("issues").len(), 1);
    assert_eq!(get(&bed, "/bugs/api?state=nonsense", &[JSON]).await.status, 400);
}

// ---- the same event twice is one occurrence --------------------------------------

#[tokio::test]
async fn a_retried_envelope_does_not_double_count() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    let envelope = fixture("python-exception.envelope");

    post_envelope(&bed, id, &key, envelope.clone()).await;
    post_envelope(&bed, id, &key, envelope).await;
    bed.bugs.digested(2).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    assert_eq!(issues["issues"].as_array().expect("issues").len(), 1);
    assert_eq!(issues["issues"][0]["events"], 1, "the same event id is one occurrence");
}

// ---- a message event with no exception -------------------------------------------

#[tokio::test]
async fn an_event_with_only_a_log_message_still_becomes_an_issue() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    post_envelope(&bed, id, &key, fixture("log-message.envelope")).await;
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue = &issues["issues"][0];
    assert_eq!(
        issue["grouping_key"],
        "Log Message: [API] Fout tijdens verbinden: <quoted>"
    );
    assert_eq!(issue["level"], "error");
}

// ---- code origin: in-app frames link into the code browser ------------------------

#[tokio::test]
async fn in_app_stack_frames_link_into_the_declared_repo() {
    let (bed, _bucket) = bugs_bed();
    let created = send(
        &bed.router,
        Method::POST,
        "/bugs",
        &[("content-type", "application/json")],
        serde_json::json!({"name": "api", "repo": "demo"}).to_string().into_bytes(),
    )
    .await;
    let created = created.json();
    let (id, key) =
        (created["id"].as_i64().expect("an id"), created["key"].as_str().expect("a key"));

    // The mirror has to exist before a path can be resolved against it.
    bed.mirrors.refresh_now("demo").await;

    // A frame naming a file the repo really has, and one naming a file it does not.
    // This is the whole contract: the link is only offered when it would work.
    let event = serde_json::json!({
        "event_id": "e".repeat(32),
        "platform": "python",
        "exception": {"values": [{
            "type": "RuntimeError",
            "value": "boom",
            "stacktrace": {"frames": [
                {"filename": "probe_capture_exception.py", "lineno": 24, "in_app": true},
                {"filename": "src/app.txt", "lineno": 1, "in_app": true},
            ]},
        }]},
    })
    .to_string();
    let body = format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes();
    post_envelope(&bed, id, key, body).await;
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue_id = issues["issues"][0]["id"].as_i64().expect("an issue id");
    let page = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[]).await;
    assert!(
        page.body.contains("href=\"/demo/blob/src/app.txt#L1\""),
        "a frame whose file the repo has links to the line"
    );
    assert!(page.body.contains("probe_capture_exception.py:24"), "the other frame is shown");
    assert!(
        !page.body.contains("/demo/blob/probe_capture_exception.py"),
        "and is not linked: the repo has no such file, so the link would 404"
    );

    // A project with no repo has nowhere to link to, so the frame is plain text.
    let (other_id, other_key) = project(&bed, "plain").await;
    post_envelope(&bed, other_id, &other_key, fixture("python-exception.envelope")).await;
    bed.bugs.digested(2).await;
    let issues = get(&bed, "/bugs/plain", &[JSON]).await.json();
    let issue_id = issues["issues"][0]["id"].as_i64().expect("an issue id");
    let page = get(&bed, &format!("/bugs/plain/issues/{issue_id}"), &[]).await;
    assert!(page.body.contains("probe_capture_exception.py:24"), "still shown");
    assert!(!page.body.contains("/blob/probe_capture_exception.py"), "but never linked");
}

// ---- review follow-ups ------------------------------------------------------------

#[tokio::test]
async fn a_bare_array_exception_renders_the_same_as_the_wrapped_form() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // The undocumented but real shape: `exception` as a bare array. Grouping has
    // always taken it; the detail page read `exception.values` only, so the issue
    // appeared with the right title and rendered with no exception and no stack.
    let event = serde_json::json!({
        "event_id": "d".repeat(32),
        "timestamp": "2026-08-19T04:05:06Z",
        "platform": "python",
        "exception": [{
            "type": "ConnectionError",
            "value": "the socket went away",
            "stacktrace": {"frames": [{
                "filename": "src/app.txt",
                "lineno": 12,
                "function": "connect",
                "in_app": true,
            }]},
        }],
    })
    .to_string();
    let body = format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes();
    post_envelope(&bed, id, &key, body).await;
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue_id = issues["issues"][0]["id"].as_i64().expect("an issue id");
    assert_eq!(issues["issues"][0]["grouping_key"], "ConnectionError: the socket went away");

    let page = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[]).await;
    assert!(page.body.contains("the socket went away"), "the exception value renders");
    assert!(page.body.contains("src/app.txt:12"), "and so does its stack");
}

#[tokio::test]
async fn an_envelope_with_no_event_id_still_gets_one_back() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // A session-only envelope: legal, carries no event, and an SDK still reads the
    // response body.
    let body = b"{}\n{\"type\":\"session\"}\n{\"sid\":\"9d1d\",\"status\":\"ok\"}\n".to_vec();
    let answer = post_envelope(&bed, id, &key, body).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let minted = answer.json()["id"].as_str().expect("an id, never a bare {}").to_owned();
    assert_eq!(minted.len(), 32);
    assert!(minted.chars().all(|c| c.is_ascii_hexdigit()));

    // Nothing to group, so no issue.
    bed.bugs.digested(1).await;
    assert!(get(&bed, "/bugs/api", &[JSON]).await.json()["issues"]
        .as_array()
        .expect("issues")
        .is_empty());
}

#[tokio::test]
async fn the_default_sentinel_works_without_spaces_too() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let event = |sentinel: &str, value: &str, event_id: &str| {
        let payload = serde_json::json!({
            "event_id": event_id,
            "timestamp": "2026-08-19T04:05:06Z",
            "platform": "python",
            "fingerprint": [sentinel, "tenant-7"],
            "exception": {"values": [{"type": "KeyError", "value": value}]},
        })
        .to_string();
        format!("{{}}\n{{\"type\":\"event\"}}\n{payload}\n").into_bytes()
    };

    // Two different exceptions under the tight spelling. If `{{default}}` were taken
    // as a literal they would collapse into one issue — a silent over-merge.
    post_envelope(&bed, id, &key, event("{{default}}", "one", &"a".repeat(32))).await;
    post_envelope(&bed, id, &key, event("{{default}}", "two", &"b".repeat(32))).await;
    // And the spaced spelling reaches the same key as the tight one.
    post_envelope(&bed, id, &key, event("{{ default }}", "one", &"c".repeat(32))).await;
    bed.bugs.digested(3).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 2, "the two exceptions stay apart: {issues:?}");
    let one = issues.iter().find(|issue| issue["grouping_key"] == "KeyError: one ⋄ tenant-7");
    assert_eq!(one.expect("the shared key")["events"], 2, "both spellings are one issue");
}

#[tokio::test]
async fn a_long_number_in_a_message_does_not_open_an_issue_per_value() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let event = |epoch: u64, event_id: &str| {
        let payload = serde_json::json!({
            "event_id": event_id,
            "timestamp": "2026-08-19T04:05:06Z",
            "platform": "python",
            "exception": {"values": [{
                "type": "TimeoutError",
                "value": format!("no answer since {epoch}"),
            }]},
        })
        .to_string();
        format!("{{}}\n{{\"type\":\"event\"}}\n{payload}\n").into_bytes()
    };

    post_envelope(&bed, id, &key, event(1755561600, &"a".repeat(32))).await;
    post_envelope(&bed, id, &key, event(1755561999, &"b".repeat(32))).await;
    bed.bugs.digested(2).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 1, "a seven-digit number must not split the issue");
    assert_eq!(issues[0]["grouping_key"], "TimeoutError: no answer since <int>");
    assert_eq!(issues[0]["events"], 2);
}

// ---- quotas: a project's share of the pipeline ------------------------------------
//
// Backpressure and a quota share a status code and mean different things. The digest
// queue's 429 says "this box is behind"; a quota's says "this project has had its
// turn". The tests below are about the second one, and the property that matters most
// is *when* it fires: before the body is read, or it has saved nothing.

/// One envelope whose exception type decides its issue. Type, never a number in the
/// value: grouping parameterizes integers out on purpose, so `boom 1` … `boom 25` is
/// one issue and a test that counted on twenty-five would be testing nothing.
fn typed_envelope(event_id: &str, ty: &str) -> Vec<u8> {
    let event = serde_json::json!({
        "event_id": event_id,
        "timestamp": "2026-08-19T04:05:06Z",
        "platform": "python",
        "exception": {"values": [{"type": ty, "value": "boom"}]},
    })
    .to_string();
    format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes()
}

/// The tightest window's allowance, which is the one every test here fills.
fn tightest_quota() -> i64 {
    nashcode::bugs::quota::WINDOWS[0].2
}

/// Spend a project's whole five-minute allowance without making the requests. The gate
/// under test is the gate, not the thousand file writes it would take to reach it.
fn spend_quota(bed: &TestBed, project_id: i64) {
    for _ in 0..tightest_quota() {
        bed.bugs.count_request(project_id);
    }
}

#[tokio::test]
async fn a_project_over_its_quota_is_refused_before_its_body_is_read() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    spend_quota(&bed, id);

    // A body that could never be accepted on its own terms: it is not an envelope at
    // all. Getting 429 rather than 400 is the proof that nothing parsed it — the gate
    // ran on the project alone, which is the whole point of putting it there.
    let answer = post_envelope(&bed, id, &key, b"this is not an envelope".to_vec()).await;
    assert_eq!(answer.status, 429, "{}", answer.body);
    assert_eq!(answer.header("x-sentry-error").map(|e| e.contains("quota")), Some(true));

    // And an SDK can obey it: a delay to wait, and the rate-limit header that tells it
    // to stop sending every category rather than retry into the same refusal.
    let retry: i64 =
        answer.header("retry-after").expect("a delay").parse().expect("whole seconds");
    assert!(retry >= 1 && retry <= nashcode::bugs::quota::WINDOWS[0].1, "{retry}");
    let limits = answer.header("x-sentry-rate-limits").expect("a rate-limit header");
    assert_eq!(limits, format!("{retry}::project"), "empty categories means all of them");
    assert_eq!(answer.header("access-control-allow-origin"), Some("*"), "a browser must see it");
}

#[tokio::test]
async fn the_log_door_is_held_to_the_same_quota_as_the_envelope_door() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // Both doors count against one budget, because what the quota bounds is requests
    // through the front of the pipeline and not which shape they arrived in.
    let line = "{\"level\":\"error\",\"message\":\"boom\"}\n";
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/logs?sentry_key={key}"),
        &[("content-type", "application/x-ndjson")],
        line.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(bed.bugs.quota_state(id).expect("quota")[0].used, 1, "the log door counted");

    spend_quota(&bed, id);
    let answer = send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/logs?sentry_key={key}"),
        &[("content-type", "application/x-ndjson")],
        line.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(answer.status, 429, "{}", answer.body);
    assert!(answer.header("retry-after").is_some());
}

#[tokio::test]
async fn one_project_over_its_quota_costs_every_other_project_nothing() {
    let (bed, _bucket) = bugs_bed();
    let (loud, loud_key) = project(&bed, "loud").await;
    let (quiet, quiet_key) = project(&bed, "quiet").await;
    spend_quota(&bed, loud);

    assert_eq!(post_envelope(&bed, loud, &loud_key, typed_envelope(&"a".repeat(32), "A")).await.status, 429);
    let answer = post_envelope(&bed, quiet, &quiet_key, typed_envelope(&"b".repeat(32), "B")).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(bed.bugs.quota_state(quiet).expect("quota")[0].used, 1);
}

#[tokio::test]
async fn nothing_that_was_refused_ever_spends_a_projects_budget() {
    let (bed, _bucket) = bugs_bed();
    let (id, _key) = project(&bed, "api").await;

    // A wrong key, then an unknown project. Neither is an accepted request, so neither
    // may cost the project anything — otherwise anybody who knows a numeric id can
    // spend somebody else's month.
    let wrong = post_envelope(&bed, id, &"f".repeat(32), typed_envelope(&"a".repeat(32), "A")).await;
    assert_eq!(wrong.status, 403);
    let missing = post_envelope(&bed, 9999, &"f".repeat(32), typed_envelope(&"b".repeat(32), "B")).await;
    assert_eq!(missing.status, 404, "an unknown project is a verdict, and a 4xx is final");

    assert_eq!(bed.bugs.quota_state(id).expect("quota")[0].used, 0);
}

#[tokio::test]
async fn an_unknown_project_stays_a_404_with_the_quota_gate_in_place() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    spend_quota(&bed, id);

    // The gate sits after the project lookup, so a project that is not there is still
    // answered by the lookup and not by the quota. An SDK reads 404 as "this DSN is
    // wrong" and 429 as "come back", and confusing the two either wedges a sender or
    // loses its events.
    let answer = post_envelope(&bed, 9999, &key, typed_envelope(&"a".repeat(32), "A")).await;
    assert_eq!(answer.status, 404, "{}", answer.body);
    assert_eq!(answer.header("x-sentry-error"), Some("unknown project"));
}

// ---- eviction: what goes when a project is full -----------------------------------

/// The event objects a project has in the bucket.
fn event_objects(bucket: &Path, project_id: i64) -> Vec<String> {
    let dir = bucket.join(format!("projects/{project_id}/events"));
    let mut names: Vec<String> = walk(&dir)
        .into_iter()
        .filter_map(|path| Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn eviction_holds_the_cap_and_never_takes_the_evidence() {
    let (bed, bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    // A noisy issue and a quiet one. The types differ, not a number in the value.
    let mut sent = 0;
    for n in 0..12 {
        post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 100 + n), "NoisyError")).await;
        sent += 1;
    }
    for n in 0..3 {
        post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 200 + n), "QuietError")).await;
        sent += 1;
    }
    bed.bugs.digested(sent).await;

    // Resolve the noisy issue and break it again, so it carries a regression trigger —
    // the second of the two kinds of event eviction is never allowed to take.
    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let noisy = issues["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .find(|issue| issue["title"].as_str().is_some_and(|title| title.contains("NoisyError")))
        .expect("the noisy issue")["id"]
        .as_i64()
        .expect("an id");
    common::post_form(&bed.router, &format!("/bugs/api/issues/{noisy}/state"), &[("state", "resolved")]).await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 300), "NoisyError")).await;
    sent += 1;
    bed.bugs.digested(sent).await;

    let before = bed.bugs.events(noisy, 100).expect("events");
    let kept: Vec<String> =
        before.iter().filter(|event| event.keep).map(|event| event.event_id.clone()).collect();
    assert_eq!(kept.len(), 2, "the first event and the regression trigger");
    assert_eq!(event_objects(&bucket, id).len(), sent as usize, "one object per event");

    // Now hold it to five.
    let evicted = bed.bugs.evict_to(5).await;
    assert!(evicted > 0, "sixteen events do not fit in five");

    // Rows and objects went together. An object with no row would be invisible and a
    // row with no object would be a dead link on the issue page; neither is allowed.
    let remaining_objects = event_objects(&bucket, id);
    let noisy_rows = bed.bugs.events(noisy, 100).expect("events");
    let quiet = issues["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .find(|issue| issue["title"].as_str().is_some_and(|title| title.contains("QuietError")))
        .expect("the quiet issue")["id"]
        .as_i64()
        .expect("an id");
    let quiet_rows = bed.bugs.events(quiet, 100).expect("events");
    assert_eq!(
        remaining_objects.len(),
        noisy_rows.len() + quiet_rows.len(),
        "every surviving row still has its object, and nothing survived without one"
    );

    // The two protected events are still there, and so is the quiet issue's first one.
    let survivors: Vec<String> =
        noisy_rows.iter().map(|event| event.event_id.clone()).collect();
    for protected in &kept {
        assert!(survivors.contains(protected), "eviction took {protected}, which it may never");
    }
    assert!(!quiet_rows.is_empty(), "a quiet issue must not be emptied by a noisy neighbour");
    assert!(
        quiet_rows.iter().any(|event| event.keep),
        "the quiet issue keeps its first-seen event"
    );
}

#[tokio::test]
async fn a_project_under_its_cap_is_left_alone() {
    let (bed, bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    for n in 0..3 {
        post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 400 + n), "SmallError")).await;
    }
    bed.bugs.digested(3).await;

    assert_eq!(bed.bugs.evict_to(10).await, 0, "under the cap is nothing to do");
    assert_eq!(event_objects(&bucket, id).len(), 3);
}

// ---- mute rules -------------------------------------------------------------------

/// Mute an issue with a rule, through the form the issue page posts.
async fn mute(bed: &TestBed, issue_id: i64, rule: &str) {
    let (status, _, body) = common::post_form(
        &bed.router,
        &format!("/bugs/api/issues/{issue_id}/state"),
        &[("state", "muted"), ("rule", rule)],
    )
    .await;
    assert_eq!(status, 303, "{body}");
}

/// The one issue of a project that has exactly one.
async fn only_issue(bed: &TestBed) -> i64 {
    let issues = get(bed, "/bugs/api", &[JSON]).await.json();
    let issues = issues["issues"].as_array().expect("issues").clone();
    assert_eq!(issues.len(), 1, "{issues:?}");
    issues[0]["id"].as_i64().expect("an id")
}

async fn state_of(bed: &TestBed, issue_id: i64) -> String {
    let issue = get(bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    issue["issue"]["state"].as_str().expect("a state").to_owned()
}

#[tokio::test]
async fn muting_for_ever_is_still_there_and_still_means_for_ever() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 1), "MutedError")).await;
    bed.bugs.digested(1).await;
    let issue_id = only_issue(&bed).await;
    mute(&bed, issue_id, "forever").await;

    for n in 2..8 {
        post_envelope(&bed, id, &key, typed_envelope(&format!("{n:032x}"), "MutedError")).await;
    }
    bed.bugs.digested(7).await;
    assert_eq!(state_of(&bed, issue_id).await, "muted");

    let shown = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(shown["mute"]["kind"], "forever");
}

#[tokio::test]
async fn a_mute_for_a_duration_lifts_itself_when_the_next_event_arrives_after_it() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 1), "TimedError")).await;
    bed.bugs.digested(1).await;
    let issue_id = only_issue(&bed).await;

    // One second, so the deadline is a real deadline and the test is not a sleep.
    mute(&bed, issue_id, "for:1").await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 2), "TimedError")).await;
    bed.bugs.digested(2).await;
    assert_eq!(state_of(&bed, issue_id).await, "muted", "inside the hour it stays quiet");

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 3), "TimedError")).await;
    bed.bugs.digested(3).await;
    assert_eq!(state_of(&bed, issue_id).await, "unresolved", "the deadline passed");

    // The rule is spent. Nothing re-arms it, so the issue is an ordinary open issue.
    let shown = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert!(shown["mute"].is_null(), "{shown:?}");
}

#[tokio::test]
async fn a_mute_until_lifts_on_the_nth_event_inside_the_window_and_not_before() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 1), "LoudError")).await;
    bed.bugs.digested(1).await;
    let issue_id = only_issue(&bed).await;
    mute(&bed, issue_id, "until:3:3600").await;

    // Two is not three.
    for n in 2..=3 {
        post_envelope(&bed, id, &key, typed_envelope(&format!("{n:032x}"), "LoudError")).await;
    }
    bed.bugs.digested(3).await;
    assert_eq!(state_of(&bed, issue_id).await, "muted");
    let shown = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(shown["mute"]["seen"], 2);
    assert_eq!(shown["mute"]["needed"], 3);

    // The third inside the window is the one that speaks up.
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 4), "LoudError")).await;
    bed.bugs.digested(4).await;
    assert_eq!(state_of(&bed, issue_id).await, "unresolved");
}

#[tokio::test]
async fn a_mute_rule_that_will_not_parse_is_refused_rather_than_downgraded() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 1), "PickyError")).await;
    bed.bugs.digested(1).await;
    let issue_id = only_issue(&bed).await;

    // Somebody who asked for an hour and silently got "for ever" would not find out
    // until the outage they missed.
    let (status, _, _) = common::post_form(
        &bed.router,
        &format!("/bugs/api/issues/{issue_id}/state"),
        &[("state", "muted"), ("rule", "for:soon")],
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(state_of(&bed, issue_id).await, "unresolved", "and nothing moved");
}

#[tokio::test]
async fn a_reindex_never_resurrects_an_evicted_event_or_moves_the_counter() {
    let (bed, bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;

    let mut sent = 0;
    for n in 0..10 {
        post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 500 + n), "GhostError")).await;
        sent += 1;
    }
    bed.bugs.digested(sent).await;

    let issue_id = only_issue(&bed).await;
    let before = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    let counter_before = before["issue"]["events"].as_i64().expect("a count");
    assert_eq!(counter_before, 10);

    let evicted = bed.bugs.evict_to(4).await;
    assert!(evicted > 0, "ten events do not fit in four");
    let rows_after_eviction = bed.bugs.events(issue_id, 100).expect("events").len();
    let objects_after_eviction = event_objects(&bucket, id).len();
    assert_eq!(rows_after_eviction, objects_after_eviction, "rows and objects went together");

    // The reindex primitive re-reads every stored *envelope* — and the envelopes still
    // hold the payloads of the events that were just evicted. Without a tombstone each
    // one lands as an ordinary repeat: the row comes back and the issue's lifetime
    // counter moves a second time. That counter only ever goes up and the escalation
    // ladder reads it, so the inflation would be permanent.
    let queued = bed.bugs.sweep(true).await;
    assert!(queued > 0, "there are envelopes to re-read");
    bed.bugs.digested(sent + queued as u64).await;

    assert_eq!(
        bed.bugs.events(issue_id, 100).expect("events").len(),
        rows_after_eviction,
        "a reindex put an evicted event back"
    );
    assert_eq!(
        event_objects(&bucket, id).len(),
        objects_after_eviction,
        "a reindex re-wrote an evicted event's object"
    );
    let after = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[JSON]).await.json();
    assert_eq!(
        after["issue"]["events"].as_i64().expect("a count"),
        counter_before,
        "the lifetime counter moved, so the escalation ladder is now wrong for ever"
    );
}

#[tokio::test]
async fn the_project_page_says_what_is_left_of_the_ingest_quota() {
    let (bed, _bucket) = bugs_bed();
    let (id, key) = project(&bed, "api").await;
    post_envelope(&bed, id, &key, typed_envelope(&format!("{:032x}", 1), "QuotaError")).await;
    bed.bugs.digested(1).await;

    let json = get(&bed, "/bugs/api", &[JSON]).await.json();
    let windows = json["quota"].as_array().expect("the quota windows");
    assert_eq!(windows.len(), nashcode::bugs::quota::WINDOWS.len());
    assert_eq!(windows[0]["used"], 1);
    assert_eq!(windows[0]["limit"], tightest_quota());
    assert!(windows[0]["resets_at"].is_string());

    // And a person sees it too, which is what makes "why am I getting 429s" answerable.
    let page = get(&bed, "/bugs/api", &[]).await;
    assert!(page.body.contains("Ingest quota"), "the page does not mention the quota");
}
