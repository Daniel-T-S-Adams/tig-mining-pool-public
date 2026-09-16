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

use pool_domain::{Network, TraceId};
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
    /// The trace this intent was admitted under (`architecture.md` §10.1,
    /// criterion I3).
    ///
    /// `None` is a recorded absence, not a defect: §10.1 asks that the id be
    /// stored, not that admission be refused without one, and a controller
    /// that could not draw an id should lose correlation rather than stop
    /// creating precommits.
    pub trace_id: Option<TraceId>,
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
    /// The trace the intent was admitted under. Carried onto every line the
    /// gateway logs about it, which is what keeps a write transmitted after a
    /// restart correlated with the decision that ordered it.
    pub trace_id: Option<TraceId>,
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
    /// The intent does not exist.
    #[error("no intent {intent_id}")]
    NotFound { intent_id: String },
    /// A settled intent was asked to settle to a different **outcome**.
    ///
    /// §7.3 makes `CONFIRMED` and `REJECTED` terminal. Two confirmed reads
    /// disagreeing about whether one write landed is not resolved by taking
    /// the later one — it is the discrepancy §10 stops for, the same shape
    /// `WorkflowError::TerminalStateContradicted` reports for a workflow.
    ///
    /// This is the whole of what the intent ledger can refuse, and it is
    /// narrower than it may look. A precommit intent structurally carries no
    /// `benchmark_id` (`migrations/0003`, D1a), so re-settling `CONFIRMED`
    /// under a *different* benchmark id is not detectable here — the ledger
    /// records that the write landed, not which benchmark it became. Which
    /// benchmark is the workflow's fact, bound once by
    /// `workflow::confirm_precommit` and then immutable, with
    /// `workflow_one_per_benchmark` refusing a second owner. Two confirmed
    /// reads naming different benchmarks for one workflow are caught there,
    /// as a contradicted binding, and not here. Storing the id on the intent
    /// as well would be the same fact in two places, free to disagree.
    #[error("intent {intent_id} is already {recorded}; refusing to record {asked}")]
    AlreadySettled {
        intent_id: String,
        recorded: &'static str,
        asked: &'static str,
    },
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

    /// Intents of this kind that still owe TIG a write.
    ///
    /// `architecture.md` §5.1 step 5: "TIG Gateway claims that intent,
    /// reconciles it, submits at most one unresolved precommit in the
    /// serialized lane". This is the set that step draws from.
    ///
    /// `PREPARED` and `OUTCOME_UNKNOWN`, and the second is not an oversight.
    /// §7.3 has the gateway "reconcile against confirmed TIG state" before any
    /// retry, and an `OUTCOME_UNKNOWN` intent is exactly the one that needs
    /// it: reconciliation is work the gateway owes, so an intent awaiting it
    /// has to be reachable. What reconciliation must not do is *resubmit*
    /// blindly, which is the caller's rule and not this query's.
    ///
    /// Ordered oldest first, so a backlog drains in the order it accrued
    /// rather than by whatever the planner returns.
    fn claimable(
        &self,
        network: Network,
        write_kind: WriteKind,
    ) -> impl Future<Output = Result<Vec<WriteIntent>, IntentError>> + Send;

    /// Settle an intent from confirmed TIG evidence.
    ///
    /// §7.3: "`CONFIRMED` and `REJECTED` are set only from confirmed TIG
    /// reads, never from a transport status." So this takes no HTTP status and
    /// there is no way to call it with one — the caller must hold a confirmed
    /// read, which is why the controller reconciler settles intents and the
    /// gateway does not, even though the gateway is what sent the write.
    ///
    /// Until this existed nothing ever left `OUTCOME_UNKNOWN`: `resolve` set
    /// it and `reconcile` settled only the attempt row, so the intent ledger
    /// recorded a state it could not leave.
    fn settle(
        &self,
        intent_id: &str,
        outcome: &SettledOutcome,
    ) -> impl Future<Output = Result<WriteIntent, IntentError>> + Send;
}

/// The two states §7.3 calls terminal for an intent, each carrying what
/// justifies it.
///
/// A separate type from [`IntentState`] so `settle` cannot be handed
/// `PREPARED` or `OUTCOME_UNKNOWN`: §7.3 says nothing returns to `PREPARED`,
/// and moving *to* `OUTCOME_UNKNOWN` is `resolve`'s, from an ambiguous
/// transport outcome rather than a confirmed read.
///
/// `Confirmed` carries the `benchmark_id` because §7.3 says these states come
/// "only from confirmed TIG reads, never from a transport status", and a
/// caller that has one has read it — `tig_integration.md` §6.1 returns it in a
/// response body, but a *confirmed* id comes from `get-benchmarks`. It does
/// not make the rule unbreakable, and it does mean a caller cannot settle an
/// intent while holding nothing but an HTTP status, which is the mistake the
/// rule exists to prevent. `workflow::confirm_precommit` takes its evidence
/// the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettledOutcome {
    /// TIG's confirmed reads show the write landed, under this benchmark id.
    Confirmed { benchmark_id: String },
    /// TIG's confirmed reads show it did not.
    Rejected,
}

impl SettledOutcome {
    pub fn as_state(&self) -> IntentState {
        match self {
            SettledOutcome::Confirmed { .. } => IntentState::Confirmed,
            SettledOutcome::Rejected => IntentState::Rejected,
        }
    }
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

/// Every column [`row_to_intent`] reads, as one SQL fragment.
///
/// Written once because it was written five times. Adding `trace_id` to the
/// table left four of the five `SELECT`s behind, and the claim path then read
/// intents that parsed fine and carried no trace — a column stored faithfully
/// and delivered nowhere, which is worse than not having it. A reader that
/// projects a subset of these columns cannot be passed to `row_to_intent`, so
/// the list and the function it feeds change together.
///
/// A macro rather than a `const`: sqlx 0.9 wants a `'static` query string, and
/// `concat!` composes literals at compile time where `format!` would not.
macro_rules! intent_columns {
    () => {
        "intent_id::text AS intent_id, network, workflow_id, write_kind,
         generation, benchmark_id, payload_digest, payload_artifact_id,
         trace_id, state"
    };
}
pub(crate) use intent_columns;

/// The column names [`intent_columns`] selects.
///
/// Exists so a test can compare the list against the table's own catalogue: a
/// migration that adds a column the typed intent never reads is otherwise
/// invisible until a caller notices the value is always absent. Derived from
/// the macro rather than written out again, so the two cannot disagree.
pub fn intent_columns_read() -> Vec<&'static str> {
    intent_columns!()
        .split(',')
        .map(|part| {
            // "intent_id::text AS intent_id" -> "intent_id"; everything else
            // is a bare name with surrounding whitespace.
            let part = part.trim();
            match part.rsplit_once(" AS ") {
                Some((_, alias)) => alias.trim(),
                None => part,
            }
        })
        .collect()
}

/// One row, read the same way by every caller.
///
/// `pub(crate)` so `decision::admit_precommit` reads its `RETURNING` through
/// this rather than assembling a `WriteIntent` from the values it bound. Two
/// ways to build one intent is two places for a new column to be forgotten,
/// and echoing bound values back would make the returned intent agree with the
/// caller regardless of what the row holds.
pub(crate) fn row_to_intent(row: &sqlx::postgres::PgRow) -> Result<WriteIntent, IntentError> {
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

    // A stored id that no longer parses is corruption, not an absence: the
    // CHECK constraint makes it unreachable without direct SQL, and reading it
    // as `None` would quietly drop the correlation §10.1 asks for.
    let trace_text: Option<String> = row.try_get("trace_id").map_err(unavailable)?;
    let trace_id = trace_text
        .map(|t| {
            t.parse::<TraceId>()
                .map_err(|e| corrupt(format!("stored trace_id {t:?} does not parse: {e}")))
        })
        .transpose()?;

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
        trace_id,
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
        //
        // `concat!` rather than `format!`: sqlx 0.9 wants a `'static` query
        // string, and this stays a compile-time literal while still sharing
        // one column list with every other statement that feeds
        // `row_to_intent`.
        let inserted = sqlx::query(concat!(
            "INSERT INTO pool.tig_write_intent
                     (network, workflow_id, write_kind, generation, benchmark_id,
                      payload_digest, payload_artifact_id, trace_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (network, workflow_id, write_kind, generation) DO NOTHING
                 RETURNING ",
            intent_columns!(),
        ))
        .bind(new.network.as_str())
        .bind(&new.workflow_id)
        .bind(new.write_kind.as_str())
        .bind(new.generation)
        .bind(&new.benchmark_id)
        .bind(new.payload_digest.as_slice())
        .bind(&new.payload_artifact_id)
        .bind(new.trace_id.map(|t| t.to_hex()))
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
        let row = sqlx::query(concat!(
            "SELECT ",
            intent_columns!(),
            " FROM pool.tig_write_intent
                  WHERE network = $1 AND workflow_id = $2
                    AND write_kind = $3 AND generation = $4",
        ))
        .bind(network.as_str())
        .bind(workflow_id)
        .bind(write_kind.as_str())
        .bind(generation)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;

        row.as_ref().map(row_to_intent).transpose()
    }

    async fn claimable(
        &self,
        network: Network,
        write_kind: WriteKind,
    ) -> Result<Vec<WriteIntent>, IntentError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            intent_columns!(),
            " FROM pool.tig_write_intent
                  WHERE network = $1 AND write_kind = $2
                    AND state IN ('PREPARED', 'OUTCOME_UNKNOWN')
                  ORDER BY created_at, intent_id",
        ))
        .bind(network.as_str())
        .bind(write_kind.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;

        rows.iter().map(row_to_intent).collect()
    }

    async fn settle(
        &self,
        intent_id: &str,
        outcome: &SettledOutcome,
    ) -> Result<WriteIntent, IntentError> {
        // The UPDATE names the states it may move from, so a settled intent
        // matches nothing and the disagreement is reported rather than
        // silently applied. The trigger in `migrations/0003` refuses the
        // retraction too; this is what turns its exception into an answer the
        // caller can act on.
        let row = sqlx::query(concat!(
            "UPDATE pool.tig_write_intent
                    SET state = $2, updated_at = now()
                  WHERE intent_id = $1::uuid
                    AND state IN ('PREPARED', 'OUTCOME_UNKNOWN')
              RETURNING ",
            intent_columns!(),
        ))
        .bind(intent_id)
        .bind(outcome.as_state().as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;

        if let Some(row) = row {
            return row_to_intent(&row);
        }

        // Nothing moved: either the intent is gone or it is already settled.
        // Reading it back is what distinguishes those, and an idempotent
        // re-settle to the same outcome is a success rather than a conflict —
        // a caller that crashed after settling must be able to start again.
        //
        // "Same outcome" compares the state and can compare nothing more: a
        // precommit intent stores no benchmark id, so `Confirmed` under a
        // different id is indistinguishable here from the same settlement
        // repeated. `IntentError::AlreadySettled` says where that case is
        // caught instead.
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT state FROM pool.tig_write_intent WHERE intent_id = $1::uuid",
        )
        .bind(intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;
        let Some(recorded) = existing else {
            return Err(IntentError::NotFound {
                intent_id: intent_id.to_string(),
            });
        };
        if recorded == outcome.as_state().as_str() {
            return self
                .find_by_id(intent_id)
                .await?
                .ok_or_else(|| IntentError::NotFound {
                    intent_id: intent_id.to_string(),
                });
        }
        Err(IntentError::AlreadySettled {
            intent_id: intent_id.to_string(),
            recorded: IntentState::parse(&recorded)
                .map(IntentState::as_str)
                .unwrap_or("an unknown state"),
            asked: outcome.as_state().as_str(),
        })
    }
}

impl PostgresIntentRepository {
    async fn find_by_id(&self, intent_id: &str) -> Result<Option<WriteIntent>, IntentError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            intent_columns!(),
            " FROM pool.tig_write_intent WHERE intent_id = $1::uuid",
        ))
        .bind(intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;
        row.as_ref().map(row_to_intent).transpose()
    }
}

/// What one precommit intent's siblings say: the newest generation this
/// workflow has, and whether any *other* generation has been transmitted.
///
/// The two facts the gateway's claim decision is scoped by
/// (`architecture.md` §7.3 forbids a new generation once an earlier attempt
/// may have reached TIG). Read in one statement so they describe the same
/// instant — a newest generation from one read and a transmitted flag from
/// another could disagree about a generation admitted in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrecommitSiblings {
    pub newest_generation: i32,
    pub sibling_transmitted: bool,
}

/// [`PrecommitSiblings`] for the workflow that owns `intent`, excluding the
/// intent's own attempts from `sibling_transmitted`.
pub async fn precommit_siblings(
    pool: &PgPool,
    intent: &WriteIntent,
) -> Result<PrecommitSiblings, IntentError> {
    // "Transmitted" is `attempt`'s predicate, spliced in rather than
    // restated: this answer and `admit_precommit`'s must agree about which
    // attempts may have reached TIG.
    let row: (Option<i32>, bool) = sqlx::query_as(concat!(
        "SELECT MAX(i.generation),
                COALESCE(bool_or(",
        crate::attempt::intent_has_transmitted_attempt!(),
        " AND i.intent_id <> $3::uuid), false)
           FROM pool.tig_write_intent i
          WHERE i.network = $1 AND i.workflow_id = $2 AND i.write_kind = 'precommit'"
    ))
    .bind(intent.network.as_str())
    .bind(&intent.workflow_id)
    .bind(&intent.intent_id)
    .fetch_one(pool)
    .await
    .map_err(unavailable)?;
    Ok(PrecommitSiblings {
        // The intent exists, so the max is at least its own generation; a
        // NULL here means the read and the intent disagree about the world.
        newest_generation: row.0.unwrap_or(intent.generation),
        sibling_transmitted: row.1,
    })
}
