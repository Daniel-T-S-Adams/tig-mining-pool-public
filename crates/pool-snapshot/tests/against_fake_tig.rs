//! Assembly against the deterministic TIG stand-in.
//!
//! The unit tests drive the algorithm through a staged port; this proves the
//! production source — real client, real cache, real endpoint shapes — works
//! against a server that answers like TIG, including the mid-assembly block
//! advance the algorithm exists for.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use pool_snapshot::{AnchoredRead, TigSnapshotSource, assemble};
use tig_client::{ReadLimits, TigReadClient};

/// The fixture's player, not slice 1's real identity: `fake-tig` now
/// requires `get-benchmarks` to be player-scoped and matches the id against
/// its fixture, which is the point — an unscoped or wrongly-scoped call
/// fails here instead of silently returning someone else's records.
const PLAYER: &str = "0xp00l00000000000000000000000000000000000";

async fn fake_tig() -> String {
    // Absolute: a test's working directory is its crate, not the repo root.
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
    let world =
        fake_tig::build_world(fake_tig::Config::new(fixtures)).expect("fixture world loads");
    let app = fake_tig::router(world);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn source(base: &str) -> TigSnapshotSource {
    // The ceiling rather than a reader share, so the test is not pacing
    // itself against one process's slice of the budget.
    let limits = ReadLimits {
        max_backoff: Duration::from_millis(50),
        ..tig_client::testing::pool_ceiling_for_test()
    };
    let client = TigReadClient::new_unrestricted_for_test(base, limits).unwrap();
    TigSnapshotSource::new(client, PLAYER)
}

#[tokio::test]
async fn assembles_a_complete_snapshot_from_the_stand_in() {
    let base = fake_tig().await;
    let src = source(&base);

    let snapshot = assemble(&src, 3).await.expect("assembles against fake-tig");
    assert!(!snapshot.block_id.is_empty());
    assert!(
        snapshot.reads_complete,
        "fake-tig serves every anchored read, so the snapshot should be complete: {:?}",
        AnchoredRead::ALL
            .iter()
            .filter(|r| snapshot.read(**r).is_none())
            .map(|r| r.endpoint())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_second_assembly_at_the_same_block_is_served_from_the_cache() {
    // §9 caches each response by its complete request key for the life of a
    // block. Asserted on the cache's contents rather than on timing, which
    // at this scale would measure the test's patience.
    let base = fake_tig().await;
    let src = source(&base);

    let first = assemble(&src, 3).await.unwrap();
    let cached_after_first = src.cached_for(&first.block_id);
    assert_eq!(
        cached_after_first,
        AnchoredRead::ALL.len() + first.tracks.len(),
        "every anchored read plus one track read per challenge should be cached"
    );
    assert!(
        !first.tracks.is_empty(),
        "the fixture has challenges, so there should be per-challenge track reads"
    );

    let second = assemble(&src, 3).await.unwrap();
    assert_eq!(second.block_id, first.block_id, "same block");
    assert_eq!(
        src.cached_for(&second.block_id),
        cached_after_first,
        "a second pass at the same block must not add entries"
    );
    assert_eq!(second.reads, first.reads, "and must produce the same reads");
}

#[tokio::test]
async fn advancing_the_block_invalidates_the_whole_cache() {
    let base = fake_tig().await;
    let src = source(&base);

    let first = assemble(&src, 3).await.unwrap();
    assert!(src.cached_for(&first.block_id) > 0);

    // Advance the stand-in's chain.
    let client = reqwest_post(&format!("{base}/_fake/advance-block")).await;
    assert!(client, "fake-tig accepted the advance");

    let second = assemble(&src, 3).await.expect("assembles at the new block");
    assert_ne!(second.block_id, first.block_id, "the chain moved");
    assert_eq!(
        src.cached_for(&first.block_id),
        0,
        "the previous block's responses must be gone entirely, not aged out one by one"
    );
}

/// Minimal POST helper: the snapshot crate has no HTTP client of its own,
/// and pulling one in as a dev-dependency for a single control call is not
/// worth it.
async fn reqwest_post(url: &str) -> bool {
    let listener = tokio::net::TcpStream::connect(
        url.trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default(),
    )
    .await;
    let Ok(mut stream) = listener else {
        return false;
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let path = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = path
        .split_once('/')
        .map(|(_, p)| format!("/{p}"))
        .unwrap_or_else(|| "/".into());
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf).await;
    buf.starts_with("HTTP/1.1 200")
}
