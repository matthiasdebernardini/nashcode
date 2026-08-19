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

    assert_eq!(get(&bed, "/bugs", &[JSON]).await.json().as_array().expect("a list").len(), 1);
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

    post_envelope(&bed, id, key, fixture("python-exception.envelope")).await;
    bed.bugs.digested(1).await;

    let issues = get(&bed, "/bugs/api", &[JSON]).await.json();
    let issue_id = issues["issues"][0]["id"].as_i64().expect("an issue id");
    let page = get(&bed, &format!("/bugs/api/issues/{issue_id}"), &[]).await;
    assert!(
        page.body.contains("href=\"/demo/blob/probe_capture_exception.py#L24\""),
        "an in-app frame links to the file and line"
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
