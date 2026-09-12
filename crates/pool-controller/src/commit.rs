//! Creating the benchmark commitment write (`tig_integration.md` §6.2).
//!
//! §6 gives the controller "create or cancel a TIG write intent", and this is
//! the second kind it creates. The first — the precommit — is admitted with a
//! decision, under the capacity gate, because the pool chooses to make it.
//! This one the pool does not choose: TIG confirmed a precommit, a member
//! produced a package, the package was durably accepted, and the commitment
//! is what the pool owes. So there is no gate here and no draw, only
//! preconditions.
//!
//! **Every precondition is the database's.** `migrations/0011` and `0015`
//! refuse a benchmark intent without a durable acceptance and a built
//! commitment payload whose digest matches the intent's, so the invariants
//! hold for any writer rather than for the ones that called this. What is
//! here is the ordering, the error the caller can act on, and F4d's accepting
//! half — which the database cannot enforce, because whether an endpoint is
//! the real TIG is not a fact the database has.

use pool_config::TigConfig;
use pool_domain::Network;
use pool_workflow::{
    AcceptanceError, BenchmarkSubmission, IntentError, NewIntent, PayloadError,
    PostgresIntentRepository, TigWriteIntentRepository, WriteIntent, WriteKind, benchmark_digest,
    benchmark_preconditions_are_stubbed, commitment_matches_confirmed, workflow,
};
use sqlx::PgPool;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("workflow {workflow_id} does not exist on {network}")]
    NoWorkflow {
        network: Network,
        workflow_id: String,
    },
    /// The workflow is not one a commitment is owed for.
    ///
    /// §7 makes the confirmed precommit what licenses the commitment, and
    /// `PRECOMMIT_CONFIRMED` is the pool's record of having read it. A
    /// workflow anywhere else either has not earned the write yet or has
    /// already made it.
    #[error(
        "workflow {workflow_id} is {state}; a commitment is owed only from PRECOMMIT_CONFIRMED"
    )]
    NotAwaitingCommitment { workflow_id: String, state: String },
    /// The workflow has no `benchmark_id`, so there is nothing to commit for.
    #[error("workflow {workflow_id} has no confirmed benchmark id")]
    NoBenchmark { workflow_id: String },
    /// The commitment names a different benchmark than the workflow owns.
    #[error("the commitment is for {given}, but workflow {workflow_id} owns {owned}")]
    WrongBenchmark {
        workflow_id: String,
        owned: String,
        given: String,
    },
    /// F4d's accepting half: a fabricated precondition, against real TIG.
    ///
    /// Carries the host and never the URL — `architecture.md` §9 keeps
    /// credentials out of logs, and a `base_url` a caller built by hand need
    /// not have come through `Config::load`'s userinfo refusal.
    #[error(
        "refusing to commit benchmark {benchmark_id}: its preconditions were \
         fabricated by the F4a stub and the TIG endpoint is {host}, not a \
         local fake-tig"
    )]
    StubAgainstLiveEndpoint { benchmark_id: String, host: String },
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Acceptance(#[from] AcceptanceError),
    #[error(transparent)]
    Intent(#[from] IntentError),
    #[error(transparent)]
    Workflow(#[from] workflow::WorkflowError),
}

/// Create the §6.2 commitment intent for a workflow whose precommit TIG
/// confirmed.
///
/// `submission` is the body the artifact worker built from the accepted
/// package; `artifact_id` is the key it published it under, and a
/// `commitment_payload` row for that key with this body's digest must already
/// exist — `migrations/0015` refuses the intent otherwise, which is what
/// makes the digest the gateway checks reproducible.
///
/// Idempotent through the intent repository: a crash between recording the
/// intent and acting on it re-creates the identical one.
pub async fn create_commitment_intent(
    pool: &PgPool,
    network: Network,
    tig: &TigConfig,
    workflow_id: &str,
    artifact_id: &str,
    submission: &BenchmarkSubmission,
) -> Result<WriteIntent, CommitError> {
    let current = workflow::find(pool, network, workflow_id)
        .await?
        .ok_or_else(|| CommitError::NoWorkflow {
            network,
            workflow_id: workflow_id.to_string(),
        })?;
    if current.state != workflow::WorkflowState::PrecommitConfirmed {
        return Err(CommitError::NotAwaitingCommitment {
            workflow_id: workflow_id.to_string(),
            state: current.state.as_str().to_string(),
        });
    }
    let benchmark_id = current
        .benchmark_id
        .clone()
        .ok_or_else(|| CommitError::NoBenchmark {
            workflow_id: workflow_id.to_string(),
        })?;
    if submission.benchmark_id() != benchmark_id {
        return Err(CommitError::WrongBenchmark {
            workflow_id: workflow_id.to_string(),
            owned: benchmark_id,
            given: submission.benchmark_id().to_string(),
        });
    }

    // §6.2's shape, against the precommit TIG confirmed. Checked before the
    // digest is taken, so a body TIG would refuse never becomes an intent —
    // the fee is paid on submission, and a refused commitment costs it.
    if let Some(num_nonces) = confirmed_num_nonces(&current) {
        commitment_matches_confirmed(submission, num_nonces)?;
    }

    // F4d's accepting half. The creating half lives in `crate::stub` and is
    // compiled out of a default build (F4c); this half must exist in every
    // build, because the row it refuses may have been written by a build that
    // had the stub — a developer's database promoted, a dump restored — and
    // the endpoint is what decides whether that is safe.
    if benchmark_preconditions_are_stubbed(pool, network, workflow_id, &benchmark_id).await?
        && !tig.is_local_fake_tig()
    {
        return Err(CommitError::StubAgainstLiveEndpoint {
            benchmark_id,
            host: tig.host().unwrap_or_else(|| "<unparseable>".to_string()),
        });
    }

    let intents = PostgresIntentRepository::new(pool.clone());
    Ok(intents
        .create(NewIntent {
            network,
            workflow_id: workflow_id.to_string(),
            write_kind: WriteKind::Benchmark,
            // §7.3 binds a generation to the benchmark id. The first
            // commitment for a benchmark is generation 1; a changed payload
            // needs a new generation, which is D3's rule and not this
            // function's to apply — a caller that wants one asks for it by
            // creating it, and the schema refuses reuse across a different
            // benchmark.
            generation: 1,
            benchmark_id: Some(benchmark_id),
            payload_digest: benchmark_digest(submission),
            payload_artifact_id: Some(artifact_id.to_string()),
        })
        .await?)
}

/// `precommit.details.num_nonces` as the confirmed settings recorded it.
///
/// `None` when the confirmed settings do not carry it, in which case §6.2's
/// length rule has nothing to check against and the body is taken as given —
/// the database's preconditions still hold, and TIG's own validation is the
/// backstop. Not an error: `confirmed_settings` is whatever TIG published,
/// and inventing a length would be worse than not checking one.
fn confirmed_num_nonces(current: &workflow::Workflow) -> Option<u64> {
    current
        .confirmed_settings
        .as_ref()?
        .get("num_nonces")
        .and_then(serde_json::Value::as_u64)
}
