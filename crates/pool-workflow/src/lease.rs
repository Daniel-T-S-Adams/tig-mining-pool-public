//! Leases and fence tokens for long work
//! (`architecture.md` §7.5, invariant 8, criterion G3).
//!
//! Short transitions lock the workflow row and use its revision
//! ([`crate::workflow`]). This is the other half: work that cannot hold a
//! transaction open because it makes network calls or moves bulk artifacts
//! (§7.2 forbids a transaction spanning either).
//!
//! The sequence is §7.5's, and step 4 is what the fence exists for:
//!
//! 1. claim in a short transaction, incrementing the fence;
//! 2. do the work with nothing open;
//! 3. commit only with a compare-and-set on **both** the workflow revision and
//!    the exact fence token; and
//! 4. after expiry another process reclaims with a higher fence, which makes a
//!    late result from the old owner unable to commit.
//!
//! An expiry without a fence would not be enough. Expiry proves the old owner
//! *should* have stopped, not that it *did* — a process stalled in a network
//! call has no idea its lease lapsed, and would otherwise come back and commit
//! over whoever took the work.

use pool_domain::Network;
use sqlx::{PgPool, Row};

/// The kinds of long work slice 1 leases.
///
/// A closed set rather than free text: a typo'd kind would silently create a
/// second lease row for the same work, and two processes would each hold "the"
/// lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseKind {
    /// §10's serialized precommit submission lane, which §7.5 names as a
    /// singleton activity that uses a lease.
    PrecommitTransmit,
    ProofBuild,
    PackageIngest,
}

impl LeaseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseKind::PrecommitTransmit => "PRECOMMIT_TRANSMIT",
            LeaseKind::ProofBuild => "PROOF_BUILD",
            LeaseKind::PackageIngest => "PACKAGE_INGEST",
        }
    }
}

/// A held lease. The fence is what a later commit must present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub network: Network,
    pub workflow_id: String,
    pub kind: LeaseKind,
    pub fence_token: i64,
    pub lease_owner: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("lease store unavailable: {0}")]
    Unavailable(String),
    /// Someone else holds it and it has not expired.
    #[error(
        "{network} {workflow_id}/{kind} is held by {holder} for another \
         {remaining_secs}s"
    )]
    StillHeld {
        network: Network,
        workflow_id: String,
        kind: &'static str,
        holder: String,
        remaining_secs: i64,
    },
    /// §7.5 step 3: the fence or the revision moved.
    #[error(
        "{network} {workflow_id}/{kind}: fence {presented} is stale, the \
         lease is now at {current}"
    )]
    FenceLost {
        network: Network,
        workflow_id: String,
        kind: &'static str,
        presented: i64,
        current: i64,
    },
    #[error("lease duration must be positive")]
    ZeroDuration,
}

fn unavailable(e: sqlx::Error) -> LeaseError {
    LeaseError::Unavailable(e.to_string())
}

/// §7.5 step 1: claim the work, incrementing the fence.
///
/// Refuses while another owner's lease is live. Once it has expired, the claim
/// succeeds and takes a **higher** fence — which is what step 4 relies on, and
/// why the fence is never reset by a change of owner.
pub async fn claim(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    kind: LeaseKind,
    owner: &str,
    duration_secs: i64,
) -> Result<Lease, LeaseError> {
    if duration_secs <= 0 {
        return Err(LeaseError::ZeroDuration);
    }

    // One statement, so the read and the increment cannot interleave. A
    // claimant that lost the race updates nothing and reads the holder below.
    let row = sqlx::query(
        "INSERT INTO pool.work_lease
             (network, workflow_id, lease_kind, lease_owner, lease_until)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5::double precision))
         ON CONFLICT (network, workflow_id, lease_kind) DO UPDATE
             SET fence_token = pool.work_lease.fence_token + 1,
                 lease_owner = EXCLUDED.lease_owner,
                 lease_until = EXCLUDED.lease_until
             WHERE pool.work_lease.lease_until <= now()
                OR pool.work_lease.lease_owner = EXCLUDED.lease_owner
         RETURNING fence_token, lease_owner",
    )
    .bind(network.as_str())
    .bind(workflow_id)
    .bind(kind.as_str())
    .bind(owner)
    .bind(duration_secs as f64)
    .fetch_optional(pool)
    .await
    .map_err(unavailable)?;

    if let Some(row) = row {
        return Ok(Lease {
            network,
            workflow_id: workflow_id.to_string(),
            kind,
            fence_token: row.try_get("fence_token").map_err(unavailable)?,
            lease_owner: row.try_get("lease_owner").map_err(unavailable)?,
        });
    }

    // The conflict clause declined: someone else's lease is still live.
    let held = sqlx::query(
        "SELECT lease_owner,
                GREATEST(0, EXTRACT(EPOCH FROM (lease_until - now()))::bigint) AS remaining
         FROM pool.work_lease
         WHERE network = $1 AND workflow_id = $2 AND lease_kind = $3",
    )
    .bind(network.as_str())
    .bind(workflow_id)
    .bind(kind.as_str())
    .fetch_one(pool)
    .await
    .map_err(unavailable)?;

    Err(LeaseError::StillHeld {
        network,
        workflow_id: workflow_id.to_string(),
        kind: kind.as_str(),
        holder: held.try_get("lease_owner").map_err(unavailable)?,
        remaining_secs: held.try_get("remaining").map_err(unavailable)?,
    })
}

/// §7.5 step 3: prove the fence is still the current one.
///
/// Deliberately a *check* rather than a wrapper that also commits: what a
/// result commit looks like differs per kind of work, and the one thing they
/// share is that it must happen in the same transaction as this check. Callers
/// pass their own transaction so the two cannot come apart.
pub async fn require_fence(tx: &mut sqlx::PgConnection, lease: &Lease) -> Result<(), LeaseError> {
    let current: Option<i64> = sqlx::query_scalar(
        "SELECT fence_token FROM pool.work_lease
         WHERE network = $1 AND workflow_id = $2 AND lease_kind = $3
         FOR UPDATE",
    )
    .bind(lease.network.as_str())
    .bind(&lease.workflow_id)
    .bind(lease.kind.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;

    // A vanished row is a lost fence, not a free pass. The alternative —
    // treating "no lease" as "nobody objects" — would let a late writer commit
    // precisely when the bookkeeping was in the worst state.
    let current = current.unwrap_or(i64::MAX);
    if current != lease.fence_token {
        return Err(LeaseError::FenceLost {
            network: lease.network,
            workflow_id: lease.workflow_id.clone(),
            kind: lease.kind.as_str(),
            presented: lease.fence_token,
            current,
        });
    }
    Ok(())
}

/// Release a lease early. The fence still advances, so a result from the
/// released holder cannot commit afterwards.
pub async fn release(pool: &PgPool, lease: &Lease) -> Result<(), LeaseError> {
    sqlx::query(
        "UPDATE pool.work_lease
         SET fence_token = fence_token + 1, lease_until = now()
         WHERE network = $1 AND workflow_id = $2 AND lease_kind = $3
           AND fence_token = $4",
    )
    .bind(lease.network.as_str())
    .bind(&lease.workflow_id)
    .bind(lease.kind.as_str())
    .bind(lease.fence_token)
    .execute(pool)
    .await
    .map_err(unavailable)?;
    Ok(())
}
