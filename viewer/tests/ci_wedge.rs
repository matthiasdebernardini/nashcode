//! Invariant: no CI run blocks a merge forever.
//!
//! Three ways a run stops being a run: the process that owned it died, the worker
//! that owned it died, or a person put it back on the queue. None of them may leave
//! a branch with a gate nothing can satisfy.

mod common;

use std::path::Path;

use common::{get, post_json, simple_bed, stacked_fixture};
use nashcode::db::{Db, ORPHANED, now_offset, status};
use nashcode::ops::OpError;

/// Age a run's heartbeat by writing straight to the file the testbed's `Db` holds.
/// Nothing in the public API can backdate a timestamp, and waiting five minutes is
/// not a test.
fn backdate(db_path: &Path, run: i64, seconds: i64) {
    let conn = rusqlite::Connection::open(db_path).expect("the db file opens");
    conn.execute(
        "UPDATE ci_runs SET updated_at = ?2 WHERE id = ?1",
        rusqlite::params![run, now_offset(-seconds)],
    )
    .expect("heartbeat backdated");
}

#[test]
fn runs_in_flight_do_not_survive_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nashcode.db");

    let running = {
        let db = Db::open(&path).expect("db opens");
        let running = db.enqueue_run("demo", "main", "abc").expect("run recorded");
        db.set_run_status(running, status::RUNNING, 0, None).expect("status set");
        // A second run that never left the queue.
        db.enqueue_run("demo", "main", "def").expect("run recorded");
        running
    };

    // The process dies here. The next one opens the same file.
    let db = Db::open(&path).expect("db reopens");

    let recovered = db.latest_run("demo", "abc").expect("query").expect("the row is kept");
    assert_eq!(recovered.id, running);
    assert_eq!(recovered.status, status::ERROR, "a run nobody is executing is not running");
    assert_eq!(recovered.note, ORPHANED, "the row says why");

    let queued = db.latest_run("demo", "def").expect("query").expect("the row is kept");
    assert_eq!(queued.status, status::ERROR, "a queue that no longer exists cannot deliver");
}

#[tokio::test]
async fn a_run_whose_heartbeat_stopped_does_not_block_a_merge() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let tip = bed.remote_tip("demo", "part-1");
    let run = bed.db.enqueue_run("demo", "part-1", &tip).expect("run recorded");
    bed.db.set_run_status(run, status::RUNNING, 0, None).expect("status set");

    // While the heartbeat is fresh, the merge waits — exactly as it always did.
    let blocked = bed.app.ops.merge("demo", "part-1", &bed.actor(), false, false).await;
    assert!(matches!(blocked, Err(OpError::Blocked(_))), "a live run blocks");

    // The worker dies. Nothing rewrites the row; the heartbeat just ages out.
    backdate(&bed.config.db_path, run, status::HEARTBEAT_STALE_SECS + 60);

    let stale = bed.db.latest_run("demo", &tip).expect("query").expect("the row is kept");
    assert_eq!(stale.status, status::RUNNING, "the stored row is untouched");
    assert_eq!(stale.effective_status(), status::STUCK, "what it means has changed");
    assert!(!status::blocks_merge(Some(stale.effective_status())));

    bed.app
        .ops
        .merge("demo", "part-1", &bed.actor(), false, false)
        .await
        .expect("a stuck run cannot wedge the merge");
    assert_eq!(bed.remote_tip("demo", "main"), tip);
}

#[tokio::test]
async fn requeue_puts_a_stuck_run_back_on_the_queue() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let tip = bed.mirrors.repo("demo").tip("main").await.expect("tip");
    let run = bed.db.enqueue_run("demo", "main", &tip).expect("run recorded");
    bed.db.set_run_status(run, status::RUNNING, 0, None).expect("status set");
    backdate(&bed.config.db_path, run, status::HEARTBEAT_STALE_SECS + 60);

    // The page says stuck, not failed, and offers the way out.
    let (code, page) = get(&bed.router, "/demo/main/ci").await;
    assert_eq!(code, 200);
    assert!(page.contains("stuck"), "stuck reads differently from failed");
    assert!(page.contains("Requeue this run"), "the escape is on the page");

    let (code, body) = post_json(&bed.router, "/demo/main/ci/requeue", serde_json::json!({})).await;
    assert_eq!(code, 200, "{body}");

    let back = bed.db.latest_run("demo", &tip).expect("query").expect("the row is kept");
    assert_eq!(back.id, run, "the same row, not a new one");
    assert_eq!(back.status, status::QUEUED);
    assert_eq!(back.commit, tip, "requeued against the commit it was answering for");

    let (code, page) = get(&bed.router, "/demo/main/ci").await;
    assert_eq!(code, 200);
    assert!(!page.contains("Requeue this run"), "nothing left to rescue");
}
