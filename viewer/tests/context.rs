//! The context store: a meeting, an email, a chat, or a note POSTed to
//! `/{repo}/context/{kind}` lands as one committed markdown file on the default
//! branch, and the list and get endpoints read it back at the tip.

mod common;

use common::{TestBed, get, git, post_json, simple_bed, stacked_fixture};

fn meeting() -> serde_json::Value {
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

fn item(title: &str, at: &str, text: &str, source: Option<&str>) -> serde_json::Value {
    let mut value = serde_json::json!({ "title": title, "at": at, "text": text });
    if let Some(source) = source {
        value["source"] = serde_json::json!(source);
    }
    value
}

/// POST and unwrap the JSON answer.
async fn put(bed: &TestBed, kind: &str, payload: serde_json::Value) -> (u16, serde_json::Value) {
    let (status, body) = post_json(&bed.router, &format!("/demo/context/{kind}"), payload).await;
    let value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("json body: {e}\n{body}"));
    (status, value)
}

async fn bed() -> TestBed {
    let bed = simple_bed(|root: &std::path::Path| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    bed
}

#[tokio::test]
async fn every_kind_files_one_committed_markdown_file() {
    let bed = bed().await;
    let bare = bed.remote_root().join("demo.git");

    // A meeting: the extension's payload, rendered as front matter, action items, turns.
    let (status, filed) = put(&bed, "meeting", meeting()).await;
    assert_eq!(status, 201, "{filed}");
    assert_eq!(filed["ok"], true);
    let id = filed["id"].as_str().expect("an id").to_owned();
    assert!(id.starts_with("2026-06-12-1500-weekly-sync-rob-matthias-"), "{id}");
    assert_eq!(filed["path"], format!("context/meeting/2026/06/{id}.md"));
    assert!(filed["commit"].as_str().is_some_and(|c| c.len() == 40), "{filed}");

    let md = show(&bare, filed["path"].as_str().unwrap());
    assert!(md.contains("kind: \"meeting\""), "{md}");
    assert!(md.contains(&format!("id: {id:?}")), "{md}");
    assert!(md.contains("at: \"2026-06-12T15:00:00Z\""), "{md}");
    assert!(md.contains("source: \"https://meet.google.com/abc-defg-hij\""), "{md}");
    assert!(md.contains("speakers_confirmed: true"), "{md}");
    assert!(md.contains("entities: []"), "{md}");
    assert!(md.contains("digested: false"), "{md}");
    assert!(md.contains("- [ ] Send the follow-up deck (Matthias)"), "{md}");
    assert!(md.contains("**Matthias** [00:05]: Morning everyone."), "{md}");
    assert!(md.contains("**Speaker 2** [00:09]: Hey!"), "{md}");
    assert!(
        ingested_at(&md).as_str() > "2026-06-12T15:00:00Z",
        "the server clock stamps it: {md}"
    );

    let message = git(&bare, &["log", "-1", "--format=%s", "refs/heads/main"]);
    assert_eq!(message.trim(), format!("context: meeting {id}"));

    // An email, a chat, and a note: title, time, and the text verbatim.
    for (kind, title, text) in [
        ("email", "Re: invoice", "The invoice is paid. Thanks."),
        ("chat", "rob 2026-06-13", "rob: shipping tonight"),
        ("note", "Parking lot", "Ask about the DB move."),
    ] {
        let (status, filed) =
            put(&bed, kind, item(title, "2026-06-13T09:05:00Z", text, None)).await;
        assert_eq!(status, 201, "{filed}");
        let path = filed["path"].as_str().expect("a path").to_owned();
        assert!(path.starts_with(&format!("context/{kind}/2026/06/")), "{path}");
        let md = show(&bare, &path);
        assert!(md.contains(&format!("kind: {kind:?}")), "{md}");
        assert!(md.contains(&format!("title: {title:?}")), "{md}");
        assert!(md.contains("at: \"2026-06-13T09:05:00Z\""), "{md}");
        assert!(!md.contains("source:"), "no source was given: {md}");
        assert!(md.ends_with(&format!("# {title}\n\n{text}\n")), "{md}");
    }
}

#[tokio::test]
async fn a_kind_the_store_does_not_have_is_refused_and_commits_nothing() {
    let bed = bed().await;
    let before = bed.remote_tip("demo", "main");

    let (status, body) = post_json(
        &bed.router,
        "/demo/context/voicemail",
        item("Hi", "2026-06-13T09:05:00Z", "words", None),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("no context kind named 'voicemail'"), "{body}");
    assert!(body.contains("meeting, email, chat, note"), "{body}");
    assert_eq!(before, bed.remote_tip("demo", "main"), "a refused kind still pushed");
}

#[tokio::test]
async fn a_sourced_put_commits_once_and_the_second_says_it_is_already_there() {
    let bed = bed().await;

    let (status, first) =
        put(&bed, "email", item("Re: invoice", "2026-06-13T09:05:00Z", "Paid.", Some("18f2a")))
            .await;
    assert_eq!(status, 201, "{first}");
    let tip = bed.remote_tip("demo", "main");

    let (status, second) =
        put(&bed, "email", item("Re: invoice", "2026-06-13T09:05:00Z", "Paid.", Some("18f2a")))
            .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["existing"], true);
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["path"], first["path"]);
    assert!(second.get("commit").is_none(), "{second}");
    assert_eq!(tip, bed.remote_tip("demo", "main"), "the repeat committed");

    // Only the source decides the name: a different message id files a second file
    // even with the same subject and the same minute.
    let (status, other) =
        put(&bed, "email", item("Re: invoice", "2026-06-13T09:05:00Z", "Paid.", Some("99zzz")))
            .await;
    assert_eq!(status, 201, "{other}");
    assert_ne!(other["id"], first["id"]);

    let bare = bed.remote_root().join("demo.git");
    let listing = git(&bare, &["ls-tree", "-r", "--name-only", "refs/heads/main", "context/"]);
    assert_eq!(listing.lines().count(), 2, "{listing}");
}

#[tokio::test]
async fn an_unsourced_repeat_gets_a_suffix_rather_than_overwriting_the_first() {
    let bed = bed().await;
    let payload = || item("Standup", "2026-06-13T09:05:00Z", "Same minute, same title.", None);

    let (status, first) = put(&bed, "note", payload()).await;
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["id"], "2026-06-13-0905-standup");
    let (status, second) = put(&bed, "note", payload()).await;
    assert_eq!(status, 201, "{second}");
    assert_eq!(second["id"], "2026-06-13-0905-standup-2");
}

#[tokio::test]
async fn a_since_walk_repeats_nothing_and_misses_no_backfill() {
    let bed = bed().await;

    // Two items in the same `at` minute, then a backfill: older `at`, newer
    // `ingested_at`. The cursor rides on ingest, so the backfill comes last.
    put(&bed, "note", item("Alpha", "2026-06-13T09:05:00Z", "first", None)).await;
    put(&bed, "email", item("Beta", "2026-06-13T09:05:00Z", "second", Some("m1"))).await;
    put(&bed, "chat", item("Old", "2026-01-02T08:00:00Z", "backfilled", Some("m2"))).await;

    let (status, body) = get(&bed.router, "/demo/context").await;
    assert_eq!(status, 200, "{body}");
    let page: serde_json::Value = serde_json::from_str(&body).expect("json");
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 3, "{body}");
    assert_eq!(items[2]["title"], "Old", "the backfill is newest by ingest: {body}");
    assert_eq!(items[2]["at"], "2026-01-02T08:00:00Z");
    assert_eq!(items[0]["digested"], false);
    assert!(items[0]["entities"].as_array().is_some_and(|e| e.is_empty()));
    assert_eq!(items[1]["source"], "m1");
    assert_eq!(items[0]["source"], serde_json::Value::Null);

    // Ordered by the cursor, and the cursor is what `next_since` hands back.
    let cursors: Vec<&str> =
        items.iter().map(|i| i["ingested_at"].as_str().expect("ingested_at")).collect();
    assert!(cursors.windows(2).all(|w| w[0] <= w[1]), "{body}");
    assert_eq!(page["next_since"], format!("{}|chat/{}", cursors[2], items[2]["id"].as_str().unwrap()));

    // Walk it one page at a time from the start: every item once, in order.
    let mut seen: Vec<String> = Vec::new();
    let mut since = String::new();
    loop {
        let (status, body) =
            get(&bed.router, &format!("/demo/context?since={}", urlencode(&since))).await;
        assert_eq!(status, 200, "{body}");
        let page: serde_json::Value = serde_json::from_str(&body).expect("json");
        let items = page["items"].as_array().expect("items").clone();
        if items.is_empty() {
            // An empty page leaves the cursor exactly where it was.
            assert_eq!(page["next_since"], since, "{body}");
            break;
        }
        for item in &items {
            seen.push(item["id"].as_str().expect("id").to_owned());
        }
        since = page["next_since"].as_str().expect("next_since").to_owned();
    }
    assert_eq!(seen.len(), 3, "{seen:?}");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "an item came back twice: {seen:?}");

    // `kind` narrows without disturbing the cursor's meaning.
    let (status, body) = get(&bed.router, "/demo/context?kind=email").await;
    assert_eq!(status, 200, "{body}");
    let page: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(page["items"].as_array().expect("items").len(), 1, "{body}");
    assert_eq!(page["items"][0]["kind"], "email");

    let (status, body) = get(&bed.router, "/demo/context?kind=voicemail").await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn one_item_reads_back_as_its_front_matter_and_its_body() {
    let bed = bed().await;
    let (_, filed) =
        put(&bed, "email", item("Re: invoice", "2026-06-13T09:05:00Z", "Paid in full.", Some("18f2a")))
            .await;
    let id = filed["id"].as_str().expect("an id");

    let (status, body) = get(&bed.router, &format!("/demo/context/email/{id}")).await;
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["kind"], "email");
    assert_eq!(value["id"], id);
    assert_eq!(value["title"], "Re: invoice");
    assert_eq!(value["at"], "2026-06-13T09:05:00Z");
    assert_eq!(value["source"], "18f2a");
    assert_eq!(value["digested"], false);
    assert_eq!(value["path"], filed["path"]);
    assert_eq!(value["body"], "# Re: invoice\n\nPaid in full.\n");

    let (status, _) = get(&bed.router, "/demo/context/email/nobody-filed-this").await;
    assert_eq!(status, 404);
    let (status, _) = get(&bed.router, &format!("/demo/context/voicemail/{id}")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn an_unfilable_payload_is_refused_before_anything_is_committed() {
    let bed = bed().await;
    let before = bed.remote_tip("demo", "main");

    let mut bad = meeting();
    bad["segments"][0]["speaker"] = serde_json::json!("S9");
    let (status, body) = post_json(&bed.router, "/demo/context/meeting", bad).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("unknown speaker 'S9'"), "{body}");

    let mut bad = meeting();
    bad["started_at"] = serde_json::json!("yesterday");
    let (status, _) = post_json(&bed.router, "/demo/context/meeting", bad).await;
    assert_eq!(status, 400);

    let (status, body) = post_json(
        &bed.router,
        "/demo/context/email",
        item("Re: nothing", "2026-06-13T09:05:00Z", "   ", None),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("text is empty"), "{body}");

    let (status, body) = post_json(
        &bed.router,
        "/demo/context/note",
        item("Undated", "sometime", "words", None),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("RFC3339"), "{body}");

    assert_eq!(before, bed.remote_tip("demo", "main"), "a refused item still pushed");
}

/// `/transcripts` is removed, not aliased.
#[tokio::test]
async fn the_old_transcripts_route_is_gone() {
    let bed = bed().await;
    let (status, _) = post_json(&bed.router, "/demo/transcripts", meeting()).await;
    assert_ne!(status, 201, "the old route still files transcripts");
}

fn show(bare: &std::path::Path, path: &str) -> String {
    git(bare, &["show", &format!("refs/heads/main:{path}")])
}

fn ingested_at(md: &str) -> String {
    md.lines()
        .find_map(|line| line.strip_prefix("ingested_at: "))
        .map(|value| value.trim_matches('"').to_owned())
        .expect("front matter carries ingested_at")
}

/// Enough percent-encoding for a cursor in a query string.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
