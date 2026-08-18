//! The Code tab: root and subdirectory listings, READMEs, and blob views, against a
//! fixture repo built with real `git`.

mod common;

use std::path::Path;

use common::{Work, get, make_remote, post_form, simple_bed};

/// A repo with something of every kind in it: nested directories, a README at two
/// levels, a markdown file, a plain-text file, and one file that is not UTF-8.
fn code_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    work.write("README.md", "# demo repo\n\nThe plan is plans/api.md today.\n");
    work.write("src/lib.rs", "fn main() { let x = 1; }\n");
    work.write("src/README.md", "## inside src\n");
    work.write("docs/guide.md", "# Guide\n\nSome prose.\n");
    work.write("plans/api.md", "---\nbranch: feature\n---\n\n# API plan\n");
    // A PNG header: valid bytes, invalid UTF-8.
    work.write_bytes("assets/logo.png", &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0xff, 0xfe]);
    work.commit_all("initial");
    work.push("main");

    work.checkout_new("feature");
    work.write("src/lib.rs", "fn main() { let x = 2; }\n");
    work.commit_all("feature work");
    work.push("feature");
    work.checkout("main");
    work
}

/// Where a needle sits in the page, so order assertions read as order.
fn at(body: &str, needle: &str) -> usize {
    body.find(needle).unwrap_or_else(|| panic!("missing {needle}:\n{body}"))
}

#[tokio::test]
async fn the_root_listing_puts_directories_first_then_files() {
    let bed = simple_bed(code_fixture);
    let (status, body) = get(&bed.router, "/demo").await;
    assert_eq!(status, 200, "{body}");

    // Directories, alphabetical, then files.
    let assets = at(&body, "/demo/tree/assets");
    let docs = at(&body, "/demo/tree/docs");
    let plans = at(&body, "/demo/tree/plans");
    let src = at(&body, "/demo/tree/src");
    let readme = at(&body, "/demo/blob/README.md");
    assert!(assets < docs && docs < plans && plans < src, "directories out of order");
    assert!(src < readme, "a file sorted above a directory:\n{body}");

    // Rows carry a Phosphor icon and Primer Box styling.
    assert!(body.contains("ph-folder"), "no folder icon");
    assert!(body.contains("Box-row"), "not a Primer Box list");
}

#[tokio::test]
async fn the_root_readme_renders_as_markdown_below_the_listing() {
    let bed = simple_bed(code_fixture);
    let (status, body) = get(&bed.router, "/demo").await;
    assert_eq!(status, 200);
    assert!(body.contains("<h1>demo repo</h1>"), "README not rendered:\n{body}");
    assert!(body.contains("markdown-body"), "README not in a markdown body");
    assert!(
        at(&body, "/demo/blob/README.md") < at(&body, "<h1>demo repo</h1>"),
        "the README rendered above the listing"
    );
    // The shared renderer's autolinking is live here too.
    assert!(body.contains("href=\"/demo/plans/api.md\""), "plan autolink missing");
}

#[tokio::test]
async fn a_subdirectory_lists_its_own_files_and_readme() {
    let bed = simple_bed(code_fixture);
    let (status, body) = get(&bed.router, "/demo/tree/src").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("/demo/blob/src/lib.rs"), "file link missing:\n{body}");
    assert!(body.contains("<h2>inside src</h2>"), "subdirectory README not rendered");
    // A breadcrumb back to the repo root.
    assert!(body.contains("href=\"/demo\""), "no breadcrumb up:\n{body}");
    // The root's files are not in a subdirectory listing.
    assert!(!body.contains("/demo/blob/README.md"), "root file leaked into src/");
}

#[tokio::test]
async fn a_deeper_breadcrumb_links_every_step_but_the_last() {
    let bed = simple_bed(|root| {
        let work = code_fixture(root);
        work.write("src/web/pages.txt", "deep\n");
        work.commit_all("nested");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/tree/src/web").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("href=\"/demo/tree/src\""), "no crumb to src:\n{body}");
    assert!(!body.contains("href=\"/demo/tree/src/web\""), "the current dir is a link");
}

#[tokio::test]
async fn a_text_blob_renders_as_code_and_a_markdown_blob_renders_as_markdown() {
    let bed = simple_bed(code_fixture);

    let (status, body) = get(&bed.router, "/demo/blob/src/lib.rs").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("nashcode-code"), "no code styling:\n{body}");
    assert!(body.contains("fn main() { let x = 1; }"), "file text missing");
    assert!(!body.contains("markdown-body"), "a .rs file rendered as markdown");

    let (status, body) = get(&bed.router, "/demo/blob/docs/guide.md").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>Guide</h1>"), "markdown not rendered:\n{body}");
}

#[tokio::test]
async fn a_text_blob_tags_its_language_and_numbers_every_line() {
    let bed = simple_bed(code_fixture);
    let (status, body) = get(&bed.router, "/demo/blob/src/lib.rs").await;
    assert_eq!(status, 200, "{body}");

    // The server names the grammar; the browser fetches only that chunk.
    assert!(body.contains("data-lang=\"rust\""), "no language tag:\n{body}");
    assert!(body.contains("data-lines=\"1\""), "no line count:\n{body}");
    // Every line is anchorable whether or not highlighting ever runs.
    assert!(body.contains("id=\"L1\""), "no line id:\n{body}");
    assert!(body.contains("href=\"#L1\""), "no gutter link:\n{body}");
    assert!(body.contains("data-line=\"1\""), "no gutter number:\n{body}");
    assert!(body.contains("nashcode-line-code"), "no code cell:\n{body}");
}

#[tokio::test]
async fn a_file_with_no_known_grammar_is_numbered_but_not_tagged() {
    let bed = simple_bed(|root| {
        let work = code_fixture(root);
        work.write("notes.whatever", "one\ntwo\nthree\n");
        work.commit_all("odd extension");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/blob/notes.whatever").await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("data-lang="), "an unknown extension got a grammar:\n{body}");
    assert!(body.contains("data-lines=\"3\""), "not numbered:\n{body}");
    assert!(body.contains("id=\"L3\""), "the last line has no anchor:\n{body}");
}

#[tokio::test]
async fn a_huge_file_is_numbered_but_never_highlighted() {
    let bed = simple_bed(|root| {
        let work = code_fixture(root);
        let big: String = (0..5001).map(|n| format!("let x{n} = {n};\n")).collect();
        work.write("src/big.rs", &big);
        work.commit_all("a file nobody reads end to end");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/blob/src/big.rs").await;
    assert_eq!(status, 200);
    assert!(!body.contains("data-lang="), "a 5001-line file was tagged for shiki");
    assert!(body.contains("data-lines=\"5001\""), "numbering was skipped too");
    assert!(body.contains("id=\"L5001\""), "the last line has no anchor");
}

#[tokio::test]
async fn code_in_a_blob_is_escaped_not_executed() {
    let bed = simple_bed(|root| {
        let work = code_fixture(root);
        work.write("src/evil.js", "const a = \"<script>alert(1)</script>\";\n");
        work.commit_all("markup in a string");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/blob/src/evil.js").await;
    assert_eq!(status, 200);
    assert!(!body.contains("<script>alert(1)</script>"), "raw script tag survived:\n{body}");
    assert!(body.contains("&lt;script&gt;"), "not escaped:\n{body}");
}

#[tokio::test]
async fn a_binary_blob_is_offered_as_a_download_not_as_text() {
    let bed = simple_bed(code_fixture);
    let (status, body) = get(&bed.router, "/demo/blob/assets/logo.png").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Binary file, 8 bytes"), "no size card:\n{body}");
    assert!(
        body.contains("/demo/raw/main/assets/logo.png"),
        "no download link to the raw endpoint:\n{body}"
    );
    // And that link really serves the file.
    let (status, _) = get(&bed.router, "/demo/raw/main/assets/logo.png").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn tree_and_blob_are_reserved_and_a_branch_page_still_works() {
    let bed = simple_bed(code_fixture);
    // `tree` and `blob` reach the code browser, not the branch catch-all.
    let (status, body) = get(&bed.router, "/demo/tree/docs").await;
    assert_eq!(status, 200);
    assert!(body.contains("/demo/blob/docs/guide.md"), "not the listing page:\n{body}");

    // An ordinary branch name still resolves through the catch-all.
    let (status, body) = get(&bed.router, "/demo/feature").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("nashcode-diff-data"), "not the branch PR view:\n{body}");
    assert!(body.contains("let x = 2"), "branch diff missing:\n{body}");
}

#[tokio::test]
async fn missing_and_mistyped_paths_are_404_not_500() {
    let bed = simple_bed(code_fixture);
    for path in [
        "/demo/tree/nope",
        "/demo/blob/nope.txt",
        // A directory is not a blob, and must never be rendered as one.
        "/demo/blob/src",
        // Object addressing cannot walk out of the tree.
        "/demo/tree/../../etc",
    ] {
        let (status, body) = get(&bed.router, path).await;
        assert_eq!(status, 404, "{path} -> {status}: {body}");
    }
}

// ---- edit in the browser ----------------------------------------------------------

#[tokio::test]
async fn the_blob_header_offers_a_pencil_for_text_and_none_for_binaries() {
    let bed = simple_bed(code_fixture);

    let (status, body) = get(&bed.router, "/demo/blob/src/lib.rs").await;
    assert_eq!(status, 200);
    assert!(body.contains("/demo/edit/src/lib.rs"), "no pencil on a text file:\n{body}");
    assert!(body.contains("ph-pencil-simple"), "no pencil icon:\n{body}");
    // Raw is still there, exactly as before.
    assert!(body.contains("/demo/raw/main/src/lib.rs"), "raw link lost:\n{body}");

    let (status, body) = get(&bed.router, "/demo/blob/assets/logo.png").await;
    assert_eq!(status, 200);
    assert!(!body.contains("/demo/edit/"), "a binary offered a pencil:\n{body}");
}

#[tokio::test]
async fn the_edit_form_holds_the_file_and_the_tree_page_offers_a_new_one() {
    let bed = simple_bed(code_fixture);

    let (status, body) = get(&bed.router, "/demo/edit/src/lib.rs").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("fn main() { let x = 1; }"), "file not prefilled:\n{body}");
    assert!(body.contains("name=\"message\""), "no commit message field:\n{body}");
    assert!(body.contains("action=\"/demo/edit\""), "form posts somewhere else:\n{body}");
    assert!(body.contains("Commit to main"), "the button does not name the branch:\n{body}");

    // "New file" on the tree page is the same form, empty, with the directory filled in.
    let (status, tree) = get(&bed.router, "/demo/tree/src").await;
    assert_eq!(status, 200);
    assert!(tree.contains("/demo/edit?dir=src"), "no new-file button:\n{tree}");
    let (status, new) = get(&bed.router, "/demo/edit?dir=src").await;
    assert_eq!(status, 200, "{new}");
    assert!(new.contains("value=\"src/\""), "the directory was not prefilled:\n{new}");
    assert!(new.contains("New file"), "not the empty form:\n{new}");
}

#[tokio::test]
async fn editing_commits_pushes_and_redirects_to_the_file() {
    let bed = simple_bed(code_fixture);
    let before = bed.remote_tip("demo", "main");

    let (status, location, _) = post_form(
        &bed.router,
        "/demo/edit",
        &[
            ("path", "src/lib.rs"),
            ("content", "fn main() { let x = 42; }\r\n"),
            ("message", "answer the question"),
        ],
    )
    .await;
    assert_eq!(status, 303, "not a redirect after POST");
    assert_eq!(location.as_deref(), Some("/demo/blob/src/lib.rs"));

    // The push landed on the remote, not only in the mirror.
    let after = bed.remote_tip("demo", "main");
    assert_ne!(before, after, "nothing was pushed");
    let log = common::git(
        &bed.remote_root().join("demo.git"),
        &["log", "-1", "--format=%s%n%an", "main"],
    );
    assert!(log.contains("answer the question"), "commit message lost: {log}");
    assert!(log.contains("local"), "the actor was not stamped: {log}");

    // And the page serves the new bytes, with the CRLF the textarea added removed.
    let (status, body) = get(&bed.router, "/demo/blob/src/lib.rs").await;
    assert_eq!(status, 200);
    assert!(body.contains("let x = 42"), "the edit is not visible:\n{body}");
    assert!(body.contains("data-lines=\"1\""), "a stray blank line survived:\n{body}");
}

#[tokio::test]
async fn a_new_file_is_created_by_the_same_form() {
    let bed = simple_bed(code_fixture);
    let (status, location, _) = post_form(
        &bed.router,
        "/demo/edit",
        &[("path", "docs/new-note.md"), ("content", "# fresh\n"), ("message", "")],
    )
    .await;
    assert_eq!(status, 303);
    assert_eq!(location.as_deref(), Some("/demo/blob/docs/new-note.md"));

    let (status, body) = get(&bed.router, "/demo/blob/docs/new-note.md").await;
    assert_eq!(status, 200);
    assert!(body.contains("<h1>fresh</h1>"), "the new file is not there:\n{body}");
    // An empty message field still writes a readable commit.
    let log = common::git(&bed.remote_root().join("demo.git"), &["log", "-1", "--format=%s", "main"]);
    assert!(log.contains("Create docs/new-note.md"), "no default message: {log}");
}

#[tokio::test]
async fn a_path_that_escapes_the_repo_is_refused_and_the_text_comes_back() {
    let bed = simple_bed(code_fixture);
    let before = bed.remote_tip("demo", "main");
    for bad in ["../outside.md", "src/../../etc/passwd", ".git/config", ""] {
        let (status, location, body) = post_form(
            &bed.router,
            "/demo/edit",
            &[("path", bad), ("content", "keep me\n"), ("message", "nope")],
        )
        .await;
        assert_eq!(status, 200, "{bad} was not refused with a page");
        assert!(location.is_none(), "{bad} redirected as if it had committed");
        assert!(body.contains("Nothing was committed"), "no error card for {bad}:\n{body}");
        assert!(body.contains("keep me"), "the typed text was thrown away for {bad}");
    }
    assert_eq!(before, bed.remote_tip("demo", "main"), "a refused path still pushed");
}

#[tokio::test]
async fn the_branch_list_moved_to_the_stacks_page() {
    let bed = simple_bed(code_fixture);
    let (status, stacks) = get(&bed.router, "/demo/stacks").await;
    assert_eq!(status, 200, "{stacks}");
    assert!(stacks.contains("Branches"), "no branch list on the stacks page");
    assert!(stacks.contains("feature"), "branch missing from the list:\n{stacks}");
    // Below the stack graph, above the audit log.
    assert!(
        at(&stacks, "nashcode-stack-graph") < at(&stacks, ">Branches<"),
        "branch list rendered above the stack graph"
    );
    assert!(
        at(&stacks, ">Branches<") < at(&stacks, "Merge and restack log"),
        "branch list rendered below the audit log"
    );

    // And it is gone from the Code tab.
    let (_, code) = get(&bed.router, "/demo").await;
    assert!(!code.contains("Branches"), "the branch list is still on the code tab");
}
