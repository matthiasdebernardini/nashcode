//! Refs that point at nothing are reported for the whole repo, not only at the link
//! site: `/brain` lists them and the board carries a count badge.

mod common;

use common::{Work, get, get_json, simple_bed, stacked_fixture};

/// One card whose `plan:` names a file that is in no tree and whose `branch:` names a
/// branch that does not exist, plus one card whose refs both resolve.
fn dangling_fixture(root: &std::path::Path) -> Work {
    let work = stacked_fixture(root, "demo");
    work.write("plans/api.md", "---\nbranch: part-1\n---\n# API plan\n\nShip it.\n");
    work.write(
        "tasks/lost.md",
        "---\nstatus: todo\ntitle: Lost card\nplan: plans/nope.md\nbranch: ghost\n---\n\nBody.\n",
    );
    work.write(
        "tasks/found.md",
        "---\nstatus: doing\ntitle: Found card\nplan: plans/api.md\nbranch: part-1\n---\n\nBody.\n",
    );
    work.commit_all("plans and cards");
    work.push("main");
    work
}

#[tokio::test]
async fn brain_lists_every_dangling_ref_and_the_board_badges_them() {
    let bed = simple_bed(dangling_fixture);

    let (status, body) = get_json(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    let demo = brain["repos"].as_array().expect("repos")[0].clone();
    let dangling = demo["dangling"].as_array().expect("dangling array").clone();

    assert_eq!(dangling.len(), 2, "exactly the two broken refs: {dangling:?}");
    assert!(
        dangling.iter().any(|d| d["from"] == "tasks/lost.md"
            && d["key"] == "branch"
            && d["target"] == "ghost"),
        "missing branch ref: {dangling:?}"
    );
    assert!(
        dangling.iter().any(|d| d["from"] == "tasks/lost.md"
            && d["key"] == "plan"
            && d["target"] == "plans/nope.md"),
        "missing plan ref: {dangling:?}"
    );

    // The board says how many there are and names them.
    let (status, board) = get(&bed.router, "/demo/board").await;
    assert_eq!(status, 200);
    assert!(board.contains("2 dangling refs"), "badge missing: {board}");
    assert!(board.contains("plans/nope.md"), "target missing from the list");
    assert!(board.contains("ghost"), "branch target missing from the list");
}

#[tokio::test]
async fn a_repo_whose_refs_all_resolve_reports_none() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        work.write("plans/api.md", "---\ntasks: [tasks/found.md]\n---\n# API plan\n\nShip it.\n");
        work.write(
            "tasks/found.md",
            "---\nstatus: doing\nplan: plans/api.md\nbranch: part-1\n---\n\nBody.\n",
        );
        work.commit_all("plans and cards");
        work.push("main");
        work
    });

    let (status, body) = get_json(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    let demo = brain["repos"].as_array().expect("repos")[0].clone();
    assert_eq!(demo["dangling"].as_array().expect("dangling array").len(), 0);

    let (_, board) = get(&bed.router, "/demo/board").await;
    assert!(!board.contains("dangling ref"), "badge shown with nothing dangling");
}
