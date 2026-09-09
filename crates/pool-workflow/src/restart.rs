//! Restart reconciliation and missing-block detection
//! (`tig_integration.md` §10, criteria G1, G4, G5).
//!
//! §10's seven steps run **before the controller claims work**. The ordering is
//! the substance: a controller that claimed first and reconciled afterwards
//! would act on local state it had not yet checked against TIG, which is the
//! situation every rule in §7 exists to prevent.
//!
//! The pass takes confirmed TIG evidence as an argument rather than fetching
//! it. Steps 2 and 3 are reads the snapshot and read-client layers already own
//! (`architecture.md` §3), and threading them in here would put network I/O
//! inside the transaction boundary §7.2 forbids it from crossing. What this
//! module owns is steps 1, 4, 5 and the decision in 7 — what the local record
//! says, what TIG's says, and which writes still need reconciling.
//!
//! **G5, and it is a property rather than a check.** Nothing here can turn an
//! attempt into a confirmation, because the only way to advance a workflow is
//! [`crate::workflow`]'s evidence types, and those cannot be built from an
//! attempt row. A restart that finds an `ACCEPTED` attempt and no confirming
//! TIG record leaves the workflow exactly where it was.
//!
//! **What this pass does not do.** §10 step 7 also settles the *write* record.
//! Advancing a workflow from the window leaves the matching
//! `tig_write_intent` where it was — `PREPARED` or `OUTCOME_UNKNOWN` — so the
//! durable write record and the workflow disagree until something settles it,
//! and a retry path keyed on intent state has no reconciled fact to read.
//! Nothing in slice 1 drives intents to the transmitter yet, so no duplicate
//! write follows from it today — but a *stall* does, and that is the sharper
//! consequence. `migrations/0004`'s per-benchmark lane refuses `begin` while
//! an attempt for that benchmark is unresolved, so the proof write of a
//! benchmark confirmed this way cannot start, and §10.3's "ambiguous for more
//! than two target blocks" page keys on that attempt indefinitely. The
//! controller has only `SELECT` on the attempt table, so it cannot settle one
//! itself: only the gateway calling `WriteAttemptLedger::reconcile` reopens
//! the lane. Both lanes stay closed until it does. Tracked as issue #86 with
//! the intent an expiry leaves behind — one fix, in the gateway's claim path,
//! which criterion F4 builds.

use std::collections::BTreeMap;

use pool_domain::Network;
use sqlx::{PgPool, Row};

use crate::workflow::{
    self, ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof, Workflow,
    WorkflowError, WorkflowState,
};

/// The confirmed TIG records a restart reconciles against.
///
/// One window, keyed by `benchmark_id`, which §10 notes is what makes
/// reconciliation direct for benchmark and proof writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfirmedWindow {
    pub precommits: BTreeMap<String, ConfirmedPrecommit>,
    pub benchmarks: BTreeMap<String, ConfirmedBenchmark>,
    pub proofs: BTreeMap<String, ConfirmedProof>,
    pub frauds: BTreeMap<String, ConfirmedFraud>,
    /// §7's verification event: ids in `block.data.confirmed_ids.verified`.
    pub verified: Vec<String>,
    /// §7's active set: ids in `block.data.active_ids.benchmark`.
    pub active: Vec<String>,
    /// The block the sets above were read at.
    pub at_block: i64,
}

/// What one workflow needed, and got, from the pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciled {
    pub workflow_id: String,
    pub from: WorkflowState,
    pub to: WorkflowState,
}

/// A workflow the pass could not advance and could not dismiss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeedsAttention {
    /// Confirmed TIG evidence for a workflow the pool has already recorded as
    /// terminal. §10 stops for operator resolution rather than rewriting a
    /// settled history.
    Contradicted {
        workflow_id: String,
        benchmark_id: String,
        recorded_state: &'static str,
    },
    /// The pass could not advance this workflow and could not dismiss it.
    ///
    /// Collected rather than propagated: an operator told to stop needs to
    /// know what else the run found, and returning early would discard the
    /// report built so far.
    Failed { workflow_id: String, error: String },
    /// A precommit that reached TIG and whose id the pool never learned.
    ///
    /// §10: the gateway "never blindly resubmits an ambiguous precommit" — it
    /// searches confirmed precommits for the exact tuple. That search is
    /// `tig-gateway`'s (criterion E4); this pass only reports that it is owed,
    /// because the workflow has no `benchmark_id` to match on from here.
    ///
    /// Owed for an ACCEPTED attempt as much as an ambiguous one. §6.1 returns
    /// the assigned id in the response and nothing durable holds it, so a
    /// crash before `confirm_precommit` leaves a benchmark TIG created — and
    /// charged a fee for — that the pool cannot name. Reporting only the
    /// unsettled case would leave that workflow indistinguishable from one
    /// that never submitted, which is also what `admit_precommit` would read
    /// before admitting a second precommit for the same decision.
    PrecommitSearchOwed { workflow_id: String },
    /// TIG has the benchmark *active*, and the pool never saw it verified.
    ///
    /// §4.5's ladder runs `PROOF_CONFIRMED -> VERIFYING -> ACTIVE`, so an
    /// active benchmark was verified; §7 calls the active set authoritative.
    /// The pass still does not advance the workflow from it, because
    /// `confirm_verified` closes §6.1's unverified interval **at a block**,
    /// and `confirmed_ids.verified` is per-block: a verification that happened
    /// while the controller was down is not recoverable from any later read.
    /// Closing the interval at the current block instead would put the close
    /// after the fact for every block in between, and §7.6's per-block recount
    /// reads exactly that.
    ///
    /// So it is reported. Left silent it is worse than an open question: the
    /// interval never closes and `count_unverified` keeps counting the slot
    /// against `internal_pool_unverified_limit` until an operator resolves it.
    /// The deadline does not eventually clear it either —
    /// `WorkflowState::is_unfinished_local_work` puts `ProofConfirmed`
    /// outside §8's guardrail, which is correct and is also why nothing else
    /// would ever notice.
    VerificationMissed {
        workflow_id: String,
        benchmark_id: String,
    },
    /// The workflow owns a benchmark TIG's window does not mention.
    ///
    /// Not an error: §8's window is the latest 120 blocks, so an older
    /// benchmark legitimately falls out of it. Reported rather than advanced,
    /// because absence from a bounded window is not evidence of anything.
    OutsideWindow {
        workflow_id: String,
        benchmark_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartReport {
    pub advanced: Vec<Reconciled>,
    pub unchanged: Vec<String>,
    pub needs_attention: Vec<NeedsAttention>,
}

impl RestartReport {
    /// Whether the controller must not claim work on this run.
    ///
    /// §10's seven steps run **before the controller claims work**, and the
    /// point of that ordering is that claiming acts on local state already
    /// checked against TIG. A workflow this pass could not check is exactly
    /// the state §7 exists to prevent acting on — so `Ok(report)` is not by
    /// itself permission to proceed, and a caller reading only the `Result`
    /// would have taken it as such.
    ///
    /// Two of the five buckets block, and both for the same reason: they name
    /// a workflow whose true state the pool does not have. `Failed` is one the
    /// pass never finished checking. `Contradicted` is one where the pool's
    /// record and TIG's disagree, which §10 answers with operator resolution
    /// rather than more work.
    ///
    /// The other three do not, because each names a workflow whose state *is*
    /// known:
    ///
    /// - `OutsideWindow` — a benchmark aged past §8's 120-block window. §7
    ///   calls that ordinary, and blocking on it would stop the pool every
    ///   time a workflow got old.
    /// - `PrecommitSearchOwed` — work E4 will do, on a workflow whose write
    ///   this pass identified precisely. It does not block because the danger
    ///   it names is specific to *that* workflow, and `admit_precommit`
    ///   refuses it by name: a `DECIDED` workflow whose precommit already
    ///   reached TIG cannot take another generation
    ///   (`AdmissionError::PrecommitAlreadyTransmitted`). Blocking every
    ///   claim instead would stop the pool over one workflow that is already
    ///   individually safe.
    /// - `VerificationMissed` — TIG verified a benchmark in a block the pool
    ///   was down for. It costs a slot: §6.1's interval stays open until an
    ///   operator resolves it. But the accounting is *correct* while it does —
    ///   `count_unverified` counts that open interval, so `admit_precommit`
    ///   sees one fewer slot rather than over-admitting. Refusing to claim any
    ///   work would turn a capacity reduction the pool is already handling
    ///   into a full stop.
    pub fn blocks_claiming(&self) -> bool {
        self.needs_attention.iter().any(|item| {
            matches!(
                item,
                NeedsAttention::Failed { .. } | NeedsAttention::Contradicted { .. }
            )
        })
    }
}

/// §10 steps 1, 4 and 5: load every nonterminal workflow and advance it
/// monotonically from the confirmed window.
///
/// Monotonic means each workflow takes at most the transitions its evidence
/// supports, in §7's order, and never goes backwards — a window that has
/// dropped a record the pool already acted on cannot un-confirm it.
pub async fn reconcile_after_restart(
    pool: &PgPool,
    network: Network,
    window: &ConfirmedWindow,
) -> Result<RestartReport, WorkflowError> {
    let live = nonterminal(pool, network).await?;
    let mut report = RestartReport {
        advanced: Vec::new(),
        unchanged: Vec::new(),
        needs_attention: Vec::new(),
    };

    for current in live {
        let workflow_id = current.workflow_id.clone();
        let from = current.state;
        let to = match advance_one(pool, network, current, window, &mut report).await {
            Ok(to) => to,
            // Fail closed for this workflow, not for the run. The operator who
            // is told to stop needs to know what else the pass found.
            Err(e) => {
                report.needs_attention.push(NeedsAttention::Failed {
                    workflow_id,
                    error: e.to_string(),
                });
                continue;
            }
        };
        if to == from {
            report.unchanged.push(workflow_id);
        } else {
            report.advanced.push(Reconciled {
                workflow_id,
                from,
                to,
            });
        }
    }
    Ok(report)
}

/// Advance one workflow as far as the window's evidence supports, in §7's
/// order, and return where it ended.
///
/// Each step consumes a *confirmed* record. A workflow with an accepted
/// attempt and no confirming record advances nowhere, which is G5.
async fn advance_one(
    pool: &PgPool,
    network: Network,
    current: Workflow,
    window: &ConfirmedWindow,
    report: &mut RestartReport,
) -> Result<WorkflowState, WorkflowError> {
    let id = current.workflow_id.clone();
    let mut state = current.state;
    let mut revision = current.revision;

    // A workflow that never learned its benchmark id. §10's precommit lane
    // owns the search for it; from here there is nothing to match on.
    let Some(benchmark_id) = current.benchmark_id.clone() else {
        // Owed by any workflow whose precommit actually left the gateway —
        // ambiguous, still unanswered, or accepted with its id lost to a
        // crash. All three describe a write that may have created a benchmark
        // the pool cannot name, which is what E4's search recovers.
        //
        // A DECIDED workflow that never attempted one is ordinary pending
        // work, and handing the tuple search a workflow that submitted
        // nothing invites it to bind another workflow's confirmed precommit —
        // a second owner for one benchmark, against §10 invariant 1's
        // permanent mapping.
        if crate::attempt::has_transmitted_precommit_write(pool, network.as_str(), &id)
            .await
            .map_err(|e| WorkflowError::Unavailable(e.to_string()))?
        {
            report
                .needs_attention
                .push(NeedsAttention::PrecommitSearchOwed {
                    workflow_id: id.clone(),
                });
        }
        return Ok(state);
    };

    let known_to_tig = window.precommits.contains_key(&benchmark_id)
        || window.benchmarks.contains_key(&benchmark_id)
        || window.proofs.contains_key(&benchmark_id)
        || window.frauds.contains_key(&benchmark_id)
        || window.verified.iter().any(|v| v == &benchmark_id)
        || window.active.iter().any(|a| a == &benchmark_id);
    // A verified workflow awaits nothing from the window, so ageing out of it
    // is not a fact worth waking an operator for. §7 says a previously active
    // id simply leaving the active set is ordinary — "keep the compact
    // historical record" — and this bucket carries §10's stop-for-operator
    // signal, so filling it every restart would train the caller to ignore it.
    if !known_to_tig && state != WorkflowState::Verified {
        // §8: the window is the latest 120 blocks. Falling out of it is
        // ordinary ageing, and absence from a bounded window is not evidence.
        report.needs_attention.push(NeedsAttention::OutsideWindow {
            workflow_id: id.clone(),
            benchmark_id,
        });
        return Ok(state);
    }

    // Fraud first: §7 lists it as its own confirmed entry, and it is terminal
    // from wherever it is found, so applying it before the forward steps
    // avoids advancing a workflow TIG has already ruled on.
    if let Some(fraud) = window.frauds.get(&benchmark_id) {
        match workflow::confirm_fraud(pool, network, &id, revision, fraud).await {
            Ok(w) => return Ok(w.state),
            // `recorded`, not `state`: the error names the state the row was
            // already in, while `state` is what this pass has advanced it to.
            // Returning the pre-pass value classified a workflow this run had
            // just moved as `unchanged`, so the report disagreed with the row
            // §10's operator resolution is read from.
            Err(WorkflowError::TerminalStateContradicted {
                state: recorded, ..
            }) => {
                report.needs_attention.push(NeedsAttention::Contradicted {
                    workflow_id: id.clone(),
                    benchmark_id: benchmark_id.clone(),
                    recorded_state: recorded,
                });
                return Ok(state);
            }
            Err(e) => return Err(e),
        }
    }

    // Each step offers its evidence and lets the transition decide whether it
    // applies. The state preconditions used to be restated here, and drifted
    // from the ones the transitions actually enforce: `confirm_benchmark` and
    // `confirm_proof` accept their `*_SUBMITTED` predecessors — the state a
    // crash mid-submission leaves, which is the case §10 exists for — while
    // this pass only offered them evidence from `*_CONFIRMED`. Such a workflow
    // advanced nowhere and was reported nowhere.
    //
    // So there is one authority per rule now. `NotAllowed` means "that
    // evidence does not apply from here", which is an answer, not a failure.
    if let Some(benchmark) = window.benchmarks.get(&benchmark_id) {
        match workflow::confirm_benchmark(pool, network, &id, revision, benchmark).await {
            Ok(w) => {
                state = w.state;
                revision = w.revision;
            }
            Err(WorkflowError::NotAllowed { .. }) => {}
            // `recorded`, not `state`: the error names the state the row was
            // already in, while `state` is what this pass has advanced it to.
            // Returning the pre-pass value classified a workflow this run had
            // just moved as `unchanged`, so the report disagreed with the row
            // §10's operator resolution is read from.
            Err(WorkflowError::TerminalStateContradicted {
                state: recorded, ..
            }) => {
                report.needs_attention.push(NeedsAttention::Contradicted {
                    workflow_id: id.clone(),
                    benchmark_id: benchmark_id.clone(),
                    recorded_state: recorded,
                });
                return Ok(state);
            }
            Err(e) => return Err(e),
        }
    }

    // §7: a stopped benchmark sends no proof, which `confirm_proof` enforces —
    // STOPPED is terminal, so it answers with the contradiction below.
    if let Some(proof) = window.proofs.get(&benchmark_id) {
        match workflow::confirm_proof(pool, network, &id, revision, proof).await {
            Ok(w) => {
                state = w.state;
                revision = w.revision;
            }
            Err(WorkflowError::NotAllowed { .. }) => {}
            // `recorded`, not `state`: the error names the state the row was
            // already in, while `state` is what this pass has advanced it to.
            // Returning the pre-pass value classified a workflow this run had
            // just moved as `unchanged`, so the report disagreed with the row
            // §10's operator resolution is read from.
            Err(WorkflowError::TerminalStateContradicted {
                state: recorded, ..
            }) => {
                report.needs_attention.push(NeedsAttention::Contradicted {
                    workflow_id: id.clone(),
                    benchmark_id: benchmark_id.clone(),
                    recorded_state: recorded,
                });
                return Ok(state);
            }
            Err(e) => return Err(e),
        }
    }

    if window.verified.iter().any(|v| v == &benchmark_id) {
        match workflow::confirm_verified(
            pool,
            network,
            &id,
            revision,
            &benchmark_id,
            window.at_block,
        )
        .await
        {
            Ok(w) => state = w.state,
            Err(WorkflowError::NotAllowed { .. }) => {}
            // `recorded`, not `state`: the error names the state the row was
            // already in, while `state` is what this pass has advanced it to.
            // Returning the pre-pass value classified a workflow this run had
            // just moved as `unchanged`, so the report disagreed with the row
            // §10's operator resolution is read from.
            Err(WorkflowError::TerminalStateContradicted {
                state: recorded, ..
            }) => {
                report.needs_attention.push(NeedsAttention::Contradicted {
                    workflow_id: id.clone(),
                    benchmark_id: benchmark_id.clone(),
                    recorded_state: recorded,
                });
                return Ok(state);
            }
            Err(e) => return Err(e),
        }
    }

    // §10 step 5 reads the confirmed *and active* sets. The active set is the
    // only remaining evidence for a verification the pool was down for, and it
    // says the event happened without saying when — which is why this reports
    // rather than advances. See `NeedsAttention::VerificationMissed`.
    if state != WorkflowState::Verified
        && !state.is_terminal()
        && window.active.iter().any(|a| a == &benchmark_id)
    {
        report
            .needs_attention
            .push(NeedsAttention::VerificationMissed {
                workflow_id: id.clone(),
                benchmark_id,
            });
    }

    Ok(state)
}

async fn nonterminal(pool: &PgPool, network: Network) -> Result<Vec<Workflow>, WorkflowError> {
    // §10 step 1 loads "all local nonterminal workflows", and which states are
    // terminal is `WorkflowState`'s to say — not a list repeated here. The
    // list that used to be repeated here had already drifted: it named FRAUD
    // after the state was renamed FRAUDULENT to match §4.5, so a fraudulent
    // workflow was being reloaded on every restart.
    let terminal: Vec<String> = WorkflowState::ALL
        .iter()
        .filter(|state| state.is_terminal())
        .map(|state| state.as_str().to_string())
        .collect();

    let ids: Vec<String> = sqlx::query(
        "SELECT workflow_id FROM pool.workflow
         WHERE network = $1 AND state <> ALL($2)
         ORDER BY workflow_id",
    )
    .bind(network.as_str())
    .bind(&terminal)
    .fetch_all(pool)
    .await
    .map_err(|e| WorkflowError::Unavailable(e.to_string()))?
    .iter()
    .map(|row| row.get("workflow_id"))
    .collect();

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(w) = workflow::find(pool, network, &id).await? {
            out.push(w);
        }
    }
    Ok(out)
}

/// G4: record every height between what the pool had and what it then saw.
///
/// §10: the pool "must not invent per-block qualifier attribution or payouts
/// for the gap". Recording each missing height individually is what makes that
/// enforceable later — attribution is per block, so a range would have to be
/// expanded by whoever checked, and a gap nobody expanded is a gap nobody
/// noticed.
///
/// Returns the heights recorded. An observation one above the last height is
/// no gap and records nothing.
///
/// Recording is the controller reconciler's, per `architecture.md` §6.
/// *Resolving* one is not a second operation with a second owner: it is §6's
/// existing "apply an administrative override", which carries a pending
/// command row, an actor and a reason. `migrations/0010` holds the actor and
/// reason columns and freezes them once written; the command row that should
/// gate them does not exist yet, so until it does the column grant is the only
/// thing standing between the controller and an unattested resolution.
///
/// Takes an executor rather than the pool so the caller can commit these rows
/// in the same transaction that accepts the snapshot which revealed them.
/// §9 step 7 persists a snapshot with its completeness status atomically, and
/// a separate connection here would let a crash between the two advance the
/// pool's last local height past heights nothing had recorded as missing —
/// the silent, permanent loss `migrations/0010_block_data_gap.sql` exists to
/// prevent. Passing `&PgPool` still works and still means "its own
/// transaction", which is right for a caller that has no other work to commit.
pub async fn record_block_gap<'e, E>(
    executor: E,
    network: Network,
    after_height: i64,
    observed_height: i64,
) -> Result<Vec<i64>, WorkflowError>
where
    E: sqlx::PgExecutor<'e>,
{
    if observed_height <= after_height + 1 {
        return Ok(Vec::new());
    }

    let missing: Vec<i64> = ((after_height + 1)..observed_height).collect();
    // ON CONFLICT DO NOTHING: re-observing the same gap after a second restart
    // must not fail, and must not overwrite the record of when it was first
    // seen or how it was resolved.
    //
    // RETURNING, so the caller learns what was *newly* recorded rather than
    // the whole range. §10.3's alert fires on a gap being recorded, and a
    // caller driving it off the range would re-fire on every restart for a gap
    // an operator had already settled.
    let rows = sqlx::query(
        "INSERT INTO pool.block_data_gap (network, height, after_height, observed_height)
         SELECT $1, h, $2, $3 FROM unnest($4::bigint[]) AS h
         ON CONFLICT (network, height) DO NOTHING
         RETURNING height",
    )
    .bind(network.as_str())
    .bind(after_height)
    .bind(observed_height)
    .bind(&missing)
    .fetch_all(executor)
    .await
    .map_err(|e| WorkflowError::Unavailable(e.to_string()))?;

    let mut recorded: Vec<i64> = rows.iter().map(|row| row.get("height")).collect();
    recorded.sort_unstable();
    Ok(recorded)
}

/// The open gaps §10.3's alert and the readiness check read.
pub async fn open_block_gaps(pool: &PgPool, network: Network) -> Result<Vec<i64>, WorkflowError> {
    let rows = sqlx::query(
        "SELECT height FROM pool.block_data_gap
         WHERE network = $1 AND resolved_at IS NULL
         ORDER BY height",
    )
    .bind(network.as_str())
    .fetch_all(pool)
    .await
    .map_err(|e| WorkflowError::Unavailable(e.to_string()))?;
    Ok(rows.iter().map(|row| row.get("height")).collect())
}
