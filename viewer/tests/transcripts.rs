//! The transcript ingest endpoint: a meeting POSTed by the browser extension lands
//! as one committed markdown file on the default branch.

mod common;

use common::{git, post_json, simple_bed, stacked_fixture};

fn payload() -> serde_json::Value {
    serde_json::json!({
        "title": "Weekly Sync: Rob & Matthias",
        "started_at": "2026-06-12T15:00:00Z",
        "ended_at": "2026-06-12T15:30:00Z",
        "meeting_url": "https://meet.google.com/abc-defg-hij",
        "provider": "grok-batch",
        "speakers": [
            { "id": "S1", "name": "Matthias", "channel": "mic" },
            { "id": "S2", "channel": "tab" }
        ],
        "speakers_confirmed": true,
        "calendar_event": {
            "id": "evt123",
            "title": "Weekly Sync",
            "attendees": [{ "name": "Jane Doe", "email": "jane@acme.com" }]
        },
        "action_items": [{ "title": "Send the follow-up deck", "owner": "Matthias" }],
        "segments": [
            { "speaker": "S1", "start_ms": 5000, "end_ms": 9000, "text": "Morning everyone." },
            { "speaker": "S2", "start_ms": 9500, "end_ms": 12000, "text": "Hey!" }
        ]
    })
}

#[tokio::test]
async fn a_posted_transcript_is_committed_and_a_repeat_does_not_overwrite_it() {
    let bed = simple_bed(|root: &std::path::Path| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let bare = bed.remote_root().join("demo.git");

    let (status, body) = post_json(&bed.router, "/demo/transcripts", payload()).await;
    assert_eq!(status, 201, "{body}");
    let filed: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(filed["ok"], true);
    assert_eq!(filed["id"], "2026-06-12-1500-weekly-sync-rob-matthias");
    assert_eq!(
        filed["path"],
        "transcripts/2026/06/2026-06-12-1500-weekly-sync-rob-matthias.md"
    );
    assert!(filed["commit"].as_str().is_some_and(|c| c.len() == 40), "{body}");

    let filed_path = filed["path"].as_str().unwrap();
    let md = git(&bare, &["show", &format!("refs/heads/main:{filed_path}")]);
    assert!(md.contains("id: \"2026-06-12-1500-weekly-sync-rob-matthias\""), "{md}");
    assert!(md.contains("speakers_confirmed: true"), "{md}");
    assert!(md.contains("digested: false"), "{md}");
    assert!(md.contains("- [ ] Send the follow-up deck (Matthias)"), "{md}");
    assert!(md.contains("**Matthias** [00:05]: Morning everyone."), "{md}");
    assert!(md.contains("**Speaker 2** [00:09]: Hey!"), "{md}");

    let message = git(&bare, &["log", "-1", "--format=%s", "refs/heads/main"]);
    assert_eq!(message.trim(), "meeting: 2026-06-12-1500-weekly-sync-rob-matthias");

    // The same meeting filed twice keeps both, the second under a -2 suffix.
    let (status, body) = post_json(&bed.router, "/demo/transcripts", payload()).await;
    assert_eq!(status, 201, "{body}");
    let second: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(second["id"], "2026-06-12-1500-weekly-sync-rob-matthias-2");
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "refs/heads/main", "transcripts/"]);
    assert_eq!(listing.lines().count(), 2, "{listing}");
}

#[tokio::test]
async fn an_unfilable_payload_is_refused_before_anything_is_committed() {
    let bed = simple_bed(|root: &std::path::Path| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    let before = bed.remote_tip("demo", "main");

    let mut bad = payload();
    bad["segments"][0]["speaker"] = serde_json::json!("S9");
    let (status, body) = post_json(&bed.router, "/demo/transcripts", bad).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("unknown speaker 'S9'"), "{body}");

    let mut bad = payload();
    bad["started_at"] = serde_json::json!("yesterday");
    let (status, _) = post_json(&bed.router, "/demo/transcripts", bad).await;
    assert_eq!(status, 400);

    assert_eq!(before, bed.remote_tip("demo", "main"), "a refused transcript still pushed");
}
