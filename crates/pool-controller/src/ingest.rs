//! Take one block in: assemble the snapshot, record what was missed, persist.
//!
//! `tig_integration.md` §9 is the assembly and §10's last paragraph is the
//! gap rule; this is where the two meet. The pool learns a block was missed
//! at the moment it accepts the next one — the newest accepted height is
//! more than one above the last local height — so the gap is recorded as
//! part of accepting that block, not by a sweep that might never run.
//!
//! The order is the point. The gap rows go in **before** the snapshot that
//! revealed them is persisted, because "last local height" is read from the
//! persisted snapshots: persist first and crash, and the next run measures
//! from the new height with nothing recorded for the ones between it and the
//! old. Gap first and crash, and the next run re-observes the same gap —
//! `record_block_gap` is idempotent — and then persists. There is no order
//! in which the loss goes unrecorded.

use pool_domain::Network;
use pool_snapshot::store::{BlockSnapshotStore, PersistedSnapshot, StoreError};
use pool_snapshot::{Snapshot, SnapshotError, SnapshotSource, assemble};
use pool_workflow::{WorkflowError, record_block_gap};
use sqlx::PgPool;

/// One accepted block and what accepting it recorded.
#[derive(Debug)]
pub struct Ingested {
    pub snapshot: PersistedSnapshot,
    /// Heights newly recorded as data gaps by this ingestion. Empty on a
    /// consecutive block, on the first block ever observed, and when a
    /// previously recorded gap is seen again.
    pub gaps_recorded: Vec<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("snapshot store: {0}")]
    Store(#[from] StoreError),
    #[error("recording a block data gap: {0}")]
    Gap(#[from] WorkflowError),
    /// The chain's height does not fit the stored range. Not a gap: a gap
    /// is a height the pool did not see, and this one it cannot represent.
    #[error("block height {height} is outside the range the pool records")]
    HeightOutOfRange { height: u64 },
}

/// Assemble the current block's snapshot, record any gap since the last
/// local height, and persist the snapshot — in that order.
///
/// `pool` is for the gap rows. It is a separate handle from the store because
/// `record_block_gap` is `pool-workflow`'s and the store is `pool-snapshot`'s;
/// both are expected to reach the same database as the same role.
pub async fn ingest<S, T>(
    pool: &PgPool,
    store: &T,
    source: &S,
    network: Network,
    max_attempts: u32,
) -> Result<Ingested, IngestError>
where
    S: SnapshotSource,
    T: BlockSnapshotStore,
{
    let last_local = store.last_local_height(network).await?;
    let snapshot: Snapshot = assemble(source, max_attempts).await?;
    let observed = i64::try_from(snapshot.height).map_err(|_| IngestError::HeightOutOfRange {
        height: snapshot.height,
    })?;

    let gaps_recorded = match last_local {
        Some(last) => {
            let last =
                i64::try_from(last).map_err(|_| IngestError::HeightOutOfRange { height: last })?;
            record_block_gap(pool, network, last, observed).await?
        }
        // Nothing local to measure from: the first block the pool ever
        // observes is not preceded by a gap, whatever its height.
        None => Vec::new(),
    };

    let snapshot = store.persist(network, snapshot).await?;
    Ok(Ingested {
        snapshot,
        gaps_recorded,
    })
}
