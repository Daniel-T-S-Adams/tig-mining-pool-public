//! PostgreSQL adapter for the [`BlockSnapshotStore`] port.

use pool_domain::Network;
use sqlx::{PgPool, Postgres, Row};

use crate::Snapshot;
use crate::store::{
    BlockSnapshotStore, PersistedSnapshot, SnapshotRecord, StoreError, content_digest,
};

/// The accepted-snapshot record in the workflow database.
///
/// Runs as `pool_controller`, which holds `SELECT, INSERT` on the table and
/// deliberately no `UPDATE` or `DELETE` (`migrations/0002_block_snapshot.sql`).
#[derive(Debug, Clone)]
pub struct PostgresSnapshotStore {
    pool: PgPool,
}

impl PostgresSnapshotStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn unavailable(e: sqlx::Error) -> StoreError {
    StoreError::Unavailable(e.to_string())
}

/// A unique violation on the one-usable-snapshot-per-block index.
///
/// Matched by constraint name as well as SQLSTATE: `23505` alone would also
/// catch the primary key, and reporting a crash-retry as a divergent
/// snapshot would send an operator after a contradiction that never
/// happened.
fn is_usable_conflict(e: &sqlx::Error) -> bool {
    let Some(db) = e.as_database_error() else {
        return false;
    };
    db.code().as_deref() == Some("23505") && db.constraint() == Some(USABLE_INDEX)
}

/// Name of the partial unique index in `migrations/0002_block_snapshot.sql`.
const USABLE_INDEX: &str = "block_snapshot_one_usable_per_block";

/// The one row per block that a decision may be made from, if it exists.
///
/// The partial unique index makes "usable" singular, so this needs no
/// ordering or tie-break: there is at most one such row by construction.
async fn fetch_usable<'e, E>(
    executor: E,
    network: Network,
    block_id: &str,
) -> Result<Option<SnapshotRecord>, StoreError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(
        "SELECT height, reads_complete, active_cache_ready, content_digest
         FROM pool.block_snapshot
         WHERE network = $1 AND block_id = $2
           AND reads_complete AND active_cache_ready",
    )
    .bind(network.as_str())
    .bind(block_id)
    .fetch_optional(executor)
    .await
    .map_err(unavailable)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let height: i64 = row.try_get("height").map_err(unavailable)?;
    let height = u64::try_from(height).map_err(|_| StoreError::Corrupt {
        block_id: block_id.to_string(),
        reason: format!("stored height {height} is negative"),
    })?;
    let digest: Vec<u8> = row.try_get("content_digest").map_err(unavailable)?;
    let content_digest: [u8; 32] = digest
        .try_into()
        .map_err(|d: Vec<u8>| StoreError::Corrupt {
            block_id: block_id.to_string(),
            reason: format!("stored digest is {} bytes, expected 32", d.len()),
        })?;

    Ok(Some(SnapshotRecord {
        network,
        block_id: block_id.to_string(),
        height,
        reads_complete: row.try_get("reads_complete").map_err(unavailable)?,
        active_cache_ready: row.try_get("active_cache_ready").map_err(unavailable)?,
        content_digest,
    }))
}

impl BlockSnapshotStore for PostgresSnapshotStore {
    async fn persist(
        &self,
        network: Network,
        snapshot: Snapshot,
    ) -> Result<PersistedSnapshot, StoreError> {
        let content_digest = content_digest(&snapshot)?;
        let height = i64::try_from(snapshot.height).map_err(|_| StoreError::Corrupt {
            block_id: snapshot.block_id.clone(),
            reason: format!("height {} exceeds the stored range", snapshot.height),
        })?;

        let proposed = SnapshotRecord {
            network,
            block_id: snapshot.block_id.clone(),
            height: snapshot.height,
            reads_complete: snapshot.reads_complete,
            active_cache_ready: snapshot.active_cache_ready,
            content_digest,
        };

        // One statement, so the row and its completeness status become
        // visible together or not at all — §9 step 7's "atomically".
        //
        // The conflict target is the primary key, so re-persisting
        // byte-identical content (the crash-retry path, where a process died
        // between the commit and its own acknowledgement) is idempotent. A
        // conflict on any OTHER unique index is deliberately not suppressed:
        // that is how a second, different *usable* snapshot for the block
        // surfaces, and it must not pass silently.
        let inserted = sqlx::query(
            "INSERT INTO pool.block_snapshot
                 (network, block_id, content_digest, height, reads_complete, active_cache_ready)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (network, block_id, content_digest) DO NOTHING",
        )
        .bind(network.as_str())
        .bind(&snapshot.block_id)
        .bind(content_digest.as_slice())
        .bind(height)
        .bind(snapshot.reads_complete)
        .bind(snapshot.active_cache_ready)
        .execute(&self.pool)
        .await;

        if let Err(e) = inserted {
            // A different usable snapshot is already recorded for this
            // block. The block's data is immutable, so two usable assemblies
            // that disagree are a contradiction rather than a supersession —
            // an operator question, not a retry.
            if is_usable_conflict(&e) {
                return Err(StoreError::Divergent {
                    network,
                    block_id: snapshot.block_id,
                });
            }
            return Err(unavailable(e));
        }

        Ok(PersistedSnapshot::new(snapshot, proposed))
    }

    async fn load_usable_record(
        &self,
        network: Network,
        block_id: &str,
    ) -> Result<Option<SnapshotRecord>, StoreError> {
        fetch_usable(&self.pool, network, block_id).await
    }

    async fn last_local_height(&self, network: Network) -> Result<Option<u64>, StoreError> {
        // `reads_complete` only: see the trait's doc. An incomplete assembly
        // records that the block was reached, which is worth keeping, but not
        // that its per-block data was captured — and the gap row exists for
        // the second.
        let height: Option<i64> = sqlx::query_scalar(
            "SELECT max(height) FROM pool.block_snapshot
              WHERE network = $1 AND reads_complete",
        )
        .bind(network.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(unavailable)?;
        height
            .map(|h| {
                u64::try_from(h).map_err(|_| StoreError::Corrupt {
                    block_id: String::new(),
                    reason: format!("stored height {h} is negative"),
                })
            })
            .transpose()
    }
}
