//! The gateway's write-attempt ledger (`architecture.md` §3, §7.3).
//!
//! §7.3 fixes the shape: the gateway "records every attempt before sending,
//! then records the response separately". That ordering is the whole point —
//! a request that was sent and never answered has to leave a trace, because
//! `tig_integration.md` §10 reconciles a write before retrying it and can
//! only do that for attempts it knows happened.
//!
//! An unresolved attempt is one with no outcome **or an ambiguous one**: a
//! lost response is the case §10's lane exists for, and §11 forbids a
//! replacement precommit while the previous outcome is ambiguous. The lane
//! reopens when reconciliation settles the ambiguity, not when the ambiguity
//! is merely recorded. §10's precommit lane and
//! §11's benchmark rule are both expressed against that state, as partial
//! unique indexes rather than as checks in this code: a second unresolved
//! precommit is then impossible for any writer, not merely refused by the
//! one path that remembered to look.

use std::future::Future;

use sqlx::{PgPool, Row};

use crate::intent::IntentState;

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// TIG answered and accepted the write. Note that this does not confirm
    /// the workflow: §7 advances local state only from confirmed reads.
    Accepted,
    /// TIG answered and refused the write.
    Rejected,
    /// No usable answer. The write may or may not have reached TIG, so §7.3
    /// leaves the intent `OUTCOME_UNKNOWN` and §10 reconciles rather than
    /// resending.
    Ambiguous,
}

impl AttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptOutcome::Accepted => "ACCEPTED",
            AttemptOutcome::Rejected => "REJECTED",
            AttemptOutcome::Ambiguous => "AMBIGUOUS",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "ACCEPTED" => Some(AttemptOutcome::Accepted),
            "REJECTED" => Some(AttemptOutcome::Rejected),
            "AMBIGUOUS" => Some(AttemptOutcome::Ambiguous),
            _ => None,
        }
    }
}

/// One recorded attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAttempt {
    pub attempt_id: String,
    pub intent_id: String,
    pub attempt_no: i32,
    pub outcome: Option<AttemptOutcome>,
    pub http_status: Option<i32>,
    /// Whether §10 reconciliation has settled this attempt's ambiguity.
    pub reconciled: bool,
}

impl WriteAttempt {
    /// Whether this attempt still occupies its lane — the state §10's rules
    /// key on.
    ///
    /// An AMBIGUOUS outcome counts as unresolved. The partial indexes say so
    /// and this module's own definition says so; returning
    /// `outcome.is_none()` told a caller the lane was free at exactly the
    /// moment §11 forbids a replacement precommit. The index would still
    /// have refused the write, so the bug was in what the send path would
    /// have *decided*, not in what it could do.
    pub fn is_unresolved(&self) -> bool {
        matches!(self.outcome, None | Some(AttemptOutcome::Ambiguous))
    }

    /// Whether a response was recorded at all, ambiguous or not.
    pub fn has_response(&self) -> bool {
        self.outcome.is_some()
    }
}

/// "This workflow has a TIG write whose fate is unknown", as SQL.
///
/// A correlated `EXISTS` over `$1` (network) and `$2` (workflow_id), meant to
/// be spliced into a larger statement. It is a fragment rather than a function
/// because the only correct place to ask it is *inside* the write it guards:
/// `begin` inserts into `tig_write_attempt` and touches no row in
/// `pool.workflow`, so a caller that read the answer first and acted on it
/// after would be acting on a fact the database may have changed in between,
/// and no lock available to that caller closes the gap. Asking it as part of
/// the UPDATE makes the database evaluate both against one snapshot.
///
/// One definition for one rule. A second copy — a predicate read beforehand,
/// say — would be a rule stated twice, free to drift, and this module has
/// already had that bug.
///
/// **Every write kind.** §7.3 and §12 describe a gateway that dies before or
/// after a response and leaves an attempt "pending or unknown" — a fact about
/// a request, not about which endpoint it went to. A benchmark or proof write
/// stranded that way is as unsettled as a precommit, and a caller that ended
/// the workflow past it would leave the ambiguity on a terminal row that §10
/// step 1 never reloads, closing that lane with nothing scheduled to settle
/// it.
///
/// It reads **attempts only**, deliberately. `tig_write_intent.state` carries
/// `OUTCOME_UNKNOWN`, which looks like the same fact, but it is written by
/// `resolve` in the same transaction that marks the attempt `AMBIGUOUS` — a
/// second copy of the attempt's own state rather than independent evidence —
/// and `reconcile` settles only the attempt. So the intent keeps saying
/// `OUTCOME_UNKNOWN` after §10 has established what happened, and a condition
/// that read it would hold a workflow open for the life of the database once
/// any of its writes had gone ambiguous.
///
/// It is an `EXISTS` over attempts rather than a join that admits a null
/// outcome: `begin` writes the attempt row *before* the request leaves (see
/// this module's header), so zero attempt rows is positive evidence that
/// nothing reached TIG — the opposite of unsettled. A `LEFT JOIN … outcome IS
/// NULL` says the same thing about a workflow that never transmitted, and
/// `admit_precommit` gives every workflow an intent from birth.
///
/// A macro rather than a `const` so callers can `concat!` it into a statement
/// that is still a literal. sqlx 0.9 refuses a runtime-built query string
/// without an explicit `AssertSqlSafe`, and asserting safety is a worse answer
/// than not building one: this way the composed SQL is fixed at compile time
/// and there is nothing to audit.
macro_rules! unsettled_write_exists {
    () => {
        "EXISTS (
             SELECT 1
               FROM pool.tig_write_intent i
               JOIN pool.tig_write_attempt a ON a.intent_id = i.intent_id
              WHERE i.network = $1 AND i.workflow_id = $2
                AND (a.outcome IS NULL OR a.outcome = 'AMBIGUOUS')
         )"
    };
}
pub(crate) use unsettled_write_exists;

/// Whether a precommit write for `workflow_id` ever left the gateway.
///
/// A different question from [`unsettled_write_exists`], and kept separate on
/// purpose. That one asks whether a write's fate is unknown, so the pool's
/// clock must not decide it. This one asks whether a write reached TIG at
/// all, so §10's tuple search is owed for its result.
///
/// The two diverge exactly on an ACCEPTED attempt. TIG answered, so nothing
/// is unsettled — but §6.1 returns the assigned `benchmark_id` in that
/// response and nothing durable holds it: a precommit intent structurally
/// cannot carry one (`migrations/0003` D1a) and an attempt's `detail` may
/// never hold response bytes (`migrations/0004`). A crash between the
/// response and `confirm_precommit` therefore loses the id of a benchmark
/// TIG has already created and charged a fee for, and the only way back to it
/// is E4's search over the exact submitted tuple. Collapsing the two
/// questions into one predicate would either expire a workflow whose write
/// landed, or leave the search unrequested — and `admit_precommit` would
/// then read the workflow as having sent nothing and admit a second
/// precommit for the same decision.
///
/// An attempt row is the evidence, because `begin` writes it before the
/// request leaves. Zero attempt rows means nothing was sent.
///
/// `REJECTED` is excluded. TIG answered and refused, so no benchmark exists to
/// find and the tuple search would return nothing forever — filling the
/// stop-for-operator bucket that only works while it stays quiet. What such a
/// workflow is owed is `fail`, not a search.
pub async fn has_transmitted_precommit_write(
    pool: &PgPool,
    network: &str,
    workflow_id: &str,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> =
        sqlx::query_scalar(concat!("SELECT 1 WHERE ", transmitted_precommit_exists!()))
            .bind(network)
            .bind(workflow_id)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

/// The same question as [`has_transmitted_precommit_write`], as SQL.
///
/// A correlated `EXISTS` over `$1` (network) and `$2` (workflow_id). It exists
/// as a fragment for the same reason `unsettled_write_exists!` does: a caller
/// deciding whether it may write must ask *inside* that write.
/// `PostgresAttemptLedger::begin` inserts into `tig_write_attempt` and touches
/// no row a workflow or decision transaction holds a lock on, so an attempt
/// starting between a read and the write is invisible to anything read
/// beforehand.
///
/// `decision::admit_precommit` uses it that way, and the function above reads
/// the same text, so the two cannot answer differently.
macro_rules! transmitted_precommit_exists {
    () => {
        "EXISTS (
             SELECT 1
               FROM pool.tig_write_intent i
              WHERE i.network = $1 AND i.workflow_id = $2
                AND i.write_kind = 'precommit'
                AND EXISTS (
                     SELECT 1 FROM pool.tig_write_attempt a
                      WHERE a.intent_id = i.intent_id
                        AND (a.outcome IS NULL
                             OR a.outcome IN ('AMBIGUOUS', 'ACCEPTED'))
                    )
         )"
    };
}
pub(crate) use transmitted_precommit_exists;

#[derive(Debug, thiserror::Error)]
pub enum AttemptError {
    #[error("attempt ledger unavailable: {0}")]
    Unavailable(String),
    /// §10: only one unresolved precommit may occupy the serialized lane.
    #[error("a precommit attempt is already unresolved in the {network} lane")]
    PrecommitLaneOccupied { network: String },
    /// §11: never two concurrent writes for one benchmark.
    #[error("a write for benchmark {benchmark_id} is already unresolved")]
    BenchmarkWriteInFlight { benchmark_id: String },
    /// §7.3: the response is recorded once.
    #[error("attempt {attempt_id} already has an outcome")]
    AlreadyResolved { attempt_id: String },
    /// Reconciliation must produce a definitive finding.
    #[error("attempt {attempt_id}: reconciliation settles an ambiguity, it cannot restate it")]
    NotSettled { attempt_id: String },
    /// There was no ambiguity to settle.
    #[error("attempt {attempt_id} is not ambiguous; there is nothing to reconcile")]
    NotAmbiguous { attempt_id: String },
    #[error("stored attempt {attempt_id} is unreadable: {reason}")]
    Corrupt { attempt_id: String, reason: String },
}

/// The write-attempt ledger.
pub trait WriteAttemptLedger {
    /// Record an attempt **before** the request is sent.
    fn begin(
        &self,
        intent_id: &str,
    ) -> impl Future<Output = Result<WriteAttempt, AttemptError>> + Send;

    /// Record the response, separately from the attempt.
    ///
    /// An ambiguous outcome also moves the intent to `OUTCOME_UNKNOWN`, in
    /// the same transaction: a crash between the two would leave a write
    /// whose outcome nobody knows looking like one that was never attempted.
    fn resolve(
        &self,
        attempt_id: &str,
        outcome: AttemptOutcome,
        http_status: Option<i32>,
        detail: Option<&str>,
    ) -> impl Future<Output = Result<(), AttemptError>> + Send;

    /// Settle an ambiguous attempt with what reconciliation established.
    ///
    /// §10 resolves a lost response by searching confirmed TIG state, never
    /// by resending. This records that finding, and it is what reopens the
    /// serialized precommit lane — an attempt left AMBIGUOUS keeps the lane
    /// closed, which is §11's rule that no replacement precommit is issued
    /// while the previous outcome is ambiguous.
    ///
    /// Slice-1 criterion E4 supplies the search itself; this is the ledger
    /// side of it.
    fn reconcile(
        &self,
        attempt_id: &str,
        settled: AttemptOutcome,
    ) -> impl Future<Output = Result<(), AttemptError>> + Send;

    /// Attempts for one intent, oldest first.
    fn attempts_for(
        &self,
        intent_id: &str,
    ) -> impl Future<Output = Result<Vec<WriteAttempt>, AttemptError>> + Send;
}

#[derive(Debug, Clone)]
pub struct PostgresAttemptLedger {
    pool: PgPool,
}

impl PostgresAttemptLedger {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn unavailable(e: sqlx::Error) -> AttemptError {
    AttemptError::Unavailable(e.to_string())
}

/// Names of the partial unique indexes in
/// `migrations/0004_tig_write_attempt.sql`.
const PRECOMMIT_LANE_INDEX: &str = "tig_write_attempt_one_unresolved_precommit";
const BENCHMARK_INDEX: &str = "tig_write_attempt_one_unresolved_per_benchmark";

fn row_to_attempt(row: &sqlx::postgres::PgRow) -> Result<WriteAttempt, AttemptError> {
    let attempt_id: String = row.try_get("attempt_id").map_err(unavailable)?;
    let outcome_text: Option<String> = row.try_get("outcome").map_err(unavailable)?;
    let outcome = match outcome_text {
        None => None,
        Some(text) => Some(
            AttemptOutcome::parse(&text).ok_or_else(|| AttemptError::Corrupt {
                attempt_id: attempt_id.clone(),
                reason: format!("unknown outcome {text}"),
            })?,
        ),
    };
    Ok(WriteAttempt {
        attempt_id,
        intent_id: row.try_get("intent_id").map_err(unavailable)?,
        attempt_no: row.try_get("attempt_no").map_err(unavailable)?,
        outcome,
        http_status: row.try_get("http_status").map_err(unavailable)?,
        reconciled: row.try_get("reconciled").map_err(unavailable)?,
    })
}

impl WriteAttemptLedger for PostgresAttemptLedger {
    async fn begin(&self, intent_id: &str) -> Result<WriteAttempt, AttemptError> {
        // attempt_no is derived in the statement rather than by the caller,
        // so a retry cannot renumber history.
        let row = sqlx::query(
            "INSERT INTO pool.tig_write_attempt (intent_id, attempt_no)
             SELECT $1::uuid, COALESCE(MAX(attempt_no), 0) + 1
               FROM pool.tig_write_attempt WHERE intent_id = $1::uuid
             RETURNING attempt_id::text AS attempt_id, intent_id::text AS intent_id,
                       attempt_no, outcome, http_status,
                       (reconciled_at IS NOT NULL) AS reconciled",
        )
        .bind(intent_id)
        .fetch_one(&self.pool)
        .await;

        match row {
            Ok(row) => row_to_attempt(&row),
            Err(e) => {
                // The lane rules surface as unique violations on the partial
                // indexes. Reported by name so the caller learns which rule
                // stopped it, not merely that the database refused.
                if let Some(db) = e.as_database_error()
                    && db.code().as_deref() == Some("23505")
                    && matches!(
                        db.constraint(),
                        Some(PRECOMMIT_LANE_INDEX | BENCHMARK_INDEX)
                    )
                {
                    let which = db.constraint().unwrap_or_default().to_string();
                    // One extra read, only on the refusal path, so the error
                    // names the lane that is occupied. An error that said
                    // only "a write is already unresolved" would send an
                    // operator looking for which.
                    let identity = sqlx::query(
                        "SELECT network, benchmark_id FROM pool.tig_write_intent
                          WHERE intent_id = $1::uuid",
                    )
                    .bind(intent_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(unavailable)?;
                    let network: String = identity
                        .as_ref()
                        .and_then(|r| r.try_get("network").ok())
                        .unwrap_or_else(|| "unknown".to_string());
                    let benchmark_id: String = identity
                        .as_ref()
                        .and_then(|r| r.try_get::<Option<String>, _>("benchmark_id").ok())
                        .flatten()
                        .unwrap_or_else(|| "unknown".to_string());

                    return Err(if which == PRECOMMIT_LANE_INDEX {
                        AttemptError::PrecommitLaneOccupied { network }
                    } else {
                        AttemptError::BenchmarkWriteInFlight { benchmark_id }
                    });
                }
                Err(unavailable(e))
            }
        }
    }

    async fn resolve(
        &self,
        attempt_id: &str,
        outcome: AttemptOutcome,
        http_status: Option<i32>,
        detail: Option<&str>,
    ) -> Result<(), AttemptError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;

        let updated = sqlx::query(
            "UPDATE pool.tig_write_attempt
                SET outcome = $2, http_status = $3, detail = $4, resolved_at = now()
              WHERE attempt_id = $1::uuid AND outcome IS NULL
              RETURNING intent_id::text AS intent_id",
        )
        .bind(attempt_id)
        .bind(outcome.as_str())
        .bind(http_status)
        .bind(detail)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;

        let Some(row) = updated else {
            // Either no such attempt, or one that already has an outcome.
            // §7.3 records a response once, so both are refusals rather than
            // no-ops the caller could mistake for success.
            return Err(AttemptError::AlreadyResolved {
                attempt_id: attempt_id.to_string(),
            });
        };
        let intent_id: String = row.try_get("intent_id").map_err(unavailable)?;

        if outcome == AttemptOutcome::Ambiguous {
            // In the same transaction: a crash between recording the
            // ambiguity and marking the intent would leave a write whose
            // outcome nobody knows looking like one never attempted.
            //
            // Only from PREPARED. An intent the reconciler has already
            // confirmed stays confirmed — the schema refuses the retraction
            // anyway, and skipping it here keeps this from being the path
            // that discovers that.
            sqlx::query(
                "UPDATE pool.tig_write_intent
                    SET state = $2
                  WHERE intent_id = $1::uuid AND state = 'PREPARED'",
            )
            .bind(&intent_id)
            .bind(IntentState::OutcomeUnknown.as_str())
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }

        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    async fn reconcile(
        &self,
        attempt_id: &str,
        settled: AttemptOutcome,
    ) -> Result<(), AttemptError> {
        if settled == AttemptOutcome::Ambiguous {
            return Err(AttemptError::NotSettled {
                attempt_id: attempt_id.to_string(),
            });
        }
        // resolved_at is left alone: it records when the outcome BECAME
        // ambiguous, which is the fact §10.3's "ambiguous for more than two
        // target blocks" page reads. http_status and detail are left alone
        // too — they describe the lost response, and reconciliation
        // establishes what happened at TIG, not what the transport said.
        let updated = sqlx::query(
            "UPDATE pool.tig_write_attempt
                SET outcome = $2, reconciled_at = now()
              WHERE attempt_id = $1::uuid AND outcome = 'AMBIGUOUS'",
        )
        .bind(attempt_id)
        .bind(settled.as_str())
        .execute(&self.pool)
        .await
        .map_err(unavailable)?;

        if updated.rows_affected() == 0 {
            // Not ambiguous, or gone. Either way there is nothing for
            // reconciliation to settle, and reporting success would tell the
            // caller the lane had reopened when it had not.
            return Err(AttemptError::NotAmbiguous {
                attempt_id: attempt_id.to_string(),
            });
        }
        Ok(())
    }

    async fn attempts_for(&self, intent_id: &str) -> Result<Vec<WriteAttempt>, AttemptError> {
        let rows = sqlx::query(
            "SELECT attempt_id::text AS attempt_id, intent_id::text AS intent_id,
                    attempt_no, outcome, http_status,
                       (reconciled_at IS NOT NULL) AS reconciled
               FROM pool.tig_write_attempt
              WHERE intent_id = $1::uuid
              ORDER BY attempt_no",
        )
        .bind(intent_id)
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;

        rows.iter().map(row_to_attempt).collect()
    }
}
