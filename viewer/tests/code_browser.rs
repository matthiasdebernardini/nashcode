//! The Code tab: root and subdirectory listings, READMEs, and blob views, against a
//! fixture repo built with real `git`.

mod common;

use std::path::Path;

use common::{Work, get, make_remote, simple_bed};

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
