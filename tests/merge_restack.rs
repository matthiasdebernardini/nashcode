//! Merge and restack against fixture repos: real `git`, real pushes, tempdirs.

mod common;

use common::{Work, git, simple_bed, stacked_fixture};
use nashgit::db::status;
use nashgit::ops::OpError;
use nashgit::stack::StackGraph;

#[tokio::test]
async fn merge_fast_forwards_into_an_unmoved_parent() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let part1_tip = bed.remote_tip("demo", "part-1");

    let outcome = bed
        .app
        .ops
        .merge("demo", "part-1", &bed.actor(), false, false)
        .await
        .expect("merge succeeds");

    assert!(outcome.fast_forward, "parent had not moved; expected a fast-forward");
    assert_eq!(outcome.into, "main");
    // The remote's main now IS part-1's tip, and the mirror agrees.
    assert_eq!(bed.remote_tip("demo", "main"), part1_tip);
    let mirror_main = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    assert_eq!(mirror_main, part1_tip);

    // The audit trail recorded it.
    let audit = bed.db.audit("demo", 10).expect("audit reads");
    assert!(audit.iter().any(|e| e.action == "merge" && e.branch == "part-1"));
}

#[tokio::test]
async fn merge_creates_a_merge_commit_when_the_parent_moved() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    // Move main independently (a file part-1 does not touch).
    let work = Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("docs/other.txt", "moved\n");
    work.commit_all("main moves");
    work.push("main");

    let outcome = bed
        .app
        .ops
        .merge("demo", "part-1", &bed.actor(), false, false)
        .await
        .expect("merge succeeds");

    assert!(!outcome.fast_forward);
    // A --no-ff merge commit has two parents.
    let bare = bed.remote_root().join("demo.git");
    let parents = git(&bare, &["rev-list", "--parents", "-1", "refs/heads/main"]);
    assert_eq!(parents.split_whitespace().count(), 3, "expected a merge commit: {parents}");
}

#[tokio::test]
async fn red_or_running_ci_blocks_the_merge_until_confirmed() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let tip = bed.remote_tip("demo", "part-1");
    let run = bed.db.enqueue_run("demo", "part-1", &tip).expect("run recorded");
    bed.db.set_run_status(run, status::FAILED, 10, None).expect("status set");

    let blocked = bed.app.ops.merge("demo", "part-1", &bed.actor(), false, false).await;
    assert!(matches!(blocked, Err(OpError::Blocked(_))), "red CI must block");
    // The remote did not move.
    assert_ne!(bed.remote_tip("demo", "main"), tip);

    // The override (the confirm step) merges anyway.
    bed.app
        .ops
        .merge("demo", "part-1", &bed.actor(), true, false)
        .await
        .expect("override merges");
    assert_eq!(bed.remote_tip("demo", "main"), tip);
}

#[tokio::test]
async fn merge_can_delete_the_branch_in_the_same_push() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.app
        .ops
        .merge("demo", "part-2", &bed.actor(), false, true)
        .await
        .expect("merge succeeds");
    let bare = bed.remote_root().join("demo.git");
    let refs = git(&bare, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"]);
    assert!(!refs.contains("part-2"), "part-2 should be gone: {refs}");
}

#[tokio::test]
async fn restack_rebases_a_two_child_stack_in_order() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let old_part1 = bed.remote_tip("demo", "part-1");
    let old_part2 = bed.remote_tip("demo", "part-2");

    // Move main (non-conflicting), then restack everything under it.
    let work = Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("docs/other.txt", "moved\n");
    work.commit_all("main moves");
    work.push("main");

    let outcome = bed
        .app
        .ops
        .restack("demo", "main", &bed.actor(), )
        .await
        .expect("restack succeeds");

    // Parents rebase before their children.
    let order: Vec<&str> = outcome.rebased.iter().map(|(b, _, _)| b.as_str()).collect();
    let p1 = order.iter().position(|b| *b == "part-1").expect("part-1 restacked");
    let p2 = order.iter().position(|b| *b == "part-2").expect("part-2 restacked");
    assert!(p1 < p2);

    // Every tip moved, and the stack shape holds on the remote.
    let new_main = bed.remote_tip("demo", "main");
    let new_part1 = bed.remote_tip("demo", "part-1");
    let new_part2 = bed.remote_tip("demo", "part-2");
    assert_ne!(new_part1, old_part1);
    assert_ne!(new_part2, old_part2);
    let bare = bed.remote_root().join("demo.git");
    let is_ancestor = |a: &str, b: &str| {
        std::process::Command::new("git")
            .current_dir(&bare)
            .args(["merge-base", "--is-ancestor", a, b])
            .status()
            .expect("git runs")
            .success()
    };
    assert!(is_ancestor(&new_main, &new_part1));
    assert!(is_ancestor(&new_part1, &new_part2));

    // The graph re-infers the same chain afterwards.
    bed.mirrors.refresh_now("demo").await;
    let graph = StackGraph::infer(&bed.mirrors.repo("demo")).await.expect("graph");
    assert_eq!(graph.get("part-2").unwrap().parent.as_deref(), Some("part-1"));
}

#[tokio::test]
async fn a_restack_conflict_aborts_cleanly_leaving_every_branch_untouched() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before_part1 = bed.remote_tip("demo", "part-1");
    let before_part2 = bed.remote_tip("demo", "part-2");

    // Conflict: main rewrites the same line part-1 changed.
    let work = Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("src/app.txt", "conflicting\n");
    work.commit_all("main conflicts with part-1");
    work.push("main");
    let main_after_conflict = bed.remote_tip("demo", "main");

    let result = bed.app.ops.restack("demo", "main", &bed.actor()).await;
    let Err(OpError::Conflict { branch, files }) = result else {
        panic!("expected a conflict, got {result:?}");
    };
    assert_eq!(branch, "part-1");
    assert!(files.iter().any(|f| f == "src/app.txt"), "conflicting file list: {files:?}");

    // Nothing was force-pushed: every branch exactly where it was.
    assert_eq!(bed.remote_tip("demo", "part-1"), before_part1);
    assert_eq!(bed.remote_tip("demo", "part-2"), before_part2);
    assert_eq!(bed.remote_tip("demo", "main"), main_after_conflict);
}

#[tokio::test]
async fn merging_flips_the_branchs_card_to_done_in_the_same_push() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        // A card that owns part-1, not yet done, with a body that must not change.
        work.write(
            "tasks/ship-part-1.md",
            "---\nstatus: doing\ntitle: Ship part 1\nbranch: part-1\n---\n\nBody stays byte-identical.\n",
        );
        work.commit_all("add card");
        work.push("main");
        work
    });

    bed.app
        .ops
        .merge("demo", "part-1", &bed.actor(), false, false)
        .await
        .expect("merge succeeds");

    let bare = bed.remote_root().join("demo.git");
    let card = git(&bare, &["show", "refs/heads/main:tasks/ship-part-1.md"]);
    assert!(card.contains("status: done"), "card not flipped: {card}");
    assert!(card.contains("Body stays byte-identical."));
    // The flip is its own commit, authored by the merging user, and the audit says so.
    let last_msg = git(&bare, &["log", "-1", "--format=%s%n%an", "refs/heads/main"]);
    assert!(last_msg.contains("Mark done"), "{last_msg}");
    assert!(last_msg.contains("Tester"), "{last_msg}");
    let audit = bed.db.audit("demo", 5).expect("audit");
    assert!(audit.iter().any(|e| e.detail.contains("tasks/ship-part-1.md")));
}

/// The bug this guards: pushes that rewrite or delete a ref used to go out with a bare
/// `--force`, so work pushed after the viewer last looked was silently destroyed. Every
/// such push now carries `--force-with-lease` against the tip we actually read, so a
/// branch that moved under us is rejected instead of thrown away.
#[tokio::test]
async fn deleting_a_branch_that_moved_under_us_is_rejected_not_forced() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    // Someone pushes to part-2 without the viewer noticing: the mirror still holds the
    // old tip, which is exactly the stale-read window a lease exists to catch.
    let other = Work::clone_from(&bed.remote_root().join("demo.git"));
    other.checkout("part-2");
    other.write("part-2-extra.txt", "written by someone else\n");
    let racing_commit = other.commit_all("concurrent work on part-2");
    other.push("part-2");

    let outcome = bed.app.ops.delete_branch("demo", "part-2", &bed.actor()).await;

    assert!(outcome.is_err(), "the delete must not discard the concurrent push");
    assert_eq!(
        bed.remote_tip("demo", "part-2"),
        racing_commit,
        "the branch and its new commit survived"
    );
}

/// The same guard, from the other side: when nothing raced us, the delete goes through.
#[tokio::test]
async fn deleting_an_unmoved_branch_still_works() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    bed.app
        .ops
        .delete_branch("demo", "part-2", &bed.actor())
        .await
        .expect("delete succeeds when the lease matches");

    let bare = bed.remote_root().join("demo.git");
    let still_there = std::process::Command::new("git")
        .current_dir(&bare)
        .args(["rev-parse", "--verify", "refs/heads/part-2"])
        .status()
        .expect("git runs")
        .success();
    assert!(!still_there, "the branch is gone from the remote");
}
