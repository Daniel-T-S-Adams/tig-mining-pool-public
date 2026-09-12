//! The active-benchmark metadata cache (`tig_integration.md` §5.2).
//!
//! The pure reduction is checked against the pinned fixture; the advance is
//! driven through staged ports so the budget, the skip of retained ids and
//! the failure path can each be provoked; the store runs against a real
//! PostgreSQL; and the whole thing is run once against a live fake-tig that
//! has a benchmark in its active set.
//!
//! Requires `POOL_TEST_SUPERUSER_URL` for the database tests; without it
//! they skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use pool_domain::Network;
use pool_snapshot::active_cache::{
    ActiveBenchmarkMeta, ActiveBenchmarkStore, Advance, BenchmarkDataSource, StoreError, advance,
    retain,
};
use pool_snapshot::store::BlockSnapshotStore;
use pool_snapshot::{
    PostgresActiveBenchmarkStore, PostgresSnapshotStore, SnapshotError, TigSnapshotSource, assemble,
};
use pool_test_support::{TempDb, is_insufficient_privilege};
use serde_json::{Value, json};
use tig_client::{ReadLimits, TigReadClient};
use tower::ServiceExt;

const NET: Network = Network::Testnet;
const PLAYER: &str = "0xp00l00000000000000000000000000000000000";

fn fixture_body() -> Value {
    serde_json::from_str(include_str!(
        "../../../fixtures/tig/v1/get-benchmark-data.json"
    ))
    .unwrap()
}

// ---- retain: the reduction ---------------------------------------------------

#[test]
fn retain_keeps_what_5_2_names_and_nothing_else() {
    let body = fixture_body();
    let meta = retain("bench_net_0001", &body).unwrap();
    assert_eq!(
        meta,
        ActiveBenchmarkMeta {
            benchmark_id: "bench_net_0001".to_string(),
            player_id: "0xdev1000000000000000000000000000000000000".to_string(),
            challenge_id: "c001".to_string(),
            algorithm_id: "a011".to_string(),
            track_id: "t001".to_string(),
            compute_type: Some("aws_c7g".to_string()),
            num_bundles: 3,
            fuel_budget: Some(1_500_000),
            hyperparameters: Some(json!({ "noise": "0.15", "restart_period": 250 })),
            precommit_block_confirmed: 100_021,
            num_active_bundles: Some(3),
            average_quality_by_bundle: body["benchmark"]["details"]["average_quality_by_bundle"]
                .as_array()
                .cloned()
                .unwrap(),
            stopped: false,
            benchmark_block_confirmed: 100_050,
        }
    );
}

#[test]
fn retain_refuses_a_body_without_a_confirmed_benchmark() {
    // An active id has a confirmed benchmark by construction; a body
    // without one contradicts the block, and caching it with holes would
    // give §7 a benchmark with no bundles to attribute.
    let mut body = fixture_body();
    body["benchmark"] = Value::Null;
    let err = retain("bench_net_0001", &body).unwrap_err();
    assert!(err.reason.contains("no confirmed benchmark"), "{err}");

    let mut body = fixture_body();
    body["benchmark"]["details"]["average_quality_by_bundle"] = json!(52);
    let err = retain("bench_net_0001", &body).unwrap_err();
    assert!(err.reason.contains("average_quality_by_bundle"), "{err}");
}

#[test]
fn retain_refuses_a_body_that_describes_another_benchmark() {
    let err = retain("bench_net_0002", &fixture_body()).unwrap_err();
    assert!(err.reason.contains("bench_net_0001"), "{err}");
}

// ---- advance: budget, skipping, failure --------------------------------------

/// A source serving staged bodies and counting what it was asked for.
struct Staged {
    bodies: BTreeMap<String, Value>,
    asked: Mutex<Vec<String>>,
}

impl Staged {
    fn with(ids: &[&str]) -> Self {
        let bodies = ids
            .iter()
            .map(|id| {
                let mut body = fixture_body();
                body["precommit"]["benchmark_id"] = json!(id);
                ((*id).to_string(), body)
            })
            .collect();
        Self {
            bodies,
            asked: Mutex::new(Vec::new()),
        }
    }
}

impl BenchmarkDataSource for Staged {
    async fn get_benchmark_data(&self, benchmark_id: &str) -> Result<Value, SnapshotError> {
        self.asked.lock().unwrap().push(benchmark_id.to_string());
        self.bodies
            .get(benchmark_id)
            .cloned()
            .ok_or_else(|| SnapshotError::Unavailable {
                endpoint: "get-benchmark-data".to_string(),
                reason: "staged outage".to_string(),
            })
    }
}

#[derive(Default)]
struct Memory {
    rows: Mutex<BTreeMap<String, ActiveBenchmarkMeta>>,
    inserts: AtomicUsize,
}

impl ActiveBenchmarkStore for Memory {
    async fn retained(&self, _: Network, ids: &[String]) -> Result<BTreeSet<String>, StoreError> {
        let rows = self.rows.lock().unwrap();
        Ok(ids
            .iter()
            .filter(|id| rows.contains_key(*id))
            .cloned()
            .collect())
    }

    async fn retain(
        &self,
        _: Network,
        meta: &ActiveBenchmarkMeta,
        _: u64,
    ) -> Result<(), StoreError> {
        self.inserts.fetch_add(1, Ordering::SeqCst);
        self.rows
            .lock()
            .unwrap()
            .entry(meta.benchmark_id.clone())
            .or_insert_with(|| meta.clone());
        Ok(())
    }
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[tokio::test]
async fn the_budget_bounds_one_pass_and_the_rest_is_reported_missing() {
    // §5.2: "an initial cache warm-up may span several blocks". Two passes
    // of two over five ids: the first retains two in id order and reports
    // three missing; the second retains two more; a third finishes.
    let source = Staged::with(&["b1", "b2", "b3", "b4", "b5"]);
    let store = Memory::default();
    let active = ids(&["b5", "b3", "b1", "b4", "b2"]);

    let first = advance(&source, &store, NET, &active, 100, 2)
        .await
        .unwrap();
    assert_eq!(
        first.fetched,
        ids(&["b1", "b2"]),
        "id order, not block order"
    );
    assert_eq!(first.missing, ids(&["b3", "b4", "b5"]));
    assert!(first.failed.is_empty());
    assert!(!first.covers_active_set());

    let second = advance(&source, &store, NET, &active, 101, 2)
        .await
        .unwrap();
    assert_eq!(second.fetched, ids(&["b3", "b4"]));
    assert_eq!(second.missing, ids(&["b5"]));

    let third = advance(&source, &store, NET, &active, 102, 2)
        .await
        .unwrap();
    assert_eq!(third.fetched, ids(&["b5"]));
    assert!(third.covers_active_set());

    // Retained ids are never asked for again.
    assert_eq!(source.asked.lock().unwrap().len(), 5);
    let fourth = advance(&source, &store, NET, &active, 103, 2)
        .await
        .unwrap();
    assert_eq!(fourth, Advance::default());
    assert_eq!(source.asked.lock().unwrap().len(), 5, "nothing re-fetched");
}

#[tokio::test]
async fn a_fetch_that_fails_leaves_the_id_missing_and_spends_its_slot() {
    // A read that fails is a fact not retained: the id stays missing, so
    // the snapshot stays unusable, and the failure is named so an operator
    // can tell it from a warm-up still in progress.
    let source = Staged::with(&["b1", "b3"]);
    let store = Memory::default();
    let active = ids(&["b1", "b2", "b3"]);

    let report = advance(&source, &store, NET, &active, 100, 10)
        .await
        .unwrap();
    assert_eq!(report.fetched, ids(&["b1", "b3"]));
    assert_eq!(report.missing, ids(&["b2"]));
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "b2");
    assert!(
        report.failed[0].1.contains("staged outage"),
        "{}",
        report.failed[0].1
    );
    assert!(!report.covers_active_set());
}

#[tokio::test]
async fn a_body_the_pool_cannot_read_is_not_retained() {
    let mut source = Staged::with(&["b1"]);
    source.bodies.get_mut("b1").unwrap()["benchmark"] = Value::Null;
    let store = Memory::default();

    let report = advance(&source, &store, NET, &ids(&["b1"]), 100, 10)
        .await
        .unwrap();
    assert!(report.fetched.is_empty());
    assert_eq!(report.missing, ids(&["b1"]));
    assert_eq!(
        store.inserts.load(Ordering::SeqCst),
        0,
        "nothing with holes is stored"
    );
}

#[tokio::test]
async fn an_empty_active_set_is_covered_without_a_fetch() {
    let source = Staged::with(&[]);
    let store = Memory::default();
    let report = advance(&source, &store, NET, &[], 100, 10).await.unwrap();
    assert!(report.covers_active_set());
    assert!(source.asked.lock().unwrap().is_empty());
}

// ---- the store, against PostgreSQL -------------------------------------------

#[tokio::test]
async fn retaining_is_idempotent_and_the_controller_cannot_rewrite_a_fact() {
    let Some(db) = TempDb::migrated("active_cache_store").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let store = PostgresActiveBenchmarkStore::new(controller.clone());
    let meta = retain("bench_net_0001", &fixture_body()).unwrap();

    let before = store
        .retained(NET, &ids(&["bench_net_0001", "other"]))
        .await
        .unwrap();
    assert!(before.is_empty());

    store.retain(NET, &meta, 100_080).await.unwrap();
    store.retain(NET, &meta, 100_081).await.unwrap();
    let after = store
        .retained(NET, &ids(&["bench_net_0001", "other"]))
        .await
        .unwrap();
    assert_eq!(after, BTreeSet::from(["bench_net_0001".to_string()]));

    // Once: the second retain changed nothing, including the height first
    // learned at.
    let (rows, height): (i64, i64) =
        sqlx::query_as("SELECT count(*), min(fetched_at_height) FROM pool.active_benchmark_meta")
            .fetch_one(&controller)
            .await
            .unwrap();
    assert_eq!((rows, height), (1, 100_080));

    // The facts are immutable, and the database says so.
    let err = sqlx::query("UPDATE pool.active_benchmark_meta SET stopped = true")
        .execute(&controller)
        .await
        .unwrap_err();
    assert!(is_insufficient_privilege(&err), "{err}");
    let err = sqlx::query("DELETE FROM pool.active_benchmark_meta")
        .execute(&controller)
        .await
        .unwrap_err();
    assert!(is_insufficient_privilege(&err), "{err}");

    // Per network.
    assert!(
        store
            .retained(Network::Mainnet, &ids(&["bench_net_0001"]))
            .await
            .unwrap()
            .is_empty()
    );
}

// ---- against fake-tig ---------------------------------------------------------

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> Value {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        b = b.header("x-api-key", k);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        status.is_success(),
        "{method} {uri} -> {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// Drive the stand-in to one ACTIVE benchmark owned by the pool, the way
/// `fake-tig`'s own happy-path test does, and return its id.
async fn activate_one(app: &Router) -> String {
    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let resp = call(
        app,
        "POST",
        "/submit-precommit",
        Some(fake_tig::DEFAULT_API_KEY),
        Some(json!({
            "settings": { "player_id": PLAYER, "block_id": block_id, "challenge_id": "c001", "algorithm_id": "a011", "track_id": "" },
            "compute_type": "aws_t4g",
            "track_settings": {
                "t001": { "num_bundles": 2, "fuel_budget": 1_000_000u64, "hyperparameters": null },
                "t002": { "num_bundles": 1, "fuel_budget": 1_000_000u64, "hyperparameters": null }
            }
        })),
    )
    .await;
    let id = resp["benchmark_id"].as_str().unwrap().to_owned();
    call(app, "POST", "/_fake/advance-block", None, None).await;

    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let benches = call(
        app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    let nonces = benches["precommits"][0]["details"]["num_nonces"]
        .as_u64()
        .unwrap();
    let quality: Vec<u64> = (0..nonces).collect();
    call(
        app,
        "POST",
        "/submit-benchmark",
        Some(fake_tig::DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": id, "stopped": false, "merkle_root": "ab".repeat(32), "solution_quality": quality })),
    )
    .await;
    call(app, "POST", "/_fake/advance-block", None, None).await;

    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let benches = call(
        app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    let sampled: Vec<u64> = benches["benchmarks"][0]["details"]["sampled_nonces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let proofs: Vec<Value> = sampled
        .iter()
        .map(|n| {
            json!({
                "leaf": { "nonce": n, "runtime_signature": 7, "fuel_consumed": 100, "solution": "sol", "cpu_arch": "arm64" },
                "branch": "00"
            })
        })
        .collect();
    call(
        app,
        "POST",
        "/submit-proof",
        Some(fake_tig::DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": id, "merkle_proofs": proofs })),
    )
    .await;
    for _ in 0..21 {
        call(app, "POST", "/_fake/advance-block", None, None).await;
    }
    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    assert_eq!(
        block["block"]["data"]["active_ids"]["benchmark"],
        json!([id]),
        "the stand-in should now list it active"
    );
    id
}

#[tokio::test]
async fn an_active_benchmark_on_the_stand_in_is_retained_and_the_snapshot_becomes_usable() {
    let Some(db) = TempDb::migrated("active_cache_fake_tig").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
    let world = fake_tig::build_world(fake_tig::Config::new(fixtures)).unwrap();
    let app = fake_tig::router(world);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let active_id = activate_one(&app).await;

    let policy = tig_client::testing::shipped_policy_for_test().unwrap();
    let limits = ReadLimits {
        max_backoff: Duration::from_millis(50),
        ..tig_client::testing::pool_ceiling_for_test(&policy)
    };
    let client =
        TigReadClient::new_unrestricted_for_test(format!("http://{addr}"), &policy, limits)
            .unwrap();
    let source = TigSnapshotSource::new(client, PLAYER);
    let cache = PostgresActiveBenchmarkStore::new(controller.clone());
    let snapshots = PostgresSnapshotStore::new(controller.clone());

    let mut snapshot = assemble(&source, 3).await.unwrap();
    let active = snapshot.active_benchmark_ids().unwrap();
    assert_eq!(active, vec![active_id.clone()]);
    assert!(
        !snapshot.active_cache_ready,
        "assembly alone never claims the cache"
    );

    // Budget zero: nothing fetched, the id is missing, the snapshot stays
    // unusable for a decision.
    let starved = advance(&source, &cache, NET, &active, snapshot.height, 0)
        .await
        .unwrap();
    assert_eq!(starved.missing, vec![active_id.clone()]);
    snapshot.active_cache_ready = starved.covers_active_set();
    let persisted = snapshots.persist(NET, snapshot.clone()).await.unwrap();
    assert!(persisted.for_decision().is_err());

    // With budget: the real endpoint is read, the facts retained, and the
    // same block re-persisted as usable.
    let fed = advance(&source, &cache, NET, &active, snapshot.height, 10)
        .await
        .unwrap();
    assert_eq!(fed.fetched, vec![active_id.clone()], "{fed:?}");
    assert!(fed.covers_active_set());
    let (challenge, algorithm, qualities): (String, String, Value) = sqlx::query_as(
        "SELECT challenge_id, algorithm_id, average_quality_by_bundle
           FROM pool.active_benchmark_meta WHERE benchmark_id = $1",
    )
    .bind(&active_id)
    .fetch_one(&controller)
    .await
    .unwrap();
    assert_eq!((challenge.as_str(), algorithm.as_str()), ("c001", "a011"));
    assert!(
        !qualities.as_array().unwrap().is_empty(),
        "the per-bundle qualities §7 attributes from"
    );
    snapshot.active_cache_ready = fed.covers_active_set();
    let usable = snapshots.persist(NET, snapshot).await.unwrap();
    assert!(usable.for_decision().is_ok());
}
