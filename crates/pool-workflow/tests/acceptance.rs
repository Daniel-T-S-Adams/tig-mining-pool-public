//! Slice-1 criterion F4a: the two facts a benchmark or proof write may not
//! exist without (`architecture.md` §13 invariants 4 and 5).
//!
//! These are **negative** assertions by design. The plan says the
//! preconditions must hold "on the fake-tig path too", which means the
//! interesting property is not that a guarded write succeeds — it is that an
//! unguarded one cannot be created at all, by anyone, including a test.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::{
    CanonicalPayload, NewIntent, PackageAcceptance, PostgresIntentRepository,
    TigWriteIntentRepository, WriteKind, record_acceptance, record_canonical_payload,
};

const NET: Network = Network::Testnet;
const PAYLOAD: [u8; 32] = [0xab; 32];
const SAMPLE: [u8; 32] = [0x5c; 32];

fn benchmark_intent(workflow: &str, benchmark: &str) -> NewIntent {
    NewIntent {
        network: NET,
        workflow_id: workflow.to_string(),
        write_kind: WriteKind::Benchmark,
        generation: 1,
        benchmark_id: Some(benchmark.to_string()),
        payload_digest: PAYLOAD,
        payload_artifact_id: None,
    }
}

fn proof_intent(workflow: &str, benchmark: &str, artifact: Option<&str>) -> NewIntent {
    NewIntent {
        network: NET,
        workflow_id: workflow.to_string(),
        write_kind: WriteKind::Proof,
        generation: 1,
        benchmark_id: Some(benchmark.to_string()),
        payload_digest: PAYLOAD,
        payload_artifact_id: artifact.map(str::to_string),
    }
}

fn acceptance(workflow: &str, benchmark: &str) -> PackageAcceptance {
    PackageAcceptance {
        network: NET,
        workflow_id: workflow.to_string(),
        benchmark_id: benchmark.to_string(),
        package_sha256: [0x7a; 32],
    }
}

fn payload(artifact: &str, workflow: &str, benchmark: &str) -> CanonicalPayload {
    CanonicalPayload {
        network: NET,
        artifact_id: artifact.to_string(),
        workflow_id: workflow.to_string(),
        benchmark_id: benchmark.to_string(),
        sample_digest: SAMPLE,
        payload_digest: PAYLOAD,
    }
}

#[tokio::test]
async fn a_benchmark_write_cannot_exist_before_durable_acceptance() {
    // §13 invariant 4, verbatim: "No benchmark commitment intent exists before
    // durable package acceptance." Refused by the database, so it holds for
    // every writer rather than for the ones that remembered to check — which
    // is what makes it an invariant rather than a convention.
    let Some(db) = TempDb::migrated("accept_invariant4").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1", "w2"]).await;
    let repo = PostgresIntentRepository::new(pool.clone());

    let error = repo
        .create(benchmark_intent("w1", "bench_a"))
        .await
        .expect_err("no acceptance is recorded for bench_a");
    assert!(
        format!("{error:?}").contains("invariant 4"),
        "the refusal should name the rule: {error:?}"
    );

    // An acceptance for a *different* benchmark is not this benchmark's. The
    // row is keyed by benchmark precisely so it cannot be borrowed.
    record_acceptance(&pool, &acceptance("w1", "bench_other"))
        .await
        .unwrap();
    repo.create(benchmark_intent("w1", "bench_a"))
        .await
        .expect_err("an acceptance for another benchmark is not evidence for this one");

    // And an acceptance recorded against a different workflow is not this
    // workflow's either.
    record_acceptance(&pool, &acceptance("w2", "bench_a"))
        .await
        .unwrap();
    repo.create(benchmark_intent("w1", "bench_a"))
        .await
        .expect_err("another workflow's acceptance is not this workflow's");

    record_acceptance(&pool, &acceptance("w1", "bench_a"))
        .await
        .unwrap();
    let intent = repo
        .create(benchmark_intent("w1", "bench_a"))
        .await
        .expect("the precondition now holds");
    assert_eq!(intent.write_kind, WriteKind::Benchmark);
}

#[tokio::test]
async fn a_proof_write_cannot_exist_before_a_canonical_payload_for_the_sample() {
    // §13 invariant 5: "No proof intent exists before a canonical proof
    // payload for the confirmed sample."
    //
    // All three of artifact, benchmark and digest must line up. A payload that
    // exists but was built for another benchmark, or bytes other than the ones
    // being transmitted, satisfies the invariant's letter and not its point:
    // the write would still send a proof nothing vouches for.
    let Some(db) = TempDb::migrated("accept_invariant5").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1", "w2"]).await;
    let repo = PostgresIntentRepository::new(pool.clone());

    // Naming no payload at all.
    let error = repo
        .create(proof_intent("w1", "bench_a", None))
        .await
        .expect_err("a proof that names no payload");
    assert!(
        format!("{error:?}").contains("invariant 5"),
        "the refusal should name the rule: {error:?}"
    );

    // Naming one that does not exist.
    repo.create(proof_intent("w1", "bench_a", Some("artifact/ghost")))
        .await
        .expect_err("a proof naming a payload nobody built");

    // A payload built for another benchmark of the same workflow.
    record_canonical_payload(&pool, &payload("artifact/other", "w1", "bench_other"))
        .await
        .unwrap();
    repo.create(proof_intent("w1", "bench_a", Some("artifact/other")))
        .await
        .expect_err("a payload for another benchmark is not this benchmark's proof");

    // A payload for the right benchmark, but over different bytes than the
    // intent will transmit.
    let mut wrong_bytes = payload("artifact/stale", "w1", "bench_a");
    wrong_bytes.payload_digest = [0x11; 32];
    record_canonical_payload(&pool, &wrong_bytes).await.unwrap();
    repo.create(proof_intent("w1", "bench_a", Some("artifact/stale")))
        .await
        .expect_err("the intent would transmit bytes this payload does not contain");

    record_canonical_payload(&pool, &payload("artifact/good", "w1", "bench_a"))
        .await
        .unwrap();
    let intent = repo
        .create(proof_intent("w1", "bench_a", Some("artifact/good")))
        .await
        .expect("artifact, benchmark and digest all agree");
    assert_eq!(intent.payload_artifact_id.as_deref(), Some("artifact/good"));
}

#[tokio::test]
async fn a_precommit_needs_neither_precondition() {
    // The rule is conditional on the write kind, which is why it is a trigger
    // and not a foreign key. §6.1 decides a precommit before any package
    // exists — there is nothing to have accepted — so a precondition applied
    // to every kind would stop the pool mining at all.
    let Some(db) = TempDb::migrated("accept_precommit").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;
    let repo = PostgresIntentRepository::new(pool.clone());

    let intent = repo
        .create(NewIntent {
            network: NET,
            workflow_id: "w1".to_string(),
            write_kind: WriteKind::Precommit,
            generation: 1,
            benchmark_id: None,
            payload_digest: PAYLOAD,
            payload_artifact_id: None,
        })
        .await
        .expect("a precommit has no package and no proof to vouch for");
    assert_eq!(intent.write_kind, WriteKind::Precommit);
}

#[tokio::test]
async fn re_recording_the_same_fact_is_idempotent_and_a_different_one_is_a_conflict() {
    // A caller that crashes between recording a precondition and using it must
    // be able to start again, so an identical re-record succeeds. A *different*
    // one must not: both rows are the evidence that a write was permitted, and
    // the write they authorised may already have gone.
    let Some(db) = TempDb::migrated("accept_idempotent").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;

    record_acceptance(&pool, &acceptance("w1", "bench_a"))
        .await
        .unwrap();
    record_acceptance(&pool, &acceptance("w1", "bench_a"))
        .await
        .expect("the same acceptance again");

    let mut different = acceptance("w1", "bench_a");
    different.package_sha256 = [0x99; 32];
    let error = record_acceptance(&pool, &different)
        .await
        .expect_err("different bytes under the same key");
    assert!(
        matches!(error, pool_workflow::AcceptanceError::Conflict { .. }),
        "{error:?}"
    );

    record_canonical_payload(&pool, &payload("artifact/a", "w1", "bench_a"))
        .await
        .unwrap();
    record_canonical_payload(&pool, &payload("artifact/a", "w1", "bench_a"))
        .await
        .expect("the same payload again");

    let mut moved = payload("artifact/a", "w1", "bench_a");
    moved.sample_digest = [0x22; 32];
    let error = record_canonical_payload(&pool, &moved)
        .await
        .expect_err("a content-addressed key naming different contents");
    assert!(
        matches!(error, pool_workflow::AcceptanceError::Conflict { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_precondition_cannot_be_rewritten_or_removed() {
    // The rows are the evidence a write was permitted. A controller that could
    // edit or delete one could authorise a write and then erase what
    // authorised it, which is the audit `architecture.md` §13 invariant 6
    // asks for. Withheld at the grant, so it is not a rule this code could
    // forget.
    let Some(db) = TempDb::migrated("accept_immutable").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;
    record_acceptance(&pool, &acceptance("w1", "bench_a"))
        .await
        .unwrap();
    record_canonical_payload(&pool, &payload("artifact/a", "w1", "bench_a"))
        .await
        .unwrap();

    for sql in [
        "UPDATE pool.package_acceptance SET package_sha256 = decode(repeat('00', 32), 'hex')",
        "DELETE FROM pool.package_acceptance",
        "UPDATE pool.canonical_payload SET payload_digest = decode(repeat('00', 32), 'hex')",
        "DELETE FROM pool.canonical_payload",
    ] {
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&pool)
            .await
            .expect_err("the controller may record these and nothing else");
        assert!(
            pool_test_support::is_insufficient_privilege(&error),
            "unexpected error for {sql}: {error}"
        );
    }
}

#[tokio::test]
async fn a_precondition_can_be_written_inside_the_caller_s_transaction() {
    // `architecture.md` §7.2 commits the durable artifact pointer, the
    // receipt, the assignment transition, the slot release and the controller
    // event together. An acceptance written on its own connection could
    // survive a rollback of the acceptance it records — a row saying a write
    // was permitted, for a package the pool did not in the end accept.
    //
    // Rolled back here rather than committed, because the property is that
    // these rows share the caller's fate. A test that only committed would
    // pass just as well against a function that used its own connection.
    let Some(db) = TempDb::migrated("accept_in_tx").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    pool_test_support::seed_workflows(&pool, "testnet", &["w1"]).await;

    let mut tx = pool.begin().await.unwrap();
    record_acceptance(&mut *tx, &acceptance("w1", "bench_a"))
        .await
        .unwrap();
    record_canonical_payload(&mut *tx, &payload("artifact/a", "w1", "bench_a"))
        .await
        .unwrap();
    // Visible to this transaction...
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.package_acceptance")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 1);
    tx.rollback().await.unwrap();

    // ...and gone with it.
    for table in ["pool.package_acceptance", "pool.canonical_payload"] {
        let left: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0, "{table} outlived the transaction that wrote it");
    }
}
