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
//! The poll and the assembly share one host, so they share one limiter
//! (`tig-client` keys it by host); the poll client only differs in its
//! deadlines, which `ReadPolicy::for_block_poll` bounds to the interval so a
//! slow poll cannot run into the next one.

use std::time::{Duration, Instant};

use pool_domain::Network;
use pool_snapshot::{PostgresSnapshotStore, SnapshotSource};
use pool_workflow::Guardrails;
use sqlx::PgPool;
use tig_client::{ReadError, TigReadClient};

use crate::ingest::{IngestError, ingest};
use crate::reconciler::{Outcome, ReconcileError, reconcile_block};

/// How many times one ingestion may restart at a new block before giving
/// up on this poll. §9 discards a snapshot the chain moved under; the next
/// poll tries again, so this bounds one poll's patience, not the pool's.
const ASSEMBLY_ATTEMPTS: u32 = 3;

pub struct Service<S> {
    pool: PgPool,
    store: PostgresSnapshotStore,
    source: S,
    poll: TigReadClient,
    network: Network,
    player_id: String,
    guardrails: Guardrails,
    /// The block most recently taken in *and reconciled from*, so a poll
    /// that sees it again does nothing.
    ///
    /// A block whose reads came back incomplete is not recorded here. Its
    /// pass reconciled nothing, and the reads that failed may succeed on the
    /// next poll; marking it seen would leave the pool blind to that block
    /// for its whole life over one transient read failure.
    last_block: Option<String>,
}

/// What one poll came to.
#[derive(Debug)]
pub enum Tick {
    /// The latest block is the one already taken in.
    Unchanged { block_id: String },
    /// A new block was taken in and reconciled from.
    Ingested(Box<Ingested>),
}

/// One block taken in.
#[derive(Debug)]
pub struct Ingested {
    pub block_id: String,
    pub height: u64,
    pub gaps_recorded: Vec<i64>,
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

impl<S: SnapshotSource> Service<S> {
    pub fn new(
        pool: PgPool,
        source: S,
        poll: TigReadClient,
        network: Network,
        player_id: impl Into<String>,
        guardrails: Guardrails,
    ) -> Self {
        Self {
            store: PostgresSnapshotStore::new(pool.clone()),
            pool,
            source,
            poll,
            network,
            player_id: player_id.into(),
            guardrails,
            last_block: None,
        }
    }

    /// The snapshot source this service reads through.
    pub fn source(&self) -> &S {
        &self.source
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
        if self.last_block.as_deref() == Some(latest_id.as_str()) {
            return Ok(Tick::Unchanged {
                block_id: latest_id,
            });
        }

        let ingested = ingest(
            &self.pool,
            &self.store,
            &self.source,
            self.network,
            ASSEMBLY_ATTEMPTS,
        )
        .await?;
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

        if matches!(outcome, Outcome::Reconciled(_)) {
            self.last_block = Some(block_id.clone());
        }
        Ok(Tick::Ingested(Box::new(Ingested {
            block_id,
            height,
            gaps_recorded: ingested.gaps_recorded,
            outcome,
        })))
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
    S: SnapshotSource,
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
