//! One card and one plan per branch. Two cards claiming the same branch is a conflict
//! the viewer names out loud, and a merge that would flip both refuses instead.

mod common;

use common::{get, simple_bed, stacked_fixture};
use nashcode::ops::OpError;

#[tokio::test]
async fn two_cards_claiming_one_branch_conflict_and_the_merge_refuses() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        work.write(
            "tasks/a.md",
            "---\nstatus: doing\ntitle: Ship part 1\nbranch: part-1\n---\n\nFirst claimant.\n",
        );
        work.write(
            "tasks/b.md",
            "---\nstatus: todo\ntitle: Ship part 1 again\nbranch: part-1\n---\n\nSecond claimant.\n",
        );
        work.commit_all("two cards, one branch");
        work.push("main");
        work
    });

    // /brain names it, so an agent sees the ambiguity without opening a page.
    let (status, brain) = get(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200);
    let state: serde_json::Value = serde_json::from_str(&brain).expect("brain is json");
    let conflicts = &state["repos"][0]["conflicts"];
    assert_eq!(conflicts[0]["branch"], "part-1", "{brain}");
    assert_eq!(conflicts[0]["kind"], "cards");
    assert_eq!(conflicts[0]["paths"], serde_json::json!(["tasks/a.md", "tasks/b.md"]));

    // The branch page says the same thing in words.
    let (status, page) = get(&bed.router, "/demo/part-1").await;
    assert_eq!(status, 200);
    assert!(
        page.contains("2 cards claim this branch: tasks/a.md, tasks/b.md"),
        "conflict missing from the branch page"
    );

    // And the merge stops before it pushes anything.
    bed.mirrors.refresh("demo").await;
    let main_before = bed.remote_tip("demo", "main");
    let refused = bed.app.ops.merge("demo", "part-1", &bed.actor(), false, false).await;
    let Err(OpError::Blocked(why)) = refused else {
        panic!("expected the merge to refuse, got {refused:?}");
    };
    assert!(why.contains("branch part-1 is claimed by 2 cards; keep one"), "{why}");
    assert_eq!(bed.remote_tip("demo", "main"), main_before, "nothing may be pushed");
}
