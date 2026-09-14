//! What the gateway does with a claimed write intent
//! (`architecture.md` §5.1 step 5).
//!
//! §5.1 step 5, in full: "TIG Gateway **claims** that intent, **reconciles
//! it**, submits at most one unresolved precommit in the serialized lane, and
//! records the attempt and response." The middle clause is the one that costs
//! money to get wrong, and it is why this is not simply "pick up an intent and
//! send it": §7.3 has the gateway reconcile against confirmed TIG state before
//! any retry and "never blindly duplicate the write".
//!
//! [`decide`] is pure over records already read, for the same reason
//! [`crate::reconcile`] is: the decision it makes — whether this write may go
//! out — is the one that licenses or forbids paying a second fee and creating
//! a second benchmark, and it should be testable against constructed records
//! rather than only against a chain that happens to be in the right state.
//!
//! It never resends on its own initiative. Every path that cannot *prove* the
//! write is absent stops instead, because the two errors are not symmetric: a
//! stopped workflow costs an operator's attention and is visible, while a
//! duplicated precommit costs a fee, creates a benchmark the pool cannot
//! attribute, and breaks §10's permanent one-benchmark-per-workflow mapping.

use pool_workflow::{
    AttemptOutcome, BenchmarkSubmission, IntentState, WriteAttempt, WriteIntent, WriteKind,
};

use crate::reconcile::{PrecommitSubmission, ReconcileError, Reconciliation, reconcile_precommit};

/// What the gateway should do with one claimed intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimDecision {
    /// Send it. Nothing has reached TIG for this intent.
    Transmit,
    /// §10's search found the write already confirmed at TIG.
    ///
    /// Carries the `benchmark_id` the lost response would have returned —
    /// which is the whole point of the search, since `tig_integration.md` §6.1
    /// returns it in a response body and nothing durable holds it.
    /// The intent settles `CONFIRMED` from this; it is **not** resent.
    AlreadyConfirmed { benchmark_id: String },
    /// A candidate matches and has not confirmed. Wait for the read, do not
    /// resend: §7 makes confirmation a read and not a response.
    AwaitConfirmation,
    /// §10's stop-for-operator. The gateway does not guess.
    StopForOperator { reason: StopReason },
    /// Nothing owed. Not a failure.
    Skip { reason: SkipReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Another generation of this workflow's precommit has already been
    /// transmitted.
    ///
    /// §7.3 forbids a new generation "once an earlier attempt may have reached
    /// TIG, unless reconciliation proves it safe", so this state should not
    /// arise — `admit_precommit` refuses a generation once a precommit has
    /// been transmitted. It can still be reached by creating two generations
    /// while both are unsent and then sending the older one, and the answer is
    /// an operator rather than a guess about which generation TIG now holds.
    SiblingGenerationTransmitted { generation: i32 },
    /// The submission handed in does not reproduce the payload this intent
    /// recorded.
    ///
    /// §7.3 binds an intent to a canonical payload by digest. A reconstructed
    /// submission that drifted from it would make §10's search look for a
    /// write this intent never described, and could bind another workflow's
    /// confirmed benchmark to it.
    PayloadNotTheRecordedOne,
    /// The intent records `OUTCOME_UNKNOWN` and no attempt exists.
    ///
    /// §7.3 writes that state in the same transaction that marks an attempt
    /// `AMBIGUOUS`, so one without the other is a contradiction in the
    /// durable record. Transmitting on the strength of the missing half would
    /// turn the pool's own record of "a write may have reached TIG" into a
    /// resend.
    UnknownOutcomeWithNoAttempt,
    /// More than one confirmed precommit matches the exact submitted tuple.
    ///
    /// §10 names this explicitly. Picking the confirmed one, or the newest,
    /// would attribute a benchmark — and its fees and rewards — to a workflow
    /// that may not own it.
    MultipleCandidates { candidates: Vec<String> },
    /// A write may have reached TIG and the search cannot find it.
    ///
    /// Absence from the window is not proof of absence from TIG: §10 step 3
    /// searches the latest 120-block window, and a precommit older than that
    /// is missing from it for reasons that have nothing to do with whether it
    /// was accepted. `reconcile` says so in as many words — `NoCandidate` is
    /// "not, on its own, a licence to resend".
    ///
    /// What *would* license one is evidence that the attempt is recent enough
    /// to be inside the window it was searched against. Establishing that is
    /// E4 and D3's, and until it exists this stops rather than guessing in the
    /// direction that pays a second fee.
    WriteUnaccountedFor,
    /// A record could not be read at all.
    Unreadable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Not a precommit.
    ///
    /// §10's tuple search is defined over a precommit's submitted settings; a
    /// benchmark or proof write reconciles by `benchmark_id` instead (§7),
    /// which is a different search this function does not implement. Answered
    /// here rather than left to a caller to remember.
    NotAPrecommit { kind: &'static str },
    /// TIG answered and refused.
    ///
    /// A definitive negative, not an ambiguity. `pool_workflow`'s attempt
    /// ledger already states the rule this follows: a refused write created no
    /// benchmark, so there is nothing for §10's search to find, and putting it
    /// through the search would fill the stop-for-operator bucket that only
    /// works while it stays quiet. What such a workflow is owed is `fail`.
    Refused,
    /// A newer generation of this workflow's precommit exists and nothing has
    /// been transmitted.
    ///
    /// §7.3 makes a new generation the way to change a canonical payload, so
    /// the newest is the pool's current decision and the older one is not to
    /// be sent. Without this, two generations created while both were unsent
    /// would each see an empty attempt list and each transmit — two fees and
    /// two benchmarks for one workflow.
    SupersededByNewerGeneration { newest: i32 },
    /// The intent is already `CONFIRMED` or `REJECTED`.
    ///
    /// §7.3 makes both terminal, so there is no write left to make.
    AlreadySettled,
    /// No built body was supplied for this intent's benchmark.
    ///
    /// §3 keeps commitment construction out of the gateway, so a pass is
    /// handed bodies rather than building them — and a driver holding one
    /// commitment claims every claimable benchmark intent, so the intents it
    /// has no bytes for are the ordinary case, not a fault.
    ///
    /// Deliberately **not** `PayloadNotTheRecordedOne`, which means a body
    /// was present and disagreed with the intent's digest — a payload
    /// integrity alarm. Reporting "no body this pass" as that alarm would
    /// raise it for every other intent on every pass, and §10.3's discipline
    /// is that a bucket which fills on every pass is one nobody reads. The
    /// next pass carrying the right body resolves this with no operator
    /// involved.
    NoBuiltPayload,
    /// The workflow this intent belongs to has ended.
    ///
    /// Issue #86: an expiry or a restart leaves a `PREPARED` intent behind,
    /// and nothing else stops it being transmitted later. `architecture.md`
    /// §13 invariant 14 forbids a restart silently producing a stray TIG
    /// write, and this is where that is enforced — the workflow row already
    /// knows whether its write is still wanted, and the gateway holds `SELECT`
    /// on it for exactly this purpose.
    ///
    /// Sending anyway would pay a fee for work the pool has written off, and
    /// close §10's single unresolved-precommit lane on a write nobody can
    /// reconcile onto a live workflow.
    WorkflowEnded { state: &'static str },
}

/// What `decide` needs to know about the workflow that owns the intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwningWorkflow {
    /// `WorkflowState::as_str` of the current state.
    pub state: &'static str,
    /// `WorkflowState::is_terminal`.
    ///
    /// Taken as an answer rather than recomputed from the string: which states
    /// are terminal is `pool_workflow`'s to say, and a second list here is a
    /// rule stated twice.
    pub is_terminal: bool,
}

/// What `decide` needs to know about this workflow's *other* precommit
/// generations.
///
/// The danger `decide` guards is workflow-scoped, not intent-scoped: two
/// unsent generations each see an empty attempt list of their own, so an
/// intent-scoped decision transmits both. `admit_precommit` allows two unsent
/// generations for one `DECIDED` workflow — it refuses only once a precommit
/// has been transmitted — so this is reachable and not hypothetical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiblingGenerations {
    /// The highest precommit generation recorded for this workflow.
    pub newest_generation: i32,
    /// Whether a precommit attempt exists on any generation **other than**
    /// this intent's.
    ///
    /// `begin` records an attempt before the request leaves, so this is the
    /// same "did anything reach TIG" question `attempt::has_transmitted_
    /// precommit_write` asks, scoped to siblings.
    pub sibling_transmitted: bool,
}

/// Decide what to do with one claimed intent.
///
/// `confirmed_precommits` is the `precommits` array of a `get-benchmarks`
/// response — §5's latest 120-block window. It is consulted only when
/// reconciliation is actually owed, so a caller with nothing to reconcile need
/// not have fetched it.
pub fn decide(
    intent: &WriteIntent,
    attempts: &[WriteAttempt],
    workflow: OwningWorkflow,
    siblings: SiblingGenerations,
    submitted: &PrecommitSubmission,
    confirmed_precommits: &[serde_json::Value],
) -> ClaimDecision {
    if intent.write_kind != WriteKind::Precommit {
        return ClaimDecision::Skip {
            reason: SkipReason::NotAPrecommit {
                kind: intent.write_kind.as_str(),
            },
        };
    }
    // Attempts that belong to some other intent would answer a question about
    // a write this one did not make. `PrecommitTransmitter::send` refuses the
    // same mismatch; a decision built on it would be worse, because it decides
    // whether to send at all.
    if let Some(stray) = attempts.iter().find(|a| a.intent_id != intent.intent_id) {
        return ClaimDecision::StopForOperator {
            reason: StopReason::Unreadable {
                reason: format!(
                    "attempt {} belongs to intent {}, not {}",
                    stray.attempt_id, stray.intent_id, intent.intent_id
                ),
            },
        };
    }
    // §7.3's binding digest, checked here and not only at the send. The
    // search below runs over `submitted`'s exact settings, so a submission
    // that has drifted from what this intent recorded would search for a
    // different write — and could bind another workflow's confirmed
    // benchmark to this one, which is the mis-attribution §10 stops for and
    // a permanent wrong owner under §6. `transmit::send` refuses the same
    // mismatch, but that guard protects only the path that sends.
    let digest = crate::transmit::precommit_digest(submitted);
    if digest != intent.payload_digest {
        return ClaimDecision::StopForOperator {
            reason: StopReason::PayloadNotTheRecordedOne,
        };
    }
    if matches!(intent.state, IntentState::Confirmed | IntentState::Rejected) {
        return ClaimDecision::Skip {
            reason: SkipReason::AlreadySettled,
        };
    }

    // Something may be in flight for *this* intent. That is reconciled before
    // anything else is considered — including whether the workflow has ended.
    //
    // The ordering is the fix for a deadlock, not a preference.
    // `migrations/0004`'s unresolved-precommit index is network-wide, so an
    // unsettled attempt closes §10's lane for every workflow in the pool. A
    // workflow can end with one outstanding: `expire_if_due` refuses to, but
    // `fail` and the operator `expire` carry no such guard. Skipping on
    // terminality first would leave that ambiguity unreconciled for ever and
    // the lane shut behind it.
    //
    // None of the reconciliation answers transmits, so reconciling a workflow
    // that has ended is safe. What it settles is the *write*, so the lane
    // reopens; advancing the workflow is the controller's, from confirmed
    // reads, and §10 reports a contradiction rather than acting on one.
    let unsettled = attempts
        .iter()
        .any(|a| a.is_unresolved() || a.outcome == Some(AttemptOutcome::Accepted));
    if unsettled {
        return reconciled(submitted, confirmed_precommits);
    }

    // Every attempt was definitively refused. Not an ambiguity, and putting it
    // through §10's search would find nothing and report that nothing as a
    // write that may have reached TIG.
    if !attempts.is_empty() {
        return ClaimDecision::Skip {
            reason: SkipReason::Refused,
        };
    }

    // From here nothing was ever sent for this intent.
    if workflow.is_terminal {
        return ClaimDecision::Skip {
            reason: SkipReason::WorkflowEnded {
                state: workflow.state,
            },
        };
    }
    if intent.state == IntentState::OutcomeUnknown {
        return ClaimDecision::StopForOperator {
            reason: StopReason::UnknownOutcomeWithNoAttempt,
        };
    }
    // Supersession first. Once the newest generation has been sent, the older
    // one is still PREPARED and still claimable, and it must read as ordinary
    // supersession on every scan — not as a stop. The stop below is for the
    // state §7.3 forbids, an *older* generation having been sent under a
    // newer one, and reaching it through the normal path would fill the
    // bucket that only works while it stays quiet.
    if intent.generation < siblings.newest_generation {
        return ClaimDecision::Skip {
            reason: SkipReason::SupersededByNewerGeneration {
                newest: siblings.newest_generation,
            },
        };
    }
    if siblings.sibling_transmitted {
        return ClaimDecision::StopForOperator {
            reason: StopReason::SiblingGenerationTransmitted {
                generation: intent.generation,
            },
        };
    }

    ClaimDecision::Transmit
}

/// §7.3's "reconcile before any retry", and §10's tuple search — the only
/// caller of it that is not a test.
fn reconciled(
    submitted: &PrecommitSubmission,
    confirmed_precommits: &[serde_json::Value],
) -> ClaimDecision {
    match reconcile_precommit(confirmed_precommits, submitted) {
        Ok(Reconciliation::Confirmed { benchmark_id }) => {
            ClaimDecision::AlreadyConfirmed { benchmark_id }
        }
        Ok(Reconciliation::PendingConfirmation) => ClaimDecision::AwaitConfirmation,
        Ok(Reconciliation::StopForOperator { candidates }) => ClaimDecision::StopForOperator {
            reason: StopReason::MultipleCandidates { candidates },
        },
        Ok(Reconciliation::NoCandidate) => ClaimDecision::StopForOperator {
            reason: StopReason::WriteUnaccountedFor,
        },
        Err(ReconcileError::Shape { index, reason }) => ClaimDecision::StopForOperator {
            reason: StopReason::Unreadable {
                reason: format!("precommit at index {index}: {reason}"),
            },
        },
    }
}

/// A `get-benchmarks.benchmarks` entry the pool cannot read.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ConfirmedReadError {
    /// §7 says this entry is confirmed, but it does not say what.
    #[error("benchmark at index {index} is confirmed but carries no `id`")]
    NoId { index: usize },
}

/// The benchmarks §7 says are confirmed, and nothing else.
///
/// `decide_benchmark` used to take `&[String]`, which left the caller to
/// remember the test — and a `Vec<String>` of precommit ids, or of every
/// benchmark in the read regardless of state, would have type-checked. §7's
/// rule is a **non-null `state.block_confirmed`**, not membership, and a
/// commitment settled on mere membership would be settled on evidence TIG
/// has not given.
///
/// The only constructor runs the test, so holding one of these is the proof.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfirmedBenchmarks(Vec<String>);

impl ConfirmedBenchmarks {
    /// From `get-benchmarks.benchmarks`, keeping the entries §7 confirms.
    ///
    /// A confirmed entry with no `id` is an error, not a silent drop. That
    /// matches the other two readers of the same collection —
    /// `reconcile::candidate_of` and the controller's `benchmark_entries`
    /// both refuse a record they cannot read — and for the reason
    /// `candidate_of` states: the unreadable record might be the pool's own,
    /// so dropping it turns "confirmed" into "not confirmed". Here that
    /// would leave a commitment waiting on a read that has in fact settled
    /// it, which is the unbounded wait of issue #13 arrived at by a bug
    /// rather than by TIG.
    ///
    /// An *unconfirmed* entry with no id is not an error: §7 draws nothing
    /// from it either way, and the pool has no claim on its shape.
    pub fn from_read(benchmarks: &[serde_json::Value]) -> Result<Self, ConfirmedReadError> {
        let mut ids = Vec::new();
        for (index, entry) in benchmarks.iter().enumerate() {
            if !pool_workflow::block_confirmed(entry) {
                continue;
            }
            let id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or(ConfirmedReadError::NoId { index })?;
            ids.push(id.to_string());
        }
        Ok(Self(ids))
    }

    fn contains(&self, benchmark_id: &str) -> bool {
        self.0.iter().any(|id| id == benchmark_id)
    }
}

/// What a benchmark commitment intent is owed (`tig_integration.md` §6.2).
///
/// A separate judgement from [`decide`], because §10 makes the two kinds
/// reconcile differently and pretending otherwise would be the bug: "For
/// benchmark and proof writes, `benchmark_id` makes reconciliation direct. A
/// lost precommit HTTP response is harder because the client may not know the
/// generated ID." A commitment already names its benchmark in the request, so
/// there is no tuple search, no multi-candidate stop, and nothing for the
/// response to teach the pool.
///
/// It is also not in the precommit lane. §10's single unresolved request is
/// about precommits — the write whose identity is unknown until it answers —
/// and §11's rule for the rest is narrower: "never send two concurrent writes
/// for the same benchmark", which is what the intent's own attempts say.
///
/// `confirmed_benchmarks` is a [`ConfirmedBenchmarks`], which is §7's test
/// already applied — an entry in `get-benchmarks.benchmarks` with a non-null
/// `state.block_confirmed`. Membership in it is the evidence, and its absence
/// is not evidence of anything.
pub fn decide_benchmark(
    intent: &WriteIntent,
    attempts: &[WriteAttempt],
    owning: OwningWorkflow,
    confirmed_benchmarks: &ConfirmedBenchmarks,
    submission: Option<&BenchmarkSubmission>,
) -> ClaimDecision {
    if intent.write_kind != WriteKind::Benchmark {
        return ClaimDecision::Skip {
            reason: SkipReason::NotAPrecommit {
                kind: intent.write_kind.as_str(),
            },
        };
    }
    let Some(benchmark_id) = intent.benchmark_id.clone() else {
        // §7.3 binds a benchmark write's generation to its benchmark, and
        // `NewIntent` refuses one without it. A row here without it is a
        // record the pool cannot act on rather than one to guess at.
        return ClaimDecision::StopForOperator {
            reason: StopReason::Unreadable {
                reason: "a benchmark intent with no benchmark_id".to_string(),
            },
        };
    };

    // Settled first: §7.3 makes CONFIRMED and REJECTED terminal, and a
    // settled intent owes nothing whatever the reads say.
    if matches!(intent.state, IntentState::Confirmed | IntentState::Rejected) {
        return ClaimDecision::Skip {
            reason: SkipReason::AlreadySettled,
        };
    }

    // The direct reconciliation §10 promises, and it comes before terminality
    // for the reason the precommit path's does: a workflow that ended while
    // its write was in flight still has an unresolved attempt, and only
    // settling it says what became of the write.
    if confirmed_benchmarks.contains(&benchmark_id) {
        return ClaimDecision::AlreadyConfirmed { benchmark_id };
    }

    // TIG answered and refused. Nothing was created, so there is nothing to
    // reconcile and nothing to resend — what the workflow is owed is `fail`.
    if !attempts.is_empty()
        && attempts
            .iter()
            .all(|a| a.outcome == Some(AttemptOutcome::Rejected))
    {
        return ClaimDecision::Skip {
            reason: SkipReason::Refused,
        };
    }

    // A write is out and TIG has not confirmed it. §11: never a second
    // concurrent write for one benchmark, and §7 makes the confirmation a
    // read rather than a response — so this waits, whatever the response
    // said. An ACCEPTED attempt waits here just as an unresolved one does:
    // the commitment exists at TIG or it does not, and only the read says.
    //
    // **This wait is unbounded, and that is a known gap — issue #13.** An
    // AMBIGUOUS attempt for a write TIG never applied waits for a read that
    // will never name it, while `workflow::transition` refuses to expire the
    // workflow past an unsettled attempt. Fail-closed is right — §10 forbids
    // a blind resubmission, and a commitment reconciles directly, so absence
    // from the read is not the evidence a tuple search's `NoCandidate` is —
    // but nothing yet says the wait has gone on too long. `architecture.md`
    // §10.3 already asks for that alert; it needs a target block time the
    // pool does not configure yet.
    if attempts
        .iter()
        .any(|a| a.is_unresolved() || a.outcome == Some(AttemptOutcome::Accepted))
    {
        return ClaimDecision::AwaitConfirmation;
    }

    // The intent's own record says a write may have reached TIG while no
    // attempt row says so. §7.3 writes both in one transaction, so one
    // without the other is a contradiction — and transmitting on the missing
    // half would turn "may have been sent" into "send again", paying a second
    // fee for a commitment that may already stand.
    if intent.state == IntentState::OutcomeUnknown {
        return ClaimDecision::StopForOperator {
            reason: StopReason::UnknownOutcomeWithNoAttempt,
        };
    }

    // Only now does terminality matter: nothing is in flight, so a workflow
    // the pool has written off gets no write (issue #86, invariant 14).
    if owning.is_terminal {
        return ClaimDecision::Skip {
            reason: SkipReason::WorkflowEnded {
                state: owning.state,
            },
        };
    }

    // **Not checked here: §7.3's workflow-scoped half.** `decide` consults
    // `SiblingGenerations` because a precommit workflow can hold several
    // generations, and a newer one must not send while an older one may have
    // reached TIG. This reasons only over the intent's own attempts.
    //
    // That is sound only while one benchmark generation can exist, and today
    // exactly one can: `create_commitment_intent` writes `generation: 1`,
    // nothing else writes a benchmark intent, and asking twice returns the
    // same row rather than a second one. A superseding generation is D3's
    // rule and D3 has not landed.
    //
    // **D3 must add the sibling read here**, because the per-benchmark
    // unresolved index does not cover this: once the first generation's
    // attempt is ACCEPTED it is resolved, the index stops blocking, and a
    // second generation would decide `Transmit` and pay a second fee for one
    // benchmark. A `benchmark_siblings` analogue of `precommit_siblings`,
    // returning `SupersededByNewerGeneration` / `SiblingGenerationTransmitted`
    // before `Transmit`, is what that needs. Writing it now would be untested
    // against a state the schema cannot reach, which is how dead checks get
    // in — the premise is pinned by a test instead
    // (`a_second_commitment_intent_for_one_workflow_is_refused`).

    // §7.3's binding digest, checked before the decision to transmit and so
    // before any attempt row exists. `send_benchmark` refuses the same
    // mismatch, but only after `begin_fenced` has written the attempt — and
    // the caller then has an attempt it cannot settle except by closing it
    // REJECTED, which every later pass reads as `Refused`: TIG answered and
    // said no. It did not. `mining_system.md` §8 keeps a pool fault from
    // being recorded as TIG's, and a durable false refusal is exactly that.
    //
    // Checked here rather than where `decide` checks it, because only this
    // branch touches the body: a settled intent, a direct reconciliation and
    // a wait all reason over the intent's own `benchmark_id`, and stopping
    // those for an operator would raise an alarm about a body none of them
    // would have used.
    //
    // Absent and wrong are both "these are not this intent's bytes", and
    // neither may become an attempt — but they are not the same *report*.
    //
    // The driver holds one commitment and the pass claims every claimable
    // benchmark intent, so being handed no body for this one is the ordinary
    // case and the next pass with the right body settles it. A body that is
    // present and digests differently is not ordinary: it is the disagreement
    // §7.3's digest exists to catch, and it needs an operator.
    let Some(submission) = submission else {
        return ClaimDecision::Skip {
            reason: SkipReason::NoBuiltPayload,
        };
    };
    if crate::transmit::benchmark_digest(submission) != intent.payload_digest {
        return ClaimDecision::StopForOperator {
            reason: StopReason::PayloadNotTheRecordedOne,
        };
    }

    ClaimDecision::Transmit
}
