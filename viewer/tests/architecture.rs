//! The Architecture tab: the submit endpoint, the read endpoints, and the page.
//!
//! The diagram is untrusted text an agent posted. The page test that matters most is
//! the escaping one — a diagram containing `<script>` must arrive as text, never as
//! markup. The `GET /{repo}/code/graph` half of the loop belongs to code
//! intelligence and is covered by `code_intelligence.rs`.

mod common;

use std::path::Path;

use common::{Work, get, get_json, make_remote, post_json, simple_bed};

fn arch_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    work.write("README.md", "# demo\n");
    work.write("src/main.rs", "fn main() {}\n");
    work.commit_all("initial");
    work.push("main");
    work
}

/// The same repo, plus a committed `ARCHITECTURE.md` for the empty-state fallback.
fn documented_fixture(root: &Path) -> Work {
    let work = arch_fixture(root);
    work.write(
        "ARCHITECTURE.md",
        "# Shape\n\nProse first.\n\n```mermaid\ngraph TD;\n  viewer-->dgit;\n```\n",
    );
    work.commit_all("architecture");
    work.push("main");
    work
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|error| panic!("not JSON ({error}):\n{body}"))
}

#[tokio::test]
async fn a_submission_round_trips_and_history_keeps_every_one() {
    let bed = simple_bed(arch_fixture);

    let (status, body) = post_json(
        &bed.router,
        "/demo/architecture",
        serde_json::json!({"mermaid": "graph TD;\n  a-->b;", "title": "First", "note": "Why."}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let first = json(&body);
    assert_eq!(first["mermaid"], "graph TD;\n  a-->b;");
    assert_eq!(first["title"], "First");
    assert_eq!(first["note"], "Why.");
    assert_eq!(first["author"], "local");
    let first_id = first["id"].as_i64().expect("an id");

    let (status, body) = get_json(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["id"].as_i64(), Some(first_id));

    let (status, body) = post_json(
        &bed.router,
        "/demo/architecture",
        serde_json::json!({"mermaid": "graph LR;\n  b-->c;", "title": "Second"}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let second_id = json(&body)["id"].as_i64().expect("an id");

    // The newest submission is the latest; nothing was overwritten.
    let (status, body) = get_json(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 200, "{body}");
    let latest = json(&body);
    assert_eq!(latest["id"].as_i64(), Some(second_id));
    assert_eq!(latest["title"], "Second");
    assert!(latest["note"].is_null(), "an absent note is null:\n{body}");

    // History is newest first, and carries no diagram sources.
    let (status, body) = get_json(&bed.router, "/demo/architecture?history").await;
    assert_eq!(status, 200, "{body}");
    let history = json(&body);
    let rows = history.as_array().expect("an array");
    assert_eq!(rows.len(), 2, "{body}");
    assert_eq!(rows[0]["id"].as_i64(), Some(second_id));
    assert_eq!(rows[1]["id"].as_i64(), Some(first_id));
    assert!(rows[0].get("mermaid").is_none(), "history carries sources:\n{body}");

    // And the first one is still readable by id.
    let (status, body) = get_json(&bed.router, &format!("/demo/architecture?id={first_id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["mermaid"], "graph TD;\n  a-->b;");
}

#[tokio::test]
async fn an_empty_or_oversized_diagram_is_refused() {
    let bed = simple_bed(arch_fixture);

    for empty in ["", "   \n  "] {
        let (status, body) =
            post_json(&bed.router, "/demo/architecture", serde_json::json!({"mermaid": empty}))
                .await;
        assert!((400..500).contains(&status), "empty diagram accepted: {status} {body}");
    }

    let huge = "a".repeat(64 * 1024 + 1);
    let (status, body) =
        post_json(&bed.router, "/demo/architecture", serde_json::json!({"mermaid": huge})).await;
    assert!((400..500).contains(&status), "oversized diagram accepted: {status} {body}");

    // The other stored-and-rendered fields are capped too.
    let (status, body) = post_json(
        &bed.router,
        "/demo/architecture",
        serde_json::json!({"mermaid": "graph TD;", "title": "t".repeat(513)}),
    )
    .await;
    assert!((400..500).contains(&status), "oversized title accepted: {status} {body}");
    let (status, body) = post_json(
        &bed.router,
        "/demo/architecture",
        serde_json::json!({"mermaid": "graph TD;", "note": "n".repeat(64 * 1024 + 1)}),
    )
    .await;
    assert!((400..500).contains(&status), "oversized note accepted: {status} {body}");

    // Nothing was stored, so the page still shows the empty state.
    let (status, body) = get_json(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn an_unknown_repo_is_refused_on_both_sides() {
    let bed = simple_bed(arch_fixture);
    let (status, body) = get_json(&bed.router, "/nope/architecture").await;
    assert!((400..500).contains(&status), "{status} {body}");
    let (status, body) =
        post_json(&bed.router, "/nope/architecture", serde_json::json!({"mermaid": "graph TD;"}))
            .await;
    assert!((400..500).contains(&status), "{status} {body}");
}

#[tokio::test]
async fn the_page_teaches_the_loop_before_anything_is_submitted() {
    let bed = simple_bed(arch_fixture);
    let (status, body) = get(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("/demo/code/graph"), "no recipe on the empty page:\n{body}");
    assert!(body.contains("/demo/architecture"), "no submit URL on the empty page:\n{body}");

    // Every page carries the tab.
    let (status, home) = get(&bed.router, "/demo").await;
    assert_eq!(status, 200, "{home}");
    assert!(home.contains("/demo/architecture"), "no Architecture tab:\n{home}");
    assert!(home.contains("Architecture"), "no Architecture label:\n{home}");
}

#[tokio::test]
async fn the_empty_page_falls_back_to_the_repos_own_architecture_md() {
    let bed = simple_bed(documented_fixture);
    let (status, body) = get(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("viewer--&gt;dgit;"), "the committed diagram is missing:\n{body}");
    // The prose around the block is not a diagram.
    assert!(!body.contains("Prose first"), "the whole file leaked in:\n{body}");
}

#[tokio::test]
async fn a_submitted_diagram_reaches_the_page_as_text() {
    let bed = simple_bed(arch_fixture);
    let hostile = "graph TD;\n  a[\"<script>alert(1)</script>\"]-->b;";
    let (status, body) = post_json(
        &bed.router,
        "/demo/architecture",
        serde_json::json!({"mermaid": hostile, "title": "Drawn", "note": "A **note**."}),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let (status, body) = get(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Drawn"), "no title:\n{body}");
    assert!(body.contains("<strong>note</strong>"), "the note is not markdown:\n{body}");
    assert!(body.contains("&lt;script&gt;"), "the diagram is not escaped:\n{body}");
    assert!(!body.contains("<script>alert(1)</script>"), "stored XSS:\n{body}");
    // The client reads the source out of the DOM; the server hands it over as text.
    assert!(body.contains("nashcode-mermaid-source"), "no source element:\n{body}");
}

#[tokio::test]
async fn architecture_is_reserved_and_outranks_a_branch_of_the_same_name() {
    let bed = simple_bed(|root| {
        let work = arch_fixture(root);
        work.checkout_new("architecture");
        work.write("src/main.rs", "fn main() { println!(\"hi\") }\n");
        work.commit_all("branch work");
        work.push("architecture");
        work.checkout("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/architecture").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("nashcode-code"), "a branch swallowed the tab:\n{body}");
    assert!(!body.contains("nashcode-diff-data"), "the branch PR view won:\n{body}");

    // The branch is still reachable everywhere else.
    let (status, stacks) = get(&bed.router, "/demo/stacks").await;
    assert_eq!(status, 200);
    assert!(stacks.contains("architecture"), "the branch vanished:\n{stacks}");
}
