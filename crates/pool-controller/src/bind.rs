//! Bind a confirmed precommit to the workflow that submitted it.
//!
//! The one transition the restart pass cannot make. `reconcile_after_restart`
//! advances a workflow from confirmed evidence keyed by its `benchmark_id` —
//! and a `DECIDED` workflow has none. TIG assigns the id in the precommit
//! response (`tig_integration.md` §6.1), nothing durable holds it, and §7
//! makes confirmation a *read* anyway. So the id has to be found: §10's
//! tuple search over the exact settings the pool submitted, which is the
//! same search the gateway reconciles with before it retries.
//!
//! This is the controller's use of it. §6 gives "advance confirmed TIG
//! lifecycle" to the controller reconciler, and this is where `DECIDED`
//! becomes `PRECOMMIT_CONFIRMED` — and where the precommit intent settles
//! `CONFIRMED`, which §7.3 says comes only from a confirmed read. Both in
//! the same place, from the same evidence, so they cannot disagree.
//!
//! Only workflows whose precommit actually left the gateway are searched.
//! One that never transmitted has nothing to find, and searching for it
//! anyway would invite the search to bind someone else's confirmed precommit
//! to a workflow that submitted nothing — a second owner for one benchmark.

use pool_domain::Network;
use pool_workflow::{
    ConfirmedPrecommit, PostgresIntentRepository, PrecommitSubmission, Reconciliation,
    SettledOutcome, TigWriteIntentRepository, WorkflowState, WriteKind, attempt, payload_inputs,
    reconcile_precommit, workflow,
};
use serde_json::Value;
use sqlx::PgPool;

/// One workflow the pass looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound {
    /// The search found exactly one confirmed match. The workflow is
    /// `PRECOMMIT_CONFIRMED` and its intent `CONFIRMED`.
    Confirmed {
        workflow_id: String,
        benchmark_id: String,
    },
    /// A match exists and has not confirmed. Nothing to do yet.
    Pending { workflow_id: String },
    /// Nothing matches. Not, on its own, evidence the write is absent (§10
    /// step 3's window is bounded); reported so an operator sees it age.
    NotFound { workflow_id: String },
    /// §10's stop-for-operator: several candidates, or a record the pool
    /// cannot read.
    StopForOperator { workflow_id: String, reason: String },
    /// The pass could not evaluate this workflow. Collected, not propagated.
    Failed { workflow_id: String, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BindReport {
    pub outcomes: Vec<Bound>,
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error(transparent)]
    Workflow(#[from] workflow::WorkflowError),
}

/// Bind every `DECIDED` workflow whose precommit reached TIG and whose
/// confirmed entry is in `precommits`.
///
/// `precommits` is the `precommits` array of a `get-benchmarks` response,
/// raw: the search matches on the entry's settings and details, which the
/// typed `ConfirmedWindow` does not carry because nothing else needs them.
pub async fn bind_confirmed_precommits(
    pool: &PgPool,
    network: Network,
    player_id: &str,
    precommits: &[Value],
) -> Result<BindReport, BindError> {
    let decided = workflow::in_state(pool, network, WorkflowState::Decided).await?;
    let mut report = BindReport::default();
    for current in decided {
        let id = current.workflow_id.clone();
        match bind_one(pool, network, player_id, precommits, &current).await {
            Ok(Some(bound)) => report.outcomes.push(bound),
            Ok(None) => {}
            Err(e) => report.outcomes.push(Bound::Failed {
                workflow_id: id,
                error: e.to_string(),
            }),
        }
    }
    Ok(report)
}

#[derive(Debug, thiserror::Error)]
enum OneError {
    #[error(transparent)]
    Workflow(#[from] workflow::WorkflowError),
    #[error(transparent)]
    Intent(#[from] pool_workflow::IntentError),
    #[error(transparent)]
    Admission(#[from] pool_workflow::AdmissionError),
    #[error(transparent)]
    Payload(#[from] pool_workflow::PayloadError),
    #[error(transparent)]
    Attempt(#[from] sqlx::Error),
    #[error("workflow {workflow_id} is DECIDED with no precommit intent")]
    NoIntent { workflow_id: String },
    #[error("workflow {workflow_id} generation {generation} has no decision record")]
    NoDecision {
        workflow_id: String,
        generation: i32,
    },
}

/// `None` when there is nothing to search for: the workflow never sent a
/// precommit.
async fn bind_one(
    pool: &PgPool,
    network: Network,
    player_id: &str,
    precommits: &[Value],
    current: &workflow::Workflow,
) -> Result<Option<Bound>, OneError> {
    let workflow_id = current.workflow_id.as_str();

    // Only a write that left the gateway can be found. `begin` records the
    // attempt before the request goes, so this is the same evidence
    // `admit_precommit` and the restart pass read.
    if !attempt::has_transmitted_precommit_write(pool, network.as_str(), workflow_id).await? {
        return Ok(None);
    }

    // The newest generation is the live decision (§7.3), and the one whose
    // settings the search must use. An older generation's settings would
    // search for a write that was superseded before it was sent.
    let intents = PostgresIntentRepository::new(pool.clone());
    let intent = intents
        .claimable(network, WriteKind::Precommit)
        .await?
        .into_iter()
        .filter(|i| i.workflow_id == workflow_id)
        .max_by_key(|i| i.generation)
        .ok_or_else(|| OneError::NoIntent {
            workflow_id: workflow_id.to_string(),
        })?;
    let inputs = payload_inputs(pool, network, workflow_id, intent.generation)
        .await?
        .ok_or_else(|| OneError::NoDecision {
            workflow_id: workflow_id.to_string(),
            generation: intent.generation,
        })?;
    let submitted = PrecommitSubmission::from_decision(player_id, &inputs)?;

    let found = match reconcile_precommit(precommits, &submitted) {
        Ok(found) => found,
        Err(e) => {
            return Ok(Some(Bound::StopForOperator {
                workflow_id: workflow_id.to_string(),
                reason: e.to_string(),
            }));
        }
    };
    let benchmark_id = match found {
        Reconciliation::Confirmed { benchmark_id } => benchmark_id,
        Reconciliation::PendingConfirmation => {
            return Ok(Some(Bound::Pending {
                workflow_id: workflow_id.to_string(),
            }));
        }
        Reconciliation::NoCandidate => {
            return Ok(Some(Bound::NotFound {
                workflow_id: workflow_id.to_string(),
            }));
        }
        Reconciliation::StopForOperator { candidates } => {
            return Ok(Some(Bound::StopForOperator {
                workflow_id: workflow_id.to_string(),
                reason: format!(
                    "{} candidates match: {}",
                    candidates.len(),
                    candidates.join(", ")
                ),
            }));
        }
    };

    // The confirmed entry, as `confirm_precommit`'s evidence. Read from the
    // same array the search ran over, so the id and the details it binds
    // come from one record.
    let shape = |field: &'static str| {
        OneError::Payload(pool_workflow::PayloadError::Shape {
            track: benchmark_id.clone(),
            field,
            expected: "present, as the search saw it",
        })
    };
    let entry = precommits
        .iter()
        .find(|p| p.get("benchmark_id").and_then(Value::as_str) == Some(benchmark_id.as_str()))
        .ok_or_else(|| shape("benchmark_id"))?;
    let evidence = ConfirmedPrecommit {
        benchmark_id: benchmark_id.clone(),
        block_confirmed: entry["state"]["block_confirmed"]
            .as_i64()
            .ok_or_else(|| shape("state.block_confirmed"))?,
        block_started: entry["details"]["block_started"]
            .as_i64()
            .ok_or_else(|| shape("details.block_started"))?,
        // §6.2's commitment is built to exactly this length. A *detail* TIG
        // assigns, so it is not in `settings` and has to be carried across
        // explicitly — the same split `block_started` sits on.
        num_nonces: entry["details"]["num_nonces"].as_i64(),
        track_id: entry["settings"]["track_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        settings: entry["settings"].clone(),
    };

    // Workflow first, then the intent. `confirm_precommit` binds the id
    // immutably and `workflow_one_per_benchmark` refuses a second owner, so
    // if this id already belongs to another workflow the transition fails
    // here and the intent is left unsettled — which is the right order: a
    // settled intent for a workflow that could not take the benchmark would
    // be a claim with nothing behind it.
    workflow::confirm_precommit(pool, network, workflow_id, current.revision, &evidence).await?;
    intents
        .settle(
            &intent.intent_id,
            &SettledOutcome::Confirmed {
                benchmark_id: benchmark_id.clone(),
            },
        )
        .await?;

    Ok(Some(Bound::Confirmed {
        workflow_id: workflow_id.to_string(),
        benchmark_id,
    }))
}
