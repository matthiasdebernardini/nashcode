//! The Docs tab: the repo's own markdown as a wiki, against fixture repos built with
//! real `git`.

mod common;

use std::path::Path;

use common::{Work, get, make_remote, simple_bed};

/// A repo whose markdown spreads across the tree, with cross-references written the
/// way an author writes them on GitHub.
fn wiki_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    work.write("README.md", "# demo repo\n\nStart at [the guide](docs/guide.md).\n");
    work.write("lat.md", "# The contract\n\nRead this first.\n");
    work.write(
        "docs/guide.md",
        "# Guide\n\nBack to the [README](../README.md), on to [deploy](deploy/how.md).\n\
         The code lives in [lib.rs](../src/lib.rs).\n",
    );
    work.write("docs/deploy/how.md", "# How to deploy\n\nPush and wait.\n");
    work.write("src/lib.rs", "fn main() {}\n");
    work.commit_all("initial");
    work.push("main");
    work
}

fn at(body: &str, needle: &str) -> usize {
    body.find(needle).unwrap_or_else(|| panic!("missing {needle}:\n{body}"))
}

#[tokio::test]
async fn the_wiki_home_falls_back_to_the_root_readme() {
    let bed = simple_bed(wiki_fixture);
    let (status, body) = get(&bed.router, "/demo/docs").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>demo repo</h1>"), "the README is not the home page:\n{body}");
    assert!(body.contains("markdown-body"), "not rendered by the plans renderer:\n{body}");
}

#[tokio::test]
async fn docs_index_md_wins_the_home_page_when_the_repo_has_one() {
    let bed = simple_bed(|root| {
        let work = wiki_fixture(root);
        work.write("docs/index.md", "# The wiki\n\nEverything starts here.\n");
        work.commit_all("a real index");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/docs").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>The wiki</h1>"), "docs/index.md did not win:\n{body}");
    assert!(!body.contains("<h1>demo repo</h1>"), "the README rendered too:\n{body}");
}

#[tokio::test]
async fn any_markdown_file_renders_in_the_same_frame() {
    let bed = simple_bed(wiki_fixture);
    let (status, body) = get(&bed.router, "/demo/docs/docs/deploy/how.md").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>How to deploy</h1>"), "page not rendered:\n{body}");
    // The sidebar is still there: the frame is persistent, not per-page.
    assert!(body.contains("nashcode-wiki-nav"), "no sidebar on a deep page:\n{body}");
    assert!(body.contains("/demo/docs/lat.md"), "the sidebar lost its other pages:\n{body}");
}

#[tokio::test]
async fn the_sidebar_pins_lat_md_nests_directories_and_marks_the_current_page() {
    let bed = simple_bed(wiki_fixture);
    let (status, body) = get(&bed.router, "/demo/docs/docs/guide.md").await;
    assert_eq!(status, 200, "{body}");

    // lat.md is the contract, so it sits above everything else — including the
    // directories that would otherwise sort first.
    assert!(
        at(&body, "/demo/docs/lat.md") < at(&body, "<summary>docs</summary>"),
        "lat.md is not pinned to the top:\n{body}"
    );
    assert!(
        at(&body, "/demo/docs/lat.md") < at(&body, "/demo/docs/README.md"),
        "lat.md sorted with the ordinary pages:\n{body}"
    );
    // Once, not twice: the pin replaces its place in the list.
    assert_eq!(body.matches("href=\"/demo/docs/lat.md\"").count(), 1, "lat.md listed twice");

    // Directories are collapsible, and the one holding the current page is open.
    assert!(body.contains("<summary>docs</summary>"), "no directory node:\n{body}");
    assert!(body.contains("<summary>deploy</summary>"), "no nested directory:\n{body}");
    assert!(body.contains("<details class=\"nashcode-wiki-dir\" open=\"\">"), "docs/ is closed:\n{body}");

    // The current page is marked.
    assert!(
        body.contains("nashcode-wiki-link is-current\" href=\"/demo/docs/docs/guide.md\""),
        "the current page is not highlighted:\n{body}"
    );
    // Non-markdown never enters the wiki.
    assert!(!body.contains("/demo/docs/src/lib.rs"), "a .rs file got a wiki page:\n{body}");
}

#[tokio::test]
async fn relative_links_between_pages_rewrite_to_their_docs_urls() {
    let bed = simple_bed(wiki_fixture);
    let (status, body) = get(&bed.router, "/demo/docs/docs/guide.md").await;
    assert_eq!(status, 200, "{body}");
    // Up a level, and down into a subdirectory.
    assert!(body.contains("href=\"/demo/docs/README.md\""), "../README.md not rewritten:\n{body}");
    assert!(
        body.contains("href=\"/demo/docs/docs/deploy/how.md\""),
        "a sibling directory link was not rewritten:\n{body}"
    );
    // A non-markdown target belongs to the code browser, not the wiki.
    assert!(body.contains("href=\"/demo/blob/src/lib.rs\""), "../src/lib.rs not sent to /blob/:\n{body}");

    // And the home page's own relative link works the same way.
    let (_, home) = get(&bed.router, "/demo/docs").await;
    assert!(home.contains("href=\"/demo/docs/docs/guide.md\""), "home link not rewritten:\n{home}");
}

#[tokio::test]
async fn the_wiki_refuses_non_markdown_and_missing_paths() {
    let bed = simple_bed(wiki_fixture);
    for path in [
        "/demo/docs/src/lib.rs",
        "/demo/docs/nope.md",
        "/demo/docs/../../etc/passwd",
    ] {
        let (status, body) = get(&bed.router, path).await;
        assert_eq!(status, 404, "{path} -> {status}: {body}");
    }
}

#[tokio::test]
async fn front_matter_is_stripped_and_markup_stays_escaped() {
    let bed = simple_bed(|root| {
        let work = wiki_fixture(root);
        work.write(
            "plans/api.md",
            "---\nbranch: feature\n---\n\n# API plan\n\n<script>alert(1)</script>\n",
        );
        work.commit_all("a plan is a wiki page too");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/docs/plans/api.md").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<h1>API plan</h1>"), "not rendered:\n{body}");
    assert!(!body.contains("branch: feature"), "front matter leaked into the page:\n{body}");
    assert!(!body.contains("<script>alert(1)</script>"), "script survived:\n{body}");
    assert!(body.contains("&lt;script&gt;"), "not escaped:\n{body}");
}

#[tokio::test]
async fn docs_is_reserved_and_outranks_a_branch_of_the_same_name() {
    let bed = simple_bed(|root| {
        let work = wiki_fixture(root);
        work.checkout_new("docs");
        work.write("docs/guide.md", "# Guide\n\nChanged on a branch.\n");
        work.commit_all("branch work");
        work.push("docs");
        work.checkout("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/docs").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("nashcode-wiki-nav"), "a branch named docs swallowed the wiki:\n{body}");
    assert!(!body.contains("nashcode-diff-data"), "the branch PR view won:\n{body}");

    // The branch is still reachable by every other name it has.
    let (status, stacks) = get(&bed.router, "/demo/stacks").await;
    assert_eq!(status, 200);
    assert!(stacks.contains("docs"), "the branch vanished from the stacks page:\n{stacks}");
}

#[tokio::test]
async fn every_repo_page_offers_the_docs_tab() {
    let bed = simple_bed(wiki_fixture);
    let (status, body) = get(&bed.router, "/demo").await;
    assert_eq!(status, 200);
    assert!(body.contains("href=\"/demo/docs\""), "no Docs tab on the code page:\n{body}");
    assert!(body.contains("ph-book-open"), "the tab has no icon:\n{body}");

    let (_, wiki) = get(&bed.router, "/demo/docs").await;
    assert!(wiki.contains("aria-current=\"page\""), "the Docs tab is not marked active:\n{wiki}");
}

#[tokio::test]
async fn a_repo_with_no_markdown_says_so_instead_of_failing() {
    let bed = simple_bed(|root| {
        let remote = make_remote(root, "demo");
        let work = Work::clone_from(&remote);
        work.write("src/lib.rs", "fn main() {}\n");
        work.commit_all("code only");
        work.push("main");
        work
    });
    let (status, body) = get(&bed.router, "/demo/docs").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("No markdown in this repo yet"), "no empty state:\n{body}");
}

#[tokio::test]
async fn the_wiki_never_offers_to_edit_a_page() {
    let bed = simple_bed(wiki_fixture);
    let (status, body) = get(&bed.router, "/demo/docs/docs/guide.md").await;
    assert_eq!(status, 200);
    assert!(!body.contains("/demo/edit/"), "the wiki grew an editor:\n{body}");
    // Reading the source is a link to the code browser, where the pencil lives.
    assert!(body.contains("/demo/blob/docs/guide.md"), "no link to the source:\n{body}");
}
