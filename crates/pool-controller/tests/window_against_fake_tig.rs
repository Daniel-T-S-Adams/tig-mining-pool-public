//! The window builder against a server, rather than against JSON a test wrote.
//!
//! `tests/window.rs` checks the shapes §7 names, which is what decides whether
//! a read confirms anything. It cannot check that those are the shapes
//! anything actually serves — a builder and a test that agree on the wrong key
//! name pass together. This drives `fake-tig` to each confirmed state and
//! reads the result through the same path the controller will.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use fake_tig::{Config, DEFAULT_API_KEY, build_world, router};
use http_body_util::BodyExt;
use pool_controller::window::confirmed_window;
use serde_json::{Value, json};
use tower::ServiceExt;

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";

fn app() -> Router {
    let dir = format!("{}/../../fixtures/tig/v1", env!("CARGO_MANIFEST_DIR"));
    router(build_world(Config::new(dir)).expect("fixture loads"))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> Value {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        builder = builder.header("x-api-key", k);
    }
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request builds");
    let response = app.clone().oneshot(request).await.expect("handler runs");
    // Every call here is expected to succeed. Tolerating a 4xx would let a
    // refused control — a `/_fake/verify` the server declined, say — pass
    // silently and turn the assertion that follows into a check of nothing.
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        status.is_success(),
        "{method} {uri} -> {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }
}

async fn advance(app: &Router, n: u64) {
    call(
        app,
        "POST",
        "/_fake/advance-block",
        None,
        Some(json!({ "count": n })),
    )
    .await;
}

/// Read both endpoints the way the controller will and build the window.
async fn window(app: &Router) -> pool_workflow::restart::ConfirmedWindow {
    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let benchmarks = call(
        app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    confirmed_window(&benchmarks, &block).expect("a real server's reads are usable")
}

#[tokio::test]
async fn each_confirmed_state_a_real_server_serves_reaches_the_window() {
    let app = app();

    // Nothing yet: an empty window from an empty player, not an error.
    let w = window(&app).await;
    assert!(w.precommits.is_empty() && w.benchmarks.is_empty());

    // 1. Precommit submitted. §7 is explicit that this is not confirmation,
    //    and the server lists it with a null `state.block_confirmed` — so the
    //    builder must return nothing for it.
    let block = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let resp = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(json!({
            "settings": { "player_id": PLAYER, "block_id": block_id,
                          "challenge_id": "c001", "algorithm_id": "a011", "track_id": "" },
            "track_settings": {
                "t001": { "hyperparameters": null, "fuel_budget": 1000000, "num_bundles": 2 },
                "t002": { "hyperparameters": null, "fuel_budget": 1000000, "num_bundles": 1 }
            },
            "compute_type": "aws_t4g"
        })),
    )
    .await;
    let bench_id = resp["benchmark_id"].as_str().unwrap().to_owned();

    let w = window(&app).await;
    assert!(
        w.precommits.is_empty(),
        "an accepted write is listed but unconfirmed; §7 says that is not evidence"
    );

    // 2. Confirmed precommit, with the two fields the workflow needs.
    advance(&app, 1).await;
    let w = window(&app).await;
    let p = w.precommits.get(&bench_id).expect("confirmed precommit");
    assert!(p.block_confirmed > 0);
    assert!(
        p.block_started > 0,
        "§8's deadlines are ages from block_started"
    );
    assert!(!p.track_id.is_empty(), "TIG selected a track");

    // 3. Confirmed benchmark, with `stopped` — the flag that decides whether a
    //    proof is ever built.
    let quality: Vec<i64> = (0..80).collect();
    call(
        &app,
        "POST",
        "/submit-benchmark",
        Some(DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": bench_id, "stopped": false,
                     "merkle_root": "ab".repeat(32), "solution_quality": quality })),
    )
    .await;
    advance(&app, 1).await;
    let w = window(&app).await;
    let b = w.benchmarks.get(&bench_id).expect("confirmed benchmark");
    assert!(!b.stopped);

    // 4. Confirmed proof.
    let benchmarks_body = call(
        &app,
        "GET",
        &format!(
            "/get-benchmarks?block_id={}&player_id={PLAYER}",
            call(&app, "GET", "/get-block?include_data=true", None, None).await["block"]["id"]
                .as_str()
                .unwrap()
        ),
        None,
        None,
    )
    .await;
    let sampled: Vec<u64> = benchmarks_body["benchmarks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(bench_id))
        .unwrap()["details"]["sampled_nonces"]
        .as_array()
        .expect("sampled nonces")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let proofs: Vec<Value> = sampled
        .iter()
        .map(|n| {
            json!({ "leaf": { "nonce": n, "runtime_signature": 7, "fuel_consumed": 100,
                              "solution": "sol", "cpu_arch": "arm64" }, "branch": "00" })
        })
        .collect();
    call(
        &app,
        "POST",
        "/submit-proof",
        Some(DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": bench_id, "merkle_proofs": proofs })),
    )
    .await;
    advance(&app, 1).await;
    let w = window(&app).await;
    assert!(w.proofs.contains_key(&bench_id), "confirmed proof");

    // 5. The verification event, which lives on the block and not in the
    //    collections — and only on the block that published it.
    call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    advance(&app, 1).await;
    let w = window(&app).await;
    assert_eq!(w.verified, vec![bench_id.clone()]);

    advance(&app, 1).await;
    let w = window(&app).await;
    assert!(
        w.verified.is_empty(),
        "the event belongs to the block that carried it, so a later read does not see it"
    );

    // 6. The active set, which is a standing fact rather than an event.
    advance(&app, 20).await;
    let w = window(&app).await;
    assert_eq!(w.active, vec![bench_id.clone()]);
}

#[tokio::test]
async fn a_fraud_ruling_a_real_server_serves_reaches_the_window() {
    // Fraud is read from `get-benchmarks.frauds`, not the block, which is the
    // one collection the pinned fixtures only ever show empty — so this is the
    // only place the builder's fraud path meets a real response.
    let app = app();
    let block = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let resp = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(json!({
            "settings": { "player_id": PLAYER, "block_id": block_id,
                          "challenge_id": "c001", "algorithm_id": "a011", "track_id": "" },
            "track_settings": {
                "t001": { "hyperparameters": null, "fuel_budget": 1000000, "num_bundles": 2 },
                "t002": { "hyperparameters": null, "fuel_budget": 1000000, "num_bundles": 1 }
            },
            "compute_type": "aws_t4g"
        })),
    )
    .await;
    let bench_id = resp["benchmark_id"].as_str().unwrap().to_owned();
    advance(&app, 1).await;

    call(
        &app,
        "POST",
        "/_fake/fraud",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    advance(&app, 1).await;

    let w = window(&app).await;
    let fraud = w
        .frauds
        .get(&bench_id)
        .expect("the ruling reaches the window");
    assert!(fraud.block_confirmed > 0);
}
