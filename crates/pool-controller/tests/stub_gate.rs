//! Slice-1 criteria F4a and F4d: the stub records the preconditions
//! `architecture.md` §13 invariants 4 and 5 require, and refuses to do it
//! anywhere but against a local `fake-tig`.
//!
//! F4c — that the stub is absent from a default build at all — is not
//! testable from here. A test binary necessarily has the feature on (the
//! dev-dependency turns it on), so a test asserting absence would be
//! asserting it about a build where it is present. `scripts/feature-gate.sh`
//! proves it instead, by compiling a probe outside the workspace, and
//! `make check` runs that. This file covers the other half: what the stub does
//! when it *is* compiled in.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_config::TigConfig;
use pool_controller::stub::{
    StubError, stub_acceptance, stub_canonical_payload, stub_commitment_payload,
};
use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::{NewIntent, PostgresIntentRepository, TigWriteIntentRepository, WriteKind};

const NET: Network = Network::Testnet;
const PAYLOAD: [u8; 32] = [0xab; 32];

fn fake() -> TigConfig {
    TigConfig {
        base_url: "http://127.0.0.1:8080".to_string(),
        player_id: "0x2935a721068da756b28cba896efdb64e8909dfae".to_string(),
    }
}

fn live() -> TigConfig {
    TigConfig {
        base_url: "https://api.tig.foundation".to_string(),
        player_id: "0x2935a721068da756b28cba896efdb64e8909dfae".to_string(),
    }
}

#[tokio::test]
async fn the_stub_satisfies_the_preconditions_it_stands_in_for() {
    // F4a: "simulated bytes are fine, a missing precondition is not". The
    // point of the stub is that the *check* still runs — the write path is
    // exercised with its guard intact, so slice 1 cannot enshrine an
    // unguarded commitment path in a passing test.
    let Some(db) = TempDb::migrated("stub_satisfies").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;
    let repo = PostgresIntentRepository::new(pool.clone());

    // Without the stub, the invariants refuse both writes.
    repo.create(NewIntent {
        network: NET,
        workflow_id: "w1".to_string(),
        write_kind: WriteKind::Benchmark,
        generation: 1,
        benchmark_id: Some("bench_a".to_string()),
        payload_digest: PAYLOAD,
        payload_artifact_id: None,
    })
    .await
    .expect_err("invariant 4 is still in force");

    // Acceptance alone is not enough: `migrations/0015` also requires the
    // built commitment the gateway will check its bytes against.
    stub_acceptance(&pool, &fake(), NET, "w1", "bench_a")
        .await
        .unwrap();
    repo.create(NewIntent {
        network: NET,
        workflow_id: "w1".to_string(),
        write_kind: WriteKind::Benchmark,
        generation: 1,
        benchmark_id: Some("bench_a".to_string()),
        payload_digest: PAYLOAD,
        payload_artifact_id: Some("artifact/w1/commitment".to_string()),
    })
    .await
    .expect_err("acceptance without a built commitment is half a precondition");

    stub_commitment_payload(
        &pool,
        &fake(),
        NET,
        "artifact/w1/commitment",
        "w1",
        "bench_a",
        PAYLOAD,
    )
    .await
    .unwrap();
    repo.create(NewIntent {
        network: NET,
        workflow_id: "w1".to_string(),
        write_kind: WriteKind::Benchmark,
        generation: 1,
        benchmark_id: Some("bench_a".to_string()),
        payload_digest: PAYLOAD,
        payload_artifact_id: Some("artifact/w1/commitment".to_string()),
    })
    .await
    .expect("both halves now hold");

    stub_canonical_payload(
        &pool,
        &fake(),
        NET,
        "artifact/w1/proof",
        "w1",
        "bench_a",
        PAYLOAD,
    )
    .await
    .unwrap();
    repo.create(NewIntent {
        network: NET,
        workflow_id: "w1".to_string(),
        write_kind: WriteKind::Proof,
        generation: 1,
        benchmark_id: Some("bench_a".to_string()),
        payload_digest: PAYLOAD,
        payload_artifact_id: Some("artifact/w1/proof".to_string()),
    })
    .await
    .expect("invariant 5 is satisfied by the stub payload");
}

#[tokio::test]
async fn the_stub_refuses_a_live_endpoint_and_writes_nothing() {
    // F4d, defence in depth behind F4c. A build carrying the feature is
    // exactly the thing someone eventually points at a real endpoint, so the
    // two guards fail independently.
    //
    // Refusing is not enough on its own — it has to refuse *before* writing.
    // A guard that recorded the row and then returned an error would leave the
    // fabricated precondition behind, and invariant 4 asks whether the row
    // exists, not how the call that made it ended.
    let Some(db) = TempDb::migrated("stub_live").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;

    let error = stub_acceptance(&pool, &live(), NET, "w1", "bench_a")
        .await
        .expect_err("a live endpoint gets no fabricated acceptance");
    assert!(matches!(error, StubError::NotFakeTig { .. }), "{error:?}");
    // The endpoint is named, because that is the fact that decided it.
    assert!(
        error.to_string().contains("api.tig.foundation"),
        "the refusal should say what it was pointed at: {error}"
    );

    let error = stub_canonical_payload(
        &pool,
        &live(),
        NET,
        "artifact/w1/proof",
        "w1",
        "bench_a",
        PAYLOAD,
    )
    .await
    .expect_err("nor a fabricated payload");
    assert!(matches!(error, StubError::NotFakeTig { .. }), "{error:?}");

    let error = stub_commitment_payload(
        &pool,
        &live(),
        NET,
        "artifact/w1/commitment",
        "w1",
        "bench_a",
        PAYLOAD,
    )
    .await
    .expect_err("nor a fabricated commitment");
    assert!(matches!(error, StubError::NotFakeTig { .. }), "{error:?}");

    for table in [
        "pool.package_acceptance",
        "pool.canonical_payload",
        "pool.commitment_payload",
    ] {
        let rows: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 0, "{table} was written despite the refusal");
    }
}

#[tokio::test]
async fn the_guard_reads_the_endpoint_and_not_the_network() {
    // A2 pins `network` to exactly "testnet", so a guard keyed to it would
    // pass in the one case F4d exists to prevent: a testnet-configured
    // process pointed at the real testnet API. Both calls here are on
    // `Network::Testnet`; only the endpoint differs, and only the endpoint
    // decides.
    let Some(db) = TempDb::migrated("stub_endpoint").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1", "w2"]).await;

    stub_acceptance(&pool, &fake(), Network::Testnet, "w1", "bench_a")
        .await
        .expect("testnet against the local fake");
    stub_acceptance(&pool, &live(), Network::Testnet, "w2", "bench_b")
        .await
        .expect_err("the same network, a real endpoint");
}

#[tokio::test]
async fn a_stub_row_says_it_is_one() {
    // F4d's other half — refusing to *accept* a fabricated record — needs to
    // tell one from a real acceptance, and that is a fact about the row's
    // origin rather than its contents. It can only be recorded when the row is
    // written: a stub row inserted without it is indistinguishable from a real
    // acceptance for the rest of the database's life.
    //
    // The guard that reads this lands with the intent-creation path it guards
    // (F4). What lands here is the evidence, because it cannot land later.
    let Some(db) = TempDb::migrated("stub_marks_origin").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;

    stub_acceptance(&pool, &fake(), NET, "w1", "bench_a")
        .await
        .unwrap();
    stub_canonical_payload(&pool, &fake(), NET, "artifact/a", "w1", "bench_a", PAYLOAD)
        .await
        .unwrap();

    for table in ["pool.package_acceptance", "pool.canonical_payload"] {
        let stubbed: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT bool_and(stub_origin) FROM {table}"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(stubbed, "{table} does not record that the stub wrote it");
    }

    // And it cannot be relabelled. `migrations/0011` withholds UPDATE, so the
    // grant refuses first; the trigger is what holds if a later slice grants
    // UPDATE for some legitimate column.
    let error = sqlx::query("UPDATE pool.package_acceptance SET stub_origin = false")
        .execute(&pool)
        .await
        .expect_err("a fabricated precondition cannot become a real one");
    assert!(
        pool_test_support::is_insufficient_privilege(&error)
            || error.to_string().contains("stub_origin is immutable"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn the_real_recording_path_does_not_mark_rows_as_stubs() {
    // The other direction, and the one that decides whether the flag means
    // anything: a row written through `pool_workflow::record_acceptance` — the
    // path step 2's real acceptance will use — must not claim to be
    // fabricated, or F4d's guard would refuse genuine work.
    let Some(db) = TempDb::migrated("stub_origin_default").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;

    pool_workflow::record_acceptance(
        &pool,
        &pool_workflow::PackageAcceptance {
            network: NET,
            workflow_id: "w1".to_string(),
            benchmark_id: "bench_a".to_string(),
            package_sha256: [0x11; 32],
            stub_origin: false,
        },
    )
    .await
    .unwrap();

    let stubbed: bool =
        sqlx::query_scalar("SELECT bool_or(stub_origin) FROM pool.package_acceptance")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!stubbed, "a real acceptance must not read as fabricated");
}
