//! The embedded asset tree. The JS bundle is code-split, so the entry is only useful
//! if its sibling chunks are served from the same directory.

mod common;

use common::{get, simple_bed, stacked_fixture};

#[tokio::test]
async fn the_js_entry_and_its_chunks_are_served_from_one_directory() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));

    let (status, entry) = get(&bed.router, "/assets/nashcode.js").await;
    assert_eq!(status, 200);

    // esbuild emits relative imports; served from /assets/ they resolve to siblings.
    let chunk = entry
        .split("\"./")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the entry imports at least one chunk");
    assert!(chunk.starts_with("chunk-"), "unexpected import target: {chunk}");

    let (status, body) = get(&bed.router, &format!("/assets/{chunk}")).await;
    assert_eq!(status, 200, "{chunk} not served");
    assert!(!body.is_empty());

    let (status, _) = get(&bed.router, "/assets/nashcode.css").await;
    assert_eq!(status, 200);
}

/// The two client-side mermaid controls are load-bearing security/perf layers with no
/// other coverage: strict mode sanitizes rendered SVG, and the dynamic import keeps
/// the ~700 KiB library off every non-architecture page.
#[tokio::test]
async fn mermaid_stays_strict_and_out_of_the_entry_chunk() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let (status, entry) = get(&bed.router, "/assets/nashcode.js").await;
    assert_eq!(status, 200);
    assert!(
        entry.contains(r#"securityLevel:"strict""#) || entry.contains(r#"securityLevel: "strict""#),
        "mermaid lost strict mode"
    );
    // The entry is ~250 KiB today; mermaid riding along would multiply that.
    assert!(entry.len() < 400 * 1024, "entry chunk grew to {} bytes — did mermaid stop lazy-loading?", entry.len());
}

#[tokio::test]
async fn an_unknown_asset_is_404_and_no_path_escapes_the_bundle() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    for path in ["/assets/nope.js", "/assets/..%2F..%2FCargo.toml", "/assets/%2Fetc%2Fpasswd"] {
        let (status, _) = get(&bed.router, path).await;
        assert_eq!(status, 404, "{path} should not resolve");
    }
}
