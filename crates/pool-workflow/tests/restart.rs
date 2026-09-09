//! Slice-1 criteria G1, G4 and G5: restart reconciliation and missing blocks
//! (`tig_integration.md` §10), against a real PostgreSQL 18.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::restart::{self, ConfirmedWindow, NeedsAttention, Reconciled};
use pool_workflow::workflow::{
    self, ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof, Owner,
    POOL_BOOTSTRAP_OWNER, Submitted, WorkflowState,
};
use pool_workflow::{AttemptOutcome, PostgresAttemptLedger, WriteAttemptLedger};
use serde_json::json;
use sqlx::Connection;

const NET: Network = Network::Testnet;

fn precommit(benchmark: &str, block: i64) -> ConfirmedPrecommit {
    ConfirmedPrecommit {
        benchmark_id: benchmark.to_string(),
        block_confirmed: block,
        // TIG's own record of when the benchmark began; §8's deadlines are
        // ages from here, and it is a few blocks before confirmation.
        block_started: block - 2,
        track_id: "t002".to_string(),
        settings: json!({ "num_bundles": 5 }),
    }
}

/// The decision's anchor height, where §6.1's unverified interval opens.
const DECIDED_AT: i64 = 90;

async fn decided(pool: &sqlx::PgPool, id: &str) -> pool_workflow::Workflow {
    workflow::create(
        pool,
        NET,
        id,
        Owner::PoolBootstrap,
        POOL_BOOTSTRAP_OWNER,
        DECIDED_AT,
    )
    .await
    .unwrap()
}

/// A workflow that already owns a benchmark, as one does after its precommit
/// confirmed before the restart.
async fn owning(pool: &sqlx::PgPool, id: &str, benchmark: &str) -> pool_workflow::Workflow {
    let w = decided(pool, id).await;
    workflow::confirm_precommit(pool, NET, id, w.revision, &precommit(benchmark, 100))
        .await
        .unwrap()
}

fn window(at_block: i64) -> ConfirmedWindow {
    ConfirmedWindow {
        at_block,
        ..ConfirmedWindow::default()
    }
}

#[tokio::test]
async fn a_restart_advances_each_workflow_as_far_as_its_evidence_goes() {
    // §10 steps 1, 4 and 5. Four workflows at different points, one pass, and
    // each lands where its own confirmed evidence puts it — the
    // `restart_recovery_reconciles_three_workflows` shape from
    // `fixtures/queue-lifecycle/v1`, including the one place slice 1 cannot
    // reach the fixture's answer (w4, below).
    let Some(db) = TempDb::migrated("restart_three").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    owning(&pool, "w1", "bench_a").await;
    owning(&pool, "w2", "bench_b").await;
    owning(&pool, "w3", "bench_c").await;
    owning(&pool, "w4", "bench_d").await;

    let mut w = window(200);
    // w1: benchmark confirmed only.
    w.benchmarks.insert(
        "bench_a".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    );
    // w2: benchmark, proof and verification all present — one pass, three steps.
    w.benchmarks.insert(
        "bench_b".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_b".to_string(),
            block_confirmed: 111,
            stopped: false,
        },
    );
    w.proofs.insert(
        "bench_b".to_string(),
        ConfirmedProof {
            benchmark_id: "bench_b".to_string(),
            block_confirmed: 121,
        },
    );
    w.verified.push("bench_b".to_string());
    // w3: TIG stopped it. §7 sends no proof.
    w.benchmarks.insert(
        "bench_c".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_c".to_string(),
            block_confirmed: 112,
            stopped: true,
        },
    );
    // w4 is the fixture's wf_2008 exactly: a confirmed proof and membership of
    // `active_ids.benchmark`, with no `confirmed_ids.verified` entry — the
    // verification happened in a block this pass never read.
    //
    // The fixture expects PROOF_SUBMITTED -> PROOF_CONFIRMED -> ACTIVE. Slice
    // 1 reaches the first of those and stops: §4.5's ladder puts VERIFYING and
    // ACTIVE past VERIFIED, and this enum's twelve states end at VERIFIED, so
    // ACTIVE is not a state the pool can record yet. Advancing to VERIFIED
    // instead would be worse than stopping — `confirm_verified` closes §6.1's
    // interval *at a block*, and the block that verified this benchmark is
    // gone, so it would be closed after the fact for every block in between
    // and §7.6's per-block recount reads exactly that. The divergence is
    // recorded in `fixtures/queue-lifecycle/v1/README.md`.
    w.benchmarks.insert(
        "bench_d".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_d".to_string(),
            block_confirmed: 113,
            stopped: false,
        },
    );
    w.proofs.insert(
        "bench_d".to_string(),
        ConfirmedProof {
            benchmark_id: "bench_d".to_string(),
            block_confirmed: 123,
        },
    );
    w.active.push("bench_d".to_string());

    let report = restart::reconcile_after_restart(&pool, NET, &w)
        .await
        .unwrap();
    assert_eq!(report.advanced.len(), 4, "{report:?}");
    assert_eq!(
        report.needs_attention,
        vec![NeedsAttention::VerificationMissed {
            workflow_id: "w4".to_string(),
            benchmark_id: "bench_d".to_string(),
        }],
        "the fixture's ACTIVE is reported, not invented: {report:?}"
    );
    assert!(
        !report.blocks_claiming(),
        "a missed verification is work to resolve, not a reason to stop: {report:?}"
    );

    let state = |id: &str| {
        let pool = pool.clone();
        let id = id.to_string();
        async move {
            workflow::find(&pool, NET, &id)
                .await
                .unwrap()
                .unwrap()
                .state
        }
    };
    assert_eq!(state("w1").await, WorkflowState::BenchmarkConfirmed);
    assert_eq!(state("w2").await, WorkflowState::Verified);
    assert_eq!(state("w3").await, WorkflowState::Stopped);
    assert_eq!(
        state("w4").await,
        WorkflowState::ProofConfirmed,
        "as far as slice 1's ladder goes; the fixture's ACTIVE is out of scope"
    );
}

#[tokio::test]
async fn an_accepted_attempt_with_no_confirmation_advances_nothing() {
    // G5, and `architecture.md` invariant 14: a restart cannot turn an attempt
    // into a confirmation. The attempt below says TIG answered 200 and
    // accepted the write. The window says nothing. The workflow does not move.
    let Some(db) = TempDb::migrated("restart_g5").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = owning(&pool, "w1", "bench_a").await;
    assert_eq!(w.state, WorkflowState::PrecommitConfirmed);

    // A real accepted attempt: TIG answered 200 and the ledger says so.
    // §13 invariant 4 first — 0011 refuses a benchmark write with no durable
    // acceptance, and this test's subject is what an *accepted attempt* does
    // to a workflow, not what precedes the intent.
    pool_test_support::seed_acceptances(&pool, "testnet", &[("w1", "bench_a")]).await;
    let intent_id: String = sqlx::query_scalar(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, benchmark_id, payload_digest)
         VALUES ('testnet', 'w1', 'benchmark', 1, 'bench_a',
                 decode(repeat('ab', 32), 'hex'))
         RETURNING intent_id::text",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // The gateway writes attempts, not the controller — which is the boundary
    // that makes "an attempt is not a confirmation" structural.
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let attempt = ledger.begin(&intent_id).await.unwrap();
    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Accepted,
            Some(200),
            Some("accepted"),
        )
        .await
        .unwrap();

    // The window carries the benchmark id so the workflow is not dismissed as
    // outside the window, but carries no benchmark record.
    let mut win = window(200);
    win.precommits
        .insert("bench_a".to_string(), precommit("bench_a", 100));

    let report = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();
    assert!(report.advanced.is_empty(), "{report:?}");
    assert_eq!(report.unchanged, vec!["w1".to_string()]);
    assert_eq!(
        workflow::find(&pool, NET, "w1")
            .await
            .unwrap()
            .unwrap()
            .state,
        WorkflowState::PrecommitConfirmed,
        "an accepted attempt is a transport fact, not a protocol one"
    );
}

/// A PREPARED precommit intent, the shape `admit_precommit` leaves behind.
async fn precommit_intent(pool: &sqlx::PgPool, workflow_id: &str, digest_byte: &str) -> String {
    sqlx::query_scalar(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest)
         VALUES ('testnet', $1, 'precommit', 1, decode(repeat($2, 32), 'hex'))
         RETURNING intent_id::text",
    )
    .bind(workflow_id)
    .bind(digest_byte)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_workflow_with_no_benchmark_id_is_reported_not_guessed() {
    // §10: a lost precommit response leaves the pool without the generated id,
    // and the gateway "never blindly resubmits an ambiguous precommit" — it
    // searches on the full tuple. That search is E4's; this pass reports that
    // it is owed rather than picking a candidate.
    let Some(db) = TempDb::migrated("restart_unresolved").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    decided(&pool, "w1").await;
    // The lane's subject: a precommit write that actually left the gateway.
    // Reached through `begin`/`resolve` rather than by writing the intent
    // state directly — the ambiguity lives on the attempt and the intent
    // state is written beside it, so a test that sets one by hand can assert
    // a lane the real path never fills.
    let w1_intent = precommit_intent(&pool, "w1", "ab").await;
    let w1_attempt = ledger.begin(&w1_intent).await.unwrap();
    ledger
        .resolve(
            &w1_attempt.attempt_id,
            AttemptOutcome::Ambiguous,
            None,
            Some("no usable answer"),
        )
        .await
        .unwrap();

    // The other side of the same line, in the shape production actually
    // produces: `admit_precommit` writes the workflow and a PREPARED
    // precommit intent in one transaction (`architecture.md` §7.2), so every
    // workflow carries an intent from birth and an intent alone cannot be
    // what marks the lane. §7.3 has the gateway record an attempt *before*
    // the request leaves, so a PREPARED intent with no attempt row is
    // positive evidence that nothing was sent — ordinary pending work. If
    // this one were reported, every workflow in the pool would be, and §10's
    // stop-for-operator signal would be noise from the first restart.
    decided(&pool, "w2").await;
    precommit_intent(&pool, "w2", "cd").await;

    let mut win = window(200);
    win.precommits
        .insert("bench_a".to_string(), precommit("bench_a", 100));

    let report = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();
    assert!(report.advanced.is_empty());
    assert_eq!(
        report.needs_attention,
        vec![NeedsAttention::PrecommitSearchOwed {
            workflow_id: "w1".to_string()
        }],
        "a confirmed precommit in the window is not evidence it is *this* one, \
         and an intent that was never transmitted is not an ambiguous write"
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
async fn a_benchmark_outside_the_window_is_not_evidence_of_anything() {
    // §8: `get-benchmarks` returns the latest 120 blocks. Falling out of that
    // is ordinary ageing, so absence must not be read as a terminal outcome.
    let Some(db) = TempDb::migrated("restart_window").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    owning(&pool, "w1", "bench_old").await;

    let report = restart::reconcile_after_restart(&pool, NET, &window(9_999))
        .await
        .unwrap();
    assert_eq!(
        report.needs_attention,
        vec![NeedsAttention::OutsideWindow {
            workflow_id: "w1".to_string(),
            benchmark_id: "bench_old".to_string()
        }]
    );
    assert_eq!(
        workflow::find(&pool, NET, "w1")
            .await
            .unwrap()
            .unwrap()
            .state,
        WorkflowState::PrecommitConfirmed,
        "an aged-out benchmark is not a failed one"
    );
}

#[tokio::test]
async fn fraud_is_applied_before_any_forward_step() {
    // §7 lists fraud as its own confirmed entry and it is terminal. Applying
    // the forward steps first would advance a workflow TIG has already ruled
    // on, and the extra transitions would be recorded as though they happened.
    let Some(db) = TempDb::migrated("restart_fraud").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    owning(&pool, "w1", "bench_a").await;

    let mut win = window(200);
    win.benchmarks.insert(
        "bench_a".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    );
    win.frauds.insert(
        "bench_a".to_string(),
        ConfirmedFraud {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 115,
        },
    );

    restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();
    let w = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(w.state, WorkflowState::Fraudulent);
    assert_eq!(
        w.revision, 3,
        "one transition from the restart, not a forward step and then fraud"
    );
}

#[tokio::test]
async fn a_contradiction_after_an_advance_is_reported_as_advanced() {
    // The report says what the pass did, and a later step's contradiction does
    // not undo an earlier step's transition. §10's operator resolution is read
    // against the row this run wrote, so classifying a workflow the pass just
    // moved as `unchanged` describes a database that does not exist.
    //
    // The window here is internally inconsistent on purpose — TIG stopped the
    // benchmark and also has a proof for it — because that is exactly the
    // shape that makes the pass advance and then contradict in one visit.
    let Some(db) = TempDb::migrated("restart_advance_then_contradict").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let before = owning(&pool, "w1", "bench_a").await;

    let mut win = window(200);
    win.benchmarks.insert(
        "bench_a".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: true,
        },
    );
    win.proofs.insert(
        "bench_a".to_string(),
        ConfirmedProof {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 115,
        },
    );

    let report = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();

    assert_eq!(
        report.advanced,
        vec![Reconciled {
            workflow_id: "w1".to_string(),
            from: WorkflowState::PrecommitConfirmed,
            to: WorkflowState::Stopped,
        }],
        "the benchmark step moved it before the proof step contradicted: {report:?}"
    );
    assert!(report.unchanged.is_empty(), "it did change: {report:?}");
    assert_eq!(
        report.needs_attention,
        vec![NeedsAttention::Contradicted {
            workflow_id: "w1".to_string(),
            benchmark_id: "bench_a".to_string(),
            recorded_state: "STOPPED",
        }],
    );

    let after = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(after.state, WorkflowState::Stopped);
    assert!(
        after.revision > before.revision,
        "the row moved, so the report must say so"
    );
}

#[tokio::test]
async fn a_workflow_the_pass_could_not_check_stops_the_controller() {
    // §10's seven steps run *before the controller claims work*, and the point
    // of that ordering is that claiming acts on state already checked against
    // TIG. A workflow this pass could not finish checking is exactly what §7
    // exists to stop the controller acting on — so `Ok(report)` is not by
    // itself permission to proceed, and a caller reading only the `Result`
    // would have taken it as one.
    //
    // Collecting the failure rather than propagating it is still right: the
    // operator being told to stop needs to know what else the run found. It is
    // the *classification* that has to say "stop", not the shape of the
    // return.
    let Some(db) = TempDb::migrated("restart_failed_blocks").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();
    owning(&pool, "w1", "bench_a").await;

    let mut win = window(200);
    win.benchmarks.insert(
        "bench_a".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    );

    // A database failure in the middle of the pass, arranged the only way a
    // test can arrange one deterministically: the controller keeps its read
    // grant, so the workflow loads and then cannot be advanced.
    sqlx::query("REVOKE UPDATE ON pool.workflow FROM pool_controller")
        .execute(&mut owner)
        .await
        .unwrap();

    let report = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .expect("the run reports rather than aborting");

    sqlx::query("GRANT UPDATE ON pool.workflow TO pool_controller")
        .execute(&mut owner)
        .await
        .unwrap();

    assert!(report.advanced.is_empty(), "{report:?}");
    assert!(
        matches!(
            report.needs_attention.as_slice(),
            [NeedsAttention::Failed { workflow_id, .. }] if workflow_id == "w1"
        ),
        "{report:?}"
    );
    assert!(
        report.blocks_claiming(),
        "a workflow the pass never checked is not advisory: {report:?}"
    );
}

#[tokio::test]
async fn a_terminal_workflow_is_not_revisited() {
    // §10 step 1 loads *nonterminal* workflows. A window that has dropped a
    // record the pool already acted on must not be able to un-confirm it.
    let Some(db) = TempDb::migrated("restart_terminal").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = owning(&pool, "w1", "bench_a").await;
    let w = workflow::confirm_benchmark(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(w.state, WorkflowState::Stopped);
    let revision = w.revision;

    let report = restart::reconcile_after_restart(&pool, NET, &window(200))
        .await
        .unwrap();
    assert!(report.advanced.is_empty());
    assert!(report.unchanged.is_empty());
    assert!(report.needs_attention.is_empty());
    assert_eq!(
        workflow::find(&pool, NET, "w1")
            .await
            .unwrap()
            .unwrap()
            .revision,
        revision
    );
}

#[tokio::test]
async fn every_missing_height_is_recorded_as_its_own_gap() {
    // G4, §10. Per height rather than per range: attribution is per block, so
    // a range would have to be expanded by whoever checked it, and a gap
    // nobody expanded is a gap nobody noticed.
    let Some(db) = TempDb::migrated("restart_gap").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    assert_eq!(
        restart::record_block_gap(&pool, NET, 100, 101)
            .await
            .unwrap(),
        Vec::<i64>::new(),
        "the next height is not a gap"
    );
    assert_eq!(
        restart::record_block_gap(&pool, NET, 100, 100)
            .await
            .unwrap(),
        Vec::<i64>::new(),
        "nor is standing still"
    );

    let recorded = restart::record_block_gap(&pool, NET, 100, 105)
        .await
        .unwrap();
    assert_eq!(recorded, vec![101, 102, 103, 104]);
    assert_eq!(
        restart::open_block_gaps(&pool, NET).await.unwrap(),
        vec![101, 102, 103, 104]
    );

    // Re-observing after a second restart neither fails nor rewrites, and
    // reports nothing newly recorded — §10.3's alert must not re-fire for a
    // gap already on the record.
    let again = restart::record_block_gap(&pool, NET, 100, 105)
        .await
        .unwrap();
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(
        restart::open_block_gaps(&pool, NET).await.unwrap().len(),
        4,
        "the same gap is recorded once"
    );
}

#[tokio::test]
async fn a_resolved_gap_leaves_the_open_set_but_stays_on_the_record() {
    // §10 says public operation needs an approved recovery source. Resolving a
    // gap records how; deleting it would make the round look whole again.
    let Some(db) = TempDb::migrated("restart_gap_resolve").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    restart::record_block_gap(&pool, NET, 10, 13).await.unwrap();

    sqlx::query(
        "UPDATE pool.block_data_gap
         SET resolved_at = now(),
             resolution = 'operator accepted as unrecoverable',
             resolved_by = 'operator:on-call'
         WHERE height = 11",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        restart::open_block_gaps(&pool, NET).await.unwrap(),
        vec![12]
    );
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.block_data_gap")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 2, "the resolved gap is still on the record");

    // Re-observing the same gap after another restart must not un-resolve it.
    // A gap that reopened every time the pool restarted would keep the §10.3
    // alert firing for something an operator had already settled, and the
    // record of how it was settled would be gone.
    restart::record_block_gap(&pool, NET, 10, 13).await.unwrap();
    assert_eq!(
        restart::open_block_gaps(&pool, NET).await.unwrap(),
        vec![12],
        "the resolved gap stays resolved"
    );
    let resolution: Option<String> =
        sqlx::query_scalar("SELECT resolution FROM pool.block_data_gap WHERE height = 11")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        resolution.as_deref(),
        Some("operator accepted as unrecoverable")
    );

    // A resolved gap must say how it was resolved AND by whom. §6 gives an
    // administrative override an actor and a reason; either alone is a record
    // an audit cannot use, so the CHECK takes all three or none.
    for partial in [
        "resolved_at = now()",
        "resolved_at = now(), resolution = 'accepted'",
        "resolved_at = now(), resolved_by = 'operator:on-call'",
        "resolved_at = now(), resolution = '   ', resolved_by = 'operator:on-call'",
        "resolved_at = now(), resolution = 'accepted', resolved_by = ' '",
        "resolution = 'accepted', resolved_by = 'operator:on-call'",
    ] {
        let sql = format!("UPDATE pool.block_data_gap SET {partial} WHERE height = 12");
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("resolution_is_whole"),
            "unexpected error for {partial}: {error}"
        );
    }
}

#[tokio::test]
async fn a_gap_record_can_only_ever_be_resolved() {
    // §10 leans on this row to keep a round from looking whole when its
    // per-block attribution is incomplete. A table-level UPDATE grant would
    // let the evidence be rewritten or a resolution undone — neither of which
    // the CHECK can see — so both are refused structurally.
    let Some(db) = TempDb::migrated("restart_gap_immutable").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    restart::record_block_gap(&pool, NET, 10, 13).await.unwrap();

    // The evidence columns are not the controller's to change.
    for column in ["height = 99", "after_height = 0", "observed_height = 99"] {
        let sql = format!("UPDATE pool.block_data_gap SET {column} WHERE height = 11");
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .expect_err("the gap evidence is immutable");
        assert!(
            pool_test_support::is_insufficient_privilege(&error)
                || error.to_string().contains("evidence is immutable"),
            "unexpected error for {column}: {error}"
        );
    }

    // Resolving is permitted...
    sqlx::query(
        "UPDATE pool.block_data_gap
         SET resolved_at = now(),
             resolution = 'operator accepted as unrecoverable',
             resolved_by = 'operator:on-call'
         WHERE height = 11",
    )
    .execute(&pool)
    .await
    .unwrap();

    // ...and un-resolving is not. A gap that reopened would re-fire §10.3's
    // alert for something already settled, and lose the record of how.
    // ...and neither is rewriting how or by whom it was settled. The CHECK
    // sees a complete resolution either way, so only the trigger can tell an
    // edit from the original write.
    for edit in [
        "resolved_at = NULL, resolution = NULL, resolved_by = NULL",
        "resolution = 'actually it was recovered'",
        "resolved_by = 'someone else'",
        "resolved_at = now()",
    ] {
        let sql = format!("UPDATE pool.block_data_gap SET {edit} WHERE height = 11");
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .expect_err("a resolved gap stays as it was resolved");
        assert!(
            error
                .to_string()
                .contains("cannot be reopened or rewritten"),
            "unexpected error for {edit}: {error}"
        );
    }
}

#[tokio::test]
async fn only_a_workflow_whose_precommit_reached_tig_enters_the_search_lane() {
    // §10's lane is defined by a write that left the gateway, not by a missing
    // benchmark_id. Handing E4's tuple search a workflow that submitted
    // nothing invites it to bind another workflow's confirmed precommit — a
    // second owner for one benchmark, against §10 invariant 1.
    //
    // ACCEPTED belongs in the lane as much as AMBIGUOUS does. §6.1 returns the
    // assigned benchmark_id in the response, and nothing durable holds it: a
    // precommit intent structurally cannot carry one (0003 D1a) and an
    // attempt's detail may never hold response bytes (0004). A crash between
    // the response and `confirm_precommit` therefore leaves a benchmark TIG
    // created — and charged a fee for — that the pool cannot name, and the row
    // looks exactly like one that never submitted. Left unreported it is worse
    // than invisible: `admit_precommit` reads a DECIDED workflow as having
    // sent nothing and would admit a second precommit for the same decision.
    let Some(db) = TempDb::migrated("restart_lane").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);

    decided(&pool, "w_pending").await;
    decided(&pool, "w_accepted").await;
    decided(&pool, "w_ambiguous").await;

    // §10 admits one unresolved precommit at a time network-wide, so the
    // accepted one goes first and frees the lane.
    let accepted_intent = precommit_intent(&pool, "w_accepted", "ee").await;
    let accepted_attempt = ledger.begin(&accepted_intent).await.unwrap();
    ledger
        .resolve(
            &accepted_attempt.attempt_id,
            AttemptOutcome::Accepted,
            Some(200),
            Some("accepted"),
        )
        .await
        .unwrap();

    // w_rejected was sent and TIG refused it. No benchmark exists to find, so
    // the tuple search would return nothing forever; this workflow is owed
    // `fail`, not a search, and reporting it would fill the
    // stop-for-operator bucket that only works while it stays quiet.
    decided(&pool, "w_rejected").await;
    let rejected_intent = precommit_intent(&pool, "w_rejected", "12").await;
    let rejected_attempt = ledger.begin(&rejected_intent).await.unwrap();
    ledger
        .resolve(
            &rejected_attempt.attempt_id,
            AttemptOutcome::Rejected,
            Some(400),
            Some("refused"),
        )
        .await
        .unwrap();

    // Ambiguous last: it never resolves, so it holds the lane from here on.
    let ambiguous_intent = precommit_intent(&pool, "w_ambiguous", "ab").await;
    let ambiguous_attempt = ledger.begin(&ambiguous_intent).await.unwrap();
    ledger
        .resolve(
            &ambiguous_attempt.attempt_id,
            AttemptOutcome::Ambiguous,
            None,
            Some("no usable answer"),
        )
        .await
        .unwrap();

    // w_pending has an intent and no attempt: `begin` writes the attempt row
    // before the request leaves, so nothing was sent.
    precommit_intent(&pool, "w_pending", "cd").await;

    let report = restart::reconcile_after_restart(&pool, NET, &window(200))
        .await
        .unwrap();
    assert_eq!(
        report.needs_attention,
        vec![
            NeedsAttention::PrecommitSearchOwed {
                workflow_id: "w_accepted".to_string()
            },
            NeedsAttention::PrecommitSearchOwed {
                workflow_id: "w_ambiguous".to_string()
            },
        ],
        "an accepted precommit whose id was lost is owed the same search: {report:?}"
    );
    assert!(
        report.unchanged.contains(&"w_pending".to_string()),
        "a precommit that never left the gateway is ordinary work: {report:?}"
    );
    assert!(
        report.unchanged.contains(&"w_rejected".to_string()),
        "a refused write created no benchmark to search for: {report:?}"
    );
}

#[tokio::test]
async fn an_active_benchmark_the_pool_never_saw_verified_is_reported() {
    // §10 step 5 reads the confirmed *and* active sets, and §4.5's ladder runs
    // PROOF_CONFIRMED -> VERIFYING -> ACTIVE: an active benchmark was
    // verified. But `confirmed_ids.verified` is per-block, so a verification
    // that happened while the controller was down cannot be read back, and
    // `confirm_verified` closes §6.1's interval *at a block*. Advancing from
    // the active set would close it at the wrong one, and §7.6's per-block
    // recount reads exactly that — so the pass reports instead.
    //
    // Silence is the worse option: the interval never closes,
    // `count_unverified` holds the slot against the §6.1 limit forever, and
    // §8's deadline eventually records EXPIRED for a benchmark TIG verified.
    let Some(db) = TempDb::migrated("restart_active_unverified").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = owning(&pool, "w1", "bench_a").await;
    let w = workflow::confirm_benchmark(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    )
    .await
    .unwrap();
    workflow::confirm_proof(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedProof {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 115,
        },
    )
    .await
    .unwrap();

    // TIG has it active. The block that verified it is long out of reach.
    let mut win = window(200);
    win.active.push("bench_a".to_string());

    let report = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();
    assert_eq!(
        report.needs_attention,
        vec![NeedsAttention::VerificationMissed {
            workflow_id: "w1".to_string(),
            benchmark_id: "bench_a".to_string(),
        }],
        "{report:?}"
    );

    let after = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(
        after.state,
        WorkflowState::ProofConfirmed,
        "reported, not advanced: the verification block is not recoverable"
    );
    assert_eq!(after.unverified_to_block, None);

    // And a workflow already at VERIFIED is ordinary: §7 calls a previously
    // active id leaving the set ordinary too, so this bucket must not fill on
    // every restart with benchmarks that are simply doing their job.
    let verified =
        workflow::confirm_verified(&pool, NET, "w1", after.revision, "bench_a", win.at_block)
            .await
            .unwrap();
    assert_eq!(verified.state, WorkflowState::Verified);
    let again = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();
    assert!(
        again.needs_attention.is_empty(),
        "an active, verified benchmark is not a discrepancy: {again:?}"
    );
}

#[tokio::test]
async fn a_re_observed_gap_reports_nothing_newly_recorded() {
    // §10.3's alert fires when a gap is recorded. Returning the whole range on
    // every restart would re-fire it for gaps an operator had already settled.
    let Some(db) = TempDb::migrated("restart_gap_newly").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let first = restart::record_block_gap(&pool, NET, 100, 104)
        .await
        .unwrap();
    assert_eq!(first, vec![101, 102, 103]);

    let again = restart::record_block_gap(&pool, NET, 100, 104)
        .await
        .unwrap();
    assert!(
        again.is_empty(),
        "nothing new was recorded the second time: {again:?}"
    );
}

#[tokio::test]
async fn a_workflow_that_crashed_mid_submission_still_advances() {
    // The crash state §10 exists for. `record_submitted` leaves a workflow in
    // BENCHMARK_SUBMITTED or PROOF_SUBMITTED, and `confirm_benchmark` and
    // `confirm_proof` accept exactly those as entry points — a confirmation
    // can arrive without the pool having recorded the submission.
    //
    // The pass used to restate those preconditions and restated them too
    // narrowly, so such a workflow advanced nowhere, was reported nowhere, and
    // would eventually be recorded EXPIRED for a benchmark TIG had confirmed.
    let Some(db) = TempDb::migrated("restart_submitted").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = owning(&pool, "w1", "bench_a").await;
    let w = workflow::record_submitted(&pool, NET, "w1", w.revision, Submitted::Benchmark)
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::BenchmarkSubmitted);

    let mut win = window(200);
    win.benchmarks.insert(
        "bench_a".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    );
    win.proofs.insert(
        "bench_a".to_string(),
        ConfirmedProof {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 120,
        },
    );
    win.verified.push("bench_a".to_string());

    let report = restart::reconcile_after_restart(&pool, NET, &win)
        .await
        .unwrap();
    assert_eq!(report.advanced.len(), 1, "{report:?}");
    assert!(report.needs_attention.is_empty(), "{report:?}");
    assert_eq!(
        workflow::find(&pool, NET, "w1")
            .await
            .unwrap()
            .unwrap()
            .state,
        WorkflowState::Verified,
        "one pass carries it the whole way on the evidence present"
    );
}

#[tokio::test]
async fn evidence_for_a_terminal_workflow_is_reported_not_swallowed() {
    // §10 stops for operator resolution when the pool's record and TIG's
    // disagree. Letting the transition answer means that answer has to be
    // classified: `NotAllowed` is "this evidence does not apply from here",
    // and a contradiction is something an operator must see.
    let Some(db) = TempDb::migrated("restart_contradiction").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = owning(&pool, "w1", "bench_a").await;
    let w = workflow::expire(&pool, NET, "w1", w.revision, "deadline passed", 130)
        .await
        .unwrap();
    assert!(w.state.is_terminal());

    let mut win = window(200);
    win.benchmarks.insert(
        "bench_a".to_string(),
        ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 140,
            stopped: false,
        },
    );

    // A terminal workflow is not reloaded by §10 step 1, so drive the
    // classification directly: the transition is what decides, and this is the
    // answer the pass must not swallow.
    let error = workflow::confirm_benchmark(
        &pool,
        NET,
        "w1",
        w.revision,
        win.benchmarks.get("bench_a").unwrap(),
    )
    .await
    .expect_err("the record says expired");
    assert!(matches!(
        error,
        pool_workflow::WorkflowError::TerminalStateContradicted { .. }
    ));
}

#[tokio::test]
async fn a_begun_but_unanswered_attempt_enters_the_ambiguous_lane() {
    // `architecture.md` §12: "TIG Gateway dies before/after HTTP response:
    // attempt stays pending or unknown". Defining the lane by the intent state
    // alone would leave that write reported as ordinary pending work, with the
    // serialized lane still occupied and nobody told.
    let Some(db) = TempDb::migrated("restart_pending_attempt").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    decided(&pool, "w1").await;

    let intent_id: String = sqlx::query_scalar(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest)
         VALUES ('testnet', 'w1', 'precommit', 1, decode(repeat('ab', 32), 'hex'))
         RETURNING intent_id::text",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    PostgresAttemptLedger::new(db.pool_as("pool_gateway").await)
        .begin(&intent_id)
        .await
        .unwrap();

    let report = restart::reconcile_after_restart(&pool, NET, &window(200))
        .await
        .unwrap();
    assert_eq!(
        report.needs_attention,
        vec![NeedsAttention::PrecommitSearchOwed {
            workflow_id: "w1".to_string()
        }],
        "an unanswered attempt is an unsettled write: {report:?}"
    );
}

#[tokio::test]
async fn a_verified_workflow_outside_the_window_needs_no_attention() {
    // VERIFIED is deliberately non-terminal (§4.5's ladder continues to
    // ACTIVE), so it is reloaded every restart. Once its benchmark ages out of
    // §8's 120-block window, reporting it would fill the same bucket that
    // carries §10's stop-for-operator signal — every restart, forever, until
    // the caller learned to ignore the bucket.
    let Some(db) = TempDb::migrated("restart_verified_aged").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = owning(&pool, "w1", "bench_a").await;
    let w = workflow::confirm_benchmark(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    )
    .await
    .unwrap();
    let w = workflow::confirm_proof(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedProof {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 120,
        },
    )
    .await
    .unwrap();
    workflow::confirm_verified(&pool, NET, "w1", w.revision, "bench_a", 130)
        .await
        .unwrap();

    // A window that has moved on entirely.
    let report = restart::reconcile_after_restart(&pool, NET, &window(9_999))
        .await
        .unwrap();
    assert!(
        report.needs_attention.is_empty(),
        "a verified workflow awaits nothing: {report:?}"
    );
}
