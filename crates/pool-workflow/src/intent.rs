//! TIG write intents (`docs/architecture.md` §7.3).
//!
//! §7.3 owns the rules; this implements them. The one worth stating here is
//! the shape of `create`: an intent is identified by
//! `(network, workflow_id, write_kind, generation)`, so creating the same
//! intent twice with the same canonical payload is idempotent — that is the
//! crash-retry path — while the same key with a *different* payload is
//! refused. §7.3 requires an explicit new generation for a changed payload,
//! so silently accepting one, or silently keeping the old one, would both
//! leave the stored intent describing a write nobody decided on.

use std::future::Future;

use pool_domain::Network;
use sqlx::{PgPool, Row};

/// The TIG writes slice 1 records intents for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Precommit,
    Benchmark,
    Proof,
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteKind::Precommit => "precommit",
            WriteKind::Benchmark => "benchmark",
            WriteKind::Proof => "proof",
        }
    }

    /// Whether §7.3 binds this kind's generations to a TIG `benchmark_id`.
    pub fn is_benchmark_bound(self) -> bool {
        match self {
            WriteKind::Precommit => false,
            WriteKind::Benchmark | WriteKind::Proof => true,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "precommit" => Some(WriteKind::Precommit),
            "benchmark" => Some(WriteKind::Benchmark),
            "proof" => Some(WriteKind::Proof),
            _ => None,
        }
    }
}

/// Intent state.
///
/// `CONFIRMED` and `REJECTED` are set only from confirmed TIG reads, never
/// from an HTTP status: `tig_integration.md` §7 makes a recorded 200 no
/// evidence at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentState {
    /// Decided on, not yet attempted.
    Prepared,
    /// §7.3's ambiguous outcome. Reconciles against confirmed TIG state; it
    /// is never resolved by resending.
    OutcomeUnknown,
    Confirmed,
    Rejected,
}

impl IntentState {
    pub fn as_str(self) -> &'static str {
        match self {
            IntentState::Prepared => "PREPARED",
            IntentState::OutcomeUnknown => "OUTCOME_UNKNOWN",
            IntentState::Confirmed => "CONFIRMED",
            IntentState::Rejected => "REJECTED",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "PREPARED" => Some(IntentState::Prepared),
            "OUTCOME_UNKNOWN" => Some(IntentState::OutcomeUnknown),
            "CONFIRMED" => Some(IntentState::Confirmed),
            "REJECTED" => Some(IntentState::Rejected),
            _ => None,
        }
    }
}

/// An intent to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIntent {
    pub network: Network,
    pub workflow_id: String,
    pub write_kind: WriteKind,
    pub generation: i32,
    /// Required for benchmark and proof writes, absent for precommits
    /// (§7.3). Checked before the statement so the rule reports itself
    /// rather than arriving as a constraint violation.
    pub benchmark_id: Option<String>,
    pub payload_digest: [u8; 32],
    pub payload_artifact_id: Option<String>,
}

/// A recorded intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteIntent {
    pub intent_id: String,
    pub network: Network,
    pub workflow_id: String,
    pub write_kind: WriteKind,
    pub generation: i32,
    pub benchmark_id: Option<String>,
    pub payload_digest: [u8; 32],
    pub payload_artifact_id: Option<String>,
    pub state: IntentState,
}

#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    #[error("intent store unavailable: {0}")]
    Unavailable(String),
    /// The same key already carries a different canonical payload.
    ///
    /// §7.3: a changed payload requires an explicit new generation. Never
    /// resolved by overwriting, and never by silently keeping the existing
    /// row, because both leave the caller believing it recorded something it
    /// did not.
    #[error(
        "{network} workflow {workflow_id} {write_kind} generation {generation} already exists \
         with a different canonical payload; §7.3 requires a new generation"
    )]
    PayloadConflict {
        network: Network,
        workflow_id: String,
        write_kind: &'static str,
        generation: i32,
    },
    /// The benchmark binding of §7.3 was not satisfied.
    #[error("{write_kind} intents {requirement}")]
    BenchmarkBinding {
        write_kind: &'static str,
        requirement: &'static str,
    },
    #[error("stored intent {intent_id} is unreadable: {reason}")]
    Corrupt { intent_id: String, reason: String },
}

/// The `TigWriteIntentRepository` port of `architecture.md` §4.
pub trait TigWriteIntentRepository {
    /// Record an intent, or return the identical one already recorded.
    ///
    /// Only half of §7.3's generation rule lives here. A changed payload is
    /// refused without a new generation; the other half — that a new
    /// generation is *forbidden* once an earlier attempt may have reached
    /// TIG, unless reconciliation proves it safe — is not enforced, because
    /// it needs the attempt ledger. Slice-1 criterion D3 adds it. Stated so
    /// the gap is visible rather than inferred from what the code happens
    /// not to do.
    fn create(
        &self,
        new: NewIntent,
    ) -> impl Future<Output = Result<WriteIntent, IntentError>> + Send;

    /// The intent for one `(network, workflow, kind, generation)`, if any.
    fn find(
        &self,
        network: Network,
        workflow_id: &str,
        write_kind: WriteKind,
        generation: i32,
    ) -> impl Future<Output = Result<Option<WriteIntent>, IntentError>> + Send;
}

#[derive(Debug, Clone)]
pub struct PostgresIntentRepository {
    pool: PgPool,
}

impl PostgresIntentRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn unavailable(e: sqlx::Error) -> IntentError {
    IntentError::Unavailable(e.to_string())
}

fn row_to_intent(row: &sqlx::postgres::PgRow) -> Result<WriteIntent, IntentError> {
    let intent_id: String = row.try_get("intent_id").map_err(unavailable)?;
    let corrupt = |reason: String| IntentError::Corrupt {
        intent_id: intent_id.clone(),
        reason,
    };

    let network: String = row.try_get("network").map_err(unavailable)?;
    let network = network
        .parse::<Network>()
        .map_err(|_| corrupt(format!("unknown network {network}")))?;

    let kind_text: String = row.try_get("write_kind").map_err(unavailable)?;
    let write_kind =
        WriteKind::parse(&kind_text).ok_or_else(|| corrupt(format!("unknown kind {kind_text}")))?;

    let state_text: String = row.try_get("state").map_err(unavailable)?;
    let state = IntentState::parse(&state_text)
        .ok_or_else(|| corrupt(format!("unknown state {state_text}")))?;

    let digest: Vec<u8> = row.try_get("payload_digest").map_err(unavailable)?;
    let payload_digest: [u8; 32] = digest
        .try_into()
        .map_err(|d: Vec<u8>| corrupt(format!("digest is {} bytes, expected 32", d.len())))?;

    Ok(WriteIntent {
        intent_id,
        network,
        workflow_id: row.try_get("workflow_id").map_err(unavailable)?,
        write_kind,
        generation: row.try_get("generation").map_err(unavailable)?,
        benchmark_id: row.try_get("benchmark_id").map_err(unavailable)?,
        payload_digest,
        payload_artifact_id: row.try_get("payload_artifact_id").map_err(unavailable)?,
        state,
    })
}

impl TigWriteIntentRepository for PostgresIntentRepository {
    async fn create(&self, new: NewIntent) -> Result<WriteIntent, IntentError> {
        // §7.3's binding, checked here as well as by the schema. The
        // constraint is what enforces it against any writer; this is what
        // makes a caller's mistake say what is wrong instead of surfacing as
        // a check-constraint violation.
        match (new.write_kind.is_benchmark_bound(), &new.benchmark_id) {
            (true, None) => {
                return Err(IntentError::BenchmarkBinding {
                    write_kind: new.write_kind.as_str(),
                    requirement: "must name the TIG benchmark_id their generation is bound to",
                });
            }
            (false, Some(_)) => {
                return Err(IntentError::BenchmarkBinding {
                    write_kind: new.write_kind.as_str(),
                    requirement: "have no benchmark to bind to and must not name one",
                });
            }
            _ => {}
        }

        // ON CONFLICT DO NOTHING on the §7.3 key: a concurrent duplicate
        // loses here rather than raising, and exactly one row survives.
        // Written out rather than built with `format!`: sqlx 0.9 requires a
        // `'static` query string, and interpolating a column list would mean
        // reaching for AssertSqlSafe on a statement that has no need to be
        // dynamic at all.
        let inserted = sqlx::query(
            "INSERT INTO pool.tig_write_intent
                 (network, workflow_id, write_kind, generation, benchmark_id,
                  payload_digest, payload_artifact_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (network, workflow_id, write_kind, generation) DO NOTHING
             RETURNING intent_id::text AS intent_id, network, workflow_id, write_kind,
                       generation, benchmark_id, payload_digest, payload_artifact_id, state",
        )
        .bind(new.network.as_str())
        .bind(&new.workflow_id)
        .bind(new.write_kind.as_str())
        .bind(new.generation)
        .bind(&new.benchmark_id)
        .bind(new.payload_digest.as_slice())
        .bind(&new.payload_artifact_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;

        if let Some(row) = inserted {
            return row_to_intent(&row);
        }

        // Already recorded. Identical payload is the crash-retry path and is
        // idempotent; a different one is the conflict §7.3 forbids resolving
        // by anything other than a new generation.
        let existing = self
            .find(
                new.network,
                &new.workflow_id,
                new.write_kind,
                new.generation,
            )
            .await?
            .ok_or_else(|| {
                IntentError::Unavailable(
                    "insert conflicted but no intent was found; the row was deleted concurrently"
                        .to_string(),
                )
            })?;

        // The benchmark binding is compared too. Two generations of a
        // benchmark write that agreed on the payload but named different
        // benchmarks would otherwise be treated as the same intent, which is
        // exactly the reuse across a different benchmark_id §7.3 rules out.
        // The artifact pointer is compared too. The trigger makes it
        // immutable precisely because repointing it changes what is actually
        // sent; returning Ok(existing) for a caller that supplied a
        // different one would discard that pointer in silence, which is the
        // "silently keeping the old one" this module's doc rules out.
        if existing.payload_digest != new.payload_digest
            || existing.benchmark_id != new.benchmark_id
            || existing.payload_artifact_id != new.payload_artifact_id
        {
            return Err(IntentError::PayloadConflict {
                network: new.network,
                workflow_id: new.workflow_id,
                write_kind: new.write_kind.as_str(),
                generation: new.generation,
            });
        }

        Ok(existing)
    }

    async fn find(
        &self,
        network: Network,
        workflow_id: &str,
        write_kind: WriteKind,
        generation: i32,
    ) -> Result<Option<WriteIntent>, IntentError> {
        let row = sqlx::query(
            "SELECT intent_id::text AS intent_id, network, workflow_id, write_kind,
                    generation, benchmark_id, payload_digest, payload_artifact_id, state
             FROM pool.tig_write_intent
             WHERE network = $1 AND workflow_id = $2 AND write_kind = $3 AND generation = $4",
        )
        .bind(network.as_str())
        .bind(workflow_id)
        .bind(write_kind.as_str())
        .bind(generation)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;

        row.as_ref().map(row_to_intent).transpose()
    }
}
