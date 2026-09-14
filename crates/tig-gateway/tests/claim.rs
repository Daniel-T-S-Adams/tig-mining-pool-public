//! §5.1 step 5's decision: what the gateway does with a claimed intent.
//!
//! Pure over constructed records. The property under test is which situations
//! license a write and which do not, and the asymmetry that decides every
//! ambiguous case: a stopped workflow costs an operator's attention and is
//! visible; a duplicated precommit costs a fee, creates a benchmark the pool
//! cannot attribute, and breaks §10's permanent one-benchmark mapping.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pool_domain::Network;
use pool_workflow::payload::benchmark_digest;
use pool_workflow::{
    AttemptOutcome, BenchmarkSubmission, IntentState, WriteAttempt, WriteIntent, WriteKind,
};
use serde_json::json;
use tig_gateway::claim::{
    ClaimDecision, ConfirmedBenchmarks, ConfirmedReadError, OwningWorkflow, SiblingGenerations,
    SkipReason, StopReason, decide, decide_benchmark,
};
use tig_gateway::reconcile::{PrecommitSubmission, TrackSettings};

const PLAYER: &str = "0xp00l";
const BLOCK: &str = "block_100080";

fn submitted() -> PrecommitSubmission {
    let mut tracks = BTreeMap::new();
    tracks.insert(
        "t001".to_string(),
        TrackSettings {
            num_bundles: 4,
            fuel_budget: 1_500_000,
            hyperparameters: BTreeMap::new(),
        },
    );
    PrecommitSubmission {
        player_id: PLAYER.to_string(),
        block_id: BLOCK.to_string(),
        challenge_id: "c001".to_string(),
        algorithm_id: "a011".to_string(),
        compute_type: "aws_t4g".to_string(),
        track_settings: tracks,
    }
}

fn matching_precommit(benchmark_id: &str, confirmed: Option<i64>) -> serde_json::Value {
    json!({
        // `benchmark_id` sits at the top level of a `get-benchmarks`
        // precommit entry; `reconcile`'s "precommit" argument is the path
        // label its error messages use, not a nesting.
        "benchmark_id": benchmark_id,
        "settings": {
            "player_id": PLAYER, "block_id": BLOCK,
            "challenge_id": "c001", "algorithm_id": "a011", "track_id": "t001"
        },
        "details": {
            "compute_type": "aws_t4g", "num_bundles": 4, "fuel_budget": 1_500_000
        },
        "state": { "block_confirmed": confirmed },
    })
}

fn intent(state: IntentState) -> WriteIntent {
    WriteIntent {
        intent_id: "11111111-1111-1111-1111-111111111111".to_string(),
        network: Network::Testnet,
        workflow_id: "w1".to_string(),
        write_kind: WriteKind::Precommit,
        generation: 1,
        benchmark_id: None,
        // §7.3 binds the intent to its payload by digest, and `decide` checks
        // it. The real path records exactly this value at admission.
        payload_digest: tig_gateway::transmit::precommit_digest(&submitted()),
        payload_artifact_id: None,
        state,
    }
}

fn attempt(outcome: Option<AttemptOutcome>) -> WriteAttempt {
    WriteAttempt {
        attempt_id: "22222222-2222-2222-2222-222222222222".to_string(),
        intent_id: "11111111-1111-1111-1111-111111111111".to_string(),
        attempt_no: 1,
        outcome,
        http_status: None,
        reconciled: false,
        age_secs: 0,
    }
}

/// One generation, nothing sent on any sibling: the ordinary shape.
const ONLY: SiblingGenerations = SiblingGenerations {
    newest_generation: 1,
    sibling_transmitted: false,
};

const LIVE: OwningWorkflow = OwningWorkflow {
    state: "DECIDED",
    is_terminal: false,
};
const ENDED: OwningWorkflow = OwningWorkflow {
    state: "EXPIRED",
    is_terminal: true,
};

#[test]
fn an_intent_that_never_left_the_gateway_is_transmitted() {
    // The ordinary path, and the only one that sends. §7.3 records an attempt
    // before the request leaves, so zero attempts is positive evidence that
    // nothing was sent rather than an absence of evidence.
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[],
            LIVE,
            ONLY,
            &submitted(),
            &[]
        ),
        ClaimDecision::Transmit
    );
}

#[test]
fn a_workflow_that_ended_gets_no_write() {
    // Issue #86. An expiry or a restart leaves a PREPARED intent behind and
    // nothing else stops it being transmitted later — §13 invariant 14 forbids
    // a restart silently producing a stray TIG write.
    //
    // Sending would pay a fee for work the pool has written off, and close
    // §10's single unresolved-precommit lane on a write nobody can reconcile
    // onto a live workflow.
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[],
            ENDED,
            ONLY,
            &submitted(),
            &[]
        ),
        ClaimDecision::Skip {
            reason: SkipReason::WorkflowEnded { state: "EXPIRED" }
        }
    );
}

#[test]
fn an_unresolved_write_is_reconciled_even_when_the_workflow_ended() {
    // The fix for a pool-wide deadlock, not a preference about ordering.
    //
    // `migrations/0004`'s unresolved-precommit index is network-wide, so one
    // unsettled attempt closes §10's lane for every workflow in the pool. A
    // workflow can end with one outstanding — `expire_if_due` refuses to, but
    // `fail` and the operator `expire` carry no such guard — and skipping on
    // terminality first would leave that ambiguity unreconciled for ever, with
    // the lane shut behind it.
    //
    // Reconciling is safe here because no reconciliation answer transmits.
    // What it settles is the write, so the lane reopens; advancing the
    // workflow is the controller's, from confirmed reads.
    assert_eq!(
        decide(
            &intent(IntentState::OutcomeUnknown),
            &[attempt(Some(AttemptOutcome::Ambiguous))],
            ENDED,
            ONLY,
            &submitted(),
            &[matching_precommit("bench_a", Some(100))],
        ),
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        },
        "an ended workflow must still let its write be settled"
    );
}

#[test]
fn a_terminal_workflow_with_nothing_in_flight_is_skipped_without_consulting_tig() {
    // The half of the old ordering that is still right. With nothing
    // outstanding there is no lane to reopen, and reconciling a dead workflow
    // invites the tuple search to bind someone else's confirmed precommit to
    // it — a second owner for one benchmark.
    //
    // The window here would answer StopForOperator if the search ran, so a
    // Skip proves it did not.
    let two = [
        matching_precommit("bench_a", Some(100)),
        matching_precommit("bench_b", Some(101)),
    ];
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[],
            ENDED,
            ONLY,
            &submitted(),
            &two
        ),
        ClaimDecision::Skip {
            reason: SkipReason::WorkflowEnded { state: "EXPIRED" }
        }
    );
}

#[test]
fn two_unsent_generations_produce_one_write() {
    // The duplicate this decision exists to prevent, and it is reachable:
    // `admit_precommit` refuses a new generation only once a precommit has
    // been *transmitted*, so two PREPARED generations can coexist for one
    // DECIDED workflow — `pool-workflow`'s own admission test creates exactly
    // that shape.
    //
    // Each sees an empty attempt list of its own. An intent-scoped decision
    // transmits both: two fees, two benchmarks, and §10's permanent
    // one-benchmark-per-workflow mapping broken by the pool itself.
    let siblings = SiblingGenerations {
        newest_generation: 2,
        sibling_transmitted: false,
    };

    let mut older = intent(IntentState::Prepared);
    older.generation = 1;
    assert_eq!(
        decide(&older, &[], LIVE, siblings, &submitted(), &[]),
        ClaimDecision::Skip {
            reason: SkipReason::SupersededByNewerGeneration { newest: 2 }
        },
        "§7.3 makes a new generation the way to change a payload, so the newest is the live one"
    );

    let mut newest = intent(IntentState::Prepared);
    newest.generation = 2;
    assert_eq!(
        decide(&newest, &[], LIVE, siblings, &submitted(), &[]),
        ClaimDecision::Transmit,
        "exactly one of the two is sent"
    );
}

#[test]
fn a_generation_whose_sibling_was_sent_stops_for_an_operator() {
    // §7.3 forbids a new generation "once an earlier attempt may have reached
    // TIG", so this should not arise — but it is reachable by creating two
    // generations while both are unsent and then sending the older one.
    //
    // Which generation TIG now holds is not the gateway's to guess.
    let siblings = SiblingGenerations {
        newest_generation: 2,
        sibling_transmitted: true,
    };
    let mut newest = intent(IntentState::Prepared);
    newest.generation = 2;
    assert_eq!(
        decide(&newest, &[], LIVE, siblings, &submitted(), &[]),
        ClaimDecision::StopForOperator {
            reason: StopReason::SiblingGenerationTransmitted { generation: 2 }
        }
    );
}

#[test]
fn a_write_tig_refused_is_not_an_ambiguity() {
    // A definitive negative. `pool_workflow`'s attempt ledger already states
    // the rule: a refused write created no benchmark, so §10's search finds
    // nothing and reporting that nothing as "a write may have reached TIG"
    // fills the stop-for-operator bucket that only works while it stays quiet.
    //
    // What this workflow is owed is `fail`.
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[attempt(Some(AttemptOutcome::Rejected))],
            LIVE,
            ONLY,
            &submitted(),
            &[],
        ),
        ClaimDecision::Skip {
            reason: SkipReason::Refused
        }
    );
}

#[test]
fn an_unknown_outcome_with_no_attempt_is_a_contradiction_not_a_send() {
    // §7.3 writes OUTCOME_UNKNOWN in the same transaction that marks an
    // attempt AMBIGUOUS, so one without the other is a contradiction in the
    // durable record. Transmitting on the strength of the missing half turns
    // the pool's own record of "a write may have reached TIG" into a resend.
    assert_eq!(
        decide(
            &intent(IntentState::OutcomeUnknown),
            &[],
            LIVE,
            ONLY,
            &submitted(),
            &[]
        ),
        ClaimDecision::StopForOperator {
            reason: StopReason::UnknownOutcomeWithNoAttempt
        }
    );
}

#[test]
fn a_non_precommit_intent_is_not_treated_as_one() {
    // §7 reconciles benchmark and proof writes by `benchmark_id`, not by
    // §10's tuple search over submitted settings. Answering Transmit for one
    // would send a write this function never reasoned about.
    let mut benchmark = intent(IntentState::Prepared);
    benchmark.write_kind = WriteKind::Benchmark;
    benchmark.benchmark_id = Some("bench_a".to_string());
    assert_eq!(
        decide(&benchmark, &[], LIVE, ONLY, &submitted(), &[]),
        ClaimDecision::Skip {
            reason: SkipReason::NotAPrecommit { kind: "benchmark" }
        }
    );
}

#[test]
fn attempts_belonging_to_another_intent_stop_rather_than_answer() {
    // They answer a question about a write this intent did not make.
    // `PrecommitTransmitter::send` refuses the same mismatch; a decision built
    // on it would be worse, because it decides whether to send at all.
    let mut stray = attempt(Some(AttemptOutcome::Ambiguous));
    stray.intent_id = "99999999-9999-9999-9999-999999999999".to_string();
    let decision = decide(
        &intent(IntentState::Prepared),
        &[stray],
        LIVE,
        ONLY,
        &submitted(),
        &[],
    );
    assert!(
        matches!(
            decision,
            ClaimDecision::StopForOperator {
                reason: StopReason::Unreadable { .. }
            }
        ),
        "{decision:?}"
    );
}

#[test]
fn a_settled_intent_is_left_alone() {
    // §7.3 makes CONFIRMED and REJECTED terminal, so there is no write left.
    for state in [IntentState::Confirmed, IntentState::Rejected] {
        assert_eq!(
            decide(&intent(state), &[], LIVE, ONLY, &submitted(), &[]),
            ClaimDecision::Skip {
                reason: SkipReason::AlreadySettled
            },
            "{state:?}"
        );
    }
}

#[test]
fn a_lost_response_whose_write_landed_is_settled_and_not_resent() {
    // The case §10's search exists for: the response carrying the assigned
    // benchmark_id was lost, so the pool cannot name a benchmark TIG has
    // already created and charged a fee for.
    //
    // The answer is the id, not a retry.
    let found = decide(
        &intent(IntentState::OutcomeUnknown),
        &[attempt(Some(AttemptOutcome::Ambiguous))],
        LIVE,
        ONLY,
        &submitted(),
        &[matching_precommit("bench_a", Some(100))],
    );
    assert_eq!(
        found,
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
}

#[test]
fn a_matching_write_that_has_not_confirmed_is_waited_for_not_resent() {
    // §7: confirmation is a read, never a response. A candidate that matches
    // but has not confirmed is the pool's own write in flight, and treating it
    // as "nothing arrived" is exactly the blind resubmission §10 forbids.
    assert_eq!(
        decide(
            &intent(IntentState::OutcomeUnknown),
            &[attempt(Some(AttemptOutcome::Ambiguous))],
            LIVE,
            ONLY,
            &submitted(),
            &[matching_precommit("bench_a", None)],
        ),
        ClaimDecision::AwaitConfirmation
    );
}

#[test]
fn two_matching_candidates_stop_for_an_operator() {
    // §10 says so in as many words. Picking the confirmed one, or the newest,
    // attributes a benchmark and its fees and rewards to a workflow that may
    // not own it.
    let decision = decide(
        &intent(IntentState::OutcomeUnknown),
        &[attempt(Some(AttemptOutcome::Ambiguous))],
        LIVE,
        ONLY,
        &submitted(),
        &[
            matching_precommit("bench_a", Some(100)),
            matching_precommit("bench_b", None),
        ],
    );
    match decision {
        ClaimDecision::StopForOperator {
            reason: StopReason::MultipleCandidates { candidates },
        } => assert_eq!(
            candidates,
            vec!["bench_a".to_string(), "bench_b".to_string()]
        ),
        other => panic!("expected a stop, got {other:?}"),
    }
}

#[test]
fn a_sent_write_the_search_cannot_account_for_stops_rather_than_resending() {
    // The fail-closed choice, and the one worth being explicit about.
    //
    // `reconcile` says `NoCandidate` is "not, on its own, a licence to
    // resend": §10 step 3 searches the latest 120-block window, so a precommit
    // older than that window is absent from it for reasons that have nothing
    // to do with whether TIG accepted it.
    //
    // What would license a resend is evidence that the attempt is recent
    // enough to lie inside the window it was searched against. Nothing
    // establishes that yet, so this stops. The two errors are not symmetric: a
    // stop costs an operator's attention, a wrong resend costs a fee and
    // creates a benchmark the pool cannot attribute.
    for outcome in [None, Some(AttemptOutcome::Ambiguous)] {
        assert_eq!(
            decide(
                &intent(IntentState::OutcomeUnknown),
                &[attempt(outcome)],
                LIVE,
                ONLY,
                &submitted(),
                &[],
            ),
            ClaimDecision::StopForOperator {
                reason: StopReason::WriteUnaccountedFor
            },
            "{outcome:?}"
        );
    }
}

#[test]
fn a_begun_attempt_on_a_prepared_intent_still_reconciles_first() {
    // §12's "TIG Gateway dies before/after HTTP response: attempt stays
    // pending or unknown". The intent never reached OUTCOME_UNKNOWN because
    // `resolve` never ran, so keying the retry on intent state alone would
    // resend a write that may already have landed.
    //
    // The attempt row is what says something may have gone out, and it is what
    // this reads.
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[attempt(None)],
            LIVE,
            ONLY,
            &submitted(),
            &[matching_precommit("bench_a", Some(100))],
        ),
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        },
        "a PREPARED intent with an attempt is not an unsent intent"
    );
}

#[test]
fn an_unreadable_record_stops_instead_of_being_skipped() {
    // Skipping a record the pool cannot parse would turn a match into an
    // absence, and absence is what licenses acting as though nothing happened.
    let decision = decide(
        &intent(IntentState::OutcomeUnknown),
        &[attempt(Some(AttemptOutcome::Ambiguous))],
        LIVE,
        ONLY,
        &submitted(),
        &[json!({ "precommit": {}, "settings": {}, "details": {} })],
    );
    assert!(
        matches!(
            decision,
            ClaimDecision::StopForOperator {
                reason: StopReason::Unreadable { .. }
            }
        ),
        "{decision:?}"
    );
}

#[test]
fn a_submission_that_is_not_the_recorded_payload_cannot_bind_a_benchmark() {
    // §7.3 binds an intent to a canonical payload by digest, and `send`
    // refuses a mismatch — but only on the path that sends. The reconcile
    // path runs §10's search over the submission's exact settings, so a
    // reconstructed submission that drifted from the recorded one would
    // search for a write this intent never described and could bind another
    // workflow's confirmed benchmark to it.
    let mut drifted = submitted();
    drifted.challenge_id = "c002".to_string();

    // A confirmed precommit matching the *drifted* settings exists: without
    // the digest check this would come back AlreadyConfirmed for a benchmark
    // that belongs to whoever actually submitted c002.
    let mut other = matching_precommit("bench_theirs", Some(100));
    other["settings"]["challenge_id"] = json!("c002");

    assert_eq!(
        decide(
            &intent(IntentState::OutcomeUnknown),
            &[attempt(Some(AttemptOutcome::Ambiguous))],
            LIVE,
            ONLY,
            &drifted,
            &[other],
        ),
        ClaimDecision::StopForOperator {
            reason: StopReason::PayloadNotTheRecordedOne
        }
    );
}

#[test]
fn an_older_generation_reads_as_superseded_after_the_newest_is_sent() {
    // The ordinary end of `two_unsent_generations_produce_one_write`: the
    // newest was sent, the older is still PREPARED and still claimable, and
    // it must read as supersession on every scan — not as the stop reserved
    // for the state §7.3 forbids. A bucket that fills on the normal path is a
    // bucket nobody reads.
    let after_send = SiblingGenerations {
        newest_generation: 2,
        sibling_transmitted: true,
    };
    let mut older = intent(IntentState::Prepared);
    older.generation = 1;
    assert_eq!(
        decide(&older, &[], LIVE, after_send, &submitted(), &[]),
        ClaimDecision::Skip {
            reason: SkipReason::SupersededByNewerGeneration { newest: 2 }
        }
    );
}

#[test]
fn an_accepted_attempt_reconciles_rather_than_reading_as_refused() {
    // The `Accepted` half of "unsettled", which no other test reaches. An
    // accepted write is a paid-for benchmark whose confirmation has not been
    // read yet; treating it as settled-and-done — or worse, as refused because
    // it is not unresolved — would skip a write that landed and leave the
    // benchmark id unbound.
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[attempt(Some(AttemptOutcome::Accepted))],
            LIVE,
            ONLY,
            &submitted(),
            &[matching_precommit("bench_a", Some(100))],
        ),
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
    assert_eq!(
        decide(
            &intent(IntentState::Prepared),
            &[attempt(Some(AttemptOutcome::Accepted))],
            LIVE,
            ONLY,
            &submitted(),
            &[matching_precommit("bench_a", None)],
        ),
        ClaimDecision::AwaitConfirmation,
        "accepted and not yet confirmed is waited for, not skipped"
    );
}

// ---- benchmark commitments (§6.2), which reconcile directly -----------------

/// `get-benchmarks.benchmarks` as TIG returns it, with `bench_a` confirmed.
///
/// Built as the read rather than as a list of ids, so the test exercises §7's
/// actual test — a non-null `state.block_confirmed` — and not a caller's
/// memory of it.
fn confirmed_read() -> ConfirmedBenchmarks {
    ConfirmedBenchmarks::from_read(&[json!({
        "id": "bench_a",
        "state": { "block_confirmed": 42 }
    })])
    .unwrap()
}

/// The same entry, present but not yet confirmed. §7 says this confirms
/// nothing, and membership alone must not settle a commitment.
fn unconfirmed_read() -> ConfirmedBenchmarks {
    ConfirmedBenchmarks::from_read(&[json!({
        "id": "bench_a",
        "state": { "block_confirmed": serde_json::Value::Null }
    })])
    .unwrap()
}

/// The body the artifact worker built for `bench_a`.
fn commitment() -> BenchmarkSubmission {
    BenchmarkSubmission {
        benchmark_id: "bench_a".to_string(),
        merkle_root: "ab".repeat(32),
        solution_quality: vec![1, 2, 3, 4],
    }
}

fn benchmark_intent(state: IntentState) -> WriteIntent {
    WriteIntent {
        intent_id: "11111111-1111-1111-1111-111111111111".to_string(),
        network: Network::Testnet,
        workflow_id: "w1".to_string(),
        write_kind: WriteKind::Benchmark,
        generation: 1,
        benchmark_id: Some("bench_a".to_string()),
        // Taken from the body rather than invented, so a decision that
        // checks §7.3's binding digest is exercised against a body that
        // genuinely is this intent's.
        payload_digest: benchmark_digest(&commitment()),
        payload_artifact_id: Some("artifact/w1/commitment".to_string()),
        state,
    }
}

fn live() -> OwningWorkflow {
    OwningWorkflow {
        state: "PRECOMMIT_CONFIRMED",
        is_terminal: false,
    }
}

fn ended() -> OwningWorkflow {
    OwningWorkflow {
        state: "EXPIRED",
        is_terminal: true,
    }
}

#[test]
fn an_unsent_commitment_on_a_live_workflow_is_transmitted() {
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&commitment())
        ),
        ClaimDecision::Transmit
    );
}

#[test]
fn being_in_the_read_is_not_being_confirmed() {
    // §7's test is a non-null `state.block_confirmed`, not membership. An
    // entry can be in `get-benchmarks.benchmarks` and not yet confirmed, and
    // settling a commitment on its presence would settle it on evidence TIG
    // has not given.
    //
    // This is why the argument is a `ConfirmedBenchmarks` and not a
    // `Vec<String>`: the test runs in the only constructor, so a caller
    // cannot forget it and a list of ids cannot be passed by mistake.
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[attempt(Some(AttemptOutcome::Accepted))],
            live(),
            &unconfirmed_read(),
            Some(&commitment())
        ),
        ClaimDecision::AwaitConfirmation,
        "present but unconfirmed settles nothing"
    );
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[attempt(Some(AttemptOutcome::Accepted))],
            live(),
            &confirmed_read(),
            Some(&commitment())
        ),
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        },
    );
}

#[test]
fn a_confirmed_entry_with_no_id_is_refused_rather_than_dropped() {
    // The same rule `reconcile::candidate_of` and the controller's
    // `benchmark_entries` apply to this collection, and for the reason
    // `candidate_of` gives: the record nobody can read might be the pool's
    // own. Dropping it turns "confirmed" into "not confirmed", and a
    // commitment would then wait on a read that has in fact settled it —
    // issue #13's unbounded wait, reached by a bug rather than by TIG.
    let err = ConfirmedBenchmarks::from_read(&[
        json!({ "id": "bench_a", "state": { "block_confirmed": 42 } }),
        json!({ "state": { "block_confirmed": 43 } }),
    ])
    .expect_err("a confirmed entry that names nothing is unreadable");
    assert_eq!(err, ConfirmedReadError::NoId { index: 1 });

    // An *unconfirmed* entry with no id is not an error: §7 draws nothing
    // from it either way, so the pool has no claim on its shape.
    let ok = ConfirmedBenchmarks::from_read(&[
        json!({ "id": "bench_a", "state": { "block_confirmed": 42 } }),
        json!({ "state": { "block_confirmed": serde_json::Value::Null } }),
    ])
    .expect("an unconfirmed entry is not the pool's business");
    assert_eq!(ok, confirmed_read());
}

#[test]
fn an_intent_with_no_body_this_pass_is_skipped_not_alarmed() {
    // The driver holds one commitment and the pass claims every claimable
    // benchmark intent, so "no bytes for this one" is the ordinary case. It
    // is a `Skip`, which `needs_operator` does not count — §10.3's rule is
    // that a bucket filling on every pass is one nobody reads.
    //
    // `PayloadNotTheRecordedOne` stays reserved for a body that is present
    // and disagrees with the digest, which is a genuine integrity alarm.
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            None
        ),
        ClaimDecision::Skip {
            reason: SkipReason::NoBuiltPayload
        }
    );
}

#[test]
fn a_body_for_another_benchmark_is_an_ordinary_skip_not_an_alarm() {
    // What two live workflows actually produce. The driver holds one built
    // commitment and the pass claims every claimable benchmark intent, so the
    // second intent is handed the first one's body — present, but plainly not
    // its own.
    //
    // That is the same ordinary case as no body at all, and it must not page:
    // `architecture.md` §13 invariant 4 guarantees the second intent's own
    // payload exists, so nothing is corrupt, and §10.3's discipline is that an
    // alarm raised on every pass is one nobody reads.
    let other_benchmark = BenchmarkSubmission {
        benchmark_id: "bench_b".to_string(),
        merkle_root: "ab".repeat(32),
        solution_quality: vec![1, 2, 3, 4],
    };
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&other_benchmark)
        ),
        ClaimDecision::Skip {
            reason: SkipReason::NoBuiltPayload
        },
        "a body built for another benchmark is not this intent's body"
    );
}

#[test]
fn a_body_that_is_not_this_intents_never_becomes_an_attempt() {
    // §7.3's binding digest, and the reason it is checked before the
    // decision rather than only at the send: `send_benchmark` refuses the
    // same mismatch, but only once `begin_fenced` has written an attempt
    // row. The only way to close that row is REJECTED, which every later
    // pass reads as `Refused` — TIG answered and said no. It did not, and
    // `mining_system.md` §8 forbids recording a pool fault as TIG's.
    //
    // The driver carries one commitment across every claimable benchmark
    // intent, so this is what a second intent is handed in practice.
    // Names *this* benchmark, and still digests differently. Nothing
    // legitimate produces it: `0015` admits one commitment per benchmark, so
    // two renderings of one benchmark's bytes is a genuine disagreement.
    let same_benchmark_other_bytes = BenchmarkSubmission {
        benchmark_id: "bench_a".to_string(),
        merkle_root: "cd".repeat(32),
        solution_quality: vec![1, 2, 3, 4],
    };
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&same_benchmark_other_bytes)
        ),
        ClaimDecision::StopForOperator {
            reason: StopReason::PayloadNotTheRecordedOne
        }
    );
}

#[test]
fn a_wrong_body_does_not_disturb_a_decision_that_never_reads_one() {
    // The digest check sits just before `Transmit` and not where the
    // precommit path puts it, because only that branch touches the body. A
    // settled intent, a direct reconciliation and a wait all reason over the
    // intent's own `benchmark_id`; stopping those for an operator would
    // raise an alarm about a body none of them would have used.
    let other = BenchmarkSubmission {
        benchmark_id: "bench_a".to_string(),
        merkle_root: "cd".repeat(32),
        solution_quality: vec![1, 2, 3, 4],
    };
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Confirmed),
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&other)
        ),
        ClaimDecision::Skip {
            reason: SkipReason::AlreadySettled
        }
    );
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[],
            live(),
            &confirmed_read(),
            Some(&other)
        ),
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
}

#[test]
fn a_confirmed_benchmark_is_reconciled_directly_and_never_resent() {
    // §10: "For benchmark and proof writes, `benchmark_id` makes
    // reconciliation direct." No tuple search, because the write named its
    // benchmark in the request.
    for attempts in [
        vec![],
        vec![attempt(None)],
        vec![attempt(Some(AttemptOutcome::Ambiguous))],
        vec![attempt(Some(AttemptOutcome::Accepted))],
    ] {
        assert_eq!(
            decide_benchmark(
                &benchmark_intent(IntentState::Prepared),
                &attempts,
                live(),
                &confirmed_read(),
                Some(&commitment())
            ),
            ClaimDecision::AlreadyConfirmed {
                benchmark_id: "bench_a".to_string()
            },
            "{attempts:?}"
        );
    }
}

#[test]
fn a_write_in_flight_waits_for_the_read_whatever_the_response_said() {
    // §11 forbids a second concurrent write for one benchmark, and §7 makes
    // the confirmation a read. An ACCEPTED attempt waits exactly as an
    // unresolved one does: the commitment stands at TIG or it does not, and
    // the response is not what says so.
    for outcome in [
        None,
        Some(AttemptOutcome::Ambiguous),
        Some(AttemptOutcome::Accepted),
    ] {
        assert_eq!(
            decide_benchmark(
                &benchmark_intent(IntentState::Prepared),
                &[attempt(outcome)],
                live(),
                &ConfirmedBenchmarks::default(),
                Some(&commitment())
            ),
            ClaimDecision::AwaitConfirmation,
            "{outcome:?}"
        );
    }
}

#[test]
fn a_refused_commitment_is_not_searched_for_and_not_resent() {
    // TIG answered and refused, so no benchmark was created. What the
    // workflow is owed is `fail`, not another write.
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[attempt(Some(AttemptOutcome::Rejected))],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&commitment())
        ),
        ClaimDecision::Skip {
            reason: SkipReason::Refused
        }
    );
}

#[test]
fn reconciliation_comes_before_terminality() {
    // A workflow that ended while its write was in flight still has an
    // unresolved attempt, and only settling it says what became of the write.
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[attempt(None)],
            ended(),
            &confirmed_read(),
            Some(&commitment())
        ),
        ClaimDecision::AlreadyConfirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
    // With nothing in flight, the ended workflow gets no write.
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::Prepared),
            &[],
            ended(),
            &ConfirmedBenchmarks::default(),
            Some(&commitment())
        ),
        ClaimDecision::Skip {
            reason: SkipReason::WorkflowEnded { state: "EXPIRED" }
        }
    );
}

#[test]
fn an_unknown_outcome_with_no_attempt_stops_rather_than_sending() {
    // §7.3 writes both halves in one transaction, so one without the other
    // is a contradiction — and sending on the missing half pays a second fee
    // for a commitment that may already stand.
    assert_eq!(
        decide_benchmark(
            &benchmark_intent(IntentState::OutcomeUnknown),
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&commitment())
        ),
        ClaimDecision::StopForOperator {
            reason: StopReason::UnknownOutcomeWithNoAttempt
        }
    );
}

#[test]
fn a_settled_intent_owes_nothing() {
    for state in [IntentState::Confirmed, IntentState::Rejected] {
        assert_eq!(
            decide_benchmark(
                &benchmark_intent(state),
                &[],
                live(),
                &confirmed_read(),
                Some(&commitment())
            ),
            ClaimDecision::Skip {
                reason: SkipReason::AlreadySettled
            },
            "{state:?}"
        );
    }
}

#[test]
fn another_kind_of_intent_is_not_this_paths() {
    let mut precommit = benchmark_intent(IntentState::Prepared);
    precommit.write_kind = WriteKind::Precommit;
    precommit.benchmark_id = None;
    assert!(matches!(
        decide_benchmark(
            &precommit,
            &[],
            live(),
            &ConfirmedBenchmarks::default(),
            Some(&commitment()),
        ),
        ClaimDecision::Skip {
            reason: SkipReason::NotAPrecommit { .. }
        }
    ));
}
