//! Plans render + raw bytes, the board's three columns and move endpoint, and the
//! link graph: plan <-> card <-> branch, with dangling refs rendered as missing.

mod common;

use common::{Work, get, git, post_json, simple_bed, stacked_fixture};

fn linked_fixture(root: &std::path::Path) -> Work {
    let work = stacked_fixture(root, "demo");
    work.write(
        "plans/api.md",
        "---\nbranch: part-1\ntasks: [tasks/build.md, tasks/missing.md]\n---\n# API plan\n\nSee tasks/build.md and the branch `part-1`.\n",
    );
    work.write(
        "tasks/build.md",
        "---\nstatus: doing\ntitle: Build the API\nbranch: part-1\nplan: plans/api.md\nassignee: ada\n---\n\nCard body.\n",
    );
    work.write("tasks/todo-item.md", "---\nstatus: todo\n---\n# Later\n\nSoon.\n");
    work.write("tasks/done-item.md", "---\nstatus: done\n---\n# Shipped\n\nDone.\n");
    work.write("tasks/broken.md", "---\nstatus: [unclosed\n---\n\nStill readable.\n");
    work.commit_all("plans and cards");
    work.push("main");
    work
}

#[tokio::test]
async fn plans_list_and_render_and_raw_returns_exact_bytes() {
    let bed = simple_bed(linked_fixture);

    let (status, list) = get(&bed.router, "/demo/plans").await;
    assert_eq!(status, 200);
    assert!(list.contains("API plan"), "{list}");

    let (status, page) = get(&bed.router, "/demo/plans/api.md").await;
    assert_eq!(status, 200);
    assert!(page.contains("markdown-body"));
    assert!(page.contains("API plan"));

    // Raw: the file bytes, unchanged, text/plain.
    let original =
        "---\nbranch: part-1\ntasks: [tasks/build.md, tasks/missing.md]\n---\n# API plan\n\nSee tasks/build.md and the branch `part-1`.\n";
    let (status, raw) = get(&bed.router, "/demo/raw/main/plans/api.md").await;
    assert_eq!(status, 200);
    assert_eq!(raw, original, "raw bytes must be verbatim");

    let (status, _) = get(&bed.router, "/demo/raw/main/plans/ghost.md").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_board_shows_three_columns_and_quarantines_bad_front_matter() {
    let bed = simple_bed(linked_fixture);
    let (status, board) = get(&bed.router, "/demo/board").await;
    assert_eq!(status, 200);
    for column in ["todo", "doing", "done", "needs-attention"] {
        assert!(
            board.contains(&format!("data-status=\"{column}\"")),
            "column {column} missing"
        );
    }
    assert!(board.contains("Build the API"));
    // The malformed card is on the board, not crashing it.
    assert!(board.contains("tasks/broken.md"));
    assert!(board.contains("data-nodrop=\"true\""));
}

#[tokio::test]
async fn the_move_endpoint_rewrites_only_the_status_in_exactly_one_commit() {
    let bed = simple_bed(linked_fixture);
    bed.mirrors.refresh("demo").await;
    let bare = bed.remote_root().join("demo.git");
    let commits_before = git(&bare, &["rev-list", "--count", "refs/heads/main"]);

    let (status, body) = post_json(
        &bed.router,
        "/demo/board/move",
        serde_json::json!({ "file": "tasks/todo-item.md", "status": "doing" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let commits_after = git(&bare, &["rev-list", "--count", "refs/heads/main"]);
    assert_eq!(
        commits_before.trim().parse::<u64>().unwrap() + 1,
        commits_after.trim().parse::<u64>().unwrap(),
        "exactly one commit"
    );
    // Only the status line changed; the body is byte-identical.
    let moved = git(&bare, &["show", "refs/heads/main:tasks/todo-item.md"]);
    assert_eq!(moved, "---\nstatus: doing\n---\n# Later\n\nSoon.\n");

    // Invalid moves are refused.
    let (status, _) = post_json(
        &bed.router,
        "/demo/board/move",
        serde_json::json!({ "file": "src/app.txt", "status": "doing" }),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = post_json(
        &bed.router,
        "/demo/board/move",
        serde_json::json!({ "file": "tasks/todo-item.md", "status": "needs attention!" }),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn links_wire_plan_card_and_branch_in_both_directions() {
    let bed = simple_bed(linked_fixture);

    // The branch page is the hub: it shows the card and plan that declare it.
    let (_, branch_page) = get(&bed.router, "/demo/part-1").await;
    assert!(branch_page.contains("Build the API"), "card missing from branch page");
    assert!(branch_page.contains("API plan"), "plan missing from branch page");

    // The card shows its plan and its branch.
    let (_, card_page) = get(&bed.router, "/demo/tasks/build.md").await;
    assert!(card_page.contains("plans/api.md") || card_page.contains("API plan"));
    assert!(card_page.contains("/demo/part-1"), "branch link missing from card");

    // The plan shows the card that references it, and its dangling task renders as
    // missing without breaking the page.
    let (status, plan_page) = get(&bed.router, "/demo/plans/api.md").await;
    assert_eq!(status, 200);
    assert!(plan_page.contains("Build the API"), "backlink missing from plan");
    assert!(plan_page.contains("missing"), "dangling ref must be marked missing");

    // Path autolinking in the rendered body: tasks/build.md became a link, and the
    // backticked branch links to the PR view.
    assert!(
        plan_page.contains("<a href=\"/demo/tasks/build.md\">"),
        "path autolink missing: {plan_page}"
    );
    assert!(
        plan_page.contains("<a href=\"/demo/part-1\"><code>part-1</code></a>"),
        "branch autolink missing"
    );
}
