//! §13's observations, gathered against a real `fake-tig`.
//!
//! B2 covers each check failing in isolation. This is the other half: that
//! the evidence a check grades is actually gathered, and gathered from TIG
//! rather than from the configuration it is compared against.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use tig_client::{ReadPolicy, TigReadClient, TigReader};
use tig_gateway::evidence::confirmed_pool_player_id;

const PINNED: &str = include_str!("../../../config/tig_integration.json");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
const POOL_PLAYER: &str = "0xp00l00000000000000000000000000000000000";

async fn fake_tig() -> String {
    let world = fake_tig::build_world(fake_tig::Config::new(FIXTURES)).expect("world loads");
    let app = fake_tig::router(world);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn reader(base: &str) -> TigReadClient {
    let policy = ReadPolicy::from_config_json(PINNED).unwrap();
    TigReadClient::new(base, &policy, policy.for_reader(TigReader::Gateway)).unwrap()
}

async fn latest_block(base: &str) -> String {
    let body: serde_json::Value = reqwest::get(format!("{base}/get-block"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["block"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn check_9_reads_the_identity_from_tig_and_not_from_the_configuration() {
    let base = fake_tig().await;
    let reader = reader(&base);
    let block = latest_block(&base).await;

    let observed = confirmed_pool_player_id(&reader, &block, POOL_PLAYER)
        .await
        .expect("the fixture pool exists");
    assert_eq!(observed, POOL_PLAYER);

    // The case that matters. `get-player-data` takes `player_id` as a
    // parameter, so a check trusting the echoed value would compare the
    // configured identity against itself and pass for an account that does
    // not exist — the circularity §13.1 rejects for check 2, and that a
    // reviewer found in check 1.
    //
    // Live testnet answers an unknown id with HTTP 200 and a null player
    // rather than a 404 (verified 2026-09-15), so "the request succeeded" is
    // not the test. "TIG holds a player" is.
    let err = confirmed_pool_player_id(
        &reader,
        &block,
        "0x0000000000000000000000000000000000000001",
    )
    .await
    .expect_err("TIG holds no such player");
    assert!(err.contains("holds no player"), "{err}");
}
