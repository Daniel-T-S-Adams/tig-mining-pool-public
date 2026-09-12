//! The controller's poll against a live fake-tig: a new block is taken in
//! once, and a block already seen is left alone.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it the database test skips.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use fake_tig::{Config, SharedWorld, build_world, router};
use http_body_util::BodyExt;
use pool_controller::reconciler::Outcome;
use pool_controller::service::{self, Service, ServiceError, Tick};
use pool_domain::Network;
use pool_snapshot::active_cache::BenchmarkDataSource;
use pool_snapshot::{AnchoredRead, SnapshotError, SnapshotSource, TigSnapshotSource};
use pool_test_support::TempDb;
use pool_workflow::Guardrails;
use serde_json::{Value, json};
use tig_client::{ReadLimits, TigReadClient};
use tower::ServiceExt;

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";
const BIN: &str = env!("CARGO_BIN_EXE_pool-controller");

struct FakeTig {
    app: Router,
    world: SharedWorld,
    base: String,
}

async fn fake_tig() -> FakeTig {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
    let world = build_world(Config::new(fixtures)).expect("fixture world loads");
    let app = router(world.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    FakeTig {
        app,
        world,
        base: format!("http://{addr}"),
    }
}

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

/// Drive the stand-in to one ACTIVE benchmark owned by the pool over the
/// real write path — precommit, benchmark, proof, then the blocks until
/// activation — and return its id.
async fn tip(app: &Router) -> String {
    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    block["block"]["id"].as_str().unwrap().to_owned()
}

async fn benches(app: &Router, block_id: String) -> Value {
    call(
        app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await
}

async fn activate_one(app: &Router) -> String {
    let block_id = tip(app).await;
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

    let listed = benches(app, tip(app).await).await;
    let nonces = listed["precommits"][0]["details"]["num_nonces"]
        .as_u64()
        .unwrap();
    call(
        app,
        "POST",
        "/submit-benchmark",
        Some(fake_tig::DEFAULT_API_KEY),
        Some(json!({ "benchmark_id": id, "stopped": false, "merkle_root": "ab".repeat(32), "solution_quality": (0..nonces).collect::<Vec<u64>>() })),
    )
    .await;
    call(app, "POST", "/_fake/advance-block", None, None).await;

    let listed = benches(app, tip(app).await).await;
    let proofs: Vec<Value> = listed["benchmarks"][0]["details"]["sampled_nonces"]
        .as_array()
        .unwrap()
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
        json!([id])
    );
    id
}

/// The two clients the binary builds, at the full ceiling so the test does
/// not pace itself against one process's share. The poll differs only in
/// its deadlines, as `for_block_poll` differs from `for_reader`; two clients
/// on one host must agree on the rate fields.
fn clients(base: &str) -> (TigSnapshotSource, TigReadClient) {
    let policy = tig_client::testing::shipped_policy_for_test().unwrap();
    let limits = ReadLimits {
        max_backoff: Duration::from_millis(50),
        ..tig_client::testing::pool_ceiling_for_test(&policy)
    };
    let reader = TigReadClient::new_unrestricted_for_test(base, &policy, limits).unwrap();
    let poll = TigReadClient::new_unrestricted_for_test(
        base,
        &policy,
        ReadLimits {
            call_deadline: policy.block_poll_interval(),
            ..limits
        },
    )
    .unwrap();
    (TigSnapshotSource::new(reader, PLAYER), poll)
}

#[tokio::test]
async fn a_new_block_is_taken_in_once_and_a_seen_one_is_left_alone() {
    let Some(db) = TempDb::migrated("service_tick").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let (source, poll) = clients(&tig.base);
    let mut service = Service::new(
        controller.clone(),
        source,
        poll,
        Network::Testnet,
        PLAYER,
        Guardrails {
            max_assignment_age_blocks: 60,
            package_due_age_blocks: 110,
            workflow_expiry_age_blocks: 120,
            proof_reserve_blocks: 10,
        },
        10,
    );

    let Tick::Ingested(first) = service.tick().await.unwrap() else {
        panic!("the first poll takes the block in");
    };
    assert!(first.gaps_recorded.is_empty());
    assert!(
        matches!(first.outcome, Outcome::Reconciled(_)),
        "{:?}",
        first.outcome
    );
    let snapshots: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.block_snapshot")
        .fetch_one(&controller)
        .await
        .unwrap();
    assert_eq!(snapshots, 1);

    // Same block: nothing is assembled or persisted again.
    let Tick::Unchanged { block_id } = service.tick().await.unwrap() else {
        panic!("a block already taken in is left alone");
    };
    assert_eq!(block_id, first.block_id);
    let snapshots: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.block_snapshot")
        .fetch_one(&controller)
        .await
        .unwrap();
    assert_eq!(snapshots, 1, "no second assembly of the same block");

    // The chain moves: the next poll takes the new block in.
    tig.world.lock().unwrap().advance_block();
    let Tick::Ingested(second) = service.tick().await.unwrap() else {
        panic!("a new block is taken in");
    };
    assert_eq!(second.height, first.height + 1);
    assert_ne!(second.block_id, first.block_id);
    assert!(second.gaps_recorded.is_empty(), "consecutive");
}

/// A source whose `get-benchmarks` read can be switched off, and whose
/// `get-benchmark-data` read fails a staged number of times.
struct Flaky {
    inner: TigSnapshotSource,
    benchmarks_down: AtomicBool,
    benchmark_data_failures_left: AtomicUsize,
}

impl Flaky {
    fn over(inner: TigSnapshotSource) -> Self {
        Self {
            inner,
            benchmarks_down: AtomicBool::new(false),
            benchmark_data_failures_left: AtomicUsize::new(0),
        }
    }
}

impl SnapshotSource for Flaky {
    async fn get_block(&self) -> Result<Value, SnapshotError> {
        self.inner.get_block().await
    }

    async fn get_anchored(
        &self,
        read: AnchoredRead,
        block_id: &str,
    ) -> Result<Value, SnapshotError> {
        if read == AnchoredRead::Benchmarks && self.benchmarks_down.load(Ordering::SeqCst) {
            return Err(SnapshotError::Unavailable {
                endpoint: read.endpoint().to_string(),
                reason: "staged outage".to_string(),
            });
        }
        self.inner.get_anchored(read, block_id).await
    }

    async fn get_tracks(&self, challenge_id: &str, block_id: &str) -> Result<Value, SnapshotError> {
        self.inner.get_tracks(challenge_id, block_id).await
    }
}

impl BenchmarkDataSource for Flaky {
    async fn get_benchmark_data(&self, benchmark_id: &str) -> Result<Value, SnapshotError> {
        let left = self.benchmark_data_failures_left.load(Ordering::SeqCst);
        if left > 0 {
            self.benchmark_data_failures_left
                .store(left - 1, Ordering::SeqCst);
            return Err(SnapshotError::Unavailable {
                endpoint: "get-benchmark-data".to_string(),
                reason: "staged outage".to_string(),
            });
        }
        self.inner.get_benchmark_data(benchmark_id).await
    }
}

#[tokio::test]
async fn a_block_read_incompletely_is_tried_again_on_the_next_poll() {
    // One transient read failure must not blind the pool to a block for the
    // block's whole life: the pass reconciled nothing, so the block is not
    // "seen", and the next poll takes it in again.
    let Some(db) = TempDb::migrated("service_blind_retry").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let (source, poll) = clients(&tig.base);
    let flaky = Flaky::over(source);
    flaky.benchmarks_down.store(true, Ordering::SeqCst);
    let mut service = Service::new(
        controller.clone(),
        flaky,
        poll,
        Network::Testnet,
        PLAYER,
        Guardrails {
            max_assignment_age_blocks: 60,
            package_due_age_blocks: 110,
            workflow_expiry_age_blocks: 120,
            proof_reserve_blocks: 10,
        },
        10,
    );

    let Tick::Ingested(blind) = service.tick().await.unwrap() else {
        panic!("the block is taken in even when a read fails");
    };
    assert!(
        matches!(blind.outcome, Outcome::Blind { .. }),
        "{:?}",
        blind.outcome
    );

    // Still down: tried again, still blind — not skipped as already seen.
    let Tick::Ingested(again) = service.tick().await.unwrap() else {
        panic!("a blind block is not marked seen");
    };
    assert_eq!(again.block_id, blind.block_id);
    assert!(matches!(again.outcome, Outcome::Blind { .. }));

    // The read recovers: the same block is now reconciled, and only then
    // does a further poll leave it alone.
    service
        .source()
        .benchmarks_down
        .store(false, Ordering::SeqCst);
    let Tick::Ingested(seen) = service.tick().await.unwrap() else {
        panic!("the recovered read lets the block reconcile");
    };
    assert_eq!(seen.block_id, blind.block_id);
    assert!(
        matches!(seen.outcome, Outcome::Reconciled(_)),
        "{:?}",
        seen.outcome
    );
    assert!(matches!(
        service.tick().await.unwrap(),
        Tick::Unchanged { .. }
    ));
}

#[tokio::test]
async fn a_block_a_previous_run_finished_is_not_assembled_again() {
    // §9 allows one usable assembly per block. `get-benchmarks` is a
    // latest-state read, so a second assembly of the same block legitimately
    // differs — and two usable rows for one block is the contradiction
    // `block_snapshot_one_usable_per_block` refuses. A restart mid-block
    // must therefore not re-assemble what the previous run finished.
    let Some(db) = TempDb::migrated("service_restart").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let build = |name: &str| {
        let (source, poll) = clients(&tig.base);
        let _ = name;
        Service::new(
            controller.clone(),
            source,
            poll,
            Network::Testnet,
            PLAYER,
            Guardrails {
                max_assignment_age_blocks: 60,
                package_due_age_blocks: 110,
                workflow_expiry_age_blocks: 120,
                proof_reserve_blocks: 10,
            },
            10,
        )
    };

    let mut first_run = build("first");
    let Tick::Ingested(taken) = first_run.tick().await.unwrap() else {
        panic!("the first run takes the block in");
    };
    assert!(taken.cache.covers_active_set(), "{:?}", taken.cache);

    // A new process at the same block: nothing is assembled, and the one
    // usable row stands.
    let mut second_run = build("second");
    let Tick::AlreadyIngested { block_id } = second_run.tick().await.unwrap() else {
        panic!("a restart must not assemble a block already finished");
    };
    assert_eq!(block_id, taken.block_id);
    let usable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pool.block_snapshot WHERE reads_complete AND active_cache_ready",
    )
    .fetch_one(&controller)
    .await
    .unwrap();
    assert_eq!(usable, 1);

    // And it resumes at the next block.
    tig.world.lock().unwrap().advance_block();
    let Tick::Ingested(next) = second_run.tick().await.unwrap() else {
        panic!("the next block is taken in");
    };
    assert_eq!(next.height, taken.height + 1);
}

#[tokio::test]
async fn once_fails_on_a_failed_poll_and_the_loop_does_not() {
    // `once` exists for an operator checking a deployment, or for K3's
    // evidence run. A check that reports success after failing is worse than
    // no check, so a failed poll is fatal to it. The polling loop is the
    // opposite case: §10 makes a missed block unrecoverable, so exiting on
    // one transient read error would turn a retryable failure into a
    // permanent hole.
    let Some(db) = TempDb::migrated("service_once_fails").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    // A TIG endpoint nothing is listening on, so every poll fails.
    let (source, poll) = clients("http://127.0.0.1:1");
    let mut service = Service::new(
        controller,
        source,
        poll,
        Network::Testnet,
        PLAYER,
        Guardrails {
            max_assignment_age_blocks: 60,
            package_due_age_blocks: 110,
            workflow_expiry_age_blocks: 120,
            proof_reserve_blocks: 10,
        },
        10,
    );

    let err = service::run(&mut service, Duration::from_millis(10), true, |_| {})
        .await
        .expect_err("once must not report success after a failed poll");
    assert!(matches!(err, ServiceError::Poll(_)), "{err}");

    // The loop keeps going instead: it is still polling when the timeout
    // cuts it off, rather than having returned.
    let looping = tokio::time::timeout(
        Duration::from_millis(300),
        service::run(&mut service, Duration::from_millis(10), false, |_| {}),
    )
    .await;
    assert!(
        looping.is_err(),
        "the polling loop returned on a failed poll: {looping:?}"
    );
}

#[tokio::test]
async fn a_warm_up_that_did_not_finish_continues_on_the_next_poll() {
    // §5.2: "an initial cache warm-up may span several blocks" — but it
    // need not wait a block between passes. With an active benchmark whose
    // first fetch fails, the block is taken in unusable; the next poll,
    // same block, spends another budget, retains it, and persists the
    // block again as usable; only then is the block left alone.
    let Some(db) = TempDb::migrated("service_warm_up").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let active_id = activate_one(&tig.app).await;
    let (source, poll) = clients(&tig.base);
    let flaky = Flaky::over(source);
    flaky
        .benchmark_data_failures_left
        .store(1, Ordering::SeqCst);
    let mut service = Service::new(
        controller.clone(),
        flaky,
        poll,
        Network::Testnet,
        PLAYER,
        Guardrails {
            max_assignment_age_blocks: 60,
            package_due_age_blocks: 110,
            workflow_expiry_age_blocks: 120,
            proof_reserve_blocks: 10,
        },
        10,
    );

    let Tick::Ingested(first) = service.tick().await.unwrap() else {
        panic!("the block is taken in");
    };
    assert_eq!(
        first.cache.missing,
        vec![active_id.clone()],
        "{:?}",
        first.cache
    );
    assert!(
        matches!(first.outcome, Outcome::Reconciled(_)),
        "reconciliation does not wait on the cache"
    );
    let usable = |pool: &sqlx::PgPool| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pool.block_snapshot WHERE reads_complete AND active_cache_ready",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        usable(&controller).await,
        0,
        "not usable for a decision yet"
    );

    let Tick::CacheAdvanced {
        block_id,
        cache,
        now_usable,
    } = service.tick().await.unwrap()
    else {
        panic!("the same block continues its warm-up");
    };
    assert_eq!(block_id, first.block_id);
    assert_eq!(cache.fetched, vec![active_id]);
    assert!(now_usable);
    assert_eq!(
        usable(&controller).await,
        1,
        "the block is now persisted usable"
    );

    assert!(matches!(
        service.tick().await.unwrap(),
        Tick::Unchanged { .. }
    ));
}

#[test]
fn the_binary_fails_closed_on_a_config_it_cannot_use() {
    // Criterion A1 at the binary level: a config that names another role's
    // credential, or none, is refused before any work — with the reason on
    // stderr, since telemetry is not up yet.
    let dir = std::env::temp_dir().join(format!("pool-controller-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        r#"
network = "testnet"

[database]
host = "127.0.0.1"
port = 1
name = "pool_dev"
user = "pool_migration"
password_file = "/nonexistent"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "cli-test"
"#,
    )
    .unwrap();

    let output = Command::new(BIN)
        .arg("--config")
        .arg(&path)
        .arg("once")
        .output()
        .expect("binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pool_controller") || stderr.contains("[tig]") || stderr.contains("tig"),
        "the refusal names what is wrong: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
