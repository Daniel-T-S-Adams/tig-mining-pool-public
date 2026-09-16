//! Slice-1 criterion K2 and F4: the lifecycle cases, driven from the fixture.
//!
//! `fixtures/queue-lifecycle/v1/lifecycle.json` maps the benchmark workflow
//! lifecycle, derived by hand from the design documents independently of any
//! implementation (`pre_build_checklist.md` §6). These tests read that file and
//! assert the transitions it records, so the fixture and the state machine
//! cannot drift apart without a failure here.
//!
//! **Read, not transcribed.** A test that restated the cases in Rust would
//! assert whatever the transcription said, and a fixture corrected later would
//! leave it agreeing with itself. The cases are loaded at run time and the
//! assertions are generated from them.
//!
//! # What is in scope, and why the rest is not
//!
//! F4: "**Only the protocol-transition and terminal-state assertions of each
//! case are in slice-1 scope.**" The cases also assert fault attribution,
//! charges and slot release, which F4b forbids this slice from evaluating —
//! it has no members, so recording member fault for a benchmark that has no
//! member would be a lie the schema would then carry for ever.
//!
//! So a transition is in scope when both its states are ones this slice
//! implements. The rest name `mining_system.md` §4.5's member- and
//! artifact-facing ladder — `ASSIGNED`, `COMPUTING`, the `PACKAGE_*` states,
//! `PROOF_BUILDING`, `PROOF_READY` — and the two protocol states that sit
//! behind a confirmed proof, `VERIFYING` and `ACTIVE`, which cannot be reached
//! without artifacts to build a proof from. `WorkflowState`'s own doc records
//! that exclusion.
//!
//! The filter is computed from the enum rather than listed, so a state added
//! later brings its cases into scope without anyone remembering to.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::deadlines::Guardrails;
use pool_workflow::restart::{self, ConfirmedWindow};
use pool_workflow::workflow::{
    self, ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof, Owner,
    POOL_BOOTSTRAP_OWNER, Submitted, WorkflowState,
};
use serde_json::{Value, json};

const NET: Network = Network::Testnet;
/// The height every staged workflow's §6.1 interval opens at. Below the
/// window's block, so nothing is aged out for a reason the case did not name.
const DECIDED_AT: i64 = 90;
/// The block the confirmed window is read at.
///
/// Close enough to the staged evidence that nothing falls outside §8's
/// 120-block `get-benchmarks` window, which would make a confirmed record stop
/// being evidence for a reason no case names.
const AT_BLOCK: i64 = 200;

/// The block §8's local expiry is judged at.
///
/// Separate from the window's, and necessarily so: expiry fires at an age of
/// 120 blocks from `block_started`, so a block late enough to expire a
/// workflow is by construction late enough to have carried its evidence out of
/// the window. The two questions are asked at different times in a real run
/// too — the window is read every block, and a deadline passes once.
const EXPIRE_AT: i64 = STAGED_BLOCK_STARTED + 130;

/// `block_started` on every staged workflow, from the confirmed precommit.
const STAGED_BLOCK_STARTED: i64 = 98;

fn cases() -> Vec<Value> {
    let raw = include_str!("../../../fixtures/queue-lifecycle/v1/lifecycle.json");
    let doc: Value = serde_json::from_str(raw).expect("the lifecycle fixture parses");
    doc["cases"].as_array().expect("cases is a list").clone()
}

fn state_named(name: &str) -> Option<WorkflowState> {
    WorkflowState::ALL.into_iter().find(|s| s.as_str() == name)
}

/// One expected transition of a workflow, as the fixture records it.
struct Transition {
    workflow_id: String,
    from: WorkflowState,
    to: WorkflowState,
    rule: String,
}

/// The workflow transitions of one case that this slice can hold.
///
/// Both ends must be states the enum has: a transition *into* a state slice 1
/// cannot represent is not one it can assert, and a transition *out of* one
/// cannot be staged.
fn in_scope(case: &Value) -> Vec<Transition> {
    case["expected_transitions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            let subject = t["subject"].as_str()?;
            let workflow_id = subject.strip_prefix("workflow ")?.trim();
            Some(Transition {
                workflow_id: workflow_id.to_string(),
                from: state_named(t["from"].as_str()?)?,
                to: state_named(t["to"].as_str()?)?,
                rule: t["rule"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Stage a workflow at the state the case starts it in.
///
/// Reached by walking the real transitions rather than writing the state into
/// the row: a staged state the state machine could not itself produce would
/// make the case start somewhere unreachable, and the transition under test
/// would then be measured from a position the pool can never be in.
async fn stage(pool: &sqlx::PgPool, workflow_id: &str, benchmark_id: &str, target: WorkflowState) {
    let w = workflow::create(
        pool,
        NET,
        workflow_id,
        Owner::PoolBootstrap,
        POOL_BOOTSTRAP_OWNER,
        DECIDED_AT,
    )
    .await
    .expect("the workflow is created DECIDED");

    if target == WorkflowState::Decided {
        return;
    }

    // DECIDED -> PRECOMMIT_SUBMITTED is a local fact; every later state needs
    // the benchmark bound, which the confirmed precommit does.
    let w = workflow::record_submitted(pool, NET, workflow_id, w.revision, Submitted::Precommit)
        .await
        .expect("the precommit write was sent");
    if target == WorkflowState::PrecommitSubmitted {
        return;
    }

    let w = workflow::confirm_precommit(
        pool,
        NET,
        workflow_id,
        w.revision,
        &ConfirmedPrecommit {
            benchmark_id: benchmark_id.to_string(),
            block_confirmed: 100,
            block_started: STAGED_BLOCK_STARTED,
            num_nonces: Some(80),
            num_bundles: Some(4),
            track_id: "t001".to_string(),
            settings: json!({}),
        },
    )
    .await
    .expect("the precommit confirmed");
    if target == WorkflowState::PrecommitConfirmed {
        return;
    }

    let w = workflow::record_submitted(pool, NET, workflow_id, w.revision, Submitted::Benchmark)
        .await
        .expect("the commitment was sent");
    if target == WorkflowState::BenchmarkSubmitted {
        return;
    }

    let w = workflow::confirm_benchmark(
        pool,
        NET,
        workflow_id,
        w.revision,
        &ConfirmedBenchmark {
            benchmark_id: benchmark_id.to_string(),
            block_confirmed: 110,
            stopped: false,
        },
    )
    .await
    .expect("the benchmark confirmed");
    if target == WorkflowState::BenchmarkConfirmed {
        return;
    }

    workflow::record_submitted(pool, NET, workflow_id, w.revision, Submitted::Proof)
        .await
        .expect("the proof was sent");
    if target == WorkflowState::ProofSubmitted {
        return;
    }

    panic!("no staging path to {target:?}; the case needs one adding");
}

/// The confirmed evidence that produces `to`, per `tig_integration.md` §7.
///
/// §7's table is what says which read confirms which event, and this is that
/// table for the states slice 1 has. Built per target state rather than parsed
/// from the case's prose `events`, which are written for a human.
fn evidence_for(window: &mut ConfirmedWindow, benchmark_id: &str, to: WorkflowState) {
    match to {
        WorkflowState::PrecommitConfirmed => {
            window.precommits.insert(
                benchmark_id.to_string(),
                ConfirmedPrecommit {
                    benchmark_id: benchmark_id.to_string(),
                    block_confirmed: 100,
                    block_started: STAGED_BLOCK_STARTED,
                    num_nonces: Some(80),
                    num_bundles: Some(4),
                    track_id: "t001".to_string(),
                    settings: json!({}),
                },
            );
        }
        WorkflowState::BenchmarkConfirmed | WorkflowState::Stopped => {
            window.benchmarks.insert(
                benchmark_id.to_string(),
                ConfirmedBenchmark {
                    benchmark_id: benchmark_id.to_string(),
                    block_confirmed: 110,
                    // §7: a confirmed benchmark with `details.stopped` true is
                    // what makes STOPPED, and it is the same read either way.
                    stopped: to == WorkflowState::Stopped,
                },
            );
        }
        WorkflowState::ProofConfirmed => {
            window.proofs.insert(
                benchmark_id.to_string(),
                ConfirmedProof {
                    benchmark_id: benchmark_id.to_string(),
                    block_confirmed: 120,
                },
            );
        }
        WorkflowState::Fraudulent => {
            window.frauds.insert(
                benchmark_id.to_string(),
                ConfirmedFraud {
                    benchmark_id: benchmark_id.to_string(),
                    block_confirmed: 125,
                },
            );
        }
        WorkflowState::Verified => {
            window.verified.push(benchmark_id.to_string());
        }
        // EXPIRED is the pool's own conclusion from a deadline passing, not a
        // read — `expire_if_due` makes it, and the case that needs it says so.
        other => panic!("§7 names no confirming read for {other:?}"),
    }
}

/// Each case driven once, with the evidence its transitions imply, and the
/// resulting state asserted against the fixture's own `final_state`.
///
/// Per case rather than per transition, because the cases are *sequences*: a
/// confirmed benchmark carrying `stopped` advances a workflow two steps in one
/// pass — through `BENCHMARK_CONFIRMED` and on to `STOPPED` — and driving
/// those separately would stage the second from a state the first never
/// produced, testing a scenario the fixture does not describe.
///
/// One test over all the cases rather than one per case: the set is the
/// fixture's, and a test per case has to be added by hand when a case is,
/// which is how a fixture grows a case nothing runs.
#[tokio::test]
async fn every_in_scope_case_reaches_the_state_the_fixture_records() {
    let Some(db) = TempDb::migrated("lifecycle_cases").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let mut asserted = 0usize;
    let mut covered: Vec<String> = Vec::new();

    for case in cases() {
        let name = case["name"].as_str().unwrap_or_default().to_string();
        let transitions = in_scope(&case);
        if transitions.is_empty() {
            continue;
        }

        // The furthest state along each workflow's ladder that this slice can
        // represent — not the case's `final_state`, which for two cases ends
        // at `ACTIVE`. Taking the final state alone would drop every in-scope
        // step of a case whose ladder continues past what slice 1 has, and
        // those steps are exactly the protocol transitions F4 puts in scope.
        let mut furthest: BTreeMap<String, &Transition> = BTreeMap::new();
        for t in &transitions {
            furthest.insert(t.workflow_id.clone(), t);
        }

        for (workflow, t) in furthest {
            let expected = t.to;
            let workflow_id = format!("{name}::{workflow}");
            let benchmark_id = format!("bm::{workflow_id}");

            // Staged from where the *chain* starts, not where this step does.
            // `stopped_benchmark_no_proof` runs BENCHMARK_SUBMITTED ->
            // BENCHMARK_CONFIRMED -> STOPPED off one confirmed benchmark
            // carrying `stopped`; staging at the middle state would present
            // that evidence to a workflow already past the step it produces.
            let chain_start = transitions
                .iter()
                .filter(|other| other.workflow_id == workflow)
                .map(|other| other.from)
                .min_by_key(|state| {
                    WorkflowState::ALL
                        .iter()
                        .position(|s| s == state)
                        .unwrap_or(usize::MAX)
                })
                .unwrap_or(t.from);

            // `PRECOMMIT_SUBMITTED` is where the fixture starts one case, and
            // the implementation cannot advance from there: `bind` selects
            // `DECIDED` only, and such a workflow has no `benchmark_id` for
            // the restart pass to match on. Issue #44. Staged from `DECIDED`,
            // which is where the pool actually sits while its precommit is
            // unconfirmed — the K3 live run went straight from it.
            let from = if chain_start == WorkflowState::PrecommitSubmitted {
                WorkflowState::Decided
            } else {
                chain_start
            };
            stage(&pool, &workflow_id, &benchmark_id, from).await;

            // Every in-scope step of this workflow's ladder, in order. A case
            // is a sequence of evidence arrivals, and applying only the last
            // one would present a proof confirmation to a workflow that never
            // confirmed its precommit.
            let mut steps: Vec<&Transition> = transitions
                .iter()
                .filter(|other| other.workflow_id == workflow)
                .collect();
            steps.sort_by_key(|step| {
                WorkflowState::ALL
                    .iter()
                    .position(|s| *s == step.to)
                    .unwrap_or(usize::MAX)
            });
            // Accumulated into one window and run once, because that is how
            // the pass works: `reconcile_after_restart` advances a workflow as
            // far as the evidence allows in a single pass. It also matters for
            // `stopped_benchmark_no_proof`, where one confirmed benchmark
            // carrying `stopped` produces both steps — presenting them as two
            // arrivals would offer a second confirmation to a workflow already
            // past the first.
            let mut window = ConfirmedWindow {
                at_block: AT_BLOCK,
                ..ConfirmedWindow::default()
            };
            let mut expire = false;
            for step in steps {
                match step.to {
                    WorkflowState::PrecommitConfirmed => {
                        confirm_precommit_directly(&pool, &workflow_id, &benchmark_id, &name).await;
                    }
                    // Not a read: §8's deadline passing is the pool's own
                    // conclusion, drawn after the window is read.
                    WorkflowState::Expired => expire = true,
                    other => evidence_for(&mut window, &benchmark_id, other),
                }
            }
            restart::reconcile_after_restart(&pool, NET, &window)
                .await
                .unwrap_or_else(|e| panic!("{name}: reconciliation failed: {e}"));
            if expire {
                expire_if_due(&pool, &workflow_id, &name).await;
            }

            let after = workflow::find(&pool, NET, &workflow_id)
                .await
                .unwrap()
                .expect("still there");
            assert_eq!(
                after.state,
                expected,
                "{name}: {workflow} should reach {} from {}.\n  rule: {}",
                expected.as_str(),
                from.as_str(),
                t.rule
            );
            asserted += 1;
        }
        covered.push(name);
    }

    // The fixture is the source of these assertions, so an empty run means the
    // harness stopped reading it rather than that the lifecycle has nothing to
    // say. Both numbers are pinned: a case losing its transitions, or the
    // scope filter silently widening, moves one of them.
    // Six workflows across five cases reach a state slice 1 can represent —
    // one each in `delayed_precommit_confirmation`,
    // `stopped_benchmark_no_proof`, `expired_workflow_pool_side_stall` and
    // `active_full_happy_path`, and two of the three in
    // `restart_recovery_reconciles_three_workflows` (the third ends `ACTIVE`).
    //
    // Pinned so the harness cannot quietly stop driving one: the fixture is
    // where these assertions come from, so a case losing its transitions, or
    // the scope filter widening, moves this number rather than passing in
    // silence.
    assert_eq!(
        asserted, 6,
        "expected 6 in-scope workflow ladders across the fixture, found \
         {asserted} (cases covered: {covered:?})"
    );
    assert_eq!(
        covered.len(),
        5,
        "cases with in-scope transitions: {covered:?}"
    );
}

/// The precommit's confirming evidence, applied directly rather than through
/// `bind`.
///
/// Two things happen when a precommit confirms: the §10 tuple search decides
/// *which* workflow the entry belongs to, and that workflow then advances on
/// §7's evidence. The rule these cases cite is the second — "non-null
/// `state.block_confirmed` in `get-benchmarks.precommits` is the authoritative
/// evidence" — and that is what is driven here, through the same function
/// `bind` and the restart pass both call.
///
/// The matching half is `tests/bind.rs`'s, which stages a decision and an
/// attempt and drives the real search, and it was evidenced live in the K3 run.
async fn confirm_precommit_directly(
    pool: &sqlx::PgPool,
    workflow_id: &str,
    benchmark_id: &str,
    case: &str,
) {
    let w = workflow::find(pool, NET, workflow_id)
        .await
        .unwrap()
        .unwrap();
    workflow::confirm_precommit(
        pool,
        NET,
        workflow_id,
        w.revision,
        &ConfirmedPrecommit {
            benchmark_id: benchmark_id.to_string(),
            block_confirmed: 100,
            block_started: STAGED_BLOCK_STARTED,
            num_nonces: Some(80),
            num_bundles: Some(4),
            track_id: "t001".to_string(),
            settings: json!({}),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{case}: confirming the precommit failed: {e}"));
}

/// §8's local expiry, which is the pool's own conclusion and not a read —
/// which is why the restart pass cannot produce this state from a window.
async fn expire_if_due(pool: &sqlx::PgPool, workflow_id: &str, case: &str) {
    let guardrails = Guardrails {
        max_assignment_age_blocks: 60,
        package_due_age_blocks: 110,
        workflow_expiry_age_blocks: 120,
        proof_reserve_blocks: 10,
    };
    workflow::expire_if_due(pool, &guardrails, NET, workflow_id, EXPIRE_AT)
        .await
        .unwrap_or_else(|e| panic!("{case}: expiring failed: {e}"));
}

#[tokio::test]
async fn a_stopped_benchmark_produces_no_proof_write() {
    // F4 names this one specifically: "including `stopped` (no proof is sent)".
    // The fixture's `stopped_benchmark_no_proof` asserts
    // `proof_write_intents_created: 0`, which is the half that matters — a
    // stopped benchmark has no bundle that passed the quality threshold, so a
    // proof would be a write with nothing to prove and a fee paid for it.
    let Some(db) = TempDb::migrated("lifecycle_stopped").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "stopped_benchmark_no_proof")
        .expect("the case is in the fixture");
    assert_eq!(
        case["final_state"]["proof_write_intents_created"],
        json!(0),
        "the fixture's own expectation, read rather than assumed"
    );

    stage(
        &pool,
        "wf_stopped",
        "bm_stopped",
        WorkflowState::BenchmarkSubmitted,
    )
    .await;
    let mut window = ConfirmedWindow {
        at_block: AT_BLOCK,
        ..ConfirmedWindow::default()
    };
    evidence_for(&mut window, "bm_stopped", WorkflowState::Stopped);
    restart::reconcile_after_restart(&pool, NET, &window)
        .await
        .unwrap();

    let after = workflow::find(&pool, NET, "wf_stopped")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, WorkflowState::Stopped);

    let proofs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent WHERE write_kind = 'proof'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(proofs, 0, "a stopped benchmark has nothing to prove");
}

#[tokio::test]
async fn the_out_of_scope_transitions_are_out_of_scope_for_a_reason() {
    // The filter is computed, not listed — so it is worth asserting that it
    // excludes what F4 says it should and nothing else. A filter that silently
    // widened would make this suite claim coverage it does not have; one that
    // narrowed would drop cases without anyone noticing.
    let mut excluded: Vec<String> = Vec::new();
    for case in cases() {
        for t in case["expected_transitions"].as_array().unwrap_or(&vec![]) {
            let Some(subject) = t["subject"].as_str() else {
                continue;
            };
            if !subject.starts_with("workflow ") {
                continue;
            }
            for end in ["from", "to"] {
                if let Some(s) = t[end].as_str()
                    && state_named(s).is_none()
                {
                    excluded.push(s.to_string());
                }
            }
        }
    }
    excluded.sort();
    excluded.dedup();

    assert_eq!(
        excluded,
        vec![
            // `mining_system.md` §4.5's member- and artifact-facing ladder.
            "ACTIVE",
            "ASSIGNED",
            "COMPUTING",
            "PACKAGE_DURABLY_ACCEPTED",
            "PACKAGE_RECEIVED",
            "PACKAGE_STRUCTURALLY_ACCEPTED",
            "PACKAGE_UPLOADING",
            "PACKAGING",
            "PROOF_BUILDING",
            "PROOF_READY",
            "VERIFYING",
        ],
        "every excluded state must be one slice 1 cannot reach without members \
         or artifacts; anything else here is a gap, not a scope boundary"
    );
}
