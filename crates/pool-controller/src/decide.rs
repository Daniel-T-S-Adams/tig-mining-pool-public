//! One deciding pass: propose from a snapshot, admit the intent.
//!
//! [`crate::propose`] is the pure half — `mining_system.md` §6's stages over a
//! snapshot. This is the durable half: `architecture.md` §5.1 step 4 and
//! criterion D2, where the decision record and its `PRECOMMIT` intent commit
//! in one transaction that also recounts unverified workflows under the
//! serialized admission lease.
//!
//! The transaction itself is `pool_workflow::decision::admit_precommit`'s and
//! is not reproduced here — including D2e's rule that the anchor must still be
//! the newest usable snapshot when the transaction begins. A newer block
//! landing mid-pass therefore fails the pass rather than committing against a
//! stale anchor, and the next pass decides from the newer one. That is the
//! intended outcome: §6.3's draw is fixed by the anchor block, so free choice
//! of anchor would restore the re-roll its seed design removes.
//!
//! **Slice 1 has no members**, so the workflow `admit_precommit` creates is
//! pool-owned — criterion F6's permanent placeholder. It is created there
//! rather than here for the reason the body of `decide_once` records: the row
//! and its §6.1 unverified interval belong inside the same transaction as the
//! recount that reads them.
//!
//! `crate::service`'s poll calls this once per block it takes in, after
//! reconciling from the same snapshot. The [`Offer`] comes from
//! `[orchestration.bootstrap_offer]`, because slice 1 has no member to offer
//! compute; the gateway's `served_compute` is not it — that field scopes §13
//! check 6, and the compute a decision is made *for* is a different question.
//!
//! Configuration arrives as arguments, including the digest criterion A3
//! requires (`Config::decision_digest`), so this module stays free of
//! configuration the same way `propose` does — which is what lets a test hand
//! it a case that is hard to reach live.

use pool_domain::{Network, TraceId};
use pool_snapshot::active_cache::ActiveBenchmarkMeta;
use pool_snapshot::store::PersistedSnapshot;
use pool_workflow::decision::{
    AdmissionError, Admitted, AnchorSnapshot, NewDecision, RecordedDraw, RecordedTie,
    admit_precommit,
};
use pool_workflow::payload::{DecisionPayloadInputs, PrecommitSubmission, precommit_digest};
use pool_workflow::restart::ConfirmedWindow;
use pool_workflow::workflow::WorkflowError;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::propose::{Offer, Proposal, ProposeError, Proposed, propose};

/// What one pass did.
#[derive(Debug)]
pub enum Decided {
    /// The snapshot could not be used for a decision yet (criterion C5).
    ///
    /// Not an error: §9 gates the orchestrator on a complete snapshot and a
    /// ready active cache, and a pass that arrives before the warm-up finishes
    /// is the ordinary case, not a fault.
    SnapshotNotUsable(String),
    /// §6.2's no-action outcome: nothing compute-compatible and eligible.
    NoAction,
    /// The block's reconciliation says the controller must not claim new work
    /// (`BlockReport::blocks_claiming`, criterion G1).
    ///
    /// A distinct answer from `NoAction`: the rules were never run, because
    /// running them would decide on top of a workflow whose true state the
    /// pool does not have.
    BlockedForOperator,
    /// Reconciliation did not run over this block, so nothing may be claimed
    /// from it (`tig_integration.md` §10).
    ///
    /// Separate from [`Self::SnapshotNotUsable`] because they are different
    /// facts: that one says the snapshot could not be *read* for a decision
    /// (criterion C5), this one that it was never *reconciled from*. The two
    /// coincide today — a blind pass has incomplete reads either way — and
    /// naming them apart is what lets a test tell which gate refused, rather
    /// than each masking the other's absence.
    NotReconciled(String),
    /// A decision and its precommit intent are committed.
    Admitted(Box<Admitted>),
}

#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    #[error("proposing: {0}")]
    Propose(#[from] ProposeError),
    #[error("creating the workflow: {0}")]
    Workflow(#[from] WorkflowError),
    #[error("admitting: {0}")]
    Admission(#[from] AdmissionError),
    #[error("the proposal does not rebuild into a §6.1 body: {0}")]
    Payload(#[from] pool_workflow::payload::PayloadError),
    #[error("the anchor height {height} is outside the range the pool records")]
    HeightOutOfRange { height: u64 },
    #[error("no workflow id could be drawn: {0}")]
    NoWorkflowId(String),
    #[error(
        "the anchor block carries no config.reports.penalty_amount, which is \
         accounting.md §11.4's P[s]"
    )]
    NoPenaltyAmount,
    #[error(
        "the configured failure charge {0:?} is not an atom count that fits the \
         reserve arithmetic (accounting.md §3)"
    )]
    FailureChargeUnreadable(String),
}

/// `accounting.md` §11.4's reservation, and the inputs criterion D2c records.
///
/// ```text
/// B[t]        = proposed num_bundles for track t
/// P[s]        = live config.reports.penalty_amount at snapshot s
/// F[s,t]      = exact precommit fee implied by live challenge config and B[t]
/// X[policy]   = charge reserved for one chargeable failed benchmark
///
/// assignment_reserve[s,t] = P[s] * B[t] + F[s,t] + X[policy]
/// precommit_reserve       = max over every proposed track t
/// ```
///
/// The maximum, because TIG selects the track only after the precommit.
///
/// **Not yet the whole formula.** ADR 0010 added a per-member collateral
/// multiplier in basis points scaling the method term, so `accounting.md`
/// §11.4 now reads `ceil(P[s] * B[t] * M_bps[m] / 10_000) + F + X`. That term
/// is not wired in here and this code behaves as `M_bps = 10_000`, which is
/// the default and is correct for every member until a multiplier is set.
/// The member the multiplier belongs to does not exist until slice 2.
///
/// **`P[s]` is a block configuration value**, read from the anchor snapshot's
/// `config.reports.penalty_amount` — not a count of member reports. An earlier
/// version of this function read it as the latter, recorded an empty list and
/// dropped `P[s] * B[t]` entirely, which under-stated the pool-owned
/// benchmark's method-verification exposure by the whole method term and wrote
/// a reserve that could not be re-derived to §11.4's formula. §11.4 is
/// explicit, and a settled definition is not this function's to reinterpret.
///
/// Every input is recorded, not just the total: §11.5 requires the exact
/// penalty value with every reservation, and a maximum with no inputs cannot be
/// checked against the formula that produced it.
fn reserve(
    proposal: &Proposal,
    penalty_amount: u128,
    failure_charge_atoms: u128,
    policy_version: &str,
) -> (Value, String) {
    let mut by_track = serde_json::Map::new();
    let mut max = 0u128;
    for (track, num_bundles) in &proposal.bundle_sizing.num_bundles {
        let fee = proposal.fee_by_track.get(track).copied().unwrap_or(0);
        let method = penalty_amount.saturating_mul(u128::from(*num_bundles));
        // Saturating rather than wrapping: a reserve that wrapped would
        // under-state the exposure it exists to cover, which is the one
        // direction §11.4 cannot tolerate.
        let total = method
            .saturating_add(fee)
            .saturating_add(failure_charge_atoms);
        by_track.insert(
            track.clone(),
            json!({
                "num_bundles": num_bundles,
                "method_reserve_atoms": method.to_string(),
                "tig_fee_atoms": fee.to_string(),
                "failure_charge_atoms": failure_charge_atoms.to_string(),
                "assignment_reserve_atoms": total.to_string(),
            }),
        );
        max = max.max(total);
    }

    (
        json!({
            "penalty_amount_atoms": penalty_amount.to_string(),
            "policy_version": policy_version,
            "by_track": by_track,
            "note": "accounting.md §11.4 inputs only; slice 1 posts no batch \
                     (slice-1 plan D2c). X is unchosen (pre_build_checklist.md §5.2).",
        }),
        max.to_string(),
    )
}

/// §11.4's `P[s]`, from the anchor snapshot's block.
///
/// Required, not defaulted. A zero read for an unreadable penalty would remove
/// the method term from every reserve silently, which is the same failure the
/// formula's own history here already had once.
fn penalty_amount(snapshot: &pool_snapshot::Snapshot) -> Result<u128, DecideError> {
    let block = snapshot.block.get("block").unwrap_or(&snapshot.block);
    block
        .pointer("/config/reports/penalty_amount")
        .and_then(Value::as_str)
        .and_then(|v| v.parse::<u128>().ok())
        .ok_or(DecideError::NoPenaltyAmount)
}

/// A workflow id: 32 lowercase hex characters, drawn from the OS./// A workflow id: 32 lowercase hex characters, drawn from the OS.
///
/// Drawn rather than derived from the decision's inputs. A derived id would
/// make a second decision for the same block and challenge collide with the
/// first, and §7.3's key already prevents a duplicate *intent* — an id
/// collision would instead attach the new decision to the old workflow, which
/// is a different and much quieter failure.
fn workflow_id() -> Result<String, DecideError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| DecideError::NoWorkflowId(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Run one deciding pass against a snapshot the caller has just persisted.
#[allow(clippy::too_many_arguments)]
pub async fn decide_once(
    pool: &PgPool,
    network: Network,
    player_id: &str,
    offer: &Offer,
    persisted: &PersistedSnapshot,
    window: &ConfirmedWindow,
    active: &[ActiveBenchmarkMeta],
    limit: i64,
    failure_charge_atoms: &str,
    policy_version: &str,
    config_digest: [u8; 32],
    trace_id: Option<TraceId>,
) -> Result<Decided, DecideError> {
    // C5's gate, and the only path to the snapshot's contents: `for_decision`
    // returns the data rather than a flag beside it, so "is this usable" is
    // not a check a caller can forget.
    let snapshot = match persisted.for_decision() {
        Ok(snapshot) => snapshot,
        Err(why) => return Ok(Decided::SnapshotNotUsable(why.to_string())),
    };

    let proposal = match propose(snapshot, window, network, player_id, offer, active)? {
        Proposed::NoAction(_) => return Ok(Decided::NoAction),
        Proposed::Precommit(proposal) => *proposal,
    };

    let height =
        i64::try_from(persisted.record().height).map_err(|_| DecideError::HeightOutOfRange {
            height: persisted.record().height,
        })?;

    // The id only. `admit_precommit` creates the workflow itself, inside the
    // transaction, as F6's pool-owned placeholder with its §6.1 interval
    // opening at the anchor height.
    //
    // Creating it here first was wrong twice over: it put the row outside D2's
    // single transaction, and it opened the unverified interval before the
    // recount that reads it — so the gate counted the workflow being admitted
    // among the ones already occupying the limit and refused a pass early.
    let workflow_id = workflow_id()?;

    // Parsed before use, not with a fallback. `Config` validates the shape,
    // but `decide_once` is public and a caller that bypassed it would
    // otherwise commit a record whose `reserve_inputs` and `precommit_reserve`
    // disagree — the string written verbatim into one and a substituted zero
    // into the other. A canonical atom string can still be wider than the
    // arithmetic, which shape-checking alone does not catch.
    let charge = failure_charge_atoms
        .parse::<u128>()
        .map_err(|_| DecideError::FailureChargeUnreadable(failure_charge_atoms.to_string()))?;
    let (reserve_inputs, precommit_reserve) =
        reserve(&proposal, penalty_amount(snapshot)?, charge, policy_version);

    // The §7.3 digest, taken from the same reconstruction the gateway will use
    // to produce the bytes it sends. One function, so the intent cannot be
    // bound to a payload the sender then refuses to render.
    let submission = PrecommitSubmission::from_decision(
        player_id,
        &DecisionPayloadInputs {
            anchor_block_id: snapshot.block_id.clone(),
            selected_challenge: proposal.selected_challenge.clone(),
            selected_algorithm: proposal.selected_algorithm.clone(),
            compute_type: proposal.compute_type.clone(),
            track_settings: proposal.track_settings.clone(),
        },
    )?;

    let admitted = admit_precommit(
        pool,
        &NewDecision {
            network,
            workflow_id,
            // §7.3's first generation. A changed payload needs a new one, which
            // is D3's rule and not this pass's to apply.
            generation: 1,
            anchor: AnchorSnapshot {
                block_id: snapshot.block_id.clone(),
                content_digest: persisted.record().content_digest,
                height,
            },
            draw: RecordedDraw {
                domain: pool_domain::CHALLENGE_TIE_DOMAIN.to_string(),
                draw_ranks: proposal
                    .draw_ranks
                    .iter()
                    .map(|(id, rank)| (id.clone(), Value::String(rank.to_hex())))
                    .collect(),
                tie: match (&proposal.tie_candidates, &proposal.tie_winner) {
                    (Some(candidates), Some(winner)) => Some(RecordedTie {
                        candidates: candidates.clone(),
                        winner: winner.clone(),
                    }),
                    // Recorded only when a draw happened. The two fields move
                    // together by construction in `propose`, and a half-set
                    // pair here would be a record of a tie with no winner.
                    _ => None,
                },
            },
            selected_challenge: proposal.selected_challenge.clone(),
            selected_algorithm: proposal.selected_algorithm.clone(),
            compute_type: proposal.compute_type.clone(),
            track_settings: proposal.track_settings.clone(),
            reserve_inputs,
            precommit_reserve,
            config_digest,
            payload_digest: precommit_digest(&submission),
            trace_id,
        },
        limit,
    )
    .await?;

    Ok(Decided::Admitted(Box::new(admitted)))
}
