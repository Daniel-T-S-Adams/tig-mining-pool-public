//! Take one block in: assemble the snapshot, warm the active-benchmark
//! cache for it, record what was missed, persist.
//!
//! `tig_integration.md` §9 is the assembly, §9 step 4 and §5.2 are the cache,
//! and §10's last paragraph is the gap rule; this is where they meet. The pool learns a block was missed
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
use pool_snapshot::active_cache::{
    ActiveBenchmarkStore, Advance, BenchmarkDataSource, StoreError as CacheError, advance,
};
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
    /// What §9 step 4 did for this block's active set. Its coverage is the
    /// snapshot's `active_cache_ready`.
    pub cache: Advance,
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("snapshot store: {0}")]
    Store(#[from] StoreError),
    #[error("active-benchmark cache: {0}")]
    Cache(#[from] CacheError),
    #[error("recording a block data gap: {0}")]
    Gap(#[from] WorkflowError),
    /// The chain's height does not fit the stored range. Not a gap: a gap
    /// is a height the pool did not see, and this one it cannot represent.
    #[error("block height {height} is outside the range the pool records")]
    HeightOutOfRange { height: u64 },
}

/// Everything ingestion reads and writes through, borrowed.
pub struct Ingestor<'a, S, T, C> {
    /// For the gap rows. A separate handle from the store because
    /// `record_block_gap` is `pool-workflow`'s and the store is
    /// `pool-snapshot`'s; both reach the same database as the same role.
    pub pool: &'a PgPool,
    pub store: &'a T,
    pub cache: &'a C,
    pub source: &'a S,
    pub network: Network,
    /// How many times one assembly may restart at a new block before this
    /// ingestion gives up. §9 discards a snapshot the chain moved under;
    /// the next poll tries again, so this bounds one poll's patience.
    pub assembly_attempts: u32,
    /// `get-benchmark-data` reads one pass may spend on the cache.
    pub cache_budget: usize,
}

impl<S, T, C> Ingestor<'_, S, T, C>
where
    S: SnapshotSource + BenchmarkDataSource,
    T: BlockSnapshotStore,
    C: ActiveBenchmarkStore,
{
    /// Assemble the current block's snapshot, advance the active-benchmark
    /// cache for its active set, record any gap since the last local
    /// height, and persist the snapshot with its readiness — in that order.
    pub async fn ingest(&self) -> Result<Ingested, IngestError> {
        let last_local = self.store.last_local_height(self.network).await?;
        let mut snapshot: Snapshot = assemble(self.source, self.assembly_attempts).await?;
        let observed =
            i64::try_from(snapshot.height).map_err(|_| IngestError::HeightOutOfRange {
                height: snapshot.height,
            })?;

        // §9 step 4, against this block's active set, before step 7 persists
        // the readiness it decides. See `pool_snapshot::active_cache` for why
        // it runs here rather than inside the anchored section.
        let cache = self.warm_cache(&snapshot).await?;
        snapshot.active_cache_ready = cache.covers_active_set();

        let gaps_recorded = match last_local {
            Some(last) => {
                let last = i64::try_from(last)
                    .map_err(|_| IngestError::HeightOutOfRange { height: last })?;
                record_block_gap(self.pool, self.network, last, observed).await?
            }
            // Nothing local to measure from: the first block the pool ever
            // observes is not preceded by a gap, whatever its height.
            None => Vec::new(),
        };

        let snapshot = self.store.persist(self.network, snapshot).await?;
        Ok(Ingested {
            snapshot,
            gaps_recorded,
            cache,
        })
    }

    /// Spend one budget of fetches on the ids `snapshot`'s block lists as
    /// active and the cache does not yet hold.
    ///
    /// Callable again for a block already taken in: a warm-up that did not
    /// finish in one pass continues on the next poll rather than waiting
    /// for the next block.
    pub async fn warm_cache(&self, snapshot: &Snapshot) -> Result<Advance, IngestError> {
        let active = snapshot.active_benchmark_ids()?;
        Ok(advance(
            self.source,
            self.cache,
            self.network,
            &active,
            snapshot.height,
            self.cache_budget,
        )
        .await?)
    }
}
