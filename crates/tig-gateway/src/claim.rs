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

use pool_workflow::{AttemptOutcome, IntentState, WriteAttempt, WriteIntent, WriteKind};

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
    if siblings.sibling_transmitted {
        return ClaimDecision::StopForOperator {
            reason: StopReason::SiblingGenerationTransmitted {
                generation: intent.generation,
            },
        };
    }
    if intent.generation < siblings.newest_generation {
        return ClaimDecision::Skip {
            reason: SkipReason::SupersededByNewerGeneration {
                newest: siblings.newest_generation,
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
