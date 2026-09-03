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
    assert_eq!(precommit["details"]["num_nonces"], 80);
    // Fee matches the independently recorded expectation in expected.json.
    assert_eq!(precommit["details"]["fee_paid"], "90000000000000000");

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
