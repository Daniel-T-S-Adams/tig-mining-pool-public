//! The two facts a benchmark or proof write may not exist without
//! (`architecture.md` §13 invariants 4 and 5, criterion F4a).
//!
//! Invariant 4: no benchmark commitment intent exists before durable package
//! acceptance. Invariant 5: no proof intent exists before a canonical proof
//! payload for the confirmed sample.
//!
//! Enforcement is not here. `migrations/0011` refuses the intent, so the rule
//! holds for every writer rather than for the ones that remembered to call
//! this — which is the difference between an invariant and a convention. What
//! this module owns is *recording* the facts, and the shape of what gets
//! recorded.
//!
//! **These primitives are unguarded, and that is not the finished state.**
//! Nothing in slice 1 may fabricate an acceptance: criterion F4c puts the
//! stub that does behind a test-only cargo feature on the controller crate,
//! proven absent from a default build, and F4d refuses it at runtime unless
//! the TIG endpoint resolves to the local fake. Until that lands there is no
//! controller binary and no non-test caller, so invariants 4 and 5 rest on
//! `migrations/0011` alone — the write cannot happen without a row, but
//! nothing yet constrains who may write the row.
//!
//! Both records are minimal by design. The real durable-acceptance record
//! belongs to the member and upload slice (`pre_build_checklist.md` §10 step
//! 2), which has the assignment, the receipt and the slot release to attach to
//! it; slice 1 has no members. Guessing that shape now would mean step 2
//! migrating away from a schema invented for no consumer. The precondition,
//! though, is slice 1's: a write path that can create a benchmark intent
//! without one is a write path that will.

use pool_domain::Network;

/// A package the pool durably accepted, for one benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAcceptance {
    pub network: Network,
    pub workflow_id: String,
    pub benchmark_id: String,
    /// Whole-package SHA-256 as acceptance recorded it.
    ///
    /// Slice 1 does not re-verify these bytes — that is the artifact worker's
    /// in step 2 — but "acceptance happened" names nothing without saying
    /// which bytes were accepted.
    pub package_sha256: [u8; 32],
}

/// A canonical proof payload built for a confirmed sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPayload {
    pub network: Network,
    /// The deterministic immutable key `architecture.md` §6 publishes derived
    /// payloads under. A proof intent points at this.
    pub artifact_id: String,
    pub workflow_id: String,
    pub benchmark_id: String,
    /// The confirmed sample this payload answers.
    ///
    /// Invariant 5 is about a payload for *the confirmed sample*: one built
    /// for a different sample is as absent as none at all, so the sample is
    /// part of the record rather than context the caller keeps elsewhere.
    pub sample_digest: [u8; 32],
    /// SHA-256 over the payload bytes. The intent carries the same value and
    /// `migrations/0011` requires them to agree, so an intent cannot cite this
    /// payload while transmitting different bytes.
    pub payload_digest: [u8; 32],
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AcceptanceError {
    #[error("acceptance store unavailable: {0}")]
    Unavailable(String),
    /// The same key recorded twice with different contents.
    ///
    /// Refused rather than updated. Both rows are the evidence that a write
    /// was permitted; re-recording one with different bytes would change what
    /// the pool claims to have accepted after the fact, and the write it
    /// authorised may already have gone.
    #[error("{network} {workflow_id}: {what} is already recorded differently")]
    Conflict {
        network: Network,
        workflow_id: String,
        what: &'static str,
    },
}

fn unavailable(e: sqlx::Error) -> AcceptanceError {
    AcceptanceError::Unavailable(e.to_string())
}

/// Record that a package was durably accepted for this benchmark.
///
/// Idempotent for an identical re-record, because the caller that crashes
/// between recording and using it must be able to start again. Not idempotent
/// for a *different* one: see [`AcceptanceError::Conflict`].
pub async fn record_acceptance<'e, E>(
    executor: E,
    accepted: &PackageAcceptance,
) -> Result<(), AcceptanceError>
where
    E: sqlx::PgExecutor<'e>,
{
    // One statement, for two reasons.
    //
    // It takes an executor rather than a pool so a caller can commit this row
    // inside the transaction that produced the fact. `architecture.md` §7.2
    // has the durable artifact pointer, the receipt, the assignment
    // transition, the slot release and the controller event commit together;
    // an acceptance written on its own connection could survive a rollback of
    // the acceptance it records. That is step 2's transaction, but an API that
    // cannot join it is one step 2 has to rework.
    //
    // And a separate follow-up SELECT would have its own answer for a row that
    // changed in between — reporting an availability anomaly as a content
    // conflict. Asking in the same statement removes the gap rather than
    // classifying it. The CTE's insert is not visible to the SELECT beside it
    // (same snapshot), which is exactly right: `same` is only read when
    // nothing was inserted, and then the row predates the statement.
    let row: (bool, Option<bool>) = sqlx::query_as(
        "WITH ins AS (
             INSERT INTO pool.package_acceptance
                 (network, workflow_id, benchmark_id, package_sha256)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (network, workflow_id, benchmark_id) DO NOTHING
             RETURNING 1
         )
         SELECT EXISTS (SELECT 1 FROM ins),
                (SELECT package_sha256 = $4
                   FROM pool.package_acceptance
                  WHERE network = $1 AND workflow_id = $2
                    AND benchmark_id = $3)",
    )
    .bind(accepted.network.as_str())
    .bind(&accepted.workflow_id)
    .bind(&accepted.benchmark_id)
    .bind(accepted.package_sha256.as_slice())
    .fetch_one(executor)
    .await
    .map_err(unavailable)?;

    match row {
        (true, _) => Ok(()),
        (false, Some(true)) => Ok(()),
        (false, Some(false)) => Err(AcceptanceError::Conflict {
            network: accepted.network,
            workflow_id: accepted.workflow_id.clone(),
            what: "package acceptance",
        }),
        // Nothing inserted and nothing visible. This is the racing identical
        // insert: `ON CONFLICT DO NOTHING` waits for the other transaction,
        // skips when it commits, and the sibling `SELECT` then reads with the
        // statement's own older snapshot, which predates that commit. So the
        // row exists and this statement cannot see it.
        //
        // `ON CONFLICT DO UPDATE` would return the row and answer properly,
        // and is deliberately unavailable: it needs UPDATE, which
        // `migrations/0011` withholds because these rows are the immutable
        // evidence that a write was permitted. Keeping them immutable is
        // worth a retry.
        //
        // Reported as an outage rather than a conflict, because the caller's
        // contents were never compared — and it is the honest classification:
        // a retry sees the committed row and succeeds, which a conflict would
        // have told the caller not to attempt.
        (false, None) => Err(AcceptanceError::Unavailable(
            "package acceptance neither inserted nor visible; retry".to_string(),
        )),
    }
}

/// Record a canonical proof payload for a confirmed sample.
///
/// Same idempotency rule as [`record_acceptance`], over the whole record: an
/// artifact id is a content-addressed key, so the same id naming different
/// contents is a collision, not an update.
pub async fn record_canonical_payload<'e, E>(
    executor: E,
    payload: &CanonicalPayload,
) -> Result<(), AcceptanceError>
where
    E: sqlx::PgExecutor<'e>,
{
    // Same shape and the same two reasons as `record_acceptance`.
    let row: (bool, Option<bool>) = sqlx::query_as(
        "WITH ins AS (
             INSERT INTO pool.canonical_payload
                 (network, artifact_id, workflow_id, benchmark_id,
                  sample_digest, payload_digest)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (network, artifact_id) DO NOTHING
             RETURNING 1
         )
         SELECT EXISTS (SELECT 1 FROM ins),
                (SELECT workflow_id = $3 AND benchmark_id = $4
                        AND sample_digest = $5 AND payload_digest = $6
                   FROM pool.canonical_payload
                  WHERE network = $1 AND artifact_id = $2)",
    )
    .bind(payload.network.as_str())
    .bind(&payload.artifact_id)
    .bind(&payload.workflow_id)
    .bind(&payload.benchmark_id)
    .bind(payload.sample_digest.as_slice())
    .bind(payload.payload_digest.as_slice())
    .fetch_one(executor)
    .await
    .map_err(unavailable)?;

    match row {
        (true, _) => Ok(()),
        (false, Some(true)) => Ok(()),
        (false, Some(false)) => Err(AcceptanceError::Conflict {
            network: payload.network,
            workflow_id: payload.workflow_id.clone(),
            what: "canonical payload",
        }),
        // The racing identical insert; see `record_acceptance`.
        (false, None) => Err(AcceptanceError::Unavailable(
            "canonical payload neither inserted nor visible; retry".to_string(),
        )),
    }
}
