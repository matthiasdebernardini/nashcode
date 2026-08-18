//! Stale while revalidate: a page renders from the mirror on disk and the fetch runs
//! behind it. The one request that waits is the first view of a repo with no mirror.

mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{TestBed, simple_bed, stacked_fixture};
use nashcode::config::Config;
use nashcode::mirror::Mirrors;

/// A listener that accepts connections and answers nothing, so a git fetch against it
/// hangs. This is the real remote's 4-6 second fetch, made slow enough to measure.
struct BlackHole {
    port: u16,
    hits: Arc<AtomicUsize>,
}

impl BlackHole {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            // Hold every accepted socket open: closing one would let git fail fast.
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
            }
        });
        Self { port, hits }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// How many fetches have reached the wire.
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Wait for the first connection, up to `limit`.
    async fn wait_for_a_hit(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self.hits() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// A second mirror pool over the same mirrors on disk, pointed at another URL. This is
/// how a real viewer starts up against a remote that has since gone bad.
fn mirrors_pointing_at(bed: &TestBed, dgit_url: String) -> Mirrors {
    let config = Arc::new(Config { dgit_url, ..(*bed.config).clone() });
    Mirrors::new(config, bed.db.clone())
}

#[tokio::test]
async fn an_existing_mirror_answers_at_once_while_the_fetch_runs_behind_it() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    assert!(bed.config.mirror_path("demo").exists(), "mirror cloned");

    let hole = BlackHole::open();
    let mirrors = mirrors_pointing_at(&bed, hole.url());

    let started = Instant::now();
    let status = mirrors.refresh("demo").await;
    let waited = started.elapsed();

    assert!(waited < Duration::from_millis(500), "refresh waited {waited:?} for the fetch");
    assert!(status.available, "the mirror on disk answers");
    assert!(!status.stale, "a fetch still in flight has failed nothing, so no stale banner");

    // The fetch really did start; it is just not the page's problem.
    hole.wait_for_a_hit(Duration::from_secs(10)).await;
    assert_eq!(hole.hits(), 1, "the background fetch reached the remote");
}

#[tokio::test]
async fn only_one_fetch_per_repo_is_ever_in_flight() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    let hole = BlackHole::open();
    let mirrors = mirrors_pointing_at(&bed, hole.url());

    // A burst of page loads, each past the debounce as far as it knows.
    for _ in 0..5 {
        let started = Instant::now();
        mirrors.refresh("demo").await;
        assert!(started.elapsed() < Duration::from_millis(500), "every request stays fast");
    }

    hole.wait_for_a_hit(Duration::from_secs(10)).await;
    // Give any wrongly spawned second fetch time to show up.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hole.hits(), 1, "one fetch for the repo, not one per request");
}

#[tokio::test]
async fn a_repo_with_no_mirror_yet_blocks_until_the_clone_lands() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    assert!(!bed.config.mirror_path("demo").exists(), "nothing on disk yet");

    let status = bed.mirrors.refresh("demo").await;

    assert!(status.available);
    assert!(!status.stale);
    assert!(bed.config.mirror_path("demo").exists(), "the clone finished before refresh returned");
    let branches = bed.mirrors.repo("demo").branches().await.expect("branches");
    assert!(branches.contains(&"part-2".to_owned()), "the first view has content: {branches:?}");
}

#[tokio::test]
async fn a_failed_background_fetch_shows_up_as_stale_on_the_next_request() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;

    // Nothing listens on port 9, so the fetch fails as fast as the network can say no.
    let mirrors = mirrors_pointing_at(&bed, "http://127.0.0.1:9".to_owned());

    let mut status = mirrors.refresh("demo").await;
    assert!(!status.stale, "the first request renders before the fetch has an answer");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !status.stale && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        status = mirrors.refresh("demo").await;
    }

    assert!(status.stale, "the failed fetch surfaces on a later request");
    assert!(status.available, "the mirror on disk still answers");
    assert!(status.message.is_some(), "the banner says why");
}
