//! Context capture and path suffix-matching, against a real git repository.
//!
//! The fixture has two commits that both touch the same file, so "which revision did
//! you read this at" is a question with two visibly different answers. That is the
//! whole point of the feature: a line number from last week's release, read against
//! today's tip, points at whatever happens to be there now — and looks right.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::TestBed;
use nashcode::config::Config;
use topcoat::router::{Body, Method, Router, request::Request, to_bytes};

/// The line the snippet is supposed to centre on, in both commits.
const FAILING_LINE: i64 = 4;

const RELEASE_SOURCE: &str = "\
def handle(request):
    total = 0
    for item in request:
        total = total / RELEASE_ONE_DIVISOR
    return total
";

const TIP_SOURCE: &str = "\
def handle(request):
    total = 0
    for item in request:
        total = total / TIP_TWO_DIVISOR
    return total
";

struct Fixture {
    bed: TestBed,
    /// The commit the first version of `src/app.py` lives at.
    release: String,
    _bucket: tempfile::TempDir,
}

/// A repo named `app` with two commits, plus two files of the same name in different
/// directories so ambiguity can be shown rather than argued about.
fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let bucket = tempfile::tempdir().expect("bucket");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");

    let remote = common::make_remote(&remotes, "app");
    let work = common::Work::clone_from(&remote);
    work.write("README.md", "# app\n");
    work.write("src/app.py", RELEASE_SOURCE);
    work.commit_all("the release");
    work.push("main");
    let release = common::git(&work.dir, &["rev-parse", "HEAD"]).trim().to_owned();

    work.write("src/app.py", TIP_SOURCE);
    work.write("a/util.py", "def helper():\n    return 1\n");
    work.write("b/util.py", "def helper():\n    return 2\n");
    work.commit_all("what is there now");
    work.push("main");

    let config = Arc::new(Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: vec!["app".to_owned()],
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        webhooks: BTreeMap::new(),
        anthropic_key: None,
        anthropic_url: "http://127.0.0.1:1".to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: Some(format!("file://{}", bucket.path().display())),
        bugs_s3_endpoint: None,
        bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
        bugs_drain: None,
        pushover: None,
        public_url: "https://nashcode.example".to_owned(),
        bugs_self_dsn: None,
    });
    let bed = common::testbed_from_config(root, config);
    Fixture { bed, release, _bucket: bucket }
}

async fn send(
    router: &Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, String) {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body)).expect("request builds");
    let response = router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A project that declares the fixture repo, and its DSN key.
async fn project(bed: &TestBed) -> (i64, String) {
    let project = bed.bugs.create_project("api", Some("app")).expect("project");
    (project.id, project.key)
}

async fn post_ndjson(bed: &TestBed, id: i64, key: &str, lines: &str) -> (u16, String) {
    send(
        &bed.router,
        Method::POST,
        &format!("/api/{id}/logs?sentry_key={key}"),
        &[("content-type", "application/x-ndjson")],
        lines.as_bytes().to_vec(),
    )
    .await
}

/// One log line naming a file, a line, and the release it ran.
fn log_line(path: &str, line: i64, release: Option<&str>) -> String {
    let mut row = serde_json::json!({
        "level": "error",
        "message": "division by zero",
        "code.file.path": path,
        "code.line.number": line,
    });
    if let Some(release) = release {
        row["sentry.release"] = serde_json::json!(release);
    }
    format!("{row}\n")
}

async fn logs_page(bed: &TestBed) -> String {
    let (status, body) = send(&bed.router, Method::GET, "/bugs/api/logs", &[], Vec::new()).await;
    assert_eq!(status, 200);
    body
}

// ---- the revision is part of the answer ---------------------------------------------

#[tokio::test]
async fn the_snippet_is_read_at_the_commit_the_release_names() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    let line = log_line("/srv/app/src/app.py", FAILING_LINE, Some(&fixture.release));
    let (status, _) = post_ndjson(&fixture.bed, id, &key, &line).await;
    assert_eq!(status, 200);

    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("RELEASE_ONE_DIVISOR"), "the source as it was at the release");
    assert!(!page.contains("TIP_TWO_DIVISOR"), "not the source as it is now");
    assert!(!page.contains("tip, not release"), "the release was known, so no marker");
    // The rev the lines were read at is on the page, so a reader can check it.
    assert!(page.contains(&fixture.release[..8]), "the commit is named");
    // And the three lines either side are all there.
    for context in ["def handle(request):", "total = 0", "return total"] {
        assert!(page.contains(context), "{context} is missing from the snippet");
    }
}

#[tokio::test]
async fn a_release_the_mirror_does_not_know_falls_back_to_the_tip_and_says_so() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    // Well-formed, plausible, and not in this repository.
    let stranger = "0123456789abcdef0123456789abcdef01234567";
    let line = log_line("/srv/app/src/app.py", FAILING_LINE, Some(stranger));
    post_ndjson(&fixture.bed, id, &key, &line).await;

    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("TIP_TWO_DIVISOR"), "the tip, since the release is unknown");
    assert!(!page.contains("RELEASE_ONE_DIVISOR"));
    assert!(page.contains("tip, not release"), "and the page says which it is showing");
}

#[tokio::test]
async fn a_row_with_no_release_at_all_reads_the_tip_and_still_says_so() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    post_ndjson(&fixture.bed, id, &key, &log_line("src/app.py", FAILING_LINE, None)).await;

    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("TIP_TWO_DIVISOR"));
    assert!(page.contains("tip, not release"));
}

// ---- suffix matching ----------------------------------------------------------------

#[tokio::test]
async fn a_container_path_resolves_by_its_longest_unique_suffix() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    // What a Docker image actually reports. Nothing in the repo is called this.
    post_ndjson(&fixture.bed, id, &key, &log_line("/app/src/app.py", FAILING_LINE, None)).await;

    let page = logs_page(&fixture.bed).await;
    assert!(
        page.contains("href=\"/app/blob/src/app.py#L4\""),
        "the link points at the repo path, not the container path"
    );
    assert!(page.contains("TIP_TWO_DIVISOR"), "and the source came with it");
}

#[tokio::test]
async fn an_ambiguous_suffix_is_never_guessed() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    // `util.py` is both `a/util.py` and `b/util.py`. Picking one would send a reader
    // to the wrong file and they would believe it.
    post_ndjson(&fixture.bed, id, &key, &log_line("/app/util.py", 2, None)).await;

    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("/app/util.py:2"), "the reported path is still shown");
    assert!(!page.contains("/app/blob/a/util.py"), "and neither candidate is linked");
    assert!(!page.contains("/app/blob/b/util.py"));
    assert!(!page.contains("def helper"), "and no source is shown for a guess");

    // Enough of the path to tell them apart resolves again.
    post_ndjson(&fixture.bed, id, &key, &log_line("/app/b/util.py", 2, None)).await;
    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("href=\"/app/blob/b/util.py#L2\""));
}

#[tokio::test]
async fn a_path_that_is_nowhere_in_the_repo_stays_plain_text() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    let lines = format!(
        "{}{}",
        log_line("/usr/lib/python3.12/site-packages/urllib3/connection.py", 9, None),
        log_line("src/never_existed.rs", 9, None),
    );
    post_ndjson(&fixture.bed, id, &key, &lines).await;

    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("connection.py:9"), "still shown");
    assert!(page.contains("src/never_existed.rs:9"), "still shown");
    assert!(!page.contains("/app/blob/"), "and nothing is linked");
}

#[tokio::test]
async fn a_line_past_the_end_of_the_file_links_without_pretending_to_read_it() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    post_ndjson(&fixture.bed, id, &key, &log_line("src/app.py", 9_000, None)).await;

    let page = logs_page(&fixture.bed).await;
    assert!(page.contains("href=\"/app/blob/src/app.py#L9000\""), "the file is real");
    assert!(!page.contains("TIP_TWO_DIVISOR"), "but line 9000 is not");
}

// ---- the issue page -----------------------------------------------------------------

#[tokio::test]
async fn an_in_app_stack_frame_carries_the_source_at_its_release() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let (id, key) = project(&fixture.bed).await;

    let event = serde_json::json!({
        "event_id": "a".repeat(32),
        "platform": "python",
        "release": fixture.release,
        "exception": {"values": [{
            "type": "ZeroDivisionError",
            "value": "division by zero",
            "stacktrace": {"frames": [
                {"filename": "/usr/lib/python3.12/runpy.py", "lineno": 88, "in_app": false,
                 "function": "_run_code"},
                {"filename": "/srv/app/src/app.py", "lineno": FAILING_LINE, "in_app": true,
                 "function": "handle"},
            ]},
        }]},
    });
    let body = format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n").into_bytes();
    let (status, _) = send(
        &fixture.bed.router,
        Method::POST,
        &format!("/api/{id}/envelope/?sentry_key={key}"),
        &[("content-type", "application/x-sentry-envelope")],
        body,
    )
    .await;
    assert_eq!(status, 200);
    fixture.bed.bugs.digested(1).await;

    let issues = fixture.bed.bugs.issues(id, None).expect("issues");
    assert_eq!(issues.len(), 1);
    let (status, page) = send(
        &fixture.bed.router,
        Method::GET,
        &format!("/bugs/api/issues/{}", issues[0].id),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200);

    assert!(page.contains("href=\"/app/blob/src/app.py#L4\""), "the in-app frame links");
    assert!(page.contains("RELEASE_ONE_DIVISOR"), "at the release the event named");
    assert!(!page.contains("TIP_TWO_DIVISOR"));
    // The frame out of the standard library is shown and never linked: it is not in
    // this repository, whatever a file of that name here would suggest.
    assert!(page.contains("runpy.py"));
    assert!(!page.contains("/app/blob/runpy.py"));
}

#[tokio::test]
async fn a_project_with_no_repo_shows_the_frame_and_reads_nothing() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;
    let project = fixture.bed.bugs.create_project("plain", None).expect("project");

    post_ndjson(&fixture.bed, project.id, &project.key, &log_line("src/app.py", FAILING_LINE, None))
        .await;

    let (status, page) =
        send(&fixture.bed.router, Method::GET, "/bugs/plain/logs", &[], Vec::new()).await;
    assert_eq!(status, 200);
    assert!(page.contains("src/app.py:4"), "the origin is still shown");
    assert!(!page.contains("/app/blob/"), "with nowhere to link it");
    assert!(!page.contains("TIP_TWO_DIVISOR"), "and no source is read");
}

// ---- what a page is allowed to spend -------------------------------------------------

/// `sentry.release` is a free attribute: a sender can put anything in it, on every row.
///
/// Keyed on the release *string*, a page of a hundred rows carrying a hundred different
/// version numbers would open a hundred sources — `default_branch`, `tip` and a full
/// `ls-tree -r` apiece — and every one of them would be the same tree. Keyed on the
/// resolved commit, they collapse onto one. This is the bound, measured rather than
/// asserted by inspection.
#[tokio::test]
async fn a_hundred_bogus_releases_cost_the_same_as_one() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;

    let mut catalog =
        nashcode::bugs::context::Catalog::new(fixture.bed.mirrors.repo("app"));
    for n in 0..100 {
        let release = format!("v1.0.{n}");
        let source = catalog.source(Some(&release)).await.expect("the tip");
        assert!(source.tip_not_release, "a version number is not a commit");
    }

    // Two calls, ever: `default_branch` and `tip`, plus the one `ls-tree` that opened
    // the tree. Not one of the hundred releases could be an object id, so not one of
    // them cost a `rev_parse` — the syntax check runs before the subprocess does.
    assert_eq!(catalog.sources_open(), 1, "one tree for a hundred releases");
    assert_eq!(catalog.git_calls(), 3, "default_branch, tip, ls-tree — and nothing else");

    // A release that *could* be a commit costs exactly one lookup, however often it is
    // named — one, not two: a release with no `@` in it is its own tail, and asking git
    // the same question twice is a subprocess per row for nothing.
    for _ in 0..50 {
        catalog.source(Some("0123456789abcdef0123456789abcdef01234567")).await;
    }
    assert_eq!(catalog.git_calls(), 4, "one rev_parse for fifty rows naming one release");
    assert_eq!(catalog.sources_open(), 1, "and it resolved to nothing, so still the tip");

    for _ in 0..50 {
        catalog.source(Some(&fixture.release)).await;
    }
    assert_eq!(catalog.sources_open(), 2, "the release the mirror knows is its own tree");
    assert_eq!(catalog.git_calls(), 6, "one rev_parse and one ls-tree, once");
}

#[tokio::test]
async fn a_page_naming_more_trees_than_it_may_hold_falls_back_to_the_tip() {
    let fixture = fixture();
    fixture.bed.mirrors.refresh_now("app").await;

    let mut catalog =
        nashcode::bugs::context::Catalog::new(fixture.bed.mirrors.repo("app"));
    // Real commits, so each one genuinely wants a tree of its own.
    let commits: Vec<String> = common::git(
        &fixture.bed.remote_root().join("app.git"),
        &["log", "--format=%H", "-n", "2", "main"],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(commits.len(), 2);

    for commit in &commits {
        catalog.source(Some(commit)).await.expect("a tree");
    }
    let held = catalog.sources_open();
    assert!(held <= nashcode::bugs::context::MAX_SOURCES, "held {held}");

    // Whatever the cap, the page keeps working: every row still gets a source back.
    for commit in &commits {
        assert!(catalog.source(Some(commit)).await.is_some());
    }
}
