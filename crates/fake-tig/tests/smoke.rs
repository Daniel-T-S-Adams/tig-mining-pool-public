//! Canonical smoke scenario (issue #8): the happy path
//! precommit -> confirmed -> benchmark -> sampled nonces -> proof -> ACTIVE,
//! plus block-consistency rejection, auth, failure injection, and a real
//! loopback socket check. Run with `make smoke` (or as part of `make check`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use fake_tig::{Config, DEFAULT_API_KEY, build_world, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn fixture_dir() -> String {
    format!("{}/../../fixtures/tig/v1", env!("CARGO_MANIFEST_DIR"))
}

fn test_router() -> Router {
    router(build_world(Config::new(fixture_dir())).expect("fixture loads"))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    api_key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request builds");
    let response = app.clone().oneshot(request).await.expect("handler runs");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body reads")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

async fn advance(app: &Router, count: u64) {
    let (status, _) = call(
        app,
        "POST",
        "/_fake/advance-block",
        None,
        Some(json!({ "count": count })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

fn precommit_body(block_id: &str) -> Value {
    json!({
        "settings": {
            "player_id": "0xp00l00000000000000000000000000000000000",
            "block_id": block_id,
            "challenge_id": "c001",
            "algorithm_id": "a011",
            "track_id": ""
        },
        "track_settings": {
            "t001": { "hyperparameters": null, "fuel_budget": 1000000, "num_bundles": 2 },
            "t002": { "hyperparameters": null, "fuel_budget": 1000000, "num_bundles": 1 }
        },
        "compute_type": "aws_t4g"
    })
}

#[tokio::test]
async fn happy_path_precommit_to_active() {
    let app = test_router();

    // 1. Anchor snapshot.
    let (status, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    assert_eq!(block_id, "block_100080");

    // 2. Block-anchored reads succeed for the latest block, 400 otherwise.
    for endpoint in ["get-challenges", "get-algorithms", "get-opow"] {
        let (status, _) = call(
            &app,
            "GET",
            &format!("/{endpoint}?block_id={block_id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{endpoint} with latest block");
        let (status, err) = call(
            &app,
            "GET",
            &format!("/{endpoint}?block_id=block_1"),
            None,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{endpoint} with stale block"
        );
        assert_eq!(err["error"], "block must be latest");
    }

    // 3. Writes require the API key.
    let (status, _) = call(
        &app,
        "POST",
        "/submit-precommit",
        None,
        Some(precommit_body(&block_id)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 4. Precommit; TIG (the fake) selects track t001 -> 2 bundles x 40 = 80 nonces.
    let (status, resp) = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body(&block_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let bench_id = resp["benchmark_id"].as_str().unwrap().to_owned();

    // 5. HTTP 200 is not confirmation: entry exists with null block_confirmed.
    let (_, benches) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id=0xp00l00000000000000000000000000000000000"),
        None,
        None,
    )
    .await;
    assert!(benches["precommits"][0]["state"]["block_confirmed"].is_null());

    // 6. Advance one block: precommit confirms; confirmed settings are authoritative.
    advance(&app, 1).await;
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        block["block"]["data"]["confirmed_ids"]["precommit"][0],
        bench_id
    );
    let (_, benches) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id=0xp00l00000000000000000000000000000000000"),
        None,
        None,
    )
    .await;
    let precommit = &benches["precommits"][0];
    assert_eq!(precommit["state"]["block_confirmed"], 100081);
    assert_eq!(precommit["settings"]["track_id"], "t001");
    // `mining_system.md` §6.8: `base_fee + per_nonce_fee * num_bundles`, which
    // for c001 (base 10^16, per-nonce 10^15) over this precommit's 2 bundles
    // on t001 is 1.2 * 10^16.
    //
    // Deliberately *not* `expected.json`'s figure. That fixture records the
    // rule as "base_fee + per_nonce_fee x num_nonces" and §6.8 names it as the
    // refuted side, settled against the pinned upstream commit during the
    // spike. The fixture is immutable and correct as a record of what was
    // believed; this assertion follows the document that settled it. A test
    // citing the fixture as authority is how the fake kept charging 9 * 10^16
    // for a write TIG charges 1.2 * 10^16 for.
    assert_eq!(
        precommit["details"]["num_nonces"], 80,
        "2 bundles x 40 nonces"
    );
    assert_eq!(precommit["details"]["fee_paid"], "12000000000000000");

    // 7. Submit the benchmark commitment (80 qualities, 64-hex root).
    let quality: Vec<i64> = (0..80).collect();
    let root = "ab".repeat(32);
    let (status, resp) = call(
        &app,
        "POST",
        "/submit-benchmark",
        Some(DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": bench_id, "stopped": false, "merkle_root": root, "solution_quality": quality })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    // 8. Advance: benchmark confirms and sampled nonces are published.
    advance(&app, 1).await;
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let (_, benches) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id=0xp00l00000000000000000000000000000000000"),
        None,
        None,
    )
    .await;
    let sampled: Vec<u64> = benches["benchmarks"][0]["details"]["sampled_nonces"]
        .as_array()
        .expect("sampled nonces published")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(
        sampled.len(),
        3,
        "num_samples_gte_average + num_samples_lt_average"
    );
    assert!(sampled.iter().all(|n| *n < 80));
    // A confirmed benchmark publishes one average per bundle: 80 nonces
    // over 2 bundles, qualities 0..80, so bundle means 19 and 59.
    assert_eq!(
        benches["benchmarks"][0]["details"]["average_quality_by_bundle"],
        json!([19, 59]),
        "per-bundle averages, the fact §5.2 caches and §7 attributes from"
    );

    // 9. A proof missing a sampled nonce is rejected.
    let leaf = |nonce: u64| {
        json!({
            "leaf": { "nonce": nonce, "runtime_signature": 7, "fuel_consumed": 100, "solution": "sol", "cpu_arch": "arm64" },
            "branch": "00"
        })
    };
    let (status, _) = call(
        &app,
        "POST",
        "/submit-proof",
        Some(DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": bench_id, "merkle_proofs": [leaf(sampled[0])] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 10. The complete proof is accepted; synchronous response is not confirmation.
    let proofs: Vec<Value> = sampled.iter().map(|n| leaf(*n)).collect();
    let (status, resp) = call(
        &app,
        "POST",
        "/submit-proof",
        Some(DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": bench_id, "merkle_proofs": proofs })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    // 11. Advance until the benchmark enters the active set, then leaves it.
    advance(&app, 1).await; // proof confirms; activation scheduled
    let (_, state) = call(&app, "GET", "/_fake/state", None, None).await;
    assert!(
        state["active"].as_array().unwrap().is_empty(),
        "not active yet"
    );
    advance(&app, 20).await;
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let active = block["block"]["data"]["active_ids"]["benchmark"]
        .as_array()
        .unwrap();
    assert_eq!(
        active,
        &vec![Value::String(bench_id.clone())],
        "benchmark is ACTIVE"
    );
    advance(&app, 120).await; // beyond lifespan
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    assert!(
        block["block"]["data"]["active_ids"]["benchmark"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ambiguous_write_applies_but_fails_response() {
    let app = test_router();
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();

    let (status, _) = call(
        &app,
        "POST",
        "/_fake/inject",
        None,
        Some(json!({ "target": "submit-precommit", "mode": "ambiguous" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The write returns 500 -- but the precommit exists (OUTCOME_UNKNOWN case).
    let (status, _) = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body(&block_id)),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (_, benches) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id=0xp00l00000000000000000000000000000000000"),
        None,
        None,
    )
    .await;
    assert_eq!(
        benches["precommits"].as_array().unwrap().len(),
        1,
        "write was applied"
    );

    // Reconciliation by exact settings match is possible from the read.
    assert_eq!(benches["precommits"][0]["settings"]["challenge_id"], "c001");
}

#[tokio::test]
async fn rate_limit_and_reject_injection() {
    let app = test_router();
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();

    for (mode, expected) in [
        ("rate_limit", StatusCode::TOO_MANY_REQUESTS),
        ("reject", StatusCode::BAD_REQUEST),
    ] {
        let (status, _) = call(
            &app,
            "POST",
            "/_fake/inject",
            None,
            Some(json!({ "target": "submit-precommit", "mode": mode })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            "POST",
            "/submit-precommit",
            Some(DEFAULT_API_KEY),
            Some(precommit_body(&block_id)),
        )
        .await;
        assert_eq!(status, expected, "mode {mode}");
        // Rejected/limited writes must not be applied.
        let (_, benches) = call(
            &app,
            "GET",
            &format!("/get-benchmarks?block_id={block_id}&player_id=0xp00l00000000000000000000000000000000000"),
            None,
            None,
        )
        .await;
        assert!(
            benches["precommits"].as_array().unwrap().is_empty(),
            "mode {mode} not applied"
        );
    }
}

#[tokio::test]
async fn precommit_validation() {
    let app = test_router();
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();

    // Stale block reference (older than second-latest).
    let (status, _) = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body("block_100000")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Missing one active track.
    let mut body = precommit_body(&block_id);
    body["track_settings"]
        .as_object_mut()
        .unwrap()
        .remove("t002");
    let (status, _) = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Inactive challenge c099 exists in config but is not admitted for writes
    // against an unknown challenge id.
    let mut body = precommit_body(&block_id);
    body["settings"]["challenge_id"] = json!("c404");
    let (status, _) = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn serves_real_loopback_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let world = build_world(Config::new(fixture_dir())).expect("fixture loads");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("binds loopback");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(world)).await;
    });

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connects");
    stream
        .write_all(b"GET /get-block?include_data=true HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .expect("writes request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("reads response");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.contains("block_100080"),
        "serves the fixture anchor"
    );
}

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";

/// Drive one benchmark from precommit to a confirmed proof, returning its id.
///
/// The steps are the happy path's; only the assertions differ, so this exists
/// to get to the interesting state rather than to re-test getting there.
async fn to_confirmed_proof(app: &Router) -> String {
    let (_, block) = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let (_, resp) = call(
        app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body(&block_id)),
    )
    .await;
    let bench_id = resp["benchmark_id"].as_str().unwrap().to_owned();

    advance(app, 1).await;
    let quality: Vec<i64> = (0..80).collect();
    call(
        app,
        "POST",
        "/submit-benchmark",
        Some(DEFAULT_API_KEY),
        Some(json!({
            "benchmark_id": bench_id, "stopped": false,
            "merkle_root": "ab".repeat(32), "solution_quality": quality
        })),
    )
    .await;

    advance(app, 1).await;
    let (_, block) = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let (_, benches) = call(
        app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    let mine = benches["benchmarks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == json!(bench_id))
        .expect("our benchmark");
    let sampled: Vec<u64> = mine["details"]["sampled_nonces"]
        .as_array()
        .expect("sampled nonces published")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let proofs: Vec<Value> = sampled
        .iter()
        .map(|n| {
            json!({
                "leaf": { "nonce": n, "runtime_signature": 7, "fuel_consumed": 100,
                          "solution": "sol", "cpu_arch": "arm64" },
                "branch": "00"
            })
        })
        .collect();
    call(
        app,
        "POST",
        "/submit-proof",
        Some(DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": bench_id, "merkle_proofs": proofs })),
    )
    .await;
    advance(app, 1).await;
    bench_id
}

#[tokio::test]
async fn tig_can_rule_a_benchmark_verified() {
    // Issue #93. `tig_integration.md` §7's "Verification event" is a benchmark
    // id in `block.data.confirmed_ids.verified`. Nothing a client submits
    // causes it — it is TIG's ruling — so it needs a control of its own, the
    // same way `/_fake/advance-block` moves a chain no client moves.
    //
    // Without it the pool's VERIFIED transition cannot be reached from a
    // server at all, so a lifecycle drive would stop at PROOF_CONFIRMED and
    // `mining_system.md` §6.1's unverified interval would never close in it.
    let app = test_router();
    let bench_id = to_confirmed_proof(&app).await;

    let (status, _) = call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The block already served does not change what it said. Asking for a
    // ruling does not rewrite history; the next block publishes it.
    let (_, same) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    assert!(
        same["block"]["data"]["confirmed_ids"]["verified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a published block is fixed: two reads of one id must agree"
    );

    advance(&app, 1).await;
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let verified = block["block"]["data"]["confirmed_ids"]["verified"]
        .as_array()
        .expect("confirmed_ids.verified is published");
    assert_eq!(verified, &vec![json!(bench_id)]);
    assert_eq!(block["block"]["details"]["num_confirmed"]["verified"], 1);

    // It is a fact about one block, not a standing flag: §7 reads the set from
    // the block that published it, which is why a pool that was down for that
    // block cannot recover the event from a later read.
    advance(&app, 1).await;
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    assert!(
        block["block"]["data"]["confirmed_ids"]["verified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the event belongs to the block that carried it"
    );
}

#[tokio::test]
async fn verification_is_refused_before_there_is_a_proof_to_verify() {
    // §4.5's ladder runs PROOF_CONFIRMED -> VERIFYING -> ACTIVE. A fake that
    // produced a verification before a confirmed proof would let a test assert
    // the pool handles an order the chain never produces, while missing the
    // one it does.
    let app = test_router();
    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let (_, resp) = call(
        &app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body(&block_id)),
    )
    .await;
    let bench_id = resp["benchmark_id"].as_str().unwrap().to_owned();
    advance(&app, 1).await;

    let (status, err) = call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    let (status, err) = call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": "no_such_benchmark" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
}

#[tokio::test]
async fn tig_can_rule_a_benchmark_fraudulent() {
    // §7: "Matching entry in `get-benchmarks.frauds` with non-null
    // `state.block_confirmed`". Read from `frauds`, not from the block, which
    // is why this is not simply another confirmed_ids set.
    //
    // Fraud is what `mining_system.md`'s method verification exists to catch,
    // and the pool cannot cause it — so, like verification, it needs a control
    // rather than arriving as a side effect of a write.
    let app = test_router();
    let bench_id = to_confirmed_proof(&app).await;

    let (status, _) = call(
        &app,
        "POST",
        "/_fake/fraud",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    advance(&app, 1).await;

    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let (_, benches) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    let entry = benches["frauds"]
        .as_array()
        .expect("frauds is an array")
        .iter()
        .find(|f| f["benchmark_id"] == json!(bench_id))
        .expect("the ruling appears in get-benchmarks.frauds");
    assert!(
        !entry["state"]["block_confirmed"].is_null(),
        "§7 keys fraud confirmation on a non-null state.block_confirmed: {entry}"
    );

    // The per-benchmark read serves the same record, from the same place, so
    // the two endpoints cannot describe one ruling differently.
    let (_, data) = call(
        &app,
        "GET",
        &format!("/get-benchmark-data?benchmark_id={bench_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(data["fraud"], *entry);

    // An unruled benchmark still reads null, so the field distinguishes.
    let clean = to_confirmed_proof(&app).await;
    let (_, data) = call(
        &app,
        "GET",
        &format!("/get-benchmark-data?benchmark_id={clean}"),
        None,
        None,
    )
    .await;
    assert!(data["fraud"].is_null());
}

#[tokio::test]
async fn a_verified_benchmark_can_still_be_ruled_fraudulent() {
    // §7 lists "Fraud confirmed" and "Verification event" as independent
    // evidence, with no ordering or exclusivity between them. The pool
    // implements that: `workflow::confirm_fraud` is reachable from any
    // non-terminal state that owns a benchmark, `Verified` is deliberately
    // non-terminal, and `restart.rs` applies fraud *before* the forward steps
    // precisely because one §10 window can carry the same benchmark id in
    // `frauds` and in `verified`.
    //
    // An earlier version of this fake refused it and a test pinned the
    // refusal, which made the VERIFIED -> FRAUDULENT path undriveable from a
    // server — the fixture case `fraud_confirmed_after_proof` starts at
    // VERIFYING, so that is the path the fixture asks for.
    let app = test_router();
    let bench_id = to_confirmed_proof(&app).await;

    call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    advance(&app, 1).await;

    let (status, err) = call(
        &app,
        "POST",
        "/_fake/fraud",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{err}");
    advance(&app, 1).await;

    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let (_, benches) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    assert!(
        benches["frauds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["benchmark_id"] == json!(bench_id)),
        "a verified benchmark can still be ruled fraudulent"
    );
}

#[tokio::test]
async fn a_fraudulent_benchmark_does_not_then_verify() {
    // The one ordering that is refused, and unlike the reverse it has an
    // authority: `mining_system.md` §4.5 lists FRAUDULENT as a terminal
    // branch, and a chain does not leave one.
    let app = test_router();
    let bench_id = to_confirmed_proof(&app).await;

    call(
        &app,
        "POST",
        "/_fake/fraud",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    let (status, _) = call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "refused while the ruling is still pending, not only once published"
    );

    advance(&app, 1).await;
    let (status, _) = call(
        &app,
        "POST",
        "/_fake/verify",
        None,
        Some(json!({ "benchmark_id": bench_id })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_pool_player_can_be_served_under_a_real_address() {
    // A production binary's configuration refuses the fixture's placeholder
    // id, so a local run against this server needs the fixture served as
    // though its pool player were the configured address — everywhere the
    // fixture names it, or the server would describe one player two ways.
    const REAL: &str = "0x2935a721068da756b28cba896efdb64e8909dfae";
    let mut cfg = Config::new(fixture_dir());
    cfg.pool_player_id = Some(REAL.to_owned());
    let app = router(build_world(cfg).expect("fixture loads"));

    let (_, block) = call(&app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();

    let (status, player) = call(
        &app,
        "GET",
        &format!("/get-player-data?player_id={REAL}&block_id={block_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{player}");
    assert_eq!(player["player"]["id"], json!(REAL));

    let (status, opow) = call(
        &app,
        "GET",
        &format!("/get-opow?block_id={block_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opow}");
    assert_eq!(opow["opow"]["player_id"], json!(REAL));
    assert!(
        opow["opow"]["block_data"]["coinbase"].get(REAL).is_some(),
        "map keys are rewritten too: {}",
        opow["opow"]["block_data"]["coinbase"]
    );

    let (status, benchmarks) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?player_id={REAL}&block_id={block_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{benchmarks}");

    // And the placeholder is gone: the server knows one player.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/get-benchmarks?player_id=0xp00l00000000000000000000000000000000000&block_id={block_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let text = serde_json::to_string(&block).unwrap();
    assert!(
        !text.contains("0xp00l"),
        "the block still names the placeholder"
    );
}
