//! `blocks:` is the one edge that says an agent may not start yet. A cycle in it makes
//! "ready" unanswerable, so every card on the cycle is quarantined at ingest; a card
//! whose blockers are all `done` is reported as ready in `/brain` and on the board.

mod common;

use common::{Work, get, get_json, simple_bed, stacked_fixture};

/// Two cards that block each other. Neither can ever be ready.
fn cycle_fixture(root: &std::path::Path) -> Work {
    let work = stacked_fixture(root, "demo");
    work.write(
        "tasks/a.md",
        "---\nstatus: todo\ntitle: First\nblocks: [tasks/b.md]\n---\n\nBody.\n",
    );
    work.write(
        "tasks/b.md",
        "---\nstatus: todo\ntitle: Second\nblocks: [tasks/a.md]\n---\n\nBody.\n",
    );
    work.commit_all("two cards blocking each other");
    work.push("main");
    work
}

/// One card blocking another, the blocker finished; plus a pair where it is not.
fn ready_fixture(root: &std::path::Path) -> Work {
    let work = stacked_fixture(root, "demo");
    work.write(
        "tasks/done-blocker.md",
        "---\nstatus: done\ntitle: Finished\nblocks: tasks/ready.md\n---\n\nBody.\n",
    );
    work.write("tasks/ready.md", "---\nstatus: todo\ntitle: Ready\n---\n\nBody.\n");
    work.write(
        "tasks/open-blocker.md",
        "---\nstatus: doing\ntitle: Still open\nblocks: [tasks/blocked.md]\n---\n\nBody.\n",
    );
    work.write("tasks/blocked.md", "---\nstatus: todo\ntitle: Blocked\n---\n\nBody.\n");
    work.commit_all("a ready card and a blocked one");
    work.push("main");
    work
}

#[tokio::test]
async fn a_blocks_cycle_quarantines_every_card_on_it() {
    let bed = simple_bed(cycle_fixture);

    let (status, body) = get_json(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    let demo = brain["repos"].as_array().expect("repos")[0].clone();

    // Both cards land in the quarantine column, and neither is todo or ready.
    let quarantined: Vec<&str> = demo["cards"]["needs-attention"]
        .as_array()
        .expect("needs-attention column")
        .iter()
        .map(|card| card["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(quarantined, ["tasks/a.md", "tasks/b.md"], "{demo}");
    assert!(demo["cards"].get("todo").is_none(), "a card on a cycle is still todo: {demo}");
    assert_eq!(demo["ready"].as_array().expect("ready array").len(), 0, "{demo}");

    // And the board says which loop it is, on both cards.
    let (status, board) = get(&bed.router, "/demo/board").await;
    assert_eq!(status, 200);
    assert_eq!(
        board.matches("blocks cycle: tasks/a.md -&gt; tasks/b.md -&gt; tasks/a.md").count(),
        2,
        "the cycle is not named on both cards: {board}"
    );
}

#[tokio::test]
async fn ready_is_todo_with_every_blocker_done() {
    let bed = simple_bed(ready_fixture);

    let (status, body) = get_json(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    let demo = brain["repos"].as_array().expect("repos")[0].clone();
    assert_eq!(demo["ready"], serde_json::json!(["tasks/ready.md"]), "{demo}");

    // Both are todo on the ordinary board; `?ready=1` keeps only the one that is.
    let (status, board) = get(&bed.router, "/demo/board").await;
    assert_eq!(status, 200);
    assert!(board.contains("tasks/ready.md") && board.contains("tasks/blocked.md"), "{board}");

    let (status, filtered) = get(&bed.router, "/demo/board?ready=1").await;
    assert_eq!(status, 200);
    assert!(filtered.contains("tasks/ready.md"), "the ready card is missing: {filtered}");
    assert!(!filtered.contains("tasks/blocked.md"), "a blocked card survived the filter");
    // The filter is about the todo column only: the cards already moving stay.
    assert!(filtered.contains("tasks/open-blocker.md"), "the doing column was filtered too");
}
