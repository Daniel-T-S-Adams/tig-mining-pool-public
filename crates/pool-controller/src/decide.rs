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
//! **Nothing calls this yet.** Wiring it into the controller's poll needs one
//! thing this module does not decide: where a deployment with no members gets
//! its [`Offer`] from. Slice 1 has no member to offer compute, and the
//! gateway's `served_compute` is not it — that field scopes §13 check 6, and
//! the compute a decision is made *for* is a different question with a
//! different answer. So the pass is delivered callable and tested against a
//! real database rather than half-wired to a guess.
//!
//! The configuration digest criterion A3 requires is `Config::decision_digest`,
//! which already exists; it arrives here as an argument so this module stays
//! free of configuration, the same way `propose` does.

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
}

/// The §11.4 reservation inputs this slice can compute, and the maximum.
///
/// Criterion D2c: slice 1 records `P[s]`, `B[t]`, `F[s,t]`, the `X` policy
/// version and the resulting maximum across proposed tracks, and posts no
/// accounting batch.
///
/// `P[s]` is empty here and says so: live method-report penalties require
/// members and reports, which slice 1 has neither of. That is recorded as an
/// empty list rather than omitted, because an absent field reads as "not
/// computed" and an empty one reads as "computed, and there were none" — and
/// only the second is true.
fn reserve(
    proposal: &Proposal,
    failure_charge_atoms: &str,
    policy_version: &str,
) -> (Value, String) {
    let charge: u128 = failure_charge_atoms.parse().unwrap_or(0);
    let mut by_track = serde_json::Map::new();
    let mut max = 0u128;
    for (track, num_bundles) in &proposal.bundle_sizing.num_bundles {
        let fee = proposal.fee_by_track.get(track).copied().unwrap_or(0);
        // `max(P[s] * B[t] + F[s,t] + X)`. With no live penalties the first
        // term is zero, and saturating rather than wrapping because a reserve
        // that wrapped would under-state the exposure it exists to cover.
        let total = fee.saturating_add(charge);
        by_track.insert(
            track.clone(),
            json!({
                "num_bundles": num_bundles,
                "tig_fee_atoms": fee.to_string(),
                "failure_charge_atoms": failure_charge_atoms,
                "reserve_atoms": total.to_string(),
            }),
        );
        max = max.max(total);
    }

    (
        json!({
            // The empty list is the statement; see this function's doc.
            "live_method_penalties": [],
            "policy_version": policy_version,
            "by_track": by_track,
            "note": "accounting.md §11.4 inputs only; slice 1 posts no batch \
                     (slice-1 plan D2c). X is unchosen (pre_build_checklist.md §5.2).",
        }),
        max.to_string(),
    )
}

/// A workflow id: 32 lowercase hex characters, drawn from the OS.
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

    let (reserve_inputs, precommit_reserve) =
        reserve(&proposal, failure_charge_atoms, policy_version);

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
