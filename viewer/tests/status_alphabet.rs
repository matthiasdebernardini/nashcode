//! One status rule, applied at both doors. A card pushed with a status outside the
//! alphabet — or squatting on the reserved `needs-attention` column — is quarantined by
//! the parser, exactly as the move endpoint would have refused it.

mod common;

use common::{Work, get, post_json, simple_bed, stacked_fixture};

fn bad_status_fixture(root: &std::path::Path) -> Work {
    let work = stacked_fixture(root, "demo");
    work.write("tasks/good.md", "---\nstatus: todo\n---\n# Fine\n\nBody.\n");
    work.write("tasks/shouty.md", "---\nstatus: \"Bad Status\"\n---\n# Shouty\n\nBody.\n");
    work.write("tasks/reserved.md", "---\nstatus: needs-attention\n---\n# Squatter\n\nBody.\n");
    work.commit_all("cards with bad statuses");
    work.push("main");
    work
}

#[tokio::test]
async fn a_pushed_card_with_an_out_of_alphabet_status_is_quarantined() {
    let bed = simple_bed(bad_status_fixture);

    let (status, board) = get(&bed.router, "/demo/board").await;
    assert_eq!(status, 200);

    // Neither bad card invented a column of its own.
    assert!(
        !board.contains("data-status=\"bad status\""),
        "a pushed status outside the alphabet became a column: {board}"
    );
    assert!(board.contains("data-status=\"needs-attention\""), "quarantine column missing");

    // Both bad cards are on the board, in quarantine, with the reason shown.
    assert!(board.contains("tasks/shouty.md"), "quarantined card missing from board");
    assert!(board.contains("tasks/reserved.md"), "reserved-status card missing from board");
    assert_eq!(
        board.matches("invalid status").count(),
        2,
        "both bad cards must carry the invalid-status error: {board}"
    );

    // The good card is untouched.
    assert!(board.contains("tasks/good.md"));

    // The card page names the problem too.
    let (status, page) = get(&bed.router, "/demo/tasks/reserved.md").await;
    assert_eq!(status, 200);
    assert!(
        page.contains("Front matter problem: invalid status"),
        "card page must explain the quarantine: {page}"
    );
}

#[tokio::test]
async fn the_move_endpoint_refuses_the_same_statuses_the_parser_quarantines() {
    let bed = simple_bed(bad_status_fixture);
    bed.mirrors.refresh("demo").await;

    for bad in ["Bad Status", "needs-attention", ""] {
        let (status, body) = post_json(
            &bed.router,
            "/demo/board/move",
            serde_json::json!({ "file": "tasks/good.md", "status": bad }),
        )
        .await;
        assert_eq!(status, 400, "move to {bad:?} must be refused: {body}");
    }
}
