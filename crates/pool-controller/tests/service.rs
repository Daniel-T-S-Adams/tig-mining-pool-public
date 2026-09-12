//! The controller's poll against a live fake-tig: a new block is taken in
//! once, and a block already seen is left alone.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it the database test skips.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fake_tig::{Config, SharedWorld, build_world, router};
use pool_controller::reconciler::Outcome;
use pool_controller::service::{self, Service, ServiceError, Tick};
use pool_domain::Network;
use pool_snapshot::{AnchoredRead, SnapshotError, SnapshotSource, TigSnapshotSource};
use pool_test_support::TempDb;
use pool_workflow::Guardrails;
use serde_json::Value;
use tig_client::{ReadLimits, TigReadClient};

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";
const BIN: &str = env!("CARGO_BIN_EXE_pool-controller");

async fn fake_tig() -> (SharedWorld, String) {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
    let world = build_world(Config::new(fixtures)).expect("fixture world loads");
    let app = router(world.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (world, format!("http://{addr}"))
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
    let (world, base) = fake_tig().await;
    let (source, poll) = clients(&base);
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
    world.lock().unwrap().advance_block();
    let Tick::Ingested(second) = service.tick().await.unwrap() else {
        panic!("a new block is taken in");
    };
    assert_eq!(second.height, first.height + 1);
    assert_ne!(second.block_id, first.block_id);
    assert!(second.gaps_recorded.is_empty(), "consecutive");
}

/// A source whose `get-benchmarks` read can be switched off.
struct Flaky {
    inner: TigSnapshotSource,
    benchmarks_down: AtomicBool,
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

#[tokio::test]
async fn a_block_read_incompletely_is_tried_again_on_the_next_poll() {
    // One transient read failure must not blind the pool to a block for the
    // block's whole life: the pass reconciled nothing, so the block is not
    // "seen", and the next poll takes it in again.
    let Some(db) = TempDb::migrated("service_blind_retry").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let (_world, base) = fake_tig().await;
    let (source, poll) = clients(&base);
    let flaky = Flaky {
        inner: source,
        benchmarks_down: AtomicBool::new(true),
    };
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
