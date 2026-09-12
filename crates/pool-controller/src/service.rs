//! The controller's poll: notice a new block, take it in, reconcile from it.
//!
//! `tig_integration.md` §11 pins the `get-block` poll interval and §10 makes
//! a missed height unrecoverable, so the loop's one job is to see every
//! block. Each poll reads the latest block; when its id differs from the
//! last one taken in, the block is ingested (`crate::ingest`) and reconciled
//! (`crate::reconciler`). A block seen twice is not reconciled twice — every
//! step is idempotent, but a pass costs database reads and the evidence has
//! not changed.
//!
//! A poll that finds the block unchanged is not idle if the block's
//! active-benchmark cache is still warming: it spends another budget of
//! fetches on it (§5.2's warm-up "may span several blocks", but need not
//! wait a block between passes), and once every active id is retained the
//! same snapshot is persisted again as usable. That is the supersession
//! `pool.block_snapshot` was designed for: the block's content has not
//! changed, only what the pool knows about its active set.
//!
//! The poll and the assembly share one host, so they share one limiter
//! (`tig-client` keys it by host); the poll client only differs in its
//! deadlines, which `ReadPolicy::for_block_poll` bounds to the interval so a
//! slow poll cannot run into the next one.

use std::time::{Duration, Instant};

use pool_domain::Network;
use pool_snapshot::active_cache::{Advance, BenchmarkDataSource};
use pool_snapshot::store::BlockSnapshotStore;
use pool_snapshot::{
    PostgresActiveBenchmarkStore, PostgresSnapshotStore, Snapshot, SnapshotSource,
};
use pool_workflow::Guardrails;
use sqlx::PgPool;
use tig_client::{ReadError, TigReadClient};

use crate::ingest::{IngestError, Ingestor};
use crate::reconciler::{Outcome, ReconcileError, reconcile_block};

/// How many times one ingestion may restart at a new block before giving
/// up on this poll. §9 discards a snapshot the chain moved under; the next
/// poll tries again, so this bounds one poll's patience, not the pool's.
const ASSEMBLY_ATTEMPTS: u32 = 3;

pub struct Service<S> {
    pool: PgPool,
    store: PostgresSnapshotStore,
    cache: PostgresActiveBenchmarkStore,
    source: S,
    poll: TigReadClient,
    network: Network,
    player_id: String,
    guardrails: Guardrails,
    cache_budget: usize,
    /// The block most recently taken in *and reconciled from*, so a poll
    /// that sees it again does nothing more than continue its warm-up.
    ///
    /// A block whose reads came back incomplete is not recorded here. Its
    /// pass reconciled nothing, and the reads that failed may succeed on the
    /// next poll; marking it seen would leave the pool blind to that block
    /// for its whole life over one transient read failure.
    last: Option<Seen>,
}

/// A block taken in, kept so its warm-up can continue.
struct Seen {
    snapshot: Snapshot,
    /// Whether a snapshot of this block has been persisted usable for a
    /// decision. Once true there is nothing left to warm.
    usable: bool,
}

/// What one poll came to.
#[derive(Debug)]
pub enum Tick {
    /// The latest block is the one already taken in, and its cache was
    /// already complete.
    Unchanged { block_id: String },
    /// A previous run of the pool had already taken this block in and made
    /// it usable, so this run assembled nothing.
    ///
    /// §9 allows one usable assembly per block, and re-assembling would
    /// produce a second: `get-benchmarks` is a latest-state read, so a
    /// second assembly of the same block legitimately differs, and two
    /// usable rows for one block is the contradiction
    /// `block_snapshot_one_usable_per_block` refuses. The cost is that this
    /// run reconciles from the next block rather than this one — the same
    /// wait a restart one block later would have had.
    AlreadyIngested { block_id: String },
    /// The latest block is the one already taken in; another budget went
    /// on its cache.
    CacheAdvanced {
        block_id: String,
        cache: Advance,
        /// Whether this pass completed the warm-up and persisted the block
        /// as usable for a decision.
        now_usable: bool,
    },
    /// A new block was taken in and reconciled from.
    Ingested(Box<Ingested>),
}

/// One block taken in.
#[derive(Debug)]
pub struct Ingested {
    pub block_id: String,
    pub height: u64,
    pub gaps_recorded: Vec<i64>,
    pub cache: Advance,
    pub outcome: Outcome,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("polling the latest block: {0}")]
    Poll(#[from] ReadError),
    #[error("the latest block carries no id")]
    BlockWithoutId,
    #[error(transparent)]
    Ingest(#[from] IngestError),
    #[error(transparent)]
    Reconcile(#[from] ReconcileError),
}

impl<S: SnapshotSource + BenchmarkDataSource> Service<S> {
    pub fn new(
        pool: PgPool,
        source: S,
        poll: TigReadClient,
        network: Network,
        player_id: impl Into<String>,
        guardrails: Guardrails,
        cache_budget: usize,
    ) -> Self {
        Self {
            store: PostgresSnapshotStore::new(pool.clone()),
            cache: PostgresActiveBenchmarkStore::new(pool.clone()),
            pool,
            source,
            poll,
            network,
            player_id: player_id.into(),
            guardrails,
            cache_budget,
            last: None,
        }
    }

    /// The snapshot source this service reads through.
    pub fn source(&self) -> &S {
        &self.source
    }

    fn ingestor(&self) -> Ingestor<'_, S, PostgresSnapshotStore, PostgresActiveBenchmarkStore> {
        Ingestor {
            pool: &self.pool,
            store: &self.store,
            cache: &self.cache,
            source: &self.source,
            network: self.network,
            assembly_attempts: ASSEMBLY_ATTEMPTS,
            cache_budget: self.cache_budget,
        }
    }

    /// One poll.
    ///
    /// The block the poll saw and the block ingestion opens with may differ
    /// if the chain advanced between the two reads; ingestion's is the one
    /// recorded as taken in, since that is the one whose evidence was read.
    pub async fn tick(&mut self) -> Result<Tick, ServiceError> {
        let latest = self.poll.get_json("get-block?include_data=true").await?;
        let latest_id = latest
            .get("block")
            .unwrap_or(&latest)
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or(ServiceError::BlockWithoutId)?
            .to_string();
        if let Some(seen) = self.last.as_ref()
            && seen.snapshot.block_id == latest_id
        {
            if seen.usable {
                return Ok(Tick::Unchanged {
                    block_id: latest_id,
                });
            }
            return self.continue_warm_up().await;
        }

        // A block a *previous run* finished. Assembling it again would give
        // this block a second usable assembly — see `Tick::AlreadyIngested`
        // — so the pool waits for the next block instead. Only ever true on
        // the first poll after a restart: within one run `last` answers.
        if self
            .store
            .load_usable_record(self.network, &latest_id)
            .await
            .map_err(IngestError::from)?
            .is_some()
        {
            return Ok(Tick::AlreadyIngested {
                block_id: latest_id,
            });
        }

        let ingested = self.ingestor().ingest().await?;
        let record = ingested.snapshot.record();
        let block_id = record.block_id.clone();
        let height = record.height;

        let outcome = reconcile_block(
            &self.pool,
            self.network,
            &self.player_id,
            &self.guardrails,
            &ingested.snapshot,
        )
        .await?;

        if let Outcome::Reconciled(_) = &outcome
            && let Ok(snapshot) = ingested.snapshot.for_reconciliation()
        {
            self.last = Some(Seen {
                snapshot: snapshot.clone(),
                usable: ingested.snapshot.for_decision().is_ok(),
            });
        }
        Ok(Tick::Ingested(Box::new(Ingested {
            block_id,
            height,
            gaps_recorded: ingested.gaps_recorded,
            cache: ingested.cache,
            outcome,
        })))
    }

    /// Another budget on the last block's active set; if that completes the
    /// warm-up, the block is persisted again as usable.
    async fn continue_warm_up(&mut self) -> Result<Tick, ServiceError> {
        let Some(seen) = self.last.as_mut() else {
            unreachable!("continue_warm_up is called only with a block seen");
        };
        let ingestor = Ingestor {
            pool: &self.pool,
            store: &self.store,
            cache: &self.cache,
            source: &self.source,
            network: self.network,
            assembly_attempts: ASSEMBLY_ATTEMPTS,
            cache_budget: self.cache_budget,
        };
        let cache = ingestor.warm_cache(&seen.snapshot).await?;
        let now_usable = cache.covers_active_set();
        if now_usable {
            seen.snapshot.active_cache_ready = true;
            let persisted = self
                .store
                .persist(self.network, seen.snapshot.clone())
                .await
                .map_err(IngestError::from)?;
            seen.usable = persisted.for_decision().is_ok();
        }
        Ok(Tick::CacheAdvanced {
            block_id: seen.snapshot.block_id.clone(),
            cache,
            now_usable,
        })
    }
}

/// Poll until stopped, or exactly once.
///
/// The loop lives here rather than in `main` so the one rule that
/// distinguishes the two modes is testable: **a failed poll is fatal to
/// `once` and not to `run`.** `once` exists for an operator checking a
/// deployment or recording K3 evidence, and a check that reports success
/// after failing is worse than no check. The polling loop is the opposite
/// case — §10 makes a missed block unrecoverable, so a controller that
/// exited on one transient read error would turn a retryable failure into a
/// permanent hole in the pool's history.
///
/// `report` is handed every tick, including the ones the loop then fails
/// on, so an operator sees what the pass reached before it stopped.
pub async fn run<S, R>(
    service: &mut Service<S>,
    interval: Duration,
    once: bool,
    report: R,
) -> Result<(), ServiceError>
where
    S: SnapshotSource + BenchmarkDataSource,
    R: Fn(&Tick),
{
    loop {
        let started = Instant::now();
        match service.tick().await {
            Ok(tick) => {
                report(&tick);
                if once {
                    return Ok(());
                }
            }
            Err(e) => {
                if once {
                    return Err(e);
                }
                tracing::error!(event = "controller.tick.failed", error = %e);
            }
        }
        tokio::time::sleep(interval.saturating_sub(started.elapsed())).await;
    }
}
