//! Issue #10 acceptance: the same gateway flow passes deterministically
//! against the fake TIG server — intent before send, 200 ≠ confirmation,
//! reconcile from confirmed reads, serialized lane, ambiguous-outcome
//! handling.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use spike::{Gateway, Ledger, TigClient, fold_intents, plan_precommit};

/// Spawn fake-tig on an ephemeral loopback port in a dedicated thread with
/// its own runtime; return the base URL.
fn spawn_fake_tig() -> String {
    let fixture = format!("{}/../../fixtures/tig/v1", env!("CARGO_MANIFEST_DIR"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async move {
            let world = fake_tig::build_world(fake_tig::Config::new(fixture)).expect("world");
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tx.send(addr).expect("send addr");
            let _ = axum::serve(listener, fake_tig::router(world)).await;
        });
    });
    let addr = rx.recv().expect("addr received");
    format!("http://{addr}")
}

fn advance(base: &str, count: u64) {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{base}/_fake/advance-block"))
        .json(&serde_json::json!({ "count": count }))
        .send()
        .expect("advance");
    assert!(resp.status().is_success());
}

fn gateway(base: &str, dir: &std::path::Path) -> Gateway {
    Gateway {
        client: TigClient::new(base, "fake-testnet-key").expect("client"),
        ledger: Ledger::open(dir).expect("ledger"),
        player_id: "0xp00l00000000000000000000000000000000000".to_owned(),
    }
}

#[test]
fn precommit_to_confirmed_assignment() {
    let base = spawn_fake_tig();
    let dir = std::env::temp_dir().join(format!("spike-gw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let gw = gateway(&base, &dir);

    // Plan against the anchored snapshot.
    let block = gw.client.latest_block().expect("block");
    let block_id = block["id"].as_str().unwrap().to_owned();
    let challenges = gw.client.challenges(&block_id).expect("challenges");
    let plan = plan_precommit(
        &block,
        &challenges,
        &gw.player_id,
        "c001",
        "a011",
        "aws_t4g",
    )
    .expect("plan");
    // Fee calcs recorded under both bases; fixture per_nonce_fee is nonzero
    // so the bases genuinely differ (40 nonces vs 1 bundle on t001).
    assert!(plan.max_collateral_per_nonce_basis > plan.max_collateral_per_bundle_basis);

    // Submit: 200 but NOT confirmed; lane now busy.
    let intent = gw.submit_precommit(&plan).expect("submit");
    let states = fold_intents(&gw.ledger.read_all("intents").unwrap());
    assert_eq!(states[&intent]["state"], "SUBMITTED");
    assert!(
        gw.assert_lane_free().is_err(),
        "serialized lane must be busy"
    );
    let lines = gw.reconcile().expect("reconcile");
    assert!(
        lines[0].contains("not yet confirmed") || lines[0].contains("no candidate"),
        "{lines:?}"
    );

    // Ledger discipline: attempt line precedes response line.
    let attempts = gw.ledger.read_all("attempts").unwrap();
    assert_eq!(attempts[0]["phase"], "attempt");
    assert_eq!(attempts[1]["phase"], "response");

    // One block later the precommit confirms; reconcile writes the
    // assignment with authoritative confirmed settings.
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile 2");
    assert!(lines[0].contains("CONFIRMED"), "{lines:?}");
    let states = fold_intents(&gw.ledger.read_all("intents").unwrap());
    assert_eq!(states[&intent]["state"], "CONFIRMED");
    let bench_id = states[&intent]["benchmark_id"].as_str().unwrap();
    let assignment: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(format!("assignment-{bench_id}.json"))).unwrap(),
    )
    .unwrap();
    // TIG selected the track; confirmed settings replace the empty proposal.
    // min_num_bundles(c001)=1 -> 1 bundle x 40 nonces-per-bundle on t001.
    assert_eq!(assignment["settings"]["track_id"], "t001");
    assert_eq!(assignment["details"]["num_nonces"], 40);
    assert!(assignment["details"]["rand_hash"].as_str().is_some());

    // Lane free again; a second precommit is permitted.
    gw.assert_lane_free().expect("lane free after confirmation");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ambiguous_outcome_is_not_resubmitted() {
    let base = spawn_fake_tig();
    let dir = std::env::temp_dir().join(format!("spike-gw-amb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let gw = gateway(&base, &dir);

    // Inject the lost-response case: applied server-side, 500 to the client.
    let client = reqwest::blocking::Client::new();
    client
        .post(format!("{base}/_fake/inject"))
        .json(&serde_json::json!({ "target": "submit-precommit", "mode": "ambiguous" }))
        .send()
        .expect("inject");

    let block = gw.client.latest_block().expect("block");
    let block_id = block["id"].as_str().unwrap().to_owned();
    let challenges = gw.client.challenges(&block_id).expect("challenges");
    let plan = plan_precommit(
        &block,
        &challenges,
        &gw.player_id,
        "c001",
        "a011",
        "aws_t4g",
    )
    .expect("plan");
    let intent = gw.submit_precommit(&plan).expect("submit");
    let states = fold_intents(&gw.ledger.read_all("intents").unwrap());
    assert_eq!(states[&intent]["state"], "OUTCOME_UNKNOWN");

    // The lane stays busy — never a blind replacement precommit.
    assert!(gw.assert_lane_free().is_err());

    // Reconcile finds exactly one settings-matched candidate and, once
    // confirmed, adopts it instead of resubmitting.
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile");
    assert!(lines[0].contains("CONFIRMED"), "{lines:?}");
    let states = fold_intents(&gw.ledger.read_all("intents").unwrap());
    assert_eq!(states[&intent]["state"], "CONFIRMED");
    let _ = std::fs::remove_dir_all(&dir);
}
