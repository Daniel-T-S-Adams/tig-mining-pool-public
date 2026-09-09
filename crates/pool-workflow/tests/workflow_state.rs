//! Slice-1 criteria F1, F2, F3 and F6: the confirmation-driven state machine,
//! proven against a real PostgreSQL 18.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::WriteAttemptLedger;
use pool_workflow::workflow::{
    self, ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof, Owner,
    POOL_BOOTSTRAP_OWNER, Submitted, WorkflowError, WorkflowState,
};
use serde_json::json;
use sqlx::{Connection, Row};

const NET: Network = Network::Testnet;

/// The anchor height a decision was made at, which is where §6.1's unverified
/// interval opens.
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

fn precommit(benchmark: &str, block: i64) -> ConfirmedPrecommit {
    ConfirmedPrecommit {
        benchmark_id: benchmark.to_string(),
        block_confirmed: block,
        // TIG's own record of when the benchmark began; §8's deadlines are
        // ages from here, and it is a few blocks before confirmation.
        block_started: block - 2,
        // TIG chose the track; the pool proposed every active one.
        track_id: "t002".to_string(),
        settings: json!({
            "num_bundles": 5,
            "fuel_budget": 2_000_000u64,
            "hyperparameters": { "noise": 0.15, "restart_period": 250 }
        }),
    }
}

#[tokio::test]
async fn a_workflow_advances_only_through_confirmed_evidence() {
    // F1. The happy path exists to show what evidence each step consumes:
    // there is no function on this module that takes an HTTP status, so a
    // caller holding a 200 has nothing to call.
    let Some(db) = TempDb::migrated("wf_happy").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let w = decided(&pool, "w1").await;
    assert_eq!(w.state, WorkflowState::Decided);
    assert_eq!(w.revision, 1);
    assert_eq!(w.benchmark_id, None, "TIG has not assigned one yet");
    assert_eq!(
        w.unverified_from_block, DECIDED_AT,
        "`mining_system.md` §6.1: unverified from creation of the precommit \
         intent, not from its confirmation"
    );

    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::PrecommitConfirmed);
    assert_eq!(w.benchmark_id.as_deref(), Some("bench_a"));
    assert_eq!(
        w.unverified_from_block, DECIDED_AT,
        "§6.1 opened the interval at creation; confirmation does not move it"
    );

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
    assert_eq!(w.state, WorkflowState::BenchmarkConfirmed);

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
    assert_eq!(w.state, WorkflowState::ProofConfirmed);
    assert_eq!(w.unverified_to_block, None, "still unverified");

    let w = workflow::confirm_verified(&pool, NET, "w1", w.revision, "bench_a", 130)
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::Verified);
    assert_eq!(
        w.unverified_to_block,
        Some(130),
        "verification is what closes the interval §7.6 counts"
    );
    assert_eq!(w.revision, 5, "one revision per confirmed transition");
}

#[tokio::test]
async fn confirmed_settings_replace_the_proposed_ones() {
    // F2, and §7: "Confirmed settings/details replace proposed values." The
    // pool proposes every active track and TIG picks one, so what it picked
    // is not derivable from the proposal — it has to be stored.
    let Some(db) = TempDb::migrated("wf_settings").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;

    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();

    assert_eq!(w.confirmed_track_id.as_deref(), Some("t002"));
    assert_eq!(w.precommit_confirmed_block, Some(100));
    let settings = w.confirmed_settings.unwrap();
    assert_eq!(settings["num_bundles"], json!(5));
    // The types TIG recorded, kept as recorded.
    assert_eq!(settings["hyperparameters"]["restart_period"], json!(250));
    assert!(settings["hyperparameters"]["restart_period"].is_number());
}

#[tokio::test]
async fn a_transition_from_a_stale_revision_is_refused() {
    // F3, `architecture.md` §7.5 step 3 and invariant 8: a claimant that read
    // the row, did slow work, and came back to commit cannot land if anything
    // moved underneath it.
    let Some(db) = TempDb::migrated("wf_stale").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let stale = w.revision;

    workflow::confirm_precommit(&pool, NET, "w1", stale, &precommit("bench_a", 100))
        .await
        .unwrap();

    let error = workflow::confirm_precommit(&pool, NET, "w1", stale, &precommit("bench_b", 101))
        .await
        .expect_err("the revision moved");
    assert!(
        matches!(
            error,
            WorkflowError::StaleRevision {
                expected: 1,
                found: 2,
                ..
            }
        ),
        "unexpected error: {error}"
    );

    let stored = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(
        stored.benchmark_id.as_deref(),
        Some("bench_a"),
        "the stale writer must not have overwritten the benchmark"
    );
}

#[tokio::test]
async fn the_revision_can_never_go_backwards() {
    // The trigger, not the code: a future transition path that forgot the
    // compare-and-set is refused by PostgreSQL.
    let Some(db) = TempDb::migrated("wf_monotonic").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    decided(&pool, "w1").await;

    let error =
        sqlx::query("UPDATE pool.workflow SET revision = revision WHERE workflow_id = 'w1'")
            .execute(&pool)
            .await
            .expect_err("a revision that did not advance is a lost update");
    assert!(
        error.to_string().contains("revision must advance"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_stopped_benchmark_never_reaches_a_proof() {
    // §7: "If `details.stopped` is true, no proof is sent." That makes stopped
    // a terminal state rather than a flag beside one — a flag would leave the
    // proof path reachable and rely on a caller reading it.
    let Some(db) = TempDb::migrated("wf_stopped").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();

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
    assert!(w.state.is_terminal());
    assert_eq!(w.unverified_to_block, Some(110));

    let error = workflow::confirm_proof(
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
    .expect_err("a stopped benchmark sends no proof");
    // STOPPED is one of §4.5's terminal branches, so proof evidence arriving
    // for it is the pool's record and TIG's disagreeing.
    assert!(
        matches!(error, WorkflowError::TerminalStateContradicted { .. }),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn evidence_for_another_benchmark_cannot_advance_this_workflow() {
    // §7's mapping is per benchmark. Advancing on someone else's confirmation
    // would be the pool answering for work it does not own.
    let Some(db) = TempDb::migrated("wf_mismatch").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();

    let error = workflow::confirm_benchmark(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: "bench_other".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    )
    .await
    .expect_err("that is a different benchmark");
    assert!(matches!(error, WorkflowError::BenchmarkMismatch { .. }));
}

#[tokio::test]
async fn fraud_is_terminal_from_wherever_it_is_found() {
    // §7 lists fraud as its own confirmed entry, and TIG can record it against
    // a benchmark whose proof has confirmed or has not.
    let Some(db) = TempDb::migrated("wf_fraud").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();

    let w = workflow::confirm_fraud(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedFraud {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 115,
        },
    )
    .await
    .unwrap();
    assert_eq!(w.state, WorkflowState::Fraudulent);
    assert_eq!(w.unverified_to_block, Some(115));
    // F4b: the reason names TIG's evidence, never a member.
    let reason = w.terminal_reason.unwrap();
    assert!(!reason.to_ascii_lowercase().contains("member"), "{reason}");
}

#[tokio::test]
async fn no_slice_1_workflow_is_attributable_to_a_member() {
    // F6 and F4b, as the permanent negative assertion the plan asks for. This
    // test is not slice-scoped: a slice-1 benchmark stays pool-owned for life,
    // so it can never later be attributed to a member or have its faults
    // charged to one.
    let Some(db) = TempDb::migrated("wf_owner").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    assert_eq!(w.owner, Owner::PoolBootstrap);
    assert_eq!(w.owner_id, POOL_BOOTSTRAP_OWNER);

    let member_owned: i64 =
        sqlx::query("SELECT count(*) AS n FROM pool.workflow WHERE owner_kind = 'MEMBER'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
    assert_eq!(member_owned, 0);

    // The owner cannot be rewritten into a member's, which is what makes the
    // mapping permanent rather than merely unwritten so far.
    let error = sqlx::query(
        "UPDATE pool.workflow
         SET owner_kind = 'MEMBER', owner_id = 'member_1', revision = revision + 1
         WHERE workflow_id = 'w1'",
    )
    .execute(&pool)
    .await
    .expect_err("the owner is immutable");
    assert!(
        error.to_string().contains("owner is immutable"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_terminal_reason_may_not_attribute_member_fault() {
    // F4b in code. `mining_system.md` §8 allows a chargeable tier failure only
    // after fault attribution classifies the outcome as MEMBER, and slice 1
    // has no members to classify — so recording one would be a fault record
    // against a benchmark the pool itself owns.
    let Some(db) = TempDb::migrated("wf_fault").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;

    for reason in [
        "member package timeout",
        "chargeable tier failure",
        "slash the deposit",
    ] {
        let error = workflow::expire(&pool, NET, "w1", w.revision, reason, 140)
            .await
            .expect_err("slice 1 cannot attribute member fault");
        assert!(
            matches!(error, WorkflowError::MemberFaultNotAvailable { .. }),
            "{reason:?} gave: {error}"
        );
    }

    // A pool-side reason is fine, and is what §7's expiry actually records.
    let w = workflow::expire(
        &pool,
        NET,
        "w1",
        w.revision,
        "no confirming evidence before the deadline",
        140,
    )
    .await
    .unwrap();
    assert_eq!(w.state, WorkflowState::Expired);
    assert_eq!(
        w.unverified_to_block,
        Some(140),
        "§6.1 closes the interval on a terminal state, including one the \
         workflow reached before its precommit ever confirmed"
    );
}

#[tokio::test]
async fn one_workflow_owns_one_benchmark() {
    // Two workflows cannot both claim a benchmark: the confirmed id is what
    // ties pool state to TIG state, and two claimants would make the mapping
    // ambiguous in exactly the place `mining_system.md` §2 requires it not be.
    let Some(db) = TempDb::migrated("wf_one_benchmark").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let a = decided(&pool, "w1").await;
    let b = decided(&pool, "w2").await;

    workflow::confirm_precommit(&pool, NET, "w1", a.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let error =
        workflow::confirm_precommit(&pool, NET, "w2", b.revision, &precommit("bench_a", 101))
            .await
            .expect_err("bench_a already has a workflow");
    assert!(
        matches!(error, WorkflowError::BenchmarkAlreadyClaimed { .. }),
        "a permanent conflict, reported as one rather than as an outage: {error}"
    );
}

#[tokio::test]
async fn the_gateway_can_read_a_workflow_and_cannot_change_one() {
    // `architecture.md` invariant 2 and criterion D4: the gateway "cannot
    // decide or manufacture" a write, enforced by grants.
    let Some(db) = TempDb::migrated("wf_grants").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    decided(&controller, "w1").await;

    let gateway = db.pool_as("pool_gateway").await;
    let seen: i64 = sqlx::query("SELECT count(*) AS n FROM pool.workflow")
        .fetch_one(&gateway)
        .await
        .unwrap()
        .get("n");
    assert_eq!(seen, 1);

    let error = sqlx::query("UPDATE pool.workflow SET state = 'VERIFIED' WHERE workflow_id = 'w1'")
        .execute(&gateway)
        .await
        .expect_err("the gateway must not advance a workflow");
    assert!(
        pool_test_support::is_insufficient_privilege(&error),
        "unexpected error: {error}"
    );

    let error = sqlx::query(
        "INSERT INTO pool.workflow (workflow_id, network, owner_kind, owner_id)
         VALUES ('forged', 'testnet', 'POOL_BOOTSTRAP', 'pool-bootstrap')",
    )
    .execute(&gateway)
    .await
    .expect_err("the gateway must not create a workflow");
    assert!(
        pool_test_support::is_insufficient_privilege(&error),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn confirmed_evidence_for_a_terminal_workflow_is_a_discrepancy() {
    // `mining_system.md` §4.5 lists EXPIRED among the terminal branches and
    // §6.1 ends the unverified interval when a benchmark reaches one, so a
    // confirmation arriving afterwards is the pool's record and TIG's
    // disagreeing — not a transition. §10's answer to that is to stop for
    // operator resolution, the same shape E4 uses for an ambiguous precommit.
    //
    // An earlier version of this module let such evidence *supersede* the
    // expiry. That reinterpreted §4.5 rather than implementing it, and could
    // never have fired anyway: §10 step 1 reloads only nonterminal workflows.
    let Some(db) = TempDb::migrated("wf_contradiction").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
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
    let w = workflow::expire(&pool, NET, "w1", w.revision, "deadline passed", 130)
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::Expired);
    assert!(w.state.is_terminal(), "§4.5 lists EXPIRED as terminal");
    assert_eq!(w.unverified_to_block, Some(130));

    let error = workflow::confirm_verified(&pool, NET, "w1", w.revision, "bench_a", 140)
        .await
        .expect_err("the record and TIG disagree; that is for an operator");
    assert!(
        matches!(error, WorkflowError::TerminalStateContradicted { .. }),
        "unexpected error: {error}"
    );

    // Nothing was rewritten. The interval keeps its earlier close, which
    // §7.6's per-block aggregates may already have consumed.
    let stored = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(stored.state, WorkflowState::Expired);
    assert_eq!(stored.unverified_to_block, Some(130));
    assert_eq!(stored.revision, w.revision);
}

#[tokio::test]
async fn a_workflow_cannot_be_re_pointed_at_another_benchmark() {
    // §2's benchmark → owner mapping is permanent and §8 charges faults
    // through it, so a workflow that could be re-pointed would leave its first
    // benchmark with no owner row and nothing to attribute its faults to.
    // Guarded in the code and in the schema, because the schema is what holds
    // if a later transition path forgets.
    let Some(db) = TempDb::migrated("wf_repoint").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();

    let error = sqlx::query(
        "UPDATE pool.workflow SET benchmark_id = 'bench_b', revision = revision + 1
         WHERE workflow_id = 'w1'",
    )
    .execute(&pool)
    .await
    .expect_err("the binding is for life");
    assert!(
        error.to_string().contains("benchmark_id is immutable"),
        "unexpected error: {error}"
    );
    let stored = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(stored.benchmark_id.as_deref(), Some("bench_a"));
    let _ = w;
}

#[tokio::test]
async fn a_submission_is_recorded_and_is_not_a_confirmation() {
    // `mining_system.md` §4.5 names PRECOMMIT_SUBMITTED in the lifecycle the
    // local machine "must distinguish at least", and says in the same section
    // that "TIG HTTP acceptance alone is not protocol confirmation". Both hold
    // at once: recording that a write went out is a local fact §10 reconciles
    // against, and it advances nothing at TIG.
    let Some(db) = TempDb::migrated("wf_submitted").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;

    let w = workflow::record_submitted(&pool, NET, "w1", w.revision, Submitted::Precommit)
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::PrecommitSubmitted);
    assert_eq!(
        w.benchmark_id, None,
        "TIG assigns the id at confirmation, not at submission"
    );
    assert!(!w.state.is_terminal());
    assert!(!w.state.is_terminal());

    // The confirmation still requires confirmed evidence, and the submission
    // is not a substitute for it.
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::PrecommitConfirmed);
    assert_eq!(w.benchmark_id.as_deref(), Some("bench_a"));
}

#[tokio::test]
async fn a_confirmation_does_not_need_the_submission_to_have_been_recorded() {
    // A restart can lose the window in which the submission would have been
    // recorded. §7's confirmed read is what matters, so the confirmation is
    // reachable from DECIDED directly.
    let Some(db) = TempDb::migrated("wf_skip_submitted").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .expect("a confirmation stands on its own evidence");
    assert_eq!(w.state, WorkflowState::PrecommitConfirmed);
}

#[tokio::test]
async fn a_precommit_confirmed_after_the_deadline_is_a_discrepancy() {
    // The pool expired the workflow; TIG then confirmed the precommit. Under
    // §4.5 EXPIRED is terminal, so this is the record and TIG disagreeing
    // rather than a late transition, and §10 stops for operator resolution.
    //
    // The alternative — letting the confirmation supersede the expiry — is
    // what an earlier version of this module did. It reinterpreted §4.5, and
    // §10 step 1 reloads only nonterminal workflows, so nothing would have
    // reached that path through the documented reconciliation anyway.
    let Some(db) = TempDb::migrated("wf_late_precommit").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::expire(&pool, NET, "w1", w.revision, "deadline passed", 200)
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::Expired);

    let error =
        workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 210))
            .await
            .expect_err("the record says expired; TIG says confirmed");
    assert!(
        matches!(
            error,
            WorkflowError::TerminalStateContradicted {
                state: "EXPIRED",
                ..
            }
        ),
        "unexpected error: {error}"
    );
    let stored = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(stored.benchmark_id, None, "nothing was rewritten");
}

#[tokio::test]
async fn a_mainnet_workflow_cannot_be_pool_owned() {
    // §10 invariant 1's carve-out is bounded to testnet, and the bound is
    // enforced rather than asserted: a mainnet benchmark has a member owner or
    // it does not exist. Mainnet has no pre-member period to be excepted from.
    let Some(db) = TempDb::migrated("wf_mainnet_bootstrap").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let error = sqlx::query(
        "INSERT INTO pool.workflow
             (network, workflow_id, owner_kind, owner_id, unverified_from_block)
         VALUES ('mainnet', 'w1', 'POOL_BOOTSTRAP', 'pool-bootstrap', 1)",
    )
    .execute(&pool)
    .await
    .expect_err("the carve-out is testnet-only");
    assert!(
        error.to_string().contains("bootstrap_is_testnet_only"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_submission_cannot_skip_a_step() {
    // The ladder is §4.5's. Recording a proof submission for a workflow whose
    // benchmark has not confirmed would claim the pool sent something it had
    // no basis to send.
    let Some(db) = TempDb::migrated("wf_submit_order").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;

    for which in [Submitted::Benchmark, Submitted::Proof] {
        let error = workflow::record_submitted(&pool, NET, "w1", w.revision, which)
            .await
            .expect_err("that write has no basis yet");
        assert!(matches!(error, WorkflowError::NotAllowed { .. }));
    }
}

#[tokio::test]
async fn a_rejected_precommit_releases_its_capacity() {
    // §4.5's FAILED branch, and the reason it cannot be omitted. Admission
    // counts open §6.1 intervals, so a workflow whose precommit TIG refused
    // needs a closing path — otherwise its slot is consumed for the lifetime
    // of the database and enough rejections wedge admission entirely.
    let Some(db) = TempDb::migrated("wf_failed").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;

    let w = workflow::fail(
        &pool,
        NET,
        "w1",
        w.revision,
        "TIG refused the precommit",
        150,
    )
    .await
    .unwrap();
    assert_eq!(w.state, WorkflowState::Failed);
    assert!(w.state.is_terminal());
    assert_eq!(
        w.unverified_to_block,
        Some(150),
        "the interval closes, which is what frees the slot"
    );

    // F4b holds here too: a pool-side failure attributes nothing to a member,
    // and the check runs before the state guard so the reason is judged on its
    // own terms.
    let error = workflow::fail(&pool, NET, "w1", w.revision, "member never delivered", 151)
        .await
        .expect_err("slice 1 cannot attribute member fault");
    assert!(
        matches!(error, WorkflowError::MemberFaultNotAvailable { .. }),
        "unexpected error: {error}"
    );

    // And a terminal workflow does not fail twice.
    let error = workflow::fail(&pool, NET, "w1", w.revision, "another pool-side fault", 151)
        .await
        .expect_err("already terminal");
    assert!(matches!(error, WorkflowError::NotAllowed { .. }));
}

#[tokio::test]
async fn a_terminal_workflow_reports_later_benchmark_and_proof_evidence_as_a_discrepancy() {
    // The same §10 stop-for-operator-resolution path as the precommit case.
    // Reporting these as an ordinary disallowed transition would hide exactly
    // the pool-record-versus-TIG disagreement the path exists for.
    let Some(db) = TempDb::migrated("wf_contradiction_more").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let w = workflow::expire(&pool, NET, "w1", w.revision, "deadline passed", 130)
        .await
        .unwrap();

    let error = workflow::confirm_benchmark(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 140,
            stopped: false,
        },
    )
    .await
    .expect_err("the record says expired");
    assert!(
        matches!(error, WorkflowError::TerminalStateContradicted { .. }),
        "unexpected error: {error}"
    );

    let error = workflow::confirm_proof(
        &pool,
        NET,
        "w1",
        w.revision,
        &ConfirmedProof {
            benchmark_id: "bench_a".to_string(),
            block_confirmed: 150,
        },
    )
    .await
    .expect_err("the record says expired");
    assert!(matches!(
        error,
        WorkflowError::TerminalStateContradicted { .. }
    ));
}

#[tokio::test]
async fn verification_closes_the_interval_without_ending_the_workflow() {
    // §4.5 names four terminal branches and VERIFIED is not one: the ladder
    // continues PROOF_CONFIRMED -> VERIFYING -> ACTIVE, and §9 and §10
    // invariant 17 treat active as distinct from terminal. Closing §6.1's
    // interval is a separate fact — it stops the benchmark consuming
    // concurrency, which is not a claim that the workflow is finished.
    let Some(db) = TempDb::migrated("wf_verified_live").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
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
    let w = workflow::confirm_verified(&pool, NET, "w1", w.revision, "bench_a", 130)
        .await
        .unwrap();

    assert_eq!(w.state, WorkflowState::Verified);
    assert!(
        !w.state.is_terminal(),
        "§4.5's terminal branches are STOPPED, EXPIRED, FAILED and FRAUDULENT"
    );
    assert_eq!(w.unverified_to_block, Some(130));
    assert_eq!(
        w.terminal_reason, None,
        "a live workflow carries no terminal reason"
    );
}
#[tokio::test]
async fn a_workflow_expires_at_the_guardrail_and_records_why() {
    // F5. §8: "mark unfinished local workflow expired at age >= 120 blocks",
    // measured from TIG's `block_started`.
    let Some(db) = TempDb::migrated("wf_expiry").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let started = w.block_started.expect("TIG told us when it began");
    assert_eq!(started, 98);

    // One block short of the guardrail: nothing happens.
    let almost = started + i64::from(guardrails.workflow_expiry_age_blocks) - 1;
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w1", almost)
            .await
            .unwrap()
            .is_none(),
        "a workflow inside its guardrail is left alone"
    );

    let due = started + i64::from(guardrails.workflow_expiry_age_blocks);
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "w1", due)
        .await
        .unwrap()
        .expect("this one is due");
    assert_eq!(expired.state, WorkflowState::Expired);
    assert_eq!(expired.unverified_to_block, Some(due));

    let reason = expired.terminal_reason.unwrap();
    assert!(
        reason.contains("120"),
        "the reason names the guardrail: {reason}"
    );
    // F4b: a pool-side deadline, attributed to nobody.
    assert!(!reason.to_ascii_lowercase().contains("member"), "{reason}");

    // Sweeping again is a no-op rather than a second transition.
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w1", due + 50)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_deadline_sweep_does_not_expire_a_workflow_whose_proof_tig_confirmed() {
    // §8 expires an "unfinished local workflow", and its own accounting says
    // what finished means: the ten-block reserve before the deadline is "for
    // pool-owned commitment, sampling, proof construction, submission, and
    // confirmation". At PROOF_CONFIRMED all of that is done and only TIG's
    // verification remains — which §7 gives TIG, not the pool's clock.
    //
    // Expiring here would rewrite work whose precommit, benchmark and proof
    // TIG all confirmed into a local failure labelled "without confirmation",
    // make its artifacts deletable as terminal, and turn the verification that
    // follows into a discrepancy needing an operator.
    let Some(db) = TempDb::migrated("wf_proof_confirmed").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let started = w.block_started.unwrap();
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
            block_confirmed: 115,
        },
    )
    .await
    .unwrap();
    assert_eq!(w.state, WorkflowState::ProofConfirmed);

    let long_past = started + i64::from(guardrails.workflow_expiry_age_blocks) + 99;
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w1", long_past)
            .await
            .unwrap()
            .is_none(),
        "a confirmed proof is not unfinished work"
    );
    let after = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(after.state, WorkflowState::ProofConfirmed);
    assert_eq!(after.terminal_reason, None);
}

#[tokio::test]
async fn an_unsettled_benchmark_write_holds_the_deadline_off_too() {
    // §7.3 and §12 describe an attempt left "pending or unknown" by a gateway
    // crash. That is a fact about a request, not about which endpoint it went
    // to, so a benchmark or proof write stranded that way withholds expiry
    // exactly as a precommit does. Expiring past one strands the ambiguity on
    // a terminal row that §10 step 1 never reloads, leaving that write's lane
    // closed with nothing scheduled to settle it.
    let Some(db) = TempDb::migrated("wf_unsettled_benchmark").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let ledger = pool_workflow::PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let started = w.block_started.unwrap();
    let long_past = started + i64::from(guardrails.workflow_expiry_age_blocks) + 99;

    let intent_id: String = sqlx::query_scalar(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest, benchmark_id)
         VALUES ('testnet', 'w1', 'benchmark', 1,
                 decode(repeat('ab', 32), 'hex'), 'bench_a')
         RETURNING intent_id::text",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let attempt = ledger.begin(&intent_id).await.unwrap();

    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w1", long_past)
            .await
            .unwrap()
            .is_none(),
        "an unanswered benchmark write is not settled by a timeout"
    );

    ledger
        .resolve(
            &attempt.attempt_id,
            pool_workflow::AttemptOutcome::Rejected,
            Some(400),
            Some("refused"),
        )
        .await
        .unwrap();
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "w1", long_past)
        .await
        .unwrap()
        .expect("settled, so the deadline applies");
    assert_eq!(expired.state, WorkflowState::Expired);
}

#[tokio::test]
async fn a_precommit_that_never_confirmed_still_expires() {
    // The capacity leak this closes. §8 measures an unfinished workflow from
    // TIG's `block_started`, which exists only once the precommit confirms —
    // so a workflow whose precommit never confirmed has no such value. That
    // does not make it immortal: §6.1's interval opened when it was created,
    // `admit_precommit` counts open intervals, and nothing else would ever
    // close it. Enough of them wedge admission.
    //
    // It is measured from the decision anchor against the same guardrail,
    // which is sound on §8's own terms: a precommit's `settings.block_id` must
    // be TIG's latest or second-latest block when processed, so one that has
    // not confirmed after the whole expiry window cannot confirm afterwards.
    let Some(db) = TempDb::migrated("wf_noage").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let w = decided(&pool, "w1").await;
    assert_eq!(w.block_started, None);
    let anchor = w.unverified_from_block;

    // Inside the window, nothing happens.
    let almost = anchor + i64::from(guardrails.workflow_expiry_age_blocks) - 1;
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w1", almost)
            .await
            .unwrap()
            .is_none()
    );

    let due = anchor + i64::from(guardrails.workflow_expiry_age_blocks);
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "w1", due)
        .await
        .unwrap()
        .expect("a precommit that never confirmed is dead by §8's own rule");
    assert_eq!(expired.state, WorkflowState::Expired);
    assert_eq!(
        expired.unverified_to_block,
        Some(due),
        "the interval closes, which is what frees the slot"
    );
    assert_eq!(
        expired.terminal_reason.as_deref(),
        Some(pool_workflow::workflow::EXPIRY_REASON_NO_PRECOMMIT),
        "a distinct code: never obtaining a benchmark is not losing one"
    );
}

#[tokio::test]
async fn an_unsettled_precommit_is_never_expired_by_the_clock() {
    // §10: an ambiguous write is settled by reconciliation or an operator,
    // never by a local timeout — the gateway "never blindly resubmits an
    // ambiguous precommit". Expiring here would make the pool's clock the
    // authority over a write that may well have reached TIG, and E4's tuple
    // search would later map its confirmation onto a terminal workflow.
    let Some(db) = TempDb::migrated("wf_unsettled").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let ambiguous = decided(&pool, "w_ambiguous").await;
    let pending = decided(&pool, "w_pending").await;
    let settled = decided(&pool, "w_settled").await;
    let untransmitted = decided(&pool, "w_untransmitted").await;
    let long_past =
        ambiguous.unverified_from_block + i64::from(guardrails.workflow_expiry_age_blocks) + 99;

    let intent_of = |workflow: &str, state: &str| {
        let pool = pool.clone();
        let workflow = workflow.to_string();
        let state = state.to_string();
        async move {
            sqlx::query_scalar::<_, String>(
                "INSERT INTO pool.tig_write_intent
                     (network, workflow_id, write_kind, generation, payload_digest, state)
                 VALUES ('testnet', $1, 'precommit', 1,
                         decode(repeat('ab', 32), 'hex'), $2)
                 RETURNING intent_id::text",
            )
            .bind(workflow)
            .bind(state)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // §10's lane admits one unresolved precommit at a time, network-wide, so
    // these run in sequence: each case frees the lane before the next claims
    // it.
    let ledger = pool_workflow::PostgresAttemptLedger::new(gateway);

    // A write TIG refused is settled, so the deadline applies.
    let settled_intent = intent_of("w_settled", "PREPARED").await;
    let attempt = ledger.begin(&settled_intent).await.unwrap();
    ledger
        .resolve(
            &attempt.attempt_id,
            pool_workflow::AttemptOutcome::Rejected,
            Some(400),
            Some("refused"),
        )
        .await
        .unwrap();
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "w_settled", long_past)
        .await
        .unwrap()
        .expect("a settled write no longer holds the deadline off");
    assert_eq!(expired.state, WorkflowState::Expired);

    // An ambiguous write holds it off. Reached the way production reaches it —
    // `resolve` records the ambiguity on the attempt and moves the intent to
    // OUTCOME_UNKNOWN in one transaction — rather than by writing the intent
    // state directly, because the two are not independent facts and a test
    // that sets one by hand can assert a hold the real path never produces.
    let ambiguous_intent = intent_of("w_ambiguous", "PREPARED").await;
    let ambiguous_attempt = ledger.begin(&ambiguous_intent).await.unwrap();
    ledger
        .resolve(
            &ambiguous_attempt.attempt_id,
            pool_workflow::AttemptOutcome::Ambiguous,
            None,
            Some("no usable answer"),
        )
        .await
        .unwrap();
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w_ambiguous", long_past)
            .await
            .unwrap()
            .is_none(),
        "an ambiguous write is not settled by a timeout"
    );

    // And §10's reconciliation releases it. This is the half that decides
    // whether the hold is a deadline or a latch: `reconcile` settles the
    // attempt and does *not* move the intent, so a predicate reading the
    // intent's OUTCOME_UNKNOWN would keep this workflow — and §6.1's
    // unverified interval — open for the life of the database, once, forever,
    // for any precommit that had ever gone ambiguous.
    ledger
        .reconcile(
            &ambiguous_attempt.attempt_id,
            pool_workflow::AttemptOutcome::Rejected,
        )
        .await
        .unwrap();
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "w_ambiguous", long_past)
        .await
        .unwrap()
        .expect("reconciliation settled the write; the deadline applies again");
    assert_eq!(expired.state, WorkflowState::Expired);

    // An attempt begun and never answered is unsettled too — §12's "attempt
    // stays pending or unknown" after a gateway crash.
    let pending_intent = intent_of("w_pending", "PREPARED").await;
    ledger.begin(&pending_intent).await.unwrap();
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w_pending", long_past)
            .await
            .unwrap()
            .is_none(),
        "a begun-but-unanswered attempt is unsettled too"
    );

    // The other direction, and the one that decides whether this predicate is
    // a deadline or a wedge. `admit_precommit` creates the workflow and its
    // PREPARED intent in one transaction (`architecture.md` §7.2), so *every*
    // production workflow carries a precommit intent from birth. If a
    // never-transmitted intent counted as unsettled, no workflow could ever
    // expire and §6.1's interval would never close — the exact capacity leak
    // this sweep exists to close, reinstated by the guard meant to protect it.
    //
    // §7.3 has the gateway record an attempt *before* the request leaves, so
    // a PREPARED intent with no attempt row is positive evidence that nothing
    // was sent. It claims no lane, so it needs no turn in the sequence.
    intent_of("w_untransmitted", "PREPARED").await;
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "w_untransmitted", long_past)
        .await
        .unwrap()
        .expect("a precommit that was never sent holds nothing off");
    assert_eq!(expired.state, WorkflowState::Expired);

    let _ = (ambiguous, pending, settled, untransmitted);
}

#[tokio::test]
async fn a_confirmed_row_without_a_start_block_is_still_labelled_as_stalled() {
    // The age basis and the reason code answer different questions. A row
    // written before `block_started` existed is *confirmed* — it has a
    // benchmark — but has no start block, so it is aged from the decision
    // anchor while still being labelled as a benchmark that stalled. Keying
    // the code to `block_started` would file it under "never obtained a
    // precommit" and mislabel §10.2's metric dimension.
    let Some(db) = TempDb::migrated("wf_legacy_reason").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();
    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();

    sqlx::query("ALTER TABLE pool.workflow DISABLE TRIGGER workflow_insert_block_started")
        .execute(&mut owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pool.workflow
             (network, workflow_id, state, owner_kind, owner_id,
              unverified_from_block, benchmark_id, confirmed_track_id,
              confirmed_settings, precommit_confirmed_block)
         VALUES ('testnet', 'legacy', 'BENCHMARK_SUBMITTED', 'POOL_BOOTSTRAP',
                 'pool-bootstrap', 90, 'bench_legacy', 't002', '{}'::jsonb, 100)",
    )
    .execute(&mut owner)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE pool.workflow ENABLE TRIGGER workflow_insert_block_started")
        .execute(&mut owner)
        .await
        .unwrap();

    let due = 90 + i64::from(guardrails.workflow_expiry_age_blocks);
    let expired = workflow::expire_if_due(&pool, &guardrails, NET, "legacy", due)
        .await
        .unwrap()
        .expect("aged from the decision anchor, since it has no start block");
    assert_eq!(
        expired.terminal_reason.as_deref(),
        Some(pool_workflow::workflow::EXPIRY_REASON_UNCONFIRMED),
        "it has a benchmark; it stalled, it did not fail to get one"
    );
}

#[tokio::test]
async fn the_expiry_reason_is_a_bounded_code() {
    // `architecture.md` §10.2 uses the terminal reason as a metric dimension
    // and forbids unbounded ones; §6 asks for a "reason code". A string
    // embedding the observed age would be a new dimension value per workflow.
    let Some(db) = TempDb::migrated("wf_reason_code").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let started = w.block_started.unwrap();

    let expired = workflow::expire_if_due(
        &pool,
        &guardrails,
        NET,
        "w1",
        started + i64::from(guardrails.workflow_expiry_age_blocks) + 37,
    )
    .await
    .unwrap()
    .expect("past the guardrail");

    let reason = expired.terminal_reason.unwrap();
    assert_eq!(
        reason,
        pool_workflow::workflow::EXPIRY_REASON_UNCONFIRMED,
        "the fixture's code, and nothing per-workflow in it"
    );
    assert!(
        !reason.contains("37"),
        "the observed age belongs on the event, not in the dimension: {reason}"
    );
}

#[tokio::test]
async fn the_start_block_cannot_be_moved() {
    // A workflow whose start could move is a workflow whose deadlines could be
    // reset — the one edit §8's guardrails cannot survive.
    let Some(db) = TempDb::migrated("wf_started").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let w = decided(&pool, "w1").await;
    workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();

    let error = sqlx::query(
        "UPDATE pool.workflow SET block_started = 100000, revision = revision + 1
         WHERE workflow_id = 'w1'",
    )
    .execute(&pool)
    .await
    .expect_err("block_started is immutable once TIG has set it");
    assert!(
        error.to_string().contains("block_started is immutable"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn the_deadline_sweep_does_not_expire_a_verified_workflow() {
    // §4.5's ladder continues past VERIFIED to ACTIVE, so VERIFIED is not
    // terminal — but §8's guardrail expires *unfinished* work, and a benchmark
    // TIG has verified is not unfinished. Guarding only on `is_terminal` would
    // let the pool's clock overwrite TIG's verification, which is exactly what
    // `expire`'s own doc says it must never do.
    let Some(db) = TempDb::migrated("wf_verified_not_expired").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();

    let w = decided(&pool, "w1").await;
    let w = workflow::confirm_precommit(&pool, NET, "w1", w.revision, &precommit("bench_a", 100))
        .await
        .unwrap();
    let started = w.block_started.unwrap();
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
    let w = workflow::confirm_verified(&pool, NET, "w1", w.revision, "bench_a", 130)
        .await
        .unwrap();
    assert_eq!(w.state, WorkflowState::Verified);
    assert!(!w.state.is_terminal());

    // Far past the guardrail.
    let long_after = started + i64::from(guardrails.workflow_expiry_age_blocks) + 500;
    assert!(
        workflow::expire_if_due(&pool, &guardrails, NET, "w1", long_after)
            .await
            .unwrap()
            .is_none(),
        "a verified benchmark is not unfinished work"
    );
    let stored = workflow::find(&pool, NET, "w1").await.unwrap().unwrap();
    assert_eq!(stored.state, WorkflowState::Verified);
    assert_eq!(stored.revision, w.revision, "nothing was written");
}

#[tokio::test]
async fn a_row_predating_the_block_started_column_can_still_transition() {
    // 0007 adds `block_started` to a table 0006 already shipped, so a
    // developer's database can hold a workflow that confirmed a precommit
    // before the column existed. A table CHECK — even `NOT VALID` — is
    // re-evaluated against the whole row on every UPDATE, so such a row would
    // pass the migration and then fail its next transition, with no repair
    // path in a forward-only scheme. The rule binds when the confirmation is
    // recorded instead.
    let Some(db) = TempDb::migrated("wf_legacy_row").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();

    // The pre-0007 shape: confirmed, with no block_started. The INSERT trigger
    // refuses this now, so the row is created the way a pre-0007 database
    // holds one — the trigger disabled for this statement, which is what
    // "predates the migration" means.
    sqlx::query("ALTER TABLE pool.workflow DISABLE TRIGGER workflow_insert_block_started")
        .execute(&mut owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pool.workflow
             (network, workflow_id, state, owner_kind, owner_id,
              unverified_from_block, benchmark_id, confirmed_track_id,
              confirmed_settings, precommit_confirmed_block)
         VALUES ('testnet', 'legacy', 'PRECOMMIT_CONFIRMED', 'POOL_BOOTSTRAP',
                 'pool-bootstrap', 90, 'bench_legacy', 't002', '{}'::jsonb, 100)",
    )
    .execute(&mut owner)
    .await
    .expect("a row in the pre-0007 shape");
    sqlx::query("ALTER TABLE pool.workflow ENABLE TRIGGER workflow_insert_block_started")
        .execute(&mut owner)
        .await
        .unwrap();

    let w = workflow::find(&pool, NET, "legacy").await.unwrap().unwrap();
    assert_eq!(w.block_started, None);

    let w = workflow::confirm_benchmark(
        &pool,
        NET,
        "legacy",
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: "bench_legacy".to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    )
    .await
    .expect("a legacy row keeps transitioning");
    assert_eq!(w.state, WorkflowState::BenchmarkConfirmed);
}

#[tokio::test]
async fn an_inserted_row_must_bring_its_start_block_too() {
    // The UPDATE trigger reads OLD, so it cannot see a row created wrong in
    // the first place. Without an INSERT guard, a direct insert naming
    // `confirmed_track_id` with no `block_started` would be accepted and §8's
    // deadlines would have nothing to measure that workflow from.
    let Some(db) = TempDb::migrated("wf_insert_start").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let error = sqlx::query(
        "INSERT INTO pool.workflow
             (network, workflow_id, state, owner_kind, owner_id,
              unverified_from_block, benchmark_id, confirmed_track_id,
              confirmed_settings, precommit_confirmed_block)
         VALUES ('testnet', 'w1', 'PRECOMMIT_CONFIRMED', 'POOL_BOOTSTRAP',
                 'pool-bootstrap', 90, 'bench_a', 't002', '{}'::jsonb, 100)",
    )
    .execute(&pool)
    .await
    .expect_err("a confirmed row carries its start block");
    assert!(
        error.to_string().contains("must carry block_started"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_confirmation_must_bring_its_start_block() {
    // The other half of the same rule: a *new* confirmation cannot be recorded
    // without `block_started`, or §8's deadlines would have nothing to measure
    // the workflow's age from.
    let Some(db) = TempDb::migrated("wf_confirm_needs_start").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    decided(&pool, "w1").await;

    let error = sqlx::query(
        "UPDATE pool.workflow
         SET state = 'PRECOMMIT_CONFIRMED', revision = revision + 1,
             benchmark_id = 'bench_a', confirmed_track_id = 't002',
             confirmed_settings = '{}'::jsonb, precommit_confirmed_block = 100
         WHERE workflow_id = 'w1'",
    )
    .execute(&pool)
    .await
    .expect_err("a confirmation carries the start block");
    assert!(
        error.to_string().contains("must carry block_started"),
        "unexpected error: {error}"
    );
}

#[test]
fn all_states_round_trip_and_match_the_schema() {
    // `WorkflowState::ALL` exists so callers derive sets from the enum instead
    // of repeating a list that drifts — the restart pass's terminal list had
    // already drifted, still naming FRAUD after the state became FRAUDULENT.
    //
    // But ALL is hand-written, so nothing stops a new variant being omitted.
    // This is what keeps it honest: every entry round-trips, and the set
    // equals migration 0006's `workflow_state_known` CHECK.
    for state in WorkflowState::ALL {
        assert_eq!(
            pool_workflow::WorkflowState::parse_state(state.as_str()),
            Some(state),
            "{state:?} does not round-trip"
        );
    }

    let migration = include_str!("../../../migrations/0006_workflow.sql");
    let check = migration
        .split("CONSTRAINT workflow_state_known")
        .nth(1)
        .expect("the CHECK exists")
        .split(")),")
        .next()
        .expect("the CHECK closes");

    let mut in_schema: Vec<&str> = check
        .split('\'')
        .filter(|piece| {
            piece.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !piece.is_empty()
        })
        .collect();
    in_schema.sort_unstable();

    let mut in_enum: Vec<&str> = WorkflowState::ALL.iter().map(|s| s.as_str()).collect();
    in_enum.sort_unstable();

    assert_eq!(
        in_enum, in_schema,
        "the enum and the schema must name the same states"
    );
}
