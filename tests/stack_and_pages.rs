//! Stack inference on a real fixture repo, the diff surface, and the three core pages.

mod common;

use common::{get, simple_bed, stacked_fixture};
use nashgit::stack::StackGraph;

#[tokio::test]
async fn stack_inference_finds_parents_and_chains() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let graph = StackGraph::infer(&bed.mirrors.repo("demo")).await.expect("graph infers");

    assert_eq!(graph.default_branch, "main");
    assert_eq!(graph.get("part-1").unwrap().parent.as_deref(), Some("main"));
    assert_eq!(graph.get("part-2").unwrap().parent.as_deref(), Some("part-1"));
    assert_eq!(graph.get("feature-x").unwrap().parent.as_deref(), Some("main"));
    assert_eq!(graph.get("main").unwrap().parent, None);

    assert_eq!(graph.get("part-1").unwrap().ahead, 1);
    assert_eq!(graph.get("part-2").unwrap().ahead, 1);

    let chains = graph.chains();
    assert!(chains.contains(&vec!["main".into(), "part-1".into(), "part-2".into()]));
    assert!(chains.contains(&vec!["main".into(), "feature-x".into()]));
}

#[tokio::test]
async fn the_branch_page_carries_per_file_unified_diffs() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let (status, body) = get(&bed.router, "/demo/part-1").await;
    assert_eq!(status, 200, "{body}");
    // The embedded diff payload is a real unified diff for the changed file.
    assert!(body.contains("nashgit-diff-data"), "diff payload missing");
    assert!(body.contains("src/app.txt"), "changed file missing");
    assert!(body.contains("+two"), "added line missing from the patch: {body}");
    // The stack banner names the parent.
    assert!(body.contains("/demo/main"), "parent link missing");
}

#[tokio::test]
async fn a_two_hop_branch_diffs_against_its_stack_parent_only() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let (status, body) = get(&bed.router, "/demo/part-2").await;
    assert_eq!(status, 200);
    assert!(body.contains("src/extra.txt"));
    // part-1's change is parent history, not part of this diff.
    assert!(!body.contains("+two"), "parent's changes leaked into the child diff");
}

#[tokio::test]
async fn index_code_and_stacks_pages_render() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    for path in ["/", "/demo", "/demo/stacks", "/demo/ci", "/demo/plans", "/demo/board"] {
        let (status, body) = get(&bed.router, path).await;
        assert_eq!(status, 200, "{path} -> {status}: {body}");
    }
    let (_, code) = get(&bed.router, "/demo").await;
    assert!(code.contains("part-1") && code.contains("feature-x"));
}

#[tokio::test]
async fn a_hostile_branch_name_cannot_escape_into_inline_js() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        // Legal in git, hostile in a single-quoted JS string.
        common::git(&work.dir, &["checkout", "-b", "evil');alert(1)"]);
        work.write("docs/evil.txt", "payload\n");
        work.commit_all("evil branch");
        work.push("evil');alert(1)");
        work
    });

    let (status, page) =
        get(&bed.router, "/demo/evil%27%29%3Balert%281%29").await;
    assert_eq!(status, 200);
    // The delete confirm is static text; the branch name never reaches inline JS.
    assert!(
        !page.contains("confirm('Delete evil"),
        "branch name leaked into inline JS: {page}"
    );
    assert!(page.contains("Delete this branch on the git server?"));
}

#[tokio::test]
async fn unknown_repo_and_branch_are_4xx_not_500() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let (status, _) = get(&bed.router, "/nope").await;
    assert_eq!(status, 404);
    let (status, _) = get(&bed.router, "/demo/no-such-branch").await;
    assert_eq!(status, 404);
}
