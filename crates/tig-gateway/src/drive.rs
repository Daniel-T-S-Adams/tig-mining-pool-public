//! The gateway's claim loop (`architecture.md` §5.1 step 5): claim, decide,
//! act, record.
//!
//! [`crate::claim::decide`] is the judgement and this is the hands. It reads
//! what `decide` needs, takes the lease §7.5 names for the precommit lane,
//! and does exactly what the decision says — which for every answer but
//! [`ClaimDecision::Transmit`] is either nothing or settling an attempt. The
//! send is the one effect that costs money, and it is reached by one path.
//!
//! What this does not do is advance a workflow or settle an intent to
//! `CONFIRMED`. Those come from confirmed reads and are the controller
//! reconciler's (§6, "advance confirmed TIG lifecycle"). When §10's search
//! finds a lost write already confirmed, this settles the **attempt** — which
//! is what reopens the lane — and leaves the workflow for the next
//! reconciliation pass to advance from the same evidence.

use pool_domain::{Network, TraceId};
use pool_workflow::{
    AttemptError, AttemptOutcome, BenchmarkSubmission, IntentError, LeaseError, LeaseKind,
    PostgresAttemptLedger, PostgresIntentRepository, PrecommitSubmission, TigWriteIntentRepository,
    WriteAttemptLedger, WriteIntent, WriteKind, lease, payload_inputs, precommit_siblings,
    workflow,
};
use sqlx::PgPool;

use crate::claim::{
    ClaimDecision, ConfirmedBenchmarks, OwningWorkflow, SiblingGenerations, SkipReason, decide,
    decide_benchmark,
};
use crate::credential::TigApiKey;
use crate::lane::PostLane;
use crate::transmit::{PrecommitTransmitter, TransmitError};
use crate::write_gate::{Blocked, WriteGate};

/// Everything one run needs, borrowed.
pub struct Driver<'a> {
    /// The gateway's own pool: `SELECT` on workflow and decision, `INSERT` and
    /// `UPDATE` on attempts and leases. Nothing here needs the controller's
    /// role, and nothing here may have it.
    pub pool: &'a PgPool,
    pub network: Network,
    /// The pool's identity on this endpoint, from `[tig].player_id`.
    pub player_id: &'a str,
    /// Who holds the lease. Stable across a process's life, distinct across
    /// processes, so a fence lost to a restart names its new holder.
    pub lease_owner: &'a str,
    /// How long a claim holds the workflow. Must exceed the write policy's
    /// call timeout — `run_once` refuses otherwise — because the lease is
    /// what keeps a second claimant off an attempt whose sender is still
    /// waiting on TIG. A lease that expired mid-call would let the next
    /// claimant find the attempt unresolved, take it for a crash, and write
    /// an outcome the live sender is about to contradict.
    pub lease_secs: i64,
    pub transmitter: &'a PrecommitTransmitter,
    /// §11's POST-lane pacing, shared across runs. See [`PostLane`].
    pub lane: &'a PostLane,
    /// The §6.2 body the artifact worker built, for the benchmark pass.
    ///
    /// `None` for a driver that only runs the precommit pass. The gateway
    /// never constructs one (`architecture.md` §3) — it is handed the bytes
    /// and refuses them unless they digest to what the intent recorded, which
    /// is what `migrations/0015`'s durable record makes checkable.
    ///
    /// One body rather than a map because slice 1 drives one commitment at a
    /// time; the slice that reads derived payloads from the artifact store
    /// (§9) replaces this with that read.
    pub commitment: Option<&'a BenchmarkSubmission>,
    /// The `WRITE_READY` gate, consulted **per write**. Holding one permit for
    /// the driver's life would consult it once ever: §10.3 says losing
    /// readiness at runtime blocks *new* writes, and criterion B3 tests that,
    /// so the permit is taken inside the Transmit branch and dropped when the
    /// response is recorded — which is also what keeps `in_flight`, the
    /// number §10.3's alert reports, true.
    pub gate: &'a WriteGate,
    pub key: &'a TigApiKey,
}

/// What happened to one claimable intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentOutcome {
    pub intent_id: String,
    pub workflow_id: String,
    pub generation: i32,
    /// The trace the controller admitted this intent under (`architecture.md`
    /// §10.1, criterion I3).
    ///
    /// Carried through the outcome rather than looked up when logging: the
    /// point of storing it is that the gateway may be a different process on
    /// the far side of a restart, and by the time a line is written the only
    /// place that trace exists is the row this intent was read from.
    pub trace_id: Option<TraceId>,
    pub decision: Option<ClaimDecision>,
    pub acted: Acted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acted {
    /// One HTTP attempt was made and its outcome recorded.
    Transmitted {
        attempt_id: String,
        outcome: AttemptOutcome,
        /// TIG's assigned id on an accepted precommit. Returned to the caller
        /// and not persisted here: §7 makes confirmation a read, and the
        /// controller binds the id when it reads it.
        benchmark_id: Option<String>,
    },
    /// §10's search found the lost write confirmed; the unresolved attempt is
    /// settled so the lane reopens.
    AttemptSettled {
        attempt_id: String,
        benchmark_id: String,
    },
    /// §10's search found the write confirmed, but its attempt is younger
    /// than a write's call timeout, so the sender may still be waiting on
    /// the response and about to record it. Nothing is written over it;
    /// the next run, once the attempt has aged past the timeout, settles
    /// it if the sender did not.
    AwaitingSender { attempt_id: String },
    /// The decision required no effect.
    Nothing,
    /// Another holder has the lease, or this holder's fence lapsed between
    /// claiming and recording. Not an error: §7.5's fence is doing its job,
    /// and the next run will find it released or expired.
    LeaseHeldElsewhere,
    /// §10's lane already holds an unresolved precommit for this network.
    ///
    /// Ordinary contention, not an operator condition: while one workflow's
    /// precommit is legitimately awaiting confirmation every other claimable
    /// intent decides Transmit and is refused here, on every pass, and a
    /// bucket that fills on every pass is one nobody reads.
    LaneOccupied,
    /// `WRITE_READY` was lost (§10.3). No write is made; the revocation is
    /// what the readiness check reports, not this run. Carries §10.3's
    /// category so the report can say which kind of operator action it is.
    WriteBlocked { category: &'static str },
    /// The intent could not be evaluated. Collected, not propagated: the run
    /// continues to the next intent, and the operator sees the whole run.
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub outcomes: Vec<IntentOutcome>,
}

/// How much attention an outcome is owed (`architecture.md` §10.1, §10.3).
///
/// One classification, used both to raise the run's report and to pick a log
/// level, so the two cannot come to different conclusions about the same
/// outcome — a pass that logged at `warn` while reporting "nothing to see"
/// would teach an operator to ignore the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notability {
    /// A person has to decide something. §10.3's bucket, and the one that
    /// must stay empty on an ordinary pass to be worth reading.
    Operator,
    /// The pool changed what TIG holds, or settled a write that had. Rare,
    /// costs a fee, and is the record §10's search reads on the next crash.
    Effect,
    /// Contention the next pass resolves by itself.
    Routine,
}

impl IntentOutcome {
    /// The attempt this outcome touched, where it touched one.
    pub fn attempt_id(&self) -> Option<&str> {
        match &self.acted {
            Acted::Transmitted { attempt_id, .. }
            | Acted::AttemptSettled { attempt_id, .. }
            | Acted::AwaitingSender { attempt_id } => Some(attempt_id),
            _ => None,
        }
    }

    /// TIG's id for the benchmark, once something has established it —
    /// a response on [`Acted::Transmitted`], §10's search on the other two.
    pub fn benchmark_id(&self) -> Option<&str> {
        match (&self.acted, &self.decision) {
            (Acted::Transmitted { benchmark_id, .. }, _) => benchmark_id.as_deref(),
            (Acted::AttemptSettled { benchmark_id, .. }, _) => Some(benchmark_id),
            (_, Some(ClaimDecision::AlreadyConfirmed { benchmark_id })) => Some(benchmark_id),
            _ => None,
        }
    }

    pub fn notability(&self) -> Notability {
        // `LaneOccupied`, `LeaseHeldElsewhere` and `WriteBlocked` are Routine
        // on purpose: each is an ordinary condition the next pass resolves on
        // its own, and a bucket that fills on every pass is one nobody reads
        // (§10.3). `WriteBlocked` in particular is a revocation the readiness
        // check reports — this run is only observing the closed gate, and
        // raising it here would raise it once per claimable intent per pass.
        if matches!(self.decision, Some(ClaimDecision::StopForOperator { .. }))
            || matches!(self.acted, Acted::Failed { .. })
        {
            return Notability::Operator;
        }
        match self.acted {
            Acted::Transmitted { .. } | Acted::AttemptSettled { .. } => Notability::Effect,
            _ => Notability::Routine,
        }
    }
}

impl RunReport {
    /// Whether any intent stopped for an operator.
    pub fn needs_operator(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| o.notability() == Notability::Operator)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error(transparent)]
    Intent(#[from] IntentError),
    /// The driver's lease would expire before a write's call timeout. See
    /// `Driver::lease_secs`.
    #[error(
        "lease of {lease_secs}s does not outlast the write call timeout of {call_timeout_secs}s;          a claimant could take over an attempt whose sender is still waiting on TIG"
    )]
    LeaseShorterThanCall {
        lease_secs: i64,
        call_timeout_secs: u64,
    },
}

/// One pass over every claimable precommit intent.
///
/// `confirmed_precommits` is the `precommits` array of a `get-benchmarks`
/// response, read by the caller: fetching is the read-client layer's, and
/// threading it in here would put network I/O inside the claim.
pub async fn run_once(
    driver: &Driver<'_>,
    confirmed_precommits: &[serde_json::Value],
) -> Result<RunReport, DriveError> {
    check_lease(driver)?;
    let intents = PostgresIntentRepository::new(driver.pool.clone());
    let claimable = intents
        .claimable(driver.network, WriteKind::Precommit)
        .await?;

    let mut outcomes = Vec::with_capacity(claimable.len());
    for intent in claimable {
        let outcome = match handle(driver, &intents, &intent, confirmed_precommits).await {
            Ok((decision, acted)) => IntentOutcome {
                intent_id: intent.intent_id.clone(),
                workflow_id: intent.workflow_id.clone(),
                generation: intent.generation,
                trace_id: intent.trace_id,
                decision,
                acted,
            },
            // Fail closed for this intent, not for the run.
            Err(e) => IntentOutcome {
                intent_id: intent.intent_id.clone(),
                workflow_id: intent.workflow_id.clone(),
                generation: intent.generation,
                trace_id: intent.trace_id,
                decision: None,
                acted: Acted::Failed {
                    error: e.to_string(),
                },
            },
        };
        outcomes.push(outcome);
    }
    Ok(RunReport { outcomes })
}

/// §7.5: the lease must outlast any call made under it, or the next claimant
/// could take over an attempt whose sender is still waiting on TIG.
///
/// Public because the comparison is a fact about configuration, not about any
/// particular pass: `service::run` applies it once at startup so a gateway
/// whose lease is too short exits instead of failing every pass for ever
/// (`architecture.md` §9). Both callers must reach the same verdict on the
/// same pair, so there is one comparison rather than two that could drift —
/// the equal case in particular is a failure, since a lease that expires at
/// the instant a call times out leaves the takeover window open.
pub fn lease_outlasts_call(lease_secs: i64, call_timeout_secs: u64) -> Result<(), DriveError> {
    if u64::try_from(lease_secs).is_ok_and(|lease| lease > call_timeout_secs) {
        return Ok(());
    }
    Err(DriveError::LeaseShorterThanCall {
        lease_secs,
        call_timeout_secs,
    })
}

fn check_lease(driver: &Driver<'_>) -> Result<(), DriveError> {
    lease_outlasts_call(
        driver.lease_secs,
        driver.lane.policy().call_timeout().as_secs(),
    )
}

/// One pass over every claimable benchmark commitment intent
/// (`tig_integration.md` §6.2).
///
/// A separate pass from [`run_once`] because §10 reconciles the two kinds
/// differently — see [`decide_benchmark`] — and because they do not share a
/// lane: §10's single unresolved request is the precommit's, and §11's rule
/// for the rest is "never send two concurrent writes for the same
/// benchmark", which the intent's own attempts answer.
///
/// They do share the POST lane's pacing: §11's minimum gap between initial
/// writes is per IP, not per write kind, so both passes take a slot from the
/// same [`PostLane`].
///
/// `confirmed_benchmarks` is the set of `benchmark_id`s the caller has read
/// as confirmed (§7: a `get-benchmarks.benchmarks` entry with a non-null
/// `state.block_confirmed`).
pub async fn run_once_benchmarks(
    driver: &Driver<'_>,
    confirmed_benchmarks: &ConfirmedBenchmarks,
) -> Result<RunReport, DriveError> {
    check_lease(driver)?;
    let intents = PostgresIntentRepository::new(driver.pool.clone());
    let claimable = intents
        .claimable(driver.network, WriteKind::Benchmark)
        .await?;

    let mut outcomes = Vec::with_capacity(claimable.len());
    for intent in claimable {
        let outcome = match handle_benchmark(driver, &intent, confirmed_benchmarks).await {
            Ok((decision, acted)) => IntentOutcome {
                intent_id: intent.intent_id.clone(),
                workflow_id: intent.workflow_id.clone(),
                generation: intent.generation,
                trace_id: intent.trace_id,
                decision,
                acted,
            },
            // Fail closed for this intent, not for the run.
            Err(e) => IntentOutcome {
                intent_id: intent.intent_id.clone(),
                workflow_id: intent.workflow_id.clone(),
                generation: intent.generation,
                trace_id: intent.trace_id,
                decision: None,
                acted: Acted::Failed {
                    error: e.to_string(),
                },
            },
        };
        outcomes.push(outcome);
    }
    Ok(RunReport { outcomes })
}

async fn handle_benchmark(
    driver: &Driver<'_>,
    intent: &WriteIntent,
    confirmed_benchmarks: &ConfirmedBenchmarks,
) -> Result<(Option<ClaimDecision>, Acted), HandleError> {
    // The same lease the precommit path takes, for the same reason: §7.5
    // makes the fence — not the reads — what decides who may commit.
    //
    // Deliberately the *same* kind, not a benchmark-specific one. The lease
    // is per (network, workflow, kind), so one kind across both writes is
    // what stops two gateway passes working the same workflow at once; a
    // second kind would let a precommit and a commitment for one workflow
    // run concurrently, each holding a fence the other does not see.
    //
    // The name is therefore narrower than the meaning — it now protects
    // every TIG transmit for a workflow, not only the precommit. Renaming it
    // to something write-agnostic needs a migration (`0009` constrains
    // `lease_kind` to a known set), so it is owed as its own change rather
    // than smuggled into a review fix.
    let held = match lease::claim(
        driver.pool,
        driver.network,
        &intent.workflow_id,
        LeaseKind::PrecommitTransmit,
        driver.lease_owner,
        driver.lease_secs,
    )
    .await
    {
        Ok(lease) => lease,
        Err(LeaseError::StillHeld { .. }) => return Ok((None, Acted::LeaseHeldElsewhere)),
        Err(e) => return Err(e.into()),
    };
    let result = handle_benchmark_held(driver, intent, confirmed_benchmarks, &held).await;
    if let Err(e) = lease::release(driver.pool, &held).await {
        tracing::warn!(
            event = "gateway.lease.release_failed",
            workflow_id = %intent.workflow_id,
            error = %e,
            "lease not released; it will expire"
        );
    }
    result
}

async fn handle_benchmark_held(
    driver: &Driver<'_>,
    intent: &WriteIntent,
    confirmed_benchmarks: &ConfirmedBenchmarks,
    held: &pool_workflow::Lease,
) -> Result<(Option<ClaimDecision>, Acted), HandleError> {
    let ledger = PostgresAttemptLedger::new(driver.pool.clone());
    let owning = workflow::find(driver.pool, driver.network, &intent.workflow_id)
        .await?
        .ok_or_else(|| HandleError::NoWorkflow {
            intent_id: intent.intent_id.clone(),
            workflow_id: intent.workflow_id.clone(),
        })?;
    let attempts = ledger.attempts_for(&intent.intent_id).await?;

    let decision = decide_benchmark(
        intent,
        &attempts,
        OwningWorkflow {
            state: owning.state.as_str(),
            is_terminal: owning.state.is_terminal(),
        },
        confirmed_benchmarks,
        driver.commitment,
    );

    let acted = match &decision {
        ClaimDecision::Transmit => {
            // The body the artifact worker built. The gateway does not
            // construct it (§3: "constructing commitments or proofs" is not
            // the gateway's).
            //
            // `decide_benchmark` has already established that this body is
            // present and digests to the intent's record, so reaching
            // `Transmit` without one is a contradiction in this module rather
            // than a state the pool can be in — hence an error, not an
            // attempt row.
            let Some(submission) = driver.commitment.as_ref() else {
                return Err(HandleError::NoCommitment {
                    intent_id: intent.intent_id.clone(),
                });
            };
            let permit = match driver.gate.begin_write() {
                Ok(permit) => permit,
                Err(Blocked { revocation }) => {
                    return Ok((
                        Some(decision),
                        Acted::WriteBlocked {
                            category: revocation.category(),
                        },
                    ));
                }
            };
            driver.lane.take_slot().await;
            let attempt = match ledger.begin_fenced(&intent.intent_id, held).await {
                Ok(attempt) => attempt,
                // §11's per-benchmark rule, which is this write kind's
                // contention and not a fault: `0004`'s unresolved index is
                // per benchmark for a commitment, and `PrecommitLaneOccupied`
                // cannot fire here at all — that index is
                // `WHERE write_kind = 'precommit'`. Reported as ordinary
                // contention so it does not fill the operator bucket on every
                // pass while a write for the same benchmark is legitimately
                // out.
                Err(AttemptError::BenchmarkWriteInFlight { .. }) => {
                    return Ok((Some(decision), Acted::LaneOccupied));
                }
                Err(AttemptError::FenceLost { .. }) => {
                    return Ok((Some(decision), Acted::LeaseHeldElsewhere));
                }
                Err(e) => return Err(e.into()),
            };
            let sent = match driver
                .transmitter
                .send_benchmark(&permit, driver.key, intent, &attempt, submission)
                .await
            {
                Ok(sent) => sent,
                Err(refused) => {
                    // Nothing left, and the attempt row exists. Closed for
                    // the reason the precommit path closes its own: an
                    // attempt with no outcome is one nothing can settle.
                    if let Err(e) = ledger
                        .resolve(
                            &attempt.attempt_id,
                            AttemptOutcome::Rejected,
                            None,
                            Some("not sent: refused before the request left"),
                        )
                        .await
                    {
                        tracing::warn!(
                            event = "gateway.attempt.release_failed",
                            attempt_id = %attempt.attempt_id,
                            error = %e,
                        );
                    }
                    return Err(refused.into());
                }
            };
            ledger
                .resolve(
                    &attempt.attempt_id,
                    sent.outcome,
                    sent.http_status,
                    Some(&sent.detail),
                )
                .await?;
            drop(permit);
            Acted::Transmitted {
                attempt_id: attempt.attempt_id,
                outcome: sent.outcome,
                benchmark_id: sent.benchmark_id,
            }
        }
        ClaimDecision::AlreadyConfirmed { benchmark_id } => {
            // Direct reconciliation: the write named this benchmark, and the
            // read says it confirmed. Settle the attempt so nothing waits on
            // it; the workflow advances from the same read on the
            // controller's next pass.
            match attempts.iter().find(|a| a.is_unresolved()) {
                // The same guard the precommit path applies, for the same
                // reason: a NULL outcome younger than the call timeout may
                // belong to a sender still waiting on TIG. Writing a
                // fabricated ambiguity over a response about to be recorded
                // loses the real status, and the real sender's `resolve`
                // then fails as already-resolved and is reported as a
                // failure. The lease should make that impossible — a run
                // requires it to outlast a call — but the ledger's clock is
                // the cheaper witness.
                Some(a)
                    if a.outcome.is_none()
                        && a.age_secs
                            < i64::try_from(call_timeout_secs(driver)).unwrap_or(i64::MAX) =>
                {
                    Acted::AwaitingSender {
                        attempt_id: a.attempt_id.clone(),
                    }
                }
                Some(a) => {
                    if a.outcome.is_none() {
                        ledger
                            .resolve(
                                &a.attempt_id,
                                AttemptOutcome::Ambiguous,
                                None,
                                Some("response never recorded; found confirmed by §7's read"),
                            )
                            .await?;
                    }
                    ledger
                        .reconcile(&a.attempt_id, AttemptOutcome::Accepted)
                        .await?;
                    Acted::AttemptSettled {
                        attempt_id: a.attempt_id.clone(),
                        benchmark_id: benchmark_id.clone(),
                    }
                }
                None => Acted::Nothing,
            }
        }
        ClaimDecision::Skip {
            reason: SkipReason::NoBuiltPayload,
        } => {
            // The one skip worth a record. It is ordinary for a pass — the
            // driver holds one body and claims every claimable intent — but
            // an intent that is *never* handed its body waits forever, and
            // `needs_operator` ignores `Skip` by design. Until §10.3's
            // age-based alert exists (issue #13), a line carrying the three
            // ids is the only thing that would let an operator notice.
            //
            // At `debug`, deliberately: it fires on every pass for every
            // intent this pass has no bytes for, which is the volume that
            // makes a higher level useless. The alert is what has to key on
            // age; this only has to make the age discoverable.
            tracing::debug!(
                event = "gateway.benchmark.no_built_payload",
                workflow_id = %intent.workflow_id,
                intent_id = %intent.intent_id,
                benchmark_id = intent.benchmark_id.as_deref().unwrap_or(""),
                "no built commitment for this intent this pass"
            );
            Acted::Nothing
        }
        ClaimDecision::AwaitConfirmation
        | ClaimDecision::StopForOperator { .. }
        | ClaimDecision::Skip { .. } => Acted::Nothing,
    };
    Ok((Some(decision), acted))
}

#[derive(Debug, thiserror::Error)]
enum HandleError {
    #[error(transparent)]
    Intent(#[from] IntentError),
    #[error(transparent)]
    Attempt(#[from] AttemptError),
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error(transparent)]
    Workflow(#[from] workflow::WorkflowError),
    #[error(transparent)]
    Admission(#[from] pool_workflow::AdmissionError),
    #[error(transparent)]
    Payload(#[from] pool_workflow::PayloadError),
    #[error(transparent)]
    Transmit(#[from] TransmitError),
    #[error("intent {intent_id} has no decision record for generation {generation}")]
    NoDecision { intent_id: String, generation: i32 },
    #[error("intent {intent_id} belongs to workflow {workflow_id}, which does not exist")]
    NoWorkflow {
        intent_id: String,
        workflow_id: String,
    },
    /// A commitment is due and the driver was handed no body to send.
    ///
    /// The gateway does not build one: `architecture.md` §3 keeps
    /// "constructing commitments or proofs" out of it, and §9 grants it
    /// read-only access to derived payloads. A driver asked to run the
    /// benchmark pass without the payload has nothing legitimate to send.
    #[error("intent {intent_id} is due a commitment and the driver holds no built payload")]
    NoCommitment { intent_id: String },
}

async fn handle(
    driver: &Driver<'_>,
    intents: &PostgresIntentRepository,
    intent: &WriteIntent,
    confirmed_precommits: &[serde_json::Value],
) -> Result<(Option<ClaimDecision>, Acted), HandleError> {
    // The lease first. Everything below reads state that a concurrent claimant
    // could be changing, and §7.5 makes the fence — not the reads — what
    // decides who may commit.
    let held = match lease::claim(
        driver.pool,
        driver.network,
        &intent.workflow_id,
        LeaseKind::PrecommitTransmit,
        driver.lease_owner,
        driver.lease_secs,
    )
    .await
    {
        Ok(lease) => lease,
        Err(LeaseError::StillHeld { .. }) => return Ok((None, Acted::LeaseHeldElsewhere)),
        Err(e) => return Err(e.into()),
    };

    let result = handle_held(driver, intents, intent, confirmed_precommits, &held).await;
    // Released on every path, including failure: a lease left behind by a
    // failed handle would hold the workflow until it expired for no reason.
    // A release that itself fails is not worth failing the handle over — the
    // lease expires — but it is not worth hiding either.
    if let Err(e) = lease::release(driver.pool, &held).await {
        tracing::warn!(
            event = "gateway.lease.release_failed",
            workflow_id = %intent.workflow_id,
            error = %e,
            "lease not released; it will expire"
        );
    }
    result
}

async fn handle_held(
    driver: &Driver<'_>,
    _intents: &PostgresIntentRepository,
    intent: &WriteIntent,
    confirmed_precommits: &[serde_json::Value],
    held: &pool_workflow::Lease,
) -> Result<(Option<ClaimDecision>, Acted), HandleError> {
    let ledger = PostgresAttemptLedger::new(driver.pool.clone());

    // What `decide` needs, read under the lease.
    let owning = workflow::find(driver.pool, driver.network, &intent.workflow_id)
        .await?
        .ok_or_else(|| HandleError::NoWorkflow {
            intent_id: intent.intent_id.clone(),
            workflow_id: intent.workflow_id.clone(),
        })?;
    let attempts = ledger.attempts_for(&intent.intent_id).await?;
    let siblings = precommit_siblings(driver.pool, intent).await?;
    let inputs = payload_inputs(
        driver.pool,
        driver.network,
        &intent.workflow_id,
        intent.generation,
    )
    .await?
    .ok_or_else(|| HandleError::NoDecision {
        intent_id: intent.intent_id.clone(),
        generation: intent.generation,
    })?;
    let submission = PrecommitSubmission::from_decision(driver.player_id, &inputs)?;

    let decision = decide(
        intent,
        &attempts,
        OwningWorkflow {
            state: owning.state.as_str(),
            is_terminal: owning.state.is_terminal(),
        },
        SiblingGenerations {
            newest_generation: siblings.newest_generation,
            sibling_transmitted: siblings.sibling_transmitted,
        },
        &submission,
        confirmed_precommits,
    );

    let acted = match &decision {
        ClaimDecision::Transmit => {
            // §7.3: the attempt is recorded before the request leaves, and
            // under the fence, so a claimant whose lease lapsed cannot record
            // one (§12). The lane index refuses a second unresolved precommit
            // network-wide; both are the database's decision, not this
            // code's.
            // §10.3: readiness is consulted for every write. A gate revoked
            // since the last one blocks this one, and says why.
            let permit = match driver.gate.begin_write() {
                Ok(permit) => permit,
                Err(Blocked { revocation }) => {
                    return Ok((
                        Some(decision),
                        Acted::WriteBlocked {
                            category: revocation.category(),
                        },
                    ));
                }
            };
            // §11's gap between initial writes, taken after the permit and
            // before the attempt row, so a paced write still records its
            // attempt immediately before the request leaves (§7.3).
            driver.lane.take_slot().await;
            // Both of the refusals `begin_fenced` can raise are ordinary —
            // the lane is the database serializing precommits and the fence is
            // it serializing claimants — so neither is reported as a failure.
            let attempt = match ledger.begin_fenced(&intent.intent_id, held).await {
                Ok(attempt) => attempt,
                Err(AttemptError::PrecommitLaneOccupied { .. }) => {
                    return Ok((Some(decision), Acted::LaneOccupied));
                }
                Err(AttemptError::FenceLost { .. }) => {
                    return Ok((Some(decision), Acted::LeaseHeldElsewhere));
                }
                Err(e) => return Err(e.into()),
            };
            let sent =
                send_or_release(driver, &ledger, &permit, intent, &attempt, &submission).await?;
            // Recorded separately from the attempt (§7.3). If the process
            // dies between the send and this line, the attempt stays pending
            // and the next run reconciles it — never resends it.
            ledger
                .resolve(
                    &attempt.attempt_id,
                    sent.outcome,
                    sent.http_status,
                    Some(&sent.detail),
                )
                .await?;
            // The permit's drop is what decrements `in_flight`; it happens
            // here, after the response is on record, and not before.
            drop(permit);
            Acted::Transmitted {
                attempt_id: attempt.attempt_id,
                outcome: sent.outcome,
                benchmark_id: sent.benchmark_id,
            }
        }
        ClaimDecision::AlreadyConfirmed { benchmark_id } => {
            // The lost write landed. Settle the unresolved attempt so the
            // lane reopens; the intent and workflow advance from the same
            // confirmed read on the controller's next pass.
            //
            // Two shapes of "unresolved", and they settle differently.
            // AMBIGUOUS is a recorded lost response and `reconcile` settles
            // it. A NULL outcome is §12's crash between `begin` and `resolve`
            // — the response was never recorded at all — and `reconcile`
            // refuses it as not ambiguous, because it is not: it is nothing.
            // So the missing record is written first, as the ambiguity it was
            // at the time (which also moves the intent to OUTCOME_UNKNOWN, as
            // §7.3 has `resolve` do), and then settled. Without that step this
            // attempt would be refused on every run and §10's lane would stay
            // closed for the whole pool.
            let unresolved = attempts.iter().find(|a| a.is_unresolved());
            match unresolved {
                // A NULL outcome younger than the call timeout may belong to
                // a sender still waiting on TIG. The lease should make that
                // impossible — `run_once` requires it to outlast a call — but
                // the ledger's own clock is the cheaper witness, and writing
                // a fabricated ambiguity over a response about to be recorded
                // would lose the real status and report the real sender as
                // failed.
                Some(a)
                    if a.outcome.is_none()
                        && a.age_secs
                            < i64::try_from(call_timeout_secs(driver)).unwrap_or(i64::MAX) =>
                {
                    Acted::AwaitingSender {
                        attempt_id: a.attempt_id.clone(),
                    }
                }
                Some(a) => {
                    if a.outcome.is_none() {
                        ledger
                            .resolve(
                                &a.attempt_id,
                                AttemptOutcome::Ambiguous,
                                None,
                                Some("response never recorded; found confirmed by the §10 search"),
                            )
                            .await?;
                    }
                    ledger
                        .reconcile(&a.attempt_id, AttemptOutcome::Accepted)
                        .await?;
                    Acted::AttemptSettled {
                        attempt_id: a.attempt_id.clone(),
                        benchmark_id: benchmark_id.clone(),
                    }
                }
                // Accepted-but-unread: nothing to settle, the lane is open.
                None => Acted::Nothing,
            }
        }
        ClaimDecision::AwaitConfirmation
        | ClaimDecision::StopForOperator { .. }
        | ClaimDecision::Skip { .. } => Acted::Nothing,
    };
    Ok((Some(decision), acted))
}

fn call_timeout_secs(driver: &Driver<'_>) -> u64 {
    driver.lane.policy().call_timeout().as_secs()
}

/// Send, and if the transmitter refuses before anything leaves, close the
/// attempt it was handed.
///
/// `send` fails only on its own pre-flight checks — an attempt that is not
/// this intent's, a payload that does not hash to the recorded digest — and
/// in each case no request was made. The attempt row already exists by then
/// (§7.3 records it first), and an attempt left with no outcome holds §10's
/// lane for the whole pool until something settles it; the search cannot,
/// because nothing reached TIG for it to find. So the refusal is recorded as
/// the attempt's outcome — `REJECTED`, since no benchmark exists — and the
/// error still propagates, so the run reports the intent as failed and an
/// operator sees why nothing was sent.
async fn send_or_release(
    driver: &Driver<'_>,
    ledger: &PostgresAttemptLedger,
    permit: &crate::write_gate::WritePermit,
    intent: &WriteIntent,
    attempt: &pool_workflow::WriteAttempt,
    submission: &PrecommitSubmission,
) -> Result<crate::transmit::Transmitted, HandleError> {
    match driver
        .transmitter
        .send(permit, driver.key, intent, attempt, submission)
        .await
    {
        Ok(sent) => Ok(sent),
        Err(refused) => {
            // Bounded classification, never the error text: the column is
            // limited and the error names ids.
            let detail = match &refused {
                TransmitError::PayloadNotTheRecordedOne { .. } => {
                    "not sent: payload does not match the recorded digest"
                }
                TransmitError::AttemptNotForIntent { .. } => {
                    "not sent: attempt is not this intent's"
                }
                TransmitError::NotAPrecommitIntent { .. } => "not sent: not a precommit intent",
                TransmitError::AttemptAlreadyAnswered { .. } => {
                    "not sent: attempt already answered"
                }
                TransmitError::Client(_) => "not sent: no HTTP client",
            };
            if let Err(e) = ledger
                .resolve(
                    &attempt.attempt_id,
                    AttemptOutcome::Rejected,
                    None,
                    Some(detail),
                )
                .await
            {
                tracing::warn!(
                    event = "gateway.attempt.release_failed",
                    attempt_id = %attempt.attempt_id,
                    error = %e,
                    "an attempt nothing was sent for could not be closed; the lane stays held"
                );
            }
            Err(refused.into())
        }
    }
}

#[cfg(test)]
mod tests {
    //! The claim loop against a real database and a real fake-tig.
    //!
    //! **G2's four crash points**, each killing the process at a different
    //! moment and each asserting the fake's server-side write count, which is
    //! what turns "recovered" into "recovered without a duplicate":
    //!
    //! | `architecture.md` §12 row | test | count |
    //! |---|---|---|
    //! | dies after decision commit | `a_crash_after_the_decision_leaves_the_intent_claimable` | 1 |
    //! | dies after the attempt row, request never left | `a_crash_before_the_request_left_stops_rather_than_paying_twice` | 0 |
    //! | dies after the attempt row, request landed | `a_crash_before_the_response_was_recorded_is_recovered_and_the_lane_reopens` | 1 |
    //! | dies after TIG changed state, gateway half | `a_lost_response_is_recovered_by_search_and_the_lane_reopens` | 1 |
    //!
    //! The two middle rows leave *identical* durable state — an attempt with
    //! a NULL outcome — and that is the point: the record cannot say whether
    //! the request left. One recovers by finding the write, the other by
    //! refusing to guess, and neither sends a second time.
    //!
    //! **The fourth row is half covered here, deliberately.** §12's guarantee
    //! for it is "reconciliation advances monotonically from confirmed TIG
    //! evidence", and §6 makes that a *controller* transition. The test named
    //! above stages §10's lost response and settles the **attempt**, which is
    //! what reopens the lane; advancing the workflow from the same confirmed
    //! read is the reconciler's, and this crate cannot exercise it. That half
    //! is owed by a controller reconciliation test — see G2 in
    //! `docs/plans/slice-1-gateway.md`, which names it as outstanding rather
    //! than letting this table read as complete coverage.
    //!
    //! Inside the crate because a `TigApiKey` exists only through
    //! `credential::load`, which is crate-private on purpose. Requires
    //! `POOL_TEST_SUPERUSER_URL`; without it these skip.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use pool_domain::Network;
    use pool_test_support::TempDb;
    use pool_workflow::{
        AnchorSnapshot, DecisionPayloadInputs, IntentState, NewDecision, RecordedDraw,
        SettledOutcome, admit_precommit, precommit_digest,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::claim::{SkipReason, StopReason};
    use crate::credential;
    use crate::write_policy::WritePolicy;

    const PLAYER: &str = "0xp00l00000000000000000000000000000000000";
    const ANCHOR: &str = "block_100080";
    const ANCHOR_DIGEST: [u8; 32] = [0xcd; 32];

    /// A live fake-tig on a loopback socket, and its base URL.
    async fn fake_tig() -> String {
        let dir = format!("{}/../../fixtures/tig/v1", env!("CARGO_MANIFEST_DIR"));
        let world = fake_tig::build_world(fake_tig::Config::new(dir)).unwrap();
        let app = fake_tig::router(world);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn synthetic_key(label: &str) -> (TigApiKey, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tig-drive-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, format!("{}\n", fake_tig::DEFAULT_API_KEY)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (credential::load(&path).unwrap(), path)
    }

    fn gate() -> WriteGate {
        use crate::readiness::{
            ActiveChallengeRuntime, ApiKeyPlacement, Evidence, FixtureOutcome, ImageObservation,
            ModelValidation, OpenApiObservation, Pins, ResolvedImage, evaluate,
        };
        let pins = Pins {
            network: Network::Testnet,
            image_names: BTreeSet::from(["img".to_string()]),
            upstream_commit: "c".to_string(),
            image_digests: BTreeMap::from([("img".to_string(), "sha256:aa".to_string())]),
            platform: "linux/arm64".to_string(),
            openapi_sha256: "abc".to_string(),
            pool_player_id: PLAYER.to_string(),
        };
        let evidence = Evidence {
            config_network: Ok(Network::Testnet),
            upstream_commit: Ok(pins.upstream_commit.clone()),
            resolved_images: Ok(ImageObservation {
                resolved: Some(vec![ResolvedImage {
                    reference: "img".to_string(),
                    manifest_digest: "sha256:aa".to_string(),
                    platform: "linux/arm64".to_string(),
                }]),
                reviewed_unresolved: None,
            }),
            openapi: Ok(OpenApiObservation {
                hosted_sha256: Some("abc".to_string()),
                reviewed_local_override: None,
            }),
            response_models: Ok([
                "get-block",
                "get-challenges",
                "get-algorithms",
                "get-opow",
                "get-benchmarks",
            ]
            .iter()
            .map(|e| ModelValidation {
                endpoint: (*e).to_string(),
                valid: true,
                detail: String::new(),
            })
            .collect()),
            active_challenges: Ok(vec![ActiveChallengeRuntime {
                challenge_id: "c001".to_string(),
                runtime_pinned: true,
                compute_path_supported: true,
            }]),
            fixtures: Ok(FixtureOutcome {
                lossless_numeric_parsing: true,
                canonical_request_serialization: true,
                detail: String::new(),
            }),
            api_key: Ok(ApiKeyPlacement {
                present_in_gateway: true,
                readable_by_member_services: false,
            }),
            confirmed_pool_player_id: Ok(pins.pool_player_id.clone()),
        };
        let ready = evaluate(&pins, &evidence).unwrap();
        WriteGate::open(ready)
    }

    fn policy() -> WritePolicy {
        WritePolicy::from_config_json(include_str!("../../../config/tig_integration.json")).unwrap()
    }

    /// The shipped policy with the two durations a test has to wait out
    /// shortened to one second each: the call timeout, which is how long a
    /// NULL-outcome attempt is presumed to have a live sender, and the gap
    /// between initial writes. Everything else as shipped.
    fn fast_policy() -> WritePolicy {
        let mut root: Value =
            serde_json::from_str(include_str!("../../../config/tig_integration.json")).unwrap();
        root["write_limits"]["call_timeout_seconds"] = json!(1);
        root["write_limits"]["min_seconds_between_initial_writes"] = json!(1);
        WritePolicy::from_config_json(&root.to_string()).unwrap()
    }

    /// The decision the controller would admit for the fixture's c001/a011,
    /// with the digest computed the real way.
    fn decision(workflow: &str, generation: i32) -> NewDecision {
        use pool_domain::{challenge_tie_seed, draw_rank};
        let seed = challenge_tie_seed(Network::Testnet, ANCHOR);
        let mut draw_ranks = serde_json::Map::new();
        for c in ["c001", "c002", "c003"] {
            let rank = draw_rank(&seed, c);
            draw_ranks.insert(c.to_string(), json!(rank.to_hex()));
        }
        // The fake's fixture: c001 with tracks t001 and t002.
        let track_settings = json!({
            "t001": { "num_bundles": 2, "fuel_budget": 1_000_000u64, "hyperparameters": null },
            "t002": { "num_bundles": 1, "fuel_budget": 1_000_000u64, "hyperparameters": null }
        });
        let inputs = DecisionPayloadInputs {
            anchor_block_id: ANCHOR.to_string(),
            selected_challenge: "c001".to_string(),
            selected_algorithm: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings: track_settings.clone(),
        };
        let submission = PrecommitSubmission::from_decision(PLAYER, &inputs).unwrap();
        NewDecision {
            network: Network::Testnet,
            workflow_id: workflow.to_string(),
            generation,
            anchor: AnchorSnapshot {
                block_id: ANCHOR.to_string(),
                content_digest: ANCHOR_DIGEST,
                height: 100_080,
            },
            draw: RecordedDraw {
                domain: pool_domain::CHALLENGE_TIE_DOMAIN.to_string(),
                draw_ranks,
                tie: None,
            },
            selected_challenge: "c001".to_string(),
            selected_algorithm: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings,
            reserve_inputs: json!({}),
            precommit_reserve: "0".to_string(),
            config_digest: [0xef; 32],
            payload_digest: precommit_digest(&submission),
            trace_id: None,
        }
    }

    async fn persist_anchor(pool: &sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO pool.block_snapshot
                 (network, block_id, content_digest, height, reads_complete, active_cache_ready)
             VALUES ('testnet', $1, $2, 100080, true, true)",
        )
        .bind(ANCHOR)
        .bind(ANCHOR_DIGEST.as_slice())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn precommits_window(base: &str) -> Vec<Value> {
        let state = fake_state(base).await;
        state["benchmarks"]["precommits"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The fake's whole state, through the crate's own HTTP client.
    async fn fake_state(base: &str) -> Value {
        fake_get(base, "/_fake/state").await
    }

    async fn fake_get(base: &str, path: &str) -> Value {
        reqwest::Client::new()
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// A POST to the fake. The key, when one is needed, travels as a header
    /// through the client — never on a command line, where it would sit in
    /// argv for any process on the host to read. Harmless with the fake's
    /// public constant; the shape matters because K4 repeats a crash test
    /// against live testnet with a real credential.
    async fn fake_post(base: &str, path: &str, key: Option<&str>, body: Option<Value>) -> Value {
        let mut req = reqwest::Client::new().post(format!("{base}{path}"));
        if let Some(key) = key {
            req = req.header("X-Api-Key", key);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap().error_for_status().unwrap();
        let bytes = resp.bytes().await.unwrap();
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    struct Harness {
        _db: TempDb,
        controller: sqlx::PgPool,
        gateway: sqlx::PgPool,
        base: String,
        transmitter: PrecommitTransmitter,
        gate: WriteGate,
        key: TigApiKey,
        lane: PostLane,
        commitment: Option<BenchmarkSubmission>,
        _key_path: PathBuf,
    }

    impl Harness {
        async fn new(label: &str) -> Option<Self> {
            Self::with_policy(label, policy()).await
        }

        async fn with_policy(label: &str, policy: WritePolicy) -> Option<Self> {
            let db = TempDb::migrated(label).await?;
            let controller = db.pool_as("pool_controller").await;
            let gateway = db.pool_as("pool_gateway").await;
            persist_anchor(&controller).await;
            let base = fake_tig().await;
            let transmitter = PrecommitTransmitter::new(&base, &policy).unwrap();
            let (key, key_path) = synthetic_key(label);
            Some(Self {
                _db: db,
                controller,
                gateway,
                base,
                transmitter,
                gate: gate(),
                key,
                lane: PostLane::new(policy),
                commitment: None,
                _key_path: key_path,
            })
        }

        fn driver(&self) -> Driver<'_> {
            Driver {
                pool: &self.gateway,
                network: Network::Testnet,
                player_id: PLAYER,
                lease_owner: "gateway-test",
                // Above the shipped 60-second call timeout, as `run_once`
                // requires.
                lease_secs: 120,
                transmitter: &self.transmitter,
                lane: &self.lane,
                commitment: self.commitment.as_ref(),
                gate: &self.gate,
                key: &self.key,
            }
        }
    }

    #[tokio::test]
    async fn the_admitting_trace_reaches_the_gateway_through_the_row() {
        // Criterion I3 / `architecture.md` §10.1: "Durable jobs and intents
        // store the originating trace ID so work resumed after a restart
        // remains correlated." The gateway is a separate process from the
        // controller that admitted this intent, so the only path from one to
        // the other is the column — nothing in this call stack was present
        // when the decision was made.
        //
        // Asserted on the outcome the run loop logs from, not on the row,
        // because a value stored and never carried forward correlates nothing:
        // that is the failure this criterion is about.
        let Some(h) = Harness::new("drive_trace").await else {
            return;
        };
        let trace = TraceId::draw().unwrap();
        let mut decided = decision("w1", 1);
        decided.trace_id = Some(trace);
        admit_precommit(&h.controller, &decided, 4).await.unwrap();

        let report = run_once(&h.driver(), &[]).await.unwrap();
        assert_eq!(report.outcomes.len(), 1, "{report:?}");
        assert_eq!(
            report.outcomes[0].trace_id,
            Some(trace),
            "the intent's originating trace must reach the outcome the gateway logs"
        );
    }

    #[tokio::test]
    async fn an_intent_admitted_without_a_trace_is_still_driven() {
        // The column is nullable and §10.1 asks that the id be stored, not
        // that work be refused without one. A controller whose OS refused
        // randomness should lose correlation, not stop transmitting — so this
        // asserts the absence is carried as an absence and the send still
        // happens, rather than the intent being skipped or failing.
        let Some(h) = Harness::new("drive_no_trace").await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();

        let report = run_once(&h.driver(), &[]).await.unwrap();
        assert_eq!(report.outcomes.len(), 1, "{report:?}");
        assert_eq!(report.outcomes[0].trace_id, None);
        assert_eq!(report.outcomes[0].decision, Some(ClaimDecision::Transmit));
    }

    #[tokio::test]
    async fn a_prepared_intent_is_sent_once_and_its_attempt_recorded_around_the_send() {
        // §5.1 step 5's ordinary path: claim, decide Transmit, record the
        // attempt, send, record the response. The fake counts server-side
        // writes, which is the assertion G2 will later make under a crash and
        // the one that says "once" rather than "at least once".
        let Some(h) = Harness::new("drive_send").await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();

        let report = run_once(&h.driver(), &[]).await.unwrap();
        assert_eq!(report.outcomes.len(), 1, "{report:?}");
        let o = &report.outcomes[0];
        assert_eq!(o.decision, Some(ClaimDecision::Transmit));
        let Acted::Transmitted {
            outcome,
            benchmark_id,
            ..
        } = &o.acted
        else {
            panic!("{o:?}");
        };
        assert_eq!(*outcome, AttemptOutcome::Accepted);
        assert!(
            benchmark_id.is_some(),
            "an accepted precommit returns its id"
        );

        let state = fake_state(&h.base).await;
        assert_eq!(state["writes_received"]["submit-precommit"], json!(1));

        // A second run does not send again: the attempt is ACCEPTED, so
        // `decide` reconciles rather than transmits, and the fake now lists
        // the (unconfirmed) precommit — AwaitConfirmation.
        let window = precommits_window(&h.base).await;
        let again = run_once(&h.driver(), &window).await.unwrap();
        assert_eq!(
            again.outcomes[0].decision,
            Some(ClaimDecision::AwaitConfirmation),
            "{again:?}"
        );
        let state = fake_state(&h.base).await;
        assert_eq!(
            state["writes_received"]["submit-precommit"],
            json!(1),
            "still exactly one"
        );
    }

    #[tokio::test]
    async fn a_lost_response_is_recovered_by_search_and_the_lane_reopens() {
        // §10's case. The fake applies the write and then loses the response;
        // the attempt is AMBIGUOUS and the lane is closed. The next run must
        // find the write by its tuple, settle the attempt, and never resend.
        let Some(h) = Harness::new("drive_lost").await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();

        // Inject: apply, then fail the response.
        fake_post(
            &h.base,
            "/_fake/inject",
            None,
            Some(json!({"target": "submit-precommit", "mode": "ambiguous"})),
        )
        .await;

        let first = run_once(&h.driver(), &[]).await.unwrap();
        let Acted::Transmitted {
            outcome,
            attempt_id,
            ..
        } = &first.outcomes[0].acted
        else {
            panic!("{first:?}");
        };
        assert_eq!(*outcome, AttemptOutcome::Ambiguous);
        let state = fake_state(&h.base).await;
        assert_eq!(
            state["writes_received"]["submit-precommit"],
            json!(1),
            "the write landed"
        );

        // The search needs the confirmed window. Advance so the precommit
        // confirms, then run again with the real `precommits` array.
        fake_post(&h.base, "/_fake/advance-block", None, None).await;
        let window = precommits_window(&h.base).await;
        let second = run_once(&h.driver(), &window).await.unwrap();
        let o = &second.outcomes[0];
        assert!(
            matches!(o.decision, Some(ClaimDecision::AlreadyConfirmed { .. })),
            "{o:?}"
        );
        assert!(
            matches!(&o.acted, Acted::AttemptSettled { attempt_id: a, .. } if a == attempt_id),
            "the ambiguous attempt is what gets settled: {o:?}"
        );

        // Never resent.
        let state = fake_state(&h.base).await;
        assert_eq!(state["writes_received"]["submit-precommit"], json!(1));
        // And the lane is open again: a second workflow can now send.
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        let attempts = ledger
            .attempts_for(attempt_id_intent(&h.controller, "w1").await.as_str())
            .await
            .unwrap();
        assert!(attempts.iter().all(|a| !a.is_unresolved()), "{attempts:?}");
    }

    /// The benchmark intent's id. A workflow at this point owns two intents
    /// — its precommit and its commitment — so the kind has to be named.
    async fn benchmark_intent_id(pool: &sqlx::PgPool, workflow: &str) -> String {
        sqlx::query_scalar(
            "SELECT intent_id::text FROM pool.tig_write_intent
              WHERE workflow_id = $1 AND write_kind = 'benchmark'",
        )
        .bind(workflow)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn attempt_id_intent(pool: &sqlx::PgPool, workflow: &str) -> String {
        sqlx::query_scalar(
            "SELECT intent_id::text FROM pool.tig_write_intent WHERE workflow_id = $1",
        )
        .bind(workflow)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_stale_claimant_cannot_record_an_attempt() {
        // §12: "the write intent remains claimable; another controller uses
        // the higher lease fence." The protection is that the claimant whose
        // lease lapsed cannot record an attempt — the lane index would not
        // catch it once the newer holder's attempt has resolved.
        let Some(h) = Harness::new("drive_fence").await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();
        let intent_id = attempt_id_intent(&h.controller, "w1").await;

        // Gateway A claims, then loses the lease to B (a reclaim with a higher
        // fence, as an expired lease would allow).
        let stale = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "A",
            60,
        )
        .await
        .unwrap();
        pool_workflow::lease::release(&h.gateway, &stale)
            .await
            .unwrap();
        let _newer = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "B",
            60,
        )
        .await
        .unwrap();

        // A wakes up and tries to record an attempt with its old fence.
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        let error = ledger
            .begin_fenced(&intent_id, &stale)
            .await
            .expect_err("fence lost");
        assert!(matches!(error, AttemptError::FenceLost { .. }), "{error:?}");
        assert!(
            ledger.attempts_for(&intent_id).await.unwrap().is_empty(),
            "nothing recorded"
        );
    }

    #[tokio::test]
    async fn a_workflow_that_ended_is_skipped_and_nothing_is_sent() {
        let Some(h) = Harness::new("drive_ended").await else {
            return;
        };
        let admitted = admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();
        let w = pool_workflow::workflow::find(&h.controller, Network::Testnet, "w1")
            .await
            .unwrap()
            .unwrap();
        pool_workflow::workflow::fail(
            &h.controller,
            Network::Testnet,
            "w1",
            w.revision,
            "pool_bootstrap_abandoned",
            100_100,
        )
        .await
        .unwrap();

        let report = run_once(&h.driver(), &[]).await.unwrap();
        assert!(
            matches!(
                report.outcomes[0].decision,
                Some(ClaimDecision::Skip { .. })
            ),
            "{report:?}"
        );
        assert_eq!(report.outcomes[0].acted, Acted::Nothing);
        let state = fake_state(&h.base).await;
        assert_eq!(
            state["writes_received"].get("submit-precommit"),
            None,
            "no write for a dead workflow"
        );
        let _ = admitted;
    }

    #[test]
    fn settled_outcome_is_the_intent_ledgers_word_not_this_modules() {
        // Pinned so a future edit that has the driver settle an intent has to
        // touch this: §7.3 makes CONFIRMED come from a confirmed read, which
        // the controller holds and the gateway does not.
        let _ = IntentState::Confirmed;
        let _ = SettledOutcome::Rejected;
    }

    #[tokio::test]
    async fn a_crash_after_the_decision_leaves_the_intent_claimable() {
        // G2's first crash point, and `architecture.md` §12's row for it:
        // "Controller dies after decision commit — the write intent remains
        // claimable; another controller uses the higher lease fence."
        //
        // Staged by admitting the decision and then taking — and losing —
        // the lease under the dead process's name, which is what a process
        // that died holding one leaves behind. The row and its fence outlive
        // the process; the §12 row's "higher lease fence" is only meaningful
        // against that baseline, so it is established rather than assumed.
        //
        // A *different* holder then runs: the fence advances past the dead
        // process's, the write goes out once, and the count says once.
        let Some(h) = Harness::new("drive_crash_decision").await else {
            return;
        };
        let admitted = admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();
        assert_eq!(admitted.intent.state, IntentState::Prepared);

        // What the dead process held. Released rather than left to expire,
        // because the successor must advance the fence either way and this
        // keeps the test off the clock.
        let dead = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "gateway-dead",
            60,
        )
        .await
        .unwrap();
        pool_workflow::lease::release(&h.gateway, &dead)
            .await
            .unwrap();

        // The baseline is the row as the successor finds it, not the token
        // the dead process held: `release` advances the fence itself, so
        // `dead.fence_token` is already one behind and comparing against it
        // would hold no matter what the claim did.
        let baseline: i64 = sqlx::query_scalar(
            "SELECT fence_token FROM pool.work_lease
              WHERE network = 'testnet' AND workflow_id = 'w1'",
        )
        .fetch_one(&h.gateway)
        .await
        .unwrap();
        assert_eq!(
            fake_state(&h.base).await["writes_received"]
                .get("submit-precommit")
                .cloned()
                .unwrap_or(json!(0)),
            json!(0),
            "the dead process sent nothing"
        );

        // The successor, named differently so the fence it takes is its own.
        let successor = Driver {
            lease_owner: "gateway-successor",
            ..h.driver()
        };
        let report = run_once(&successor, &[]).await.unwrap();
        assert_eq!(report.outcomes.len(), 1, "{report:?}");
        assert!(
            matches!(
                &report.outcomes[0].acted,
                Acted::Transmitted {
                    outcome: AttemptOutcome::Accepted,
                    ..
                }
            ),
            "{report:?}"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-precommit"],
            json!(1),
            "exactly one write reached TIG"
        );

        // And the lease the successor took is recorded as its own, at a
        // fence that both the claim and the closing release advanced.
        //
        // What guards §12's "another controller uses the higher lease fence"
        // is `0009`'s trigger, not this line: a change of `lease_owner`
        // without an advancing fence is refused by the database, so a claim
        // that reused the dead process's token cannot be written at all. A
        // mutant that stops advancing on conflict fails this test through
        // that refusal, surfacing as a failed run rather than as a failed
        // assertion — which is the right place for the rule to live, since
        // it then holds for every writer and not only the tested path.
        //
        // What this line adds is the end state a correct hand-over leaves:
        // two advances, one for the claim and one for the release at the end
        // of `handle`. Hence `baseline + 1` — a run that claimed but never
        // released lands exactly on the bound and fails here.
        //
        // Recorded because two weaker forms shipped before this one and both
        // were vacuous: `fence >= 1`, which `0009`'s CHECK guarantees
        // outright, and `fence > dead.fence_token`, which the staged
        // `release` already satisfies before the successor runs at all.
        let (owner, fence): (String, i64) = sqlx::query_as(
            "SELECT lease_owner, fence_token FROM pool.work_lease
              WHERE network = 'testnet' AND workflow_id = 'w1'",
        )
        .fetch_one(&h.gateway)
        .await
        .unwrap();
        assert_eq!(owner, "gateway-successor");
        assert!(
            fence > baseline + 1,
            "the successor's claim must advance the fence: it stood at \
             {baseline} and ended at {fence}, which one release alone \
             accounts for"
        );
    }

    #[tokio::test]
    async fn a_crash_before_the_request_left_stops_rather_than_paying_twice() {
        // G2's crash point "after the attempt row but before the HTTP
        // response", in the half where the request never left at all. The
        // pool's durable state is identical to the half where it did — a
        // NULL outcome — which is the whole difficulty: the record cannot
        // distinguish "not sent" from "sent and unanswered".
        //
        // So the pool must not guess in the direction that pays. §10 refuses
        // a blind resubmission and `claim` stops for an operator when the
        // search accounts for nothing. The load-bearing assertion is the
        // write count: it stays at zero, and no second fee is paid on a
        // maybe.
        let Some(h) = Harness::with_policy("drive_crash_unsent", fast_policy()).await else {
            return;
        };
        let admitted = admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();

        // The crash: the attempt row exists under a fence, and nothing was
        // sent.
        let lease = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "crashed",
            60,
        )
        .await
        .unwrap();
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        ledger
            .begin_fenced(&admitted.intent.intent_id, &lease)
            .await
            .unwrap();
        pool_workflow::lease::release(&h.gateway, &lease)
            .await
            .unwrap();

        // The window has nothing: the write never happened. The run stops
        // instead of resending — and it does so whatever the attempt's age,
        // which is worth saying because an earlier version of this test slept
        // 1.5s first and described the wait as load-bearing.
        //
        // It is not. `decide` reaches `StopForOperator { WriteUnaccountedFor }`
        // from a no-candidate window without consulting a clock; the age guard
        // lives only in `handle_held`'s `AlreadyConfirmed` branch, which this
        // path never reaches. What keeps a still-live sender safe here is the
        // lease, which must outlast a call, not the attempt's age.
        let report = run_once(&h.driver(), &[]).await.unwrap();
        let o = &report.outcomes[0];
        assert!(
            matches!(
                &o.decision,
                Some(ClaimDecision::StopForOperator {
                    reason: StopReason::WriteUnaccountedFor
                })
            ),
            "{o:?}"
        );
        assert!(report.needs_operator(), "an operator is owed this one");
        assert_eq!(
            fake_state(&h.base).await["writes_received"]
                .get("submit-precommit")
                .cloned()
                .unwrap_or(json!(0)),
            json!(0),
            "nothing was sent, and nothing is sent now"
        );
    }

    #[tokio::test]
    async fn a_crash_before_the_response_was_recorded_is_recovered_and_the_lane_reopens() {
        // §12: "TIG Gateway dies before/after HTTP response: attempt stays
        // pending". G2's crash point "after the attempt row but before the
        // HTTP response" — except here the write *did* reach TIG, so the
        // next run must find it, settle the pending attempt, and reopen the
        // lane, without sending again.
        //
        // The attempt has a NULL outcome, not AMBIGUOUS: the response was
        // never recorded at all. `reconcile` alone refuses that, and a driver
        // that only reconciled would have been refused on every run with the
        // whole pool's lane closed behind it.
        //
        // Under the fast policy, so the attempt ages past the call timeout in
        // a second rather than a minute — the driver presumes a sender may
        // still be live until then.
        let Some(h) = Harness::with_policy("drive_crash_pending", fast_policy()).await else {
            return;
        };
        let admitted = admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();

        // The crash, staged: record the attempt under the fence, post the
        // body by hand so the write lands, and never record the response.
        let lease = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "crashed",
            60,
        )
        .await
        .unwrap();
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        let pending = ledger
            .begin_fenced(&admitted.intent.intent_id, &lease)
            .await
            .unwrap();
        pool_workflow::lease::release(&h.gateway, &lease)
            .await
            .unwrap();
        let body = pool_workflow::precommit_body(
            &PrecommitSubmission::from_decision(
                PLAYER,
                &payload_inputs(&h.gateway, Network::Testnet, "w1", 1)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap(),
        );
        fake_post(
            &h.base,
            "/submit-precommit",
            Some(fake_tig::DEFAULT_API_KEY),
            Some(body),
        )
        .await;
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-precommit"],
            json!(1)
        );

        // The lane is closed: a second workflow cannot begin.
        let other = admit_precommit(&h.controller, &decision("w2", 1), 4)
            .await
            .unwrap();
        assert!(matches!(
            ledger.begin(&other.intent.intent_id).await,
            Err(AttemptError::PrecommitLaneOccupied { .. })
        ));

        // TIG confirms; the search finds the write by its tuple. But the
        // attempt is seconds old: a sender with a 1-second call timeout could
        // still be about to record the real response, and a fabricated
        // ambiguity written over it would lose that and report the sender as
        // failed. So nothing is settled yet, and the lane stays closed.
        fake_post(&h.base, "/_fake/advance-block", None, None).await;
        let window = precommits_window(&h.base).await;
        let early = run_once(&h.driver(), &window).await.unwrap();
        let w1 = early
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w1")
            .unwrap();
        assert!(
            matches!(&w1.acted, Acted::AwaitingSender { attempt_id } if *attempt_id == pending.attempt_id),
            "too young to presume the sender dead: {w1:?}"
        );
        assert!(!early.needs_operator(), "{early:?}");
        assert!(matches!(
            ledger.begin(&other.intent.intent_id).await,
            Err(AttemptError::PrecommitLaneOccupied { .. })
        ));

        // Past the call timeout no sender can still be waiting. Now it
        // settles.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let report = run_once(&h.driver(), &window).await.unwrap();
        let w1 = report
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w1")
            .unwrap();
        assert!(
            matches!(&w1.acted, Acted::AttemptSettled { attempt_id, .. } if *attempt_id == pending.attempt_id),
            "the pending attempt is what settles: {w1:?}"
        );
        assert!(!report.needs_operator(), "{report:?}");

        // Never resent, and the lane is open: w2 was sent in the same pass.
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-precommit"],
            json!(2)
        );
        let w2 = report
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w2")
            .unwrap();
        assert!(matches!(w2.acted, Acted::Transmitted { .. }), "{w2:?}");
    }

    /// A workflow with a confirmed precommit, its preconditions recorded,
    /// and a commitment intent ready to send. Returns the body.
    async fn ready_to_commit(h: &Harness, workflow: &str) -> BenchmarkSubmission {
        use pool_workflow::{
            CommitmentPayload, PackageAcceptance, benchmark_digest, record_acceptance,
            record_commitment_payload,
        };
        // The real path: send the precommit, let the fake confirm it on the
        // next block, and take the benchmark id and `num_nonces` from what
        // TIG published — §6.2 fixes the quality vector's length at the
        // confirmed value, and inventing either would test a commitment TIG
        // has no benchmark for.
        admit_precommit(&h.controller, &decision(workflow, 1), 4)
            .await
            .unwrap();
        run_once(&h.driver(), &[]).await.unwrap();
        fake_post(&h.base, "/_fake/advance-block", None, None).await;

        // The confirmed entry no workflow has bound yet. Taking the first
        // confirmed one works for a single workflow and silently binds the
        // first workflow's benchmark to the second, which `0011`'s one-owner
        // rule then refuses — so the helper has to pick the pool's own
        // unclaimed one, the way §10's search does by settings.
        let bound: Vec<String> = sqlx::query_scalar(
            "SELECT benchmark_id FROM pool.workflow WHERE benchmark_id IS NOT NULL",
        )
        .fetch_all(&h.controller)
        .await
        .unwrap();
        let entry = precommits_window(&h.base)
            .await
            .into_iter()
            .find(|p| {
                p["state"]["block_confirmed"].is_number()
                    && p["benchmark_id"]
                        .as_str()
                        .is_some_and(|id| !bound.iter().any(|b| b == id))
            })
            .expect("the fake confirms on the next block");
        let benchmark_id = entry["benchmark_id"].as_str().unwrap().to_owned();
        let num_nonces = entry["details"]["num_nonces"].as_u64().unwrap();
        let current = workflow::find(&h.controller, Network::Testnet, workflow)
            .await
            .unwrap()
            .unwrap();
        workflow::confirm_precommit(
            &h.controller,
            Network::Testnet,
            workflow,
            current.revision,
            &workflow::ConfirmedPrecommit {
                benchmark_id: benchmark_id.clone(),
                track_id: entry["settings"]["track_id"].as_str().unwrap().to_owned(),
                // As TIG serves them: settings and details are disjoint, and
                // §6.2's length is a detail.
                settings: entry["settings"].clone(),
                num_nonces: Some(i64::try_from(num_nonces).unwrap()),
                num_bundles: None,
                block_confirmed: entry["state"]["block_confirmed"].as_i64().unwrap(),
                block_started: entry["details"]["block_started"].as_i64().unwrap(),
            },
        )
        .await
        .unwrap();

        let submission = BenchmarkSubmission {
            benchmark_id: benchmark_id.clone(),
            merkle_root: "ab".repeat(32),
            solution_quality: (0..i64::try_from(num_nonces).unwrap()).collect(),
        };
        let artifact_id = format!("artifact/{workflow}/commitment");
        record_acceptance(
            &h.controller,
            &PackageAcceptance {
                network: Network::Testnet,
                workflow_id: workflow.to_string(),
                benchmark_id: benchmark_id.clone(),
                package_sha256: [0x11; 32],
                stub_origin: false,
            },
        )
        .await
        .unwrap();
        record_commitment_payload(
            &h.controller,
            &CommitmentPayload {
                network: Network::Testnet,
                artifact_id: artifact_id.clone(),
                workflow_id: workflow.to_string(),
                benchmark_id: benchmark_id.clone(),
                payload_digest: benchmark_digest(&submission),
                stub_origin: false,
            },
        )
        .await
        .unwrap();
        PostgresIntentRepository::new(h.controller.clone())
            .create(pool_workflow::NewIntent {
                network: Network::Testnet,
                workflow_id: workflow.to_string(),
                write_kind: WriteKind::Benchmark,
                generation: 1,
                benchmark_id: Some(benchmark_id),
                payload_digest: benchmark_digest(&submission),
                payload_artifact_id: Some(artifact_id),
                trace_id: None,
            })
            .await
            .unwrap();
        submission
    }

    #[tokio::test]
    async fn a_commitment_is_sent_once_and_then_reconciled_by_its_benchmark_id() {
        // §6.2's write, and §10's direct reconciliation: the commitment names
        // its benchmark, so a confirmed read settles it with no tuple search
        // and no second send.
        let Some(mut h) = Harness::new("drive_commitment").await else {
            return;
        };
        let submission = ready_to_commit(&h, "w1").await;
        h.commitment = Some(submission.clone());

        let report = run_once_benchmarks(&h.driver(), &ConfirmedBenchmarks::default())
            .await
            .unwrap();
        assert_eq!(report.outcomes.len(), 1, "{report:?}");
        let o = &report.outcomes[0];
        assert_eq!(o.decision, Some(ClaimDecision::Transmit));
        let Acted::Transmitted { outcome, .. } = &o.acted else {
            panic!("{o:?}");
        };
        assert_eq!(*outcome, AttemptOutcome::Accepted);
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-benchmark"],
            json!(1)
        );

        // Not yet confirmed: §7 makes the read the authority, so the pass
        // waits rather than sending again.
        let again = run_once_benchmarks(&h.driver(), &ConfirmedBenchmarks::default())
            .await
            .unwrap();
        assert_eq!(
            again.outcomes[0].decision,
            Some(ClaimDecision::AwaitConfirmation),
            "{again:?}"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-benchmark"],
            json!(1),
            "still exactly one"
        );

        // The read confirms it. The attempt settles; nothing is resent.
        let confirmed = ConfirmedBenchmarks::from_read(&[json!({
            "id": submission.benchmark_id(),
            "state": { "block_confirmed": 3 }
        })])
        .unwrap();
        let settled = run_once_benchmarks(&h.driver(), &confirmed).await.unwrap();
        assert!(
            matches!(
                &settled.outcomes[0].acted,
                Acted::AttemptSettled { .. } | Acted::Nothing
            ),
            "{settled:?}"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-benchmark"],
            json!(1)
        );
        assert!(!settled.needs_operator(), "{settled:?}");
    }

    #[tokio::test]
    async fn a_commitment_pass_with_no_built_payload_records_no_attempt() {
        // The gateway does not construct commitments (§3). A driver asked to
        // run this pass without the built body has nothing legitimate to
        // send, and stops for an operator *before* an attempt row exists.
        //
        // Where it is caught is the point. `send_benchmark` refuses the same
        // body, but only after `begin_fenced` has written the attempt — and
        // the only way to close that row is REJECTED, which the next pass
        // reads as `Refused`: TIG answered and said no. It did not.
        // `mining_system.md` §8 keeps a pool fault from being recorded as
        // TIG's, so the ledger must stay empty here.
        //
        // And it is a `Skip`, not an operator stop: one driver holds one
        // commitment while the pass claims every claimable benchmark intent,
        // so this is the ordinary case for every intent but the one whose
        // bytes are in hand. The next pass with the right body sends it.
        let Some(h) = Harness::new("drive_commitment_nobody").await else {
            return;
        };
        ready_to_commit(&h, "w1").await;
        let intent = benchmark_intent_id(&h.controller, "w1").await;

        let report = run_once_benchmarks(&h.driver(), &ConfirmedBenchmarks::default())
            .await
            .unwrap();
        assert_eq!(
            report.outcomes[0].decision,
            Some(ClaimDecision::Skip {
                reason: SkipReason::NoBuiltPayload
            }),
            "{report:?}"
        );
        assert!(
            matches!(&report.outcomes[0].acted, Acted::Nothing),
            "{report:?}"
        );
        assert!(
            !report.needs_operator(),
            "an ordinary pass with no body for this intent must not page: {report:?}"
        );

        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        assert!(
            ledger.attempts_for(&intent).await.unwrap().is_empty(),
            "no attempt row may exist for a body that could not be sent"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]
                .get("submit-benchmark")
                .cloned()
                .unwrap_or(json!(0)),
            json!(0)
        );
    }

    #[tokio::test]
    async fn a_body_that_names_this_benchmark_and_differs_is_refused_before_any_attempt() {
        // The genuine integrity alarm: the body names this intent's benchmark
        // and still digests differently. `0015` admits one commitment per
        // benchmark, so nothing legitimate renders one benchmark's bytes two
        // ways — and it must be caught before `begin_fenced`, because an
        // attempt row for bytes that cannot be sent can only be closed as a
        // refusal TIG never made.
        let Some(mut h) = Harness::new("drive_commitment_wrongbody").await else {
            return;
        };
        let submission = ready_to_commit(&h, "w1").await;
        let intent = benchmark_intent_id(&h.controller, "w1").await;
        h.commitment = Some(BenchmarkSubmission {
            benchmark_id: submission.benchmark_id().to_string(),
            merkle_root: "cd".repeat(32),
            solution_quality: vec![9, 9, 9, 9],
        });

        let report = run_once_benchmarks(&h.driver(), &ConfirmedBenchmarks::default())
            .await
            .unwrap();
        assert_eq!(
            report.outcomes[0].decision,
            Some(ClaimDecision::StopForOperator {
                reason: StopReason::PayloadNotTheRecordedOne
            }),
            "{report:?}"
        );
        assert!(report.needs_operator(), "{report:?}");
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        assert!(
            ledger.attempts_for(&intent).await.unwrap().is_empty(),
            "{report:?}"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]
                .get("submit-benchmark")
                .cloned()
                .unwrap_or(json!(0)),
            json!(0)
        );
    }

    #[tokio::test]
    async fn two_live_workflows_and_one_body_page_nobody() {
        // What a second live workflow actually produces, end to end. The
        // driver holds one built commitment; the pass claims both claimable
        // benchmark intents and is handed that same body for each.
        //
        // The one it belongs to is sent. The other is an ordinary skip, and
        // the report does not page — §13 invariant 4 guarantees that intent's
        // own payload exists, and §10.3's discipline is that an alarm raised
        // on every pass is one nobody reads. This is the case the previous
        // round's split missed: it only answered a driver holding no body at
        // all, which is not what two workflows produce.
        let Some(mut h) = Harness::new("drive_two_commitments").await else {
            return;
        };
        let first = ready_to_commit(&h, "w1").await;
        ready_to_commit(&h, "w2").await;
        let other_intent = benchmark_intent_id(&h.controller, "w2").await;
        h.commitment = Some(first.clone());

        let report = run_once_benchmarks(&h.driver(), &ConfirmedBenchmarks::default())
            .await
            .unwrap();
        assert_eq!(report.outcomes.len(), 2, "{report:?}");
        assert!(
            !report.needs_operator(),
            "one body and two intents is ordinary, not an operator condition: {report:?}"
        );

        let sent = report
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w1")
            .unwrap();
        assert_eq!(sent.decision, Some(ClaimDecision::Transmit), "{sent:?}");

        let skipped = report
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w2")
            .unwrap();
        assert_eq!(
            skipped.decision,
            Some(ClaimDecision::Skip {
                reason: SkipReason::NoBuiltPayload
            }),
            "{skipped:?}"
        );

        // Exactly one write, and nothing written down for the intent this
        // pass held no bytes for.
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-benchmark"],
            json!(1)
        );
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        assert!(
            ledger.attempts_for(&other_intent).await.unwrap().is_empty(),
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn a_confirmed_commitment_waits_for_a_sender_that_may_still_be_live() {
        // The benchmark path's direct reconciliation, against an attempt
        // whose response has not been recorded yet.
        //
        // §7.3 writes the attempt and its response separately, so a NULL
        // outcome younger than the call timeout may belong to a sender still
        // waiting on TIG. Writing the fabricated ambiguity over it would lose
        // the real status, and the real sender's `resolve` would then fail as
        // already-resolved and be reported as a failure. The precommit path
        // has always refused that; this path did not, and nothing exercised
        // the branch at all.
        let Some(mut h) = Harness::with_policy("drive_commitment_young", fast_policy()).await
        else {
            return;
        };
        let submission = ready_to_commit(&h, "w1").await;
        let benchmark_id = submission.benchmark_id().to_string();
        h.commitment = Some(submission);

        // The crash: an attempt row under a fence, no response recorded.
        let intent = benchmark_intent_id(&h.controller, "w1").await;
        let lease = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "crashed",
            60,
        )
        .await
        .unwrap();
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        let pending = ledger.begin_fenced(&intent, &lease).await.unwrap();
        pool_workflow::lease::release(&h.gateway, &lease)
            .await
            .unwrap();

        // TIG's read says the benchmark confirmed. The attempt is seconds
        // old, so the pass waits instead of settling it.
        let confirmed = ConfirmedBenchmarks::from_read(&[json!({
            "id": benchmark_id,
            "state": { "block_confirmed": 3 }
        })])
        .unwrap();
        let early = run_once_benchmarks(&h.driver(), &confirmed).await.unwrap();
        assert!(
            matches!(&early.outcomes[0].acted, Acted::AwaitingSender { attempt_id } if *attempt_id == pending.attempt_id),
            "too young to presume the sender dead: {early:?}"
        );
        assert!(!early.needs_operator(), "{early:?}");
        assert!(
            ledger.attempts_for(&intent).await.unwrap()[0]
                .outcome
                .is_none(),
            "the attempt must be left exactly as the sender left it"
        );

        // Past the call timeout no sender can still be waiting, so it settles.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let settled = run_once_benchmarks(&h.driver(), &confirmed).await.unwrap();
        assert!(
            matches!(&settled.outcomes[0].acted, Acted::AttemptSettled { .. }),
            "{settled:?}"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]
                .get("submit-benchmark")
                .cloned()
                .unwrap_or(json!(0)),
            json!(0),
            "reconciliation never sends"
        );
    }

    #[tokio::test]
    async fn initial_writes_are_paced_by_the_lane_gap() {
        // §11: the POST lane is serialized with a minimum gap between initial
        // writes. The database serializes *unresolved* precommits; an
        // accepted one leaves the lane index at once, so two claimable
        // intents would otherwise go out back to back — and a burst is what
        // TIG answers with the 429 the transmitter must record as ambiguous.
        let Some(h) = Harness::with_policy("drive_paced", fast_policy()).await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();
        admit_precommit(&h.controller, &decision("w2", 1), 4)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let report = run_once(&h.driver(), &[]).await.unwrap();
        let elapsed = started.elapsed();

        assert_eq!(report.outcomes.len(), 2, "{report:?}");
        for o in &report.outcomes {
            assert!(
                matches!(
                    &o.acted,
                    Acted::Transmitted {
                        outcome: AttemptOutcome::Accepted,
                        ..
                    }
                ),
                "{o:?}"
            );
        }
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-precommit"],
            json!(2)
        );
        assert!(
            elapsed >= h.lane.policy().min_between_initial_writes(),
            "the second send left {elapsed:?} after the first; the gap is {:?}",
            h.lane.policy().min_between_initial_writes()
        );
    }

    fn outcome(decision: Option<ClaimDecision>, acted: Acted) -> IntentOutcome {
        IntentOutcome {
            intent_id: "i1".into(),
            workflow_id: "w1".into(),
            generation: 1,
            trace_id: None,
            decision,
            acted,
        }
    }

    #[test]
    fn only_a_stop_or_a_failure_asks_for_an_operator() {
        use Notability::{Effect, Operator, Routine};

        // §10.3: the bucket is worth reading only if an ordinary pass leaves
        // it empty. Every variant below occurs on a pass with one precommit
        // legitimately in flight, so each must classify as Routine.
        for acted in [
            Acted::LaneOccupied,
            Acted::LeaseHeldElsewhere,
            Acted::WriteBlocked {
                category: "readiness",
            },
            Acted::AwaitingSender {
                attempt_id: "a1".into(),
            },
            Acted::Nothing,
        ] {
            assert_eq!(
                outcome(Some(ClaimDecision::Transmit), acted.clone()).notability(),
                Routine,
                "{acted:?} is contention, not an operator condition"
            );
        }

        // The two that changed what TIG holds.
        assert_eq!(
            outcome(
                Some(ClaimDecision::Transmit),
                Acted::Transmitted {
                    attempt_id: "a1".into(),
                    outcome: AttemptOutcome::Accepted,
                    benchmark_id: Some("b1".into()),
                },
            )
            .notability(),
            Effect
        );
        assert_eq!(
            outcome(
                None,
                Acted::AttemptSettled {
                    attempt_id: "a1".into(),
                    benchmark_id: "b1".into(),
                },
            )
            .notability(),
            Effect
        );

        // The two that do not resolve themselves.
        assert_eq!(
            outcome(
                Some(ClaimDecision::StopForOperator {
                    reason: StopReason::PayloadNotTheRecordedOne,
                }),
                Acted::Nothing,
            )
            .notability(),
            Operator
        );
        assert_eq!(
            outcome(
                None,
                Acted::Failed {
                    error: "unreadable".into(),
                },
            )
            .notability(),
            Operator
        );
    }

    #[test]
    fn the_report_and_the_log_level_answer_the_same_question() {
        // Two classifications of one outcome would eventually disagree, and
        // the disagreement would read as "warned about, reported as fine".
        let stop = outcome(
            Some(ClaimDecision::StopForOperator {
                reason: StopReason::PayloadNotTheRecordedOne,
            }),
            Acted::Nothing,
        );
        let routine = outcome(Some(ClaimDecision::Transmit), Acted::LaneOccupied);

        assert!(
            RunReport {
                outcomes: vec![routine.clone(), stop.clone()],
            }
            .needs_operator()
        );
        assert!(
            !RunReport {
                outcomes: vec![routine],
            }
            .needs_operator()
        );
        assert_eq!(stop.notability(), Notability::Operator);
    }

    #[test]
    fn an_outcome_surrenders_the_ids_a_log_line_correlates_on() {
        // §10.1 wants `benchmark_id` on the line. It arrives three ways, and
        // a match that knew only the response path would drop it on exactly
        // the recovery paths §10 exists for.
        assert_eq!(
            outcome(
                Some(ClaimDecision::Transmit),
                Acted::Transmitted {
                    attempt_id: "a1".into(),
                    outcome: AttemptOutcome::Accepted,
                    benchmark_id: Some("b-response".into()),
                },
            )
            .benchmark_id(),
            Some("b-response")
        );
        assert_eq!(
            outcome(
                None,
                Acted::AttemptSettled {
                    attempt_id: "a1".into(),
                    benchmark_id: "b-settled".into(),
                },
            )
            .benchmark_id(),
            Some("b-settled")
        );
        assert_eq!(
            outcome(
                Some(ClaimDecision::AlreadyConfirmed {
                    benchmark_id: "b-searched".into(),
                }),
                Acted::Nothing,
            )
            .benchmark_id(),
            Some("b-searched")
        );

        // An ambiguous send still names its attempt: that row is what §10's
        // search reads after a crash, so a line without it cannot be followed.
        assert_eq!(
            outcome(
                Some(ClaimDecision::Transmit),
                Acted::Transmitted {
                    attempt_id: "a-ambiguous".into(),
                    outcome: AttemptOutcome::Ambiguous,
                    benchmark_id: None,
                },
            )
            .attempt_id(),
            Some("a-ambiguous")
        );
        assert_eq!(outcome(None, Acted::LaneOccupied).attempt_id(), None);
    }

    #[test]
    fn the_lease_rule_refuses_a_lease_equal_to_the_call_timeout() {
        // The boundary is the whole point: a lease that expires at the instant
        // a call times out still leaves the takeover window open, so `>` is
        // strict. `service::run` and `check_lease` share this function so a
        // value one accepts cannot be a value the other rejects for ever.
        assert!(lease_outlasts_call(61, 60).is_ok(), "longer is fine");
        assert!(
            matches!(
                lease_outlasts_call(60, 60),
                Err(DriveError::LeaseShorterThanCall { .. })
            ),
            "equal must be refused"
        );
        assert!(
            matches!(
                lease_outlasts_call(59, 60),
                Err(DriveError::LeaseShorterThanCall { .. })
            ),
            "shorter must be refused"
        );
        // A negative lease cannot convert, and must not read as unbounded.
        assert!(
            matches!(
                lease_outlasts_call(-1, 60),
                Err(DriveError::LeaseShorterThanCall { .. })
            ),
            "a negative lease must be refused"
        );
    }

    #[tokio::test]
    async fn a_lease_that_does_not_outlast_a_call_is_refused() {
        // A claimant whose lease expires mid-call lets the next claimant find
        // the attempt unresolved and take it for a crash. Refused up front,
        // not discovered in production.
        let Some(h) = Harness::new("drive_short_lease").await else {
            return;
        };
        let call_timeout_secs = h.lane.policy().call_timeout().as_secs();
        let short = Driver {
            lease_secs: i64::try_from(call_timeout_secs).unwrap(),
            ..h.driver()
        };
        let err = run_once(&short, &[]).await.expect_err("refused");
        assert!(
            matches!(err, DriveError::LeaseShorterThanCall { .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_send_refused_before_leaving_closes_its_attempt() {
        // `send` fails only on its own pre-flight checks, and by then the
        // attempt row exists. Left with no outcome it would hold §10's lane
        // for the whole pool, and the search could never settle it because
        // nothing reached TIG. Staged with a submission that does not hash to
        // the intent's recorded digest.
        let Some(h) = Harness::new("drive_send_refused").await else {
            return;
        };
        let admitted = admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();
        let lease = pool_workflow::lease::claim(
            &h.gateway,
            Network::Testnet,
            "w1",
            pool_workflow::LeaseKind::PrecommitTransmit,
            "gateway-test",
            120,
        )
        .await
        .unwrap();
        let ledger = PostgresAttemptLedger::new(h.gateway.clone());
        let attempt = ledger
            .begin_fenced(&admitted.intent.intent_id, &lease)
            .await
            .unwrap();
        let permit = h.gate.begin_write().unwrap();
        let mut inputs = payload_inputs(&h.gateway, Network::Testnet, "w1", 1)
            .await
            .unwrap()
            .unwrap();
        inputs.selected_algorithm = "a012".to_string();
        let wrong = PrecommitSubmission::from_decision(PLAYER, &inputs).unwrap();

        let err = send_or_release(
            &h.driver(),
            &ledger,
            &permit,
            &admitted.intent,
            &attempt,
            &wrong,
        )
        .await
        .expect_err("the digest check refuses");
        assert!(
            matches!(
                err,
                HandleError::Transmit(TransmitError::PayloadNotTheRecordedOne { .. })
            ),
            "{err}"
        );
        drop(permit);
        pool_workflow::lease::release(&h.gateway, &lease)
            .await
            .unwrap();

        // Nothing was sent, the attempt is closed as refused, and the lane
        // is open for the next workflow.
        assert_eq!(
            fake_state(&h.base).await["writes_received"]
                .get("submit-precommit")
                .cloned()
                .unwrap_or(json!(0)),
            json!(0),
            "the fake counts a write on arrival; none arrived"
        );
        let attempts = ledger
            .attempts_for(&admitted.intent.intent_id)
            .await
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, Some(AttemptOutcome::Rejected));
        let other = admit_precommit(&h.controller, &decision("w2", 1), 4)
            .await
            .unwrap();
        ledger
            .begin(&other.intent.intent_id)
            .await
            .expect("the lane is not held by an attempt nothing was sent for");
    }

    #[tokio::test]
    async fn readiness_is_consulted_for_every_write_not_once() {
        // §10.3: losing WRITE_READY at runtime blocks *new* writes. A driver
        // holding one permit for its life would have consulted the gate once
        // ever, and `in_flight` — the number §10.3's alert reports — would
        // never return to zero.
        let Some(h) = Harness::new("drive_gate").await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();

        let first = run_once(&h.driver(), &[]).await.unwrap();
        assert!(
            matches!(first.outcomes[0].acted, Acted::Transmitted { .. }),
            "{first:?}"
        );
        assert_eq!(
            h.gate.in_flight(),
            0,
            "the permit is dropped once the response is recorded"
        );

        // Readiness lost between passes, and a new intent decided after it.
        h.gate
            .revoke(crate::write_gate::Revocation::Authentication {
                detail: "key refused".to_string(),
            });
        admit_precommit(&h.controller, &decision("w2", 1), 4)
            .await
            .unwrap();
        // Advance so w1's write confirms and stops occupying the lane, so the
        // only thing stopping w2 is the gate.
        fake_post(&h.base, "/_fake/advance-block", None, None).await;
        let window = precommits_window(&h.base).await;
        let second = run_once(&h.driver(), &window).await.unwrap();
        let w2 = second
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w2")
            .unwrap();
        assert_eq!(
            w2.acted,
            Acted::WriteBlocked {
                category: "authentication"
            },
            "{second:?}"
        );
        assert_eq!(
            fake_state(&h.base).await["writes_received"]["submit-precommit"],
            json!(1),
            "no new write after readiness was lost"
        );
        assert!(
            !second.needs_operator(),
            "the revocation is the readiness check's alert, not this run's"
        );
    }

    #[tokio::test]
    async fn ordinary_lane_contention_is_quiet() {
        // While one workflow's precommit is legitimately unresolved, every
        // other claimable intent decides Transmit and is refused by the lane
        // index. That is the lane working, on every pass — and a report that
        // raised it would raise on every pass, which is a bucket nobody reads.
        let Some(h) = Harness::new("drive_lane").await else {
            return;
        };
        admit_precommit(&h.controller, &decision("w1", 1), 4)
            .await
            .unwrap();
        admit_precommit(&h.controller, &decision("w2", 1), 4)
            .await
            .unwrap();

        // w1 goes ambiguous and holds the lane.
        fake_post(
            &h.base,
            "/_fake/inject",
            None,
            Some(json!({"target": "submit-precommit", "mode": "ambiguous"})),
        )
        .await;
        let report = run_once(&h.driver(), &[]).await.unwrap();
        let w1 = report
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w1")
            .unwrap();
        let w2 = report
            .outcomes
            .iter()
            .find(|o| o.workflow_id == "w2")
            .unwrap();
        assert!(
            matches!(
                w1.acted,
                Acted::Transmitted {
                    outcome: AttemptOutcome::Ambiguous,
                    ..
                }
            ),
            "{w1:?}"
        );
        assert_eq!(w2.acted, Acted::LaneOccupied, "{w2:?}");
        assert!(
            !report.needs_operator(),
            "contention is not an operator condition: {report:?}"
        );
    }
}
