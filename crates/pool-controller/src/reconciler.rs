//! One block's reconciliation: what `tig_integration.md` §10 does before the
//! controller may claim work, run from one persisted snapshot.
//!
//! §10 lists seven steps. Steps 2 and 3 — the snapshot and the pool's
//! `get-benchmarks` window — are [`crate::ingest`]'s, and arrive here as the
//! [`PersistedSnapshot`]. Steps 1, 4 and 5 are `reconcile_after_restart`'s:
//! load every nonterminal workflow and advance it monotonically from the
//! records keyed by its `benchmark_id` and from the block's confirmed and
//! active sets. Step 7, reconciling a write before retrying it, is the
//! gateway's, at the moment it claims the intent. Step 6, checking an
//! accepted artifact is still available before a benchmark or proof retry,
//! has nothing to check yet: slice 1 has no artifacts, and until it does the
//! acceptance-precondition trigger (`migrations/0011`) is what stands between
//! a benchmark intent and a package that was never accepted.
//!
//! Two things the seven steps leave to the caller are done here too, because
//! they read the same evidence at the same block and must not disagree with
//! it. Binding a confirmed precommit to the `DECIDED` workflow that sent it
//! comes first: it is the one transition step 4 cannot make, since that
//! workflow has no `benchmark_id` to match on, and a workflow bound here can
//! then advance further in the same pass. Expiry comes last: §8's guardrail
//! ages unfinished work against the block just accepted, and it must see any
//! confirmation that block carried before it judges the work unfinished.
//!
//! Every step runs on every block, not only after a restart. §10 frames the
//! pass as a restart procedure because that is when local state is most
//! likely to lag TIG, but the evidence it consumes is the same on every
//! block, and a controller that only reconciled on restart would advance
//! nothing while it stayed up.

use std::collections::BTreeSet;

use pool_domain::Network;
use pool_snapshot::AnchoredRead;
use pool_snapshot::store::{NotUsable, PersistedSnapshot};
use pool_workflow::{
    Guardrails, NeedsAttention, RestartReport, WorkflowError, reconcile_after_restart, workflow,
};
use serde_json::Value;
use sqlx::PgPool;

use crate::bind::{BindError, BindReport, Bound, bind_confirmed_precommits};
use crate::window::{WindowError, confirmed_window};

/// Everything one block's pass did.
#[derive(Debug)]
pub struct BlockReport {
    pub block_id: String,
    pub height: i64,
    pub bind: BindReport,
    pub restart: RestartReport,
    /// Workflows the sweep expired at this block.
    pub expired: Vec<String>,
    /// Workflows the sweep did not judge, because this pass could not
    /// establish their state. Each is already in [`Self::needs_attention`];
    /// listed separately so it is visible that the deadline was withheld
    /// rather than found not due.
    pub expiry_withheld: Vec<String>,
    /// Workflows the sweep could not evaluate, with the error. Collected
    /// rather than propagated, as the restart pass collects its own: the
    /// operator told to stop needs the whole picture.
    pub expiry_failed: Vec<(String, String)>,
}

impl BlockReport {
    /// Whether the controller must not claim new work after this pass.
    ///
    /// The restart pass owns most of the answer (`RestartReport::blocks_claiming`
    /// says which of its buckets block, and why). Two of this pass's own
    /// buckets join it for the same reason those do — each names a workflow
    /// whose true state the pool does not have:
    ///
    /// - `Bound::StopForOperator` — the §10 tuple search found more than one
    ///   confirmed precommit for one decision, or a record it could not read.
    ///   The first is exactly the duplicate §10 and invariant 14 exist to
    ///   prevent, and claiming more work on top of it compounds the fee and
    ///   the mapping break an operator has yet to untangle.
    /// - `Bound::Failed` and an expiry failure — a workflow the pass never
    ///   finished checking, which is the restart pass's `Failed` by another
    ///   route.
    ///
    /// `Bound::NotFound` and `Bound::Pending` do not block: each names a
    /// workflow whose state is known and merely unconfirmed, and the next
    /// block may confirm it.
    pub fn blocks_claiming(&self) -> bool {
        self.restart.blocks_claiming()
            || self.bind.outcomes.iter().any(|outcome| {
                matches!(
                    outcome,
                    Bound::StopForOperator { .. } | Bound::Failed { .. }
                )
            })
            || !self.expiry_failed.is_empty()
    }

    /// Anything an operator has to look at, in one list.
    pub fn needs_attention(&self) -> Vec<String> {
        let mut items = Vec::new();
        for outcome in &self.bind.outcomes {
            match outcome {
                Bound::StopForOperator {
                    workflow_id,
                    reason,
                } => items.push(format!(
                    "{workflow_id}: precommit binding stopped: {reason}"
                )),
                Bound::Failed { workflow_id, error } => {
                    items.push(format!("{workflow_id}: precommit binding failed: {error}"));
                }
                Bound::Confirmed { .. } | Bound::Pending { .. } | Bound::NotFound { .. } => {}
            }
        }
        for item in &self.restart.needs_attention {
            items.push(match item {
                NeedsAttention::Contradicted {
                    workflow_id,
                    benchmark_id,
                    recorded_state,
                } => format!(
                    "{workflow_id}: recorded {recorded_state}, but TIG has evidence for {benchmark_id}"
                ),
                NeedsAttention::Failed { workflow_id, error } => {
                    format!("{workflow_id}: reconciliation failed: {error}")
                }
                NeedsAttention::PrecommitSearchOwed { workflow_id } => {
                    format!("{workflow_id}: precommit search owed")
                }
                NeedsAttention::VerificationMissed {
                    workflow_id,
                    benchmark_id,
                } => format!("{workflow_id}: verification of {benchmark_id} missed"),
                NeedsAttention::OutsideWindow {
                    workflow_id,
                    benchmark_id,
                } => format!("{workflow_id}: {benchmark_id} is outside the confirmed window"),
            });
        }
        for (workflow_id, error) in &self.expiry_failed {
            items.push(format!("{workflow_id}: expiry sweep failed: {error}"));
        }
        items
    }
}

/// What a pass at one block came to.
#[derive(Debug)]
pub enum Outcome {
    Reconciled(BlockReport),
    /// The snapshot's reads are incomplete, so the pass did nothing. §9:
    /// the orchestrator does no work until a complete snapshot is available
    /// — and neither does this, because a read that was not made is not
    /// evidence of absence, and absence is what would license expiring a
    /// workflow whose confirmation the missing read was carrying.
    Blind {
        block_id: String,
        height: u64,
        reason: NotUsable,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("confirmed window: {0}")]
    Window(#[from] WindowError),
    #[error("binding precommits: {0}")]
    Bind(#[from] BindError),
    #[error("workflow: {0}")]
    Workflow(#[from] WorkflowError),
    /// A complete snapshot without the read the pass is built on. Cannot
    /// happen by construction — `reads_complete` means every anchored read
    /// is present — and is an error rather than a `Blind` outcome because a
    /// snapshot that claims completeness and lacks a read is corrupt, not
    /// merely partial.
    #[error("the snapshot claims complete reads but has no {0} read")]
    MissingRead(&'static str),
    #[error("block height {height} is outside the range the pool records")]
    HeightOutOfRange { height: u64 },
}

/// Run one block's pass.
///
/// `player_id` is the pool's own TIG address, which the precommit tuple
/// search matches on.
pub async fn reconcile_block(
    pool: &PgPool,
    network: Network,
    player_id: &str,
    guardrails: &Guardrails,
    snapshot: &PersistedSnapshot,
) -> Result<Outcome, ReconcileError> {
    let snap = match snapshot.for_reconciliation() {
        Ok(snap) => snap,
        Err(reason) => {
            return Ok(Outcome::Blind {
                block_id: snapshot.record().block_id.clone(),
                height: snapshot.record().height,
                reason,
            });
        }
    };
    let height = i64::try_from(snap.height).map_err(|_| ReconcileError::HeightOutOfRange {
        height: snap.height,
    })?;

    let benchmarks = snap
        .read(AnchoredRead::Benchmarks)
        .ok_or(ReconcileError::MissingRead(
            AnchoredRead::Benchmarks.endpoint(),
        ))?;
    let window = confirmed_window(benchmarks, &snap.block)?;

    // The raw entries, for the tuple search: it matches on settings and
    // details the typed window does not carry. The window builder has
    // already refused any entry it could not read, so an entry here is one
    // it accepted.
    let precommits: Vec<Value> = benchmarks
        .get("precommits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let bind = bind_confirmed_precommits(pool, network, player_id, &precommits).await?;
    let restart = reconcile_after_restart(pool, network, &window).await?;

    // Workflows this pass could not resolve. §8's guardrail is a deadline on
    // work whose state the pool knows; these are the ones it does not.
    //
    // A `DECIDED` workflow whose §10 tuple search matched several confirmed
    // precommits is the case that matters. Its attempt is `ACCEPTED`, so the
    // in-flight guard inside `expire_if_due` does not withhold it, and once
    // its anchor ages past the guardrail the pool's clock would terminate a
    // workflow TIG has confirmed — under `workflow_expired_without_precommit`,
    // a reason code for work that never began — while the operator §10 handed
    // it to is still untangling it. Expiry would also close §6.1's interval,
    // freeing a capacity slot against a benchmark that is still live.
    //
    // `NotFound` and `Pending` are deliberately not here: those name a
    // workflow whose state *is* known and merely unconfirmed, and expiring an
    // unconfirmed precommit past §8's window is exactly what the guardrail is
    // for.
    let unresolved: BTreeSet<&str> = bind
        .outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            Bound::StopForOperator { workflow_id, .. } | Bound::Failed { workflow_id, .. } => {
                Some(workflow_id.as_str())
            }
            Bound::Confirmed { .. } | Bound::Pending { .. } | Bound::NotFound { .. } => None,
        })
        .chain(
            restart
                .needs_attention
                .iter()
                .filter_map(|item| match item {
                    // The same argument, by the other route: a workflow the
                    // restart pass could not finish checking.
                    NeedsAttention::Failed { workflow_id, .. } => Some(workflow_id.as_str()),
                    _ => None,
                }),
        )
        .collect();

    let mut expired = Vec::new();
    let mut expiry_failed = Vec::new();
    let mut expiry_withheld = Vec::new();
    for current in workflow::unfinished(pool, network).await? {
        let id = current.workflow_id;
        if unresolved.contains(id.as_str()) {
            expiry_withheld.push(id);
            continue;
        }
        match workflow::expire_if_due(pool, guardrails, network, &id, height).await {
            Ok(Some(_)) => expired.push(id),
            Ok(None) => {}
            Err(e) => expiry_failed.push((id, e.to_string())),
        }
    }

    Ok(Outcome::Reconciled(BlockReport {
        block_id: snap.block_id.clone(),
        height,
        bind,
        restart,
        expired,
        expiry_withheld,
        expiry_failed,
    }))
}
