//! The §6.2 commitment intent: created only where it is owed, and never from
//! a fabricated precondition against real TIG (criteria F4a, F4d).
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_config::TigConfig;
use pool_controller::commit::{CommitError, create_commitment_intent};
use pool_controller::stub::{stub_acceptance, stub_commitment_payload};
use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::{BenchmarkSubmission, IntentState, WorkflowState, benchmark_digest, workflow};
use serde_json::json;
use sqlx::PgPool;

const NET: Network = Network::Testnet;
const BENCH: &str = "bench_a";
const ARTIFACT: &str = "artifact/w1/commitment";

fn fake() -> TigConfig {
    TigConfig {
        base_url: "http://127.0.0.1:8080".to_string(),
        player_id: "0x2935a721068da756b28cba896efdb64e8909dfae".to_string(),
    }
}

fn live() -> TigConfig {
    TigConfig {
        base_url: "https://testnet-api.tig.foundation".to_string(),
        player_id: "0x2935a721068da756b28cba896efdb64e8909dfae".to_string(),
    }
}

fn committed(nonces: usize) -> BenchmarkSubmission {
    BenchmarkSubmission {
        benchmark_id: BENCH.to_string(),
        merkle_root: "ab".repeat(32),
        solution_quality: (0..nonces as i64).collect(),
    }
}

/// A workflow at PRECOMMIT_CONFIRMED, the way the binding pass leaves one.
///
/// The settings/details split is TIG's, and the test keeps it: §6.2's
/// `num_nonces` is a *detail*, so it is carried in its own field and never
/// inside `settings`. An earlier version of this helper injected it into
/// `settings` — a shape the confirmed read never produces — which is exactly
/// what let the length check look tested while it never ran.
async fn confirmed_workflow(pool: &PgPool, num_nonces: Option<i64>) {
    pool_test_support::seed_workflows(pool, "testnet", &["w1"]).await;
    let current = workflow::find(pool, NET, "w1").await.unwrap().unwrap();
    workflow::confirm_precommit(
        pool,
        NET,
        "w1",
        current.revision,
        &workflow::ConfirmedPrecommit {
            benchmark_id: BENCH.to_string(),
            track_id: "t001".to_string(),
            // As `get-benchmark-data` serves it: settings and details are
            // disjoint objects, and this is the settings one.
            settings: json!({
                "player_id": "0xp00l00000000000000000000000000000000000",
                "block_id": "block_100080",
                "challenge_id": "c001",
                "algorithm_id": "a011",
                "track_id": "t001"
            }),
            num_nonces,
            block_confirmed: 100_081,
            block_started: 100_081,
        },
    )
    .await
    .unwrap();
}

async fn preconditions(pool: &PgPool, submission: &BenchmarkSubmission) {
    stub_acceptance(pool, &fake(), NET, "w1", BENCH)
        .await
        .unwrap();
    stub_commitment_payload(
        pool,
        &fake(),
        NET,
        ARTIFACT,
        "w1",
        BENCH,
        benchmark_digest(submission),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_confirmed_precommit_with_both_preconditions_gets_its_commitment() {
    let Some(db) = TempDb::migrated("commit_ok").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let submission = committed(4);
    confirmed_workflow(&pool, Some(4)).await;
    preconditions(&pool, &submission).await;

    let intent = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &submission)
        .await
        .expect("the write is owed and both preconditions hold");
    assert_eq!(intent.benchmark_id.as_deref(), Some(BENCH));
    assert_eq!(intent.state, IntentState::Prepared);
    assert_eq!(intent.payload_digest, benchmark_digest(&submission));

    // Idempotent: the crash-retry path re-creates the identical intent.
    let again = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &submission)
        .await
        .unwrap();
    assert_eq!(again.intent_id, intent.intent_id);
}

#[tokio::test]
async fn a_fabricated_precondition_is_refused_against_a_live_endpoint() {
    // F4d's accepting half. The row exists — a build with the stub wrote it —
    // and the endpoint is what decides whether acting on it is safe. This
    // half is in every build, unlike the creating half.
    let Some(db) = TempDb::migrated("commit_stub_live").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let submission = committed(4);
    confirmed_workflow(&pool, Some(4)).await;
    preconditions(&pool, &submission).await;

    let err = create_commitment_intent(&pool, NET, &live(), "w1", ARTIFACT, &submission)
        .await
        .expect_err("a stubbed precondition may not become a real write");
    assert!(
        matches!(err, CommitError::StubAgainstLiveEndpoint { .. }),
        "{err}"
    );
    // Named by host, never by URL.
    assert!(
        err.to_string().contains("testnet-api.tig.foundation"),
        "{err}"
    );

    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(intents, 0, "the refusal must leave no intent behind");
}

#[tokio::test]
async fn a_real_precondition_is_fine_against_a_live_endpoint() {
    // The guard reads the rows' origin, not the endpoint alone: an earned
    // acceptance is exactly what a live commitment is supposed to rest on.
    let Some(db) = TempDb::migrated("commit_real_live").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let submission = committed(4);
    confirmed_workflow(&pool, Some(4)).await;
    pool_workflow::record_acceptance(
        &pool,
        &pool_workflow::PackageAcceptance {
            network: NET,
            workflow_id: "w1".to_string(),
            benchmark_id: BENCH.to_string(),
            package_sha256: [0x11; 32],
            stub_origin: false,
        },
    )
    .await
    .unwrap();
    pool_workflow::record_commitment_payload(
        &pool,
        &pool_workflow::CommitmentPayload {
            network: NET,
            artifact_id: ARTIFACT.to_string(),
            workflow_id: "w1".to_string(),
            benchmark_id: BENCH.to_string(),
            payload_digest: benchmark_digest(&submission),
            stub_origin: false,
        },
    )
    .await
    .unwrap();

    create_commitment_intent(&pool, NET, &live(), "w1", ARTIFACT, &submission)
        .await
        .expect("an earned precondition is what a live write rests on");
}

#[tokio::test]
async fn a_commitment_is_owed_only_from_a_confirmed_precommit() {
    let Some(db) = TempDb::migrated("commit_state").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let submission = committed(4);
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;

    let err = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &submission)
        .await
        .expect_err("a DECIDED workflow has confirmed nothing");
    assert!(
        matches!(err, CommitError::NotAwaitingCommitment { .. }),
        "{err}"
    );
    assert_eq!(
        workflow::find(&pool, NET, "w1")
            .await
            .unwrap()
            .unwrap()
            .state,
        WorkflowState::Decided
    );
}

#[tokio::test]
async fn the_quality_vector_must_be_the_length_the_confirmed_precommit_fixed() {
    // §6.2 fixes the length at the confirmed `num_nonces`, and TIG refuses a
    // body of any other length *after* the fee is paid. Checked before the
    // intent exists, so the pool never pays for a body it built wrong.
    let Some(db) = TempDb::migrated("commit_length").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let wrong = committed(3);
    confirmed_workflow(&pool, Some(4)).await;
    preconditions(&pool, &wrong).await;

    let err = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &wrong)
        .await
        .expect_err("three qualities for four nonces");
    assert!(err.to_string().contains("num_nonces is 4"), "{err}");

    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(intents, 0);
}

#[tokio::test]
async fn a_workflow_with_no_confirmed_length_is_refused_rather_than_unchecked() {
    // §6.2 builds the body to TIG's `num_nonces`. Without it there is
    // nothing to check against, and treating that as permission is what made
    // the check silently dead when it read the wrong object.
    let Some(db) = TempDb::migrated("commit_no_length").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let submission = committed(4);
    confirmed_workflow(&pool, None).await;
    preconditions(&pool, &submission).await;

    let err = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &submission)
        .await
        .expect_err("no length, no commitment");
    assert!(
        matches!(err, CommitError::NoConfirmedLength { .. }),
        "{err}"
    );

    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(intents, 0);
}

#[tokio::test]
async fn a_commitment_for_another_benchmark_is_refused() {
    let Some(db) = TempDb::migrated("commit_wrong_bench").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    confirmed_workflow(&pool, Some(4)).await;
    let other = BenchmarkSubmission {
        benchmark_id: "bench_other".to_string(),
        merkle_root: "ab".repeat(32),
        solution_quality: (0..4).collect(),
    };
    preconditions(&pool, &other).await;

    let err = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &other)
        .await
        .expect_err("the workflow owns bench_a");
    assert!(matches!(err, CommitError::WrongBenchmark { .. }), "{err}");
}

#[tokio::test]
async fn an_intent_whose_digest_is_not_the_built_payloads_is_refused() {
    // `migrations/0015`: artifact, benchmark and digest must line up, so an
    // intent cannot cite a real built payload while carrying other bytes.
    let Some(db) = TempDb::migrated("commit_digest").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let built = committed(4);
    confirmed_workflow(&pool, Some(4)).await;
    preconditions(&pool, &built).await;

    let different = BenchmarkSubmission {
        benchmark_id: BENCH.to_string(),
        merkle_root: "cd".repeat(32),
        solution_quality: (0..4).collect(),
    };
    let err = create_commitment_intent(&pool, NET, &fake(), "w1", ARTIFACT, &different)
        .await
        .expect_err("the built payload says otherwise");
    assert!(
        err.to_string().contains("commitment payload"),
        "the database's own refusal should surface: {err}"
    );
}
