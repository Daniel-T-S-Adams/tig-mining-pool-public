//! Slice-1 criteria D2, D2c, D2d and D2e: one deciding pass, committed.
//!
//! `crate::propose`'s unit tests cover §6's rules over a snapshot. These cover
//! what happens to the result: the workflow and the decision reach the
//! database together with the intent, the audit evidence §6.3 requires is in
//! the record, and the gates that must refuse do.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_controller::decide::{Decided, decide_once};
use pool_controller::propose::Offer;
use pool_controller::window::confirmed_window;
use pool_decision::challenge::OfferedCompute;
use pool_domain::{Network, TraceId};
use pool_snapshot::PostgresSnapshotStore;
use pool_snapshot::Snapshot;
use pool_snapshot::active_cache::ActiveBenchmarkMeta;
use pool_snapshot::store::{BlockSnapshotStore, PersistedSnapshot};
use pool_test_support::TempDb;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

const NET: Network = Network::Testnet;
const POOL: &str = "0x2935a721068da756b28cba896efdb64e8909dfae";
const BLOCK: &str = "block-decide-1";
const HEIGHT: u64 = 100_080;
const ROUND: u64 = 133;
/// Any non-zero adoption. Not TIG's 18-decimal scale: §12 forbids compiling an
/// observed value in, and every comparison here is between integers.
const SOME_ADOPTION: &str = "7";

fn block() -> Value {
    json!({
        "block": {
            "id": BLOCK,
            "details": {"round": ROUND, "height": HEIGHT},
            "data": {"active_ids": {"benchmark": []}},
        }
    })
}

fn benchmarks_body() -> Value {
    json!({"precommits": [], "benchmarks": [], "proofs": [], "frauds": []})
}

/// One CPU challenge with one track, one algorithm that can be run, and one
/// active benchmark to source settings from: the smallest snapshot a decision
/// can actually be made from.
fn snapshot() -> Snapshot {
    Snapshot {
        block_id: BLOCK.to_string(),
        height: HEIGHT,
        block: block(),
        reads: BTreeMap::from([
            (
                "get-challenges".to_string(),
                json!({"challenges": [{
                    "id": "c001",
                    "config": {
                        "type": "cpu",
                        "active_tracks": {"t1": {"num_nonces_per_bundle": 10}},
                        "min_num_bundles": 1,
                        "base_fee": "100",
                        "per_nonce_fee": "7",
                    },
                    "state": {"round_active": 25},
                    "block_data": {"num_qualifiers_by_track": {"t1": 30}},
                }]}),
            ),
            (
                "get-algorithms".to_string(),
                json!({
                    "codes": [{
                        "id": "c001_a001",
                        "details": {"challenge_id": "c001"},
                        "state": {"round_active": 25, "banned": false},
                        "block_data": {
                            "adoption": SOME_ADOPTION,
                            "num_qualifiers_by_track_by_player": {},
                        },
                    }],
                    "binarys": [{
                        "algorithm_id": "c001_a001",
                        "details": {
                            "compile_success": true,
                            "download_url": "https://example.invalid/b",
                        },
                        "state": {"block_confirmed": 1},
                    }],
                    "advances": [],
                }),
            ),
            ("get-benchmarks".to_string(), benchmarks_body()),
            (
                "get-opow".to_string(),
                json!({"opow": [{
                    "player_id": POOL,
                    "block_data": {"num_qualifiers_by_challenge_by_track": {}},
                }]}),
            ),
        ]),
        tracks: BTreeMap::new(),
        reads_complete: true,
        active_cache_ready: true,
    }
}

fn active() -> Vec<ActiveBenchmarkMeta> {
    vec![ActiveBenchmarkMeta {
        benchmark_id: "b-source".to_string(),
        player_id: POOL.to_string(),
        challenge_id: "c001".to_string(),
        algorithm_id: "c001_a001".to_string(),
        track_id: "t1".to_string(),
        compute_type: Some("aws_t4g".to_string()),
        num_bundles: 1,
        fuel_budget: Some(5_000_000),
        hyperparameters: Some(json!({"alpha": 3})),
        precommit_block_confirmed: 1,
        num_active_bundles: Some(4),
        average_quality_by_bundle: vec![json!(50)],
        stopped: false,
        benchmark_block_confirmed: 2,
    }]
}

fn offer() -> Offer {
    Offer {
        compute: OfferedCompute::Cpu { cores: 8 },
        tig_compute_type: "aws_t4g".to_string(),
    }
}

async fn persist(pool: &PgPool, snapshot: Snapshot) -> PersistedSnapshot {
    PostgresSnapshotStore::new(pool.clone())
        .persist(NET, snapshot)
        .await
        .expect("the snapshot persists")
}

#[allow(clippy::too_many_arguments)]
async fn run(
    pool: &PgPool,
    persisted: &PersistedSnapshot,
    limit: i64,
    trace: Option<TraceId>,
) -> Decided {
    let window = confirmed_window(&benchmarks_body(), &block()).unwrap();
    decide_once(
        pool,
        NET,
        POOL,
        &offer(),
        persisted,
        &window,
        &active(),
        limit,
        "1000",
        "unchosen-pre-build-5.2",
        [0xef; 32],
        trace,
    )
    .await
    .expect("the pass runs")
}

#[tokio::test]
async fn a_pass_commits_a_decision_its_intent_and_the_workflow_they_belong_to() {
    // D2: the decision and its PRECOMMIT intent commit in one transaction. The
    // workflow has to exist for the intent's foreign key, so all three are
    // asserted present — a pass that created a decision without an intent, or
    // an intent with no workflow, is what D2's single transaction prevents.
    let Some(db) = TempDb::migrated("decide_commits").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let persisted = persist(&pool, snapshot()).await;
    let trace = TraceId::draw().unwrap();

    let Decided::Admitted(admitted) = run(&pool, &persisted, 4, Some(trace)).await else {
        panic!("the snapshot supports a decision");
    };

    assert_eq!(admitted.intent.write_kind.as_str(), "precommit");
    assert_eq!(admitted.intent.generation, 1);
    assert_eq!(admitted.intent.state.as_str(), "PREPARED");
    assert_eq!(
        admitted.intent.trace_id,
        Some(trace),
        "criterion I3: the admitting trace reaches the durable intent"
    );

    let workflow_id = admitted.intent.workflow_id.clone();
    let row = sqlx::query(
        "SELECT w.state, w.owner_kind, w.owner_id, w.unverified_from_block,
                d.selected_challenge, d.selected_algorithm, d.compute_type,
                d.anchor_block_id, d.precommit_reserve::text AS precommit_reserve,
                d.reserve_inputs, d.tie_draw_ranks
           FROM pool.workflow w
           JOIN pool.precommit_decision d USING (workflow_id)
          WHERE w.workflow_id = $1",
    )
    .bind(&workflow_id)
    .fetch_one(&pool)
    .await
    .expect("the workflow and its decision are both there");

    assert_eq!(row.get::<String, _>("state"), "DECIDED");
    // F6: pool-owned, permanent, created now rather than retrofitted.
    assert_eq!(row.get::<String, _>("owner_kind"), "POOL_BOOTSTRAP");
    assert_eq!(row.get::<String, _>("owner_id"), "pool-bootstrap");
    assert_eq!(
        row.get::<i64, _>("unverified_from_block"),
        i64::try_from(HEIGHT).unwrap(),
        "§6.1's unverified interval opens at the anchor height"
    );

    assert_eq!(row.get::<String, _>("selected_challenge"), "c001");
    assert_eq!(row.get::<String, _>("selected_algorithm"), "c001_a001");
    assert_eq!(
        row.get::<String, _>("compute_type"),
        "aws_t4g",
        "TIG's protocol type, which is the offer's — not the challenge's class"
    );
    // D2e: the decision names the snapshot it was made from.
    assert_eq!(row.get::<String, _>("anchor_block_id"), BLOCK);

    // D2c: the reserve's inputs are recorded, and the amount is the §6.8 fee
    // for the sized track plus the configured failure charge.
    let inputs: Value = row.get("reserve_inputs");
    assert_eq!(
        inputs["live_method_penalties"],
        json!([]),
        "computed, and there were none — slice 1 has no members to report"
    );
    assert_eq!(inputs["policy_version"], json!("unchosen-pre-build-5.2"));
    assert_eq!(
        inputs["by_track"]["t1"]["tig_fee_atoms"],
        json!("128"),
        "§6.8: 100 + 7 * 4 bundles"
    );
    assert_eq!(
        row.get::<String, _>("precommit_reserve"),
        "1128",
        "the fee plus the configured failure charge, as the maximum across tracks"
    );

    // D2d: the rank map is the audit evidence.
    let ranks: Value = row.get("tie_draw_ranks");
    assert!(
        ranks
            .get("c001")
            .and_then(Value::as_str)
            .is_some_and(|r| r.len() == 64),
        "every eligible challenge carries a 64-hex rank: {ranks}"
    );
}

#[tokio::test]
async fn a_snapshot_that_is_not_decision_usable_decides_nothing() {
    // C5: the orchestrator performs no work while the snapshot is incomplete
    // or the active cache is unavailable. Not an error — a pass arriving
    // before the warm-up finishes is ordinary — but it must write nothing.
    let Some(db) = TempDb::migrated("decide_not_usable").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    for why in ["reads", "cache"] {
        let mut snapshot = snapshot();
        // Persisted through the real store, not constructed: `PersistedSnapshot`
        // has no public constructor precisely so a caller cannot assert a
        // persistence it did not perform, and an incomplete assembly *is*
        // recorded — `load_usable_record` is what refuses to return it.
        if why == "reads" {
            snapshot.reads_complete = false;
        } else {
            snapshot.active_cache_ready = false;
        }
        snapshot.block_id = format!("{BLOCK}-{why}");
        let persisted = persist(&pool, snapshot).await;

        let decided = run(&pool, &persisted, 4, None).await;
        assert!(
            matches!(decided, Decided::SnapshotNotUsable(_)),
            "{why}: expected no work, got {decided:?}"
        );
    }

    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(intents, 0, "an unusable snapshot writes nothing");
    let workflows: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.workflow")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(workflows, 0, "and creates no workflow either");
}

#[tokio::test]
async fn the_unverified_limit_refuses_the_pass_that_would_exceed_it() {
    // D2a / §10 invariant 23: the pool never creates a precommit at or above
    // `internal_pool_unverified_limit`. Driven to the limit rather than
    // asserted at one, because the count is authoritative and recounted inside
    // the lease — a cached metric must never authorize work.
    let Some(db) = TempDb::migrated("decide_limit").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let persisted = persist(&pool, snapshot()).await;

    for i in 0..2 {
        assert!(
            matches!(run(&pool, &persisted, 2, None).await, Decided::Admitted(_)),
            "pass {i} is within the limit"
        );
    }

    let window = confirmed_window(&benchmarks_body(), &block()).unwrap();
    let err = decide_once(
        &pool,
        NET,
        POOL,
        &offer(),
        &persisted,
        &window,
        &active(),
        2,
        "1000",
        "unchosen-pre-build-5.2",
        [0xef; 32],
        None,
    )
    .await
    .expect_err("the third exceeds the limit of 2");
    assert!(
        err.to_string().contains("unverified"),
        "the refusal names the limit: {err}"
    );

    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(intents, 2, "exactly the limit, and no more");
}

#[tokio::test]
async fn two_passes_over_one_snapshot_are_two_workflows_not_one() {
    // The workflow id is drawn, not derived from the decision's inputs. A
    // derived id would make the second pass collide with the first and attach
    // its decision to the existing workflow — quieter and worse than the
    // duplicate-intent conflict §7.3's key already refuses.
    let Some(db) = TempDb::migrated("decide_ids").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let persisted = persist(&pool, snapshot()).await;

    let Decided::Admitted(first) = run(&pool, &persisted, 4, None).await else {
        panic!("first pass admits");
    };
    let Decided::Admitted(second) = run(&pool, &persisted, 4, None).await else {
        panic!("second pass admits");
    };
    assert_ne!(first.intent.workflow_id, second.intent.workflow_id);
    assert_ne!(first.intent.intent_id, second.intent.intent_id);
}
