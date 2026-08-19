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

// ---- nodes link back to the code ---------------------------------------------------

/// Rust the graph can read, one `mod.rs` module directory, and a Python file that
/// defines the same name as one of the Rust ones.
fn where_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    work.write(
        "src/mirror.rs",
        "\
pub struct Mirrors {
    pub root: String,
}

pub fn refresh(name: &str) {
    let _ = name;
}
",
    );
    work.write(
        "src/code/mod.rs",
        "\
pub fn locate(label: &str) -> bool {
    label.is_empty()
}
",
    );
    work.write(
        "app/views.py",
        "\
def refresh():
    pass


class Mirrors:
    pass
",
    );
    work.commit_all("initial");
    work.push("main");
    work
}

#[tokio::test]
async fn a_batch_resolves_symbol_names_and_file_stems() {
    let bed = simple_bed(where_fixture);
    bed.index("demo").await;

    let (status, body) =
        get(&bed.router, "/demo/code/where?names=Mirrors,mirror,code,mod,app,%20").await;
    assert_eq!(status, 200, "{body}");
    let names = json(&body);
    let names = &names["names"];

    // An exact symbol name carries its own path and line.
    let symbols = names["Mirrors"]["symbols"].as_array().expect("symbols");
    assert_eq!(symbols.len(), 1, "{body}");
    assert_eq!(symbols[0]["name"], "Mirrors");
    assert_eq!(symbols[0]["kind"], "struct");
    assert_eq!(symbols[0]["path"], "src/mirror.rs");
    assert_eq!(symbols[0]["line"], 1);
    assert!(names["Mirrors"]["files"].as_array().expect("files").is_empty(), "{body}");

    // A file stem carries the file, with what it defines, in line order.
    let files = names["mirror"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "{body}");
    assert_eq!(files[0]["path"], "src/mirror.rs");
    let defined: Vec<&str> = files[0]["symbols"]
        .as_array()
        .expect("symbols")
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("name"))
        .collect();
    assert_eq!(defined, vec!["Mirrors", "refresh"], "{body}");
    assert_eq!(files[0]["symbols"][1]["line"], 5, "{body}");

    // `mod.rs` answers to the directory it is in, not to "mod". Both were asked for.
    assert_eq!(names["code"]["files"][0]["path"], "src/code/mod.rs", "{body}");
    assert!(names.get("mod").is_none(), "mod.rs answered to its own stem:\n{body}");

    // A label with nothing behind it is absent, not an empty entry. `app` is a real
    // directory in the fixture and still resolves to nothing: it names no Rust file.
    assert!(names.get("app").is_none(), "a directory answered as a file:\n{body}");
    assert!(names.get(" ").is_none(), "an empty label was answered:\n{body}");
}

#[tokio::test]
async fn only_rust_answers_a_label() {
    let bed = simple_bed(where_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/where?names=refresh,views").await;
    assert_eq!(status, 200, "{body}");
    let names = json(&body);
    let names = &names["names"];

    // `refresh` is defined in both languages; only the Rust one comes back.
    let symbols = names["refresh"]["symbols"].as_array().expect("symbols");
    assert_eq!(symbols.len(), 1, "python leaked in:\n{body}");
    assert_eq!(symbols[0]["path"], "src/mirror.rs");

    // And a Python file's stem resolves to nothing at all.
    assert!(names.get("views").is_none(), "a .py file matched:\n{body}");
}

#[tokio::test]
async fn a_file_stem_matches_whatever_case_the_diagram_used() {
    let bed = simple_bed(where_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/where?names=Mirror,MIRROR").await;
    assert_eq!(status, 200, "{body}");
    let names = json(&body);
    for label in ["Mirror", "MIRROR"] {
        assert_eq!(names["names"][label]["files"][0]["path"], "src/mirror.rs", "{label}:\n{body}");
    }

    // The stem matched, not a symbol: nothing is defined under either spelling.
    assert!(names["names"]["Mirror"]["symbols"].as_array().expect("symbols").is_empty(), "{body}");
}

/// A repo built to hit the caps: one name defined fifty-one times, twenty-one files
/// sharing one stem, and a symbol named after that stem so both halves of an answer
/// compete for the same budget.
///
/// The repeated files are byte-identical on purpose. The index is content-addressed,
/// so they are one blob behind fifty-one paths — which is exactly the shape that would
/// let a cap on rows-per-file miss the real size of an answer.
fn crowded_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);

    for n in 0..51 {
        work.write(&format!("src/h{n}.rs"), "pub fn handle() {}\n");
    }
    for n in 0..21 {
        work.write(
            &format!("src/w{n}/widget.rs"),
            "pub struct One {}\n\npub struct Two {}\n\npub struct Three {}\n",
        );
    }
    work.write("src/naming.rs", "pub fn widget() {}\n");

    work.commit_all("initial");
    work.push("main");
    work
}

#[tokio::test]
async fn one_label_cannot_return_more_than_its_budget() {
    let bed = simple_bed(crowded_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/where?names=handle,widget").await;
    assert_eq!(status, 200, "{body}");
    let names = json(&body);
    let names = &names["names"];

    // Fifty-one definitions of one name come back as fifty.
    assert_eq!(names["handle"]["symbols"].as_array().expect("symbols").len(), 50, "{body}");

    // `widget` is both a symbol and a stem, so it exercises the composition: the
    // exact match is spent first and the files divide what is left. Twenty-one files
    // share the stem and only twenty are listed.
    let widget = &names["widget"];
    assert_eq!(widget["symbols"].as_array().expect("symbols").len(), 1, "{body}");
    assert_eq!(widget["symbols"][0]["path"], "src/naming.rs", "{body}");
    let files = widget["files"].as_array().expect("files");
    assert_eq!(files.len(), 20, "the file cap did not hold:\n{body}");

    // Sixty definitions live behind those twenty files; the budget stops at fifty
    // rows for the whole answer, not fifty per list.
    let from_files: usize =
        files.iter().map(|file| file["symbols"].as_array().expect("symbols").len()).sum();
    assert_eq!(from_files, 49, "{body}");
    assert_eq!(1 + from_files, 50, "the budget did not compose:\n{body}");

    // A file past the budget keeps its path — the link to the blob is the useful
    // half — and simply lists nothing.
    assert!(
        files.last().expect("a last file")["symbols"].as_array().expect("symbols").is_empty(),
        "{body}"
    );
    assert!(
        files.last().expect("a last file")["path"].as_str().expect("path").ends_with("widget.rs"),
        "{body}"
    );
}

#[tokio::test]
async fn more_than_a_hundred_names_is_refused() {
    let bed = simple_bed(where_fixture);
    bed.index("demo").await;

    let hundred: Vec<String> = (0..100).map(|n| format!("n{n}")).collect();
    let (status, body) = get(&bed.router, &format!("/demo/code/where?names={}", hundred.join(","))).await;
    assert_eq!(status, 200, "a hundred is allowed: {body}");

    let too_many: Vec<String> = (0..101).map(|n| format!("n{n}")).collect();
    let (status, body) =
        get(&bed.router, &format!("/demo/code/where?names={}", too_many.join(","))).await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn an_unknown_repo_is_a_404_and_an_unindexed_one_is_simply_empty() {
    let bed = simple_bed(where_fixture);

    let (status, body) = get(&bed.router, "/nope/code/where?names=Mirrors").await;
    assert_eq!(status, 404, "{body}");

    // Deliberately never indexed: no index is an empty answer, never an error.
    let (status, body) = get(&bed.router, "/demo/code/where?names=Mirrors,mirror").await;
    assert_eq!(status, 200, "{body}");
    let names = json(&body);
    assert_eq!(names["names"], serde_json::json!({}), "{body}");

    // A call with no names at all is the caller's mistake, not an empty answer — and
    // every spelling of "no names" is the same mistake, however it is punctuated.
    for empty in ["/demo/code/where", "/demo/code/where?names=", "/demo/code/where?names=,,,",
        "/demo/code/where?names=%20,%20"]
    {
        let (status, body) = get(&bed.router, empty).await;
        assert_eq!(status, 400, "{empty}:\n{body}");
    }
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
