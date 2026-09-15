//! §13's observations, gathered against a real `fake-tig`.
//!
//! B2 covers each check failing in isolation. This is the other half: that
//! the evidence a check grades is actually gathered, and gathered from TIG
//! rather than from the configuration it is compared against.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use tig_client::{ReadPolicy, TigReadClient, TigReader};

use tig_gateway::evidence::{
    active_challenge_runtimes, api_key_placement, confirmed_pool_player_id, openapi_checksum,
    serialization_fixtures,
};

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

/// The runtimes the pinned file names, keyed the way check 6 looks them up.
fn pinned_images() -> BTreeSet<String> {
    serde_json::from_str::<serde_json::Value>(PINNED).unwrap()["images"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn check_6_considers_only_the_compute_this_deployment_serves() {
    let base = fake_tig().await;
    let reader = reader(&base);
    let block = latest_block(&base).await;
    let pinned = pinned_images();

    // A deployment serving nothing has nothing to consider. This is slice 1:
    // no members, nothing mined. It is true rather than acknowledged, and it
    // stops being true the moment compute is configured.
    let none = active_challenge_runtimes(&reader, &block, &pinned, &BTreeSet::new())
        .await
        .unwrap();
    assert!(none.is_empty(), "{none:?}");

    // Serving CPU brings the live CPU challenges into scope, and each is
    // judged on whether its runtime is pinned. `evaluate` fails on any that
    // is not — which is how a stale pin stops a pool mining what it has not
    // reviewed.
    let cpu = active_challenge_runtimes(
        &reader,
        &block,
        &pinned,
        &BTreeSet::from(["cpu".to_string()]),
    )
    .await
    .unwrap();
    assert!(!cpu.is_empty(), "the fixture must carry a CPU challenge");
    assert!(
        cpu.iter().all(|c| c.compute_path_supported),
        "anything in the list passed the compute filter: {cpu:?}"
    );

    // An unpinned runtime is reported, not filtered out. A gatherer that
    // dropped it would answer check 6 by omission and the gate would never
    // see the thing it exists to refuse.
    let empty_pins = BTreeSet::new();
    let unpinned = active_challenge_runtimes(
        &reader,
        &block,
        &empty_pins,
        &BTreeSet::from(["cpu".to_string()]),
    )
    .await
    .unwrap();
    assert!(unpinned.iter().all(|c| !c.runtime_pinned), "{unpinned:?}");
    assert_eq!(unpinned.len(), cpu.len());
}

#[tokio::test]
async fn check_4_hashes_what_was_served_and_fails_when_it_cannot_look() {
    // The checksum is taken over the bytes fetched, so a document that
    // changed under the pin produces a different answer rather than the
    // pinned one.
    let base = fake_tig().await;
    let served = openapi_checksum(&format!("{base}/get-block"))
        .await
        .unwrap();
    let sha = served
        .hosted_sha256
        .expect("a fetched document has a checksum");
    assert_eq!(sha.len(), 64, "{sha}");
    assert!(served.reviewed_local_override.is_none());

    // Unreachable is an error, never a silent "unchanged". §13's whole
    // posture is that evidence which could not be gathered fails its check.
    let err = openapi_checksum(&format!("{base}/does-not-exist"))
        .await
        .expect_err("a 404 is not a checksum");
    assert!(err.contains("does-not-exist"), "{err}");
}

#[test]
fn check_8_reads_who_can_open_the_key_file_from_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tig-ev-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("key");
    std::fs::write(&path, "k\n").unwrap();

    // Owner-only is the shape `architecture.md` §2.2 requires.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let placement = api_key_placement(true, &path).unwrap();
    assert!(placement.present_in_gateway);
    assert!(!placement.readable_by_member_services);

    // Group-readable is the finding, not world-readable only: a member
    // service in the same group reads it just as easily.
    for mode in [0o640, 0o604, 0o644, 0o660] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            api_key_placement(true, &path)
                .unwrap()
                .readable_by_member_services,
            "mode {mode:o} must be reported as reachable"
        );
    }

    // A key that is not loaded is reported as absent rather than as an error:
    // `evaluate` owns what absence means.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!api_key_placement(false, &path).unwrap().present_in_gateway);

    // A path that does not exist cannot be judged at all.
    assert!(api_key_placement(true, &dir.join("absent")).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_7_runs_the_fixtures_rather_than_trusting_that_ci_did() {
    // §13 gates a running process — "at startup and after any deployment" —
    // and a property proven in CI is a property of a tree. A binary built
    // from a tree whose tests never ran is the deployment this stops, and it
    // would pass a check that trusted CI. Both fixtures are compiled in.
    let outcome = serialization_fixtures().expect("the fixtures are compiled in and readable");
    assert!(outcome.lossless_numeric_parsing, "{}", outcome.detail);
    assert!(
        outcome.canonical_request_serialization,
        "{}",
        outcome.detail
    );
    assert!(outcome.detail.is_empty(), "{}", outcome.detail);
}

#[test]
fn check_7_pins_the_bytes_a_body_renders_to() {
    // What the fixture is for. TIG compares the digest of what it receives,
    // so a body differing by key order or number formatting is a different
    // write — and §6.1's fee is paid before the pool learns that.
    //
    // The expectation was taken from what the code renders, so this cannot
    // establish that the rendering is what TIG accepts; the protocol spike's
    // live runs did that. It establishes that changing the rendering has to
    // be deliberate.
    const BODY: &str = include_str!("../../../fixtures/serialization/v1/precommit-body.json");
    let doc: serde_json::Value = serde_json::from_str(BODY).unwrap();
    let expected = doc["expected_bytes"].as_str().unwrap();

    // Key order is part of it: TIG digests bytes, not a parsed object.
    assert!(
        expected.starts_with(r#"{"compute_type":"#),
        "the fixture must pin an ordering, not just a value: {expected}"
    );
    // And the hyperparameters keep their own types — §6.6 copies the source
    // benchmark's values, and re-typing either is the failure this pins.
    assert!(expected.contains(r#""alpha":7,"beta":0.25"#), "{expected}");
}
