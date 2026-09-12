//! PostgreSQL adapter for the [`ActiveBenchmarkStore`] port.

use std::collections::BTreeSet;

use pool_domain::Network;
use sqlx::PgPool;

use crate::active_cache::{ActiveBenchmarkMeta, ActiveBenchmarkStore, StoreError};

/// `pool.active_benchmark_meta`, as `pool_controller`: `SELECT, INSERT`
/// and deliberately no `UPDATE` (`migrations/0014_active_benchmark_meta.sql`).
#[derive(Debug, Clone)]
pub struct PostgresActiveBenchmarkStore {
    pool: PgPool,
}

impl PostgresActiveBenchmarkStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn unavailable(e: sqlx::Error) -> StoreError {
    StoreError::Unavailable(e.to_string())
}

fn stored_count(meta: &ActiveBenchmarkMeta, field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Corrupt {
        benchmark_id: meta.benchmark_id.clone(),
        reason: format!("{field} {value} exceeds the stored range"),
    })
}

impl ActiveBenchmarkStore for PostgresActiveBenchmarkStore {
    async fn retained(
        &self,
        network: Network,
        ids: &[String],
    ) -> Result<BTreeSet<String>, StoreError> {
        let found: Vec<String> = sqlx::query_scalar(
            "SELECT benchmark_id FROM pool.active_benchmark_meta
              WHERE network = $1 AND benchmark_id = ANY($2)",
        )
        .bind(network.as_str())
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(found.into_iter().collect())
    }

    async fn retain(
        &self,
        network: Network,
        meta: &ActiveBenchmarkMeta,
        fetched_at_height: u64,
    ) -> Result<(), StoreError> {
        let opt = |field: &str, value: Option<u64>| -> Result<Option<i64>, StoreError> {
            value.map(|v| stored_count(meta, field, v)).transpose()
        };
        // ON CONFLICT DO NOTHING: the facts are immutable, so a second
        // insert of the same id — two passes racing, or a crash between
        // the insert and its acknowledgement — changes nothing and fails
        // nothing.
        sqlx::query(
            "INSERT INTO pool.active_benchmark_meta
                 (network, benchmark_id, player_id, challenge_id, algorithm_id, track_id,
                  compute_type, num_bundles, fuel_budget, hyperparameters,
                  precommit_block_confirmed, num_active_bundles, average_quality_by_bundle,
                  stopped, benchmark_block_confirmed, fetched_at_height)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (network, benchmark_id) DO NOTHING",
        )
        .bind(network.as_str())
        .bind(&meta.benchmark_id)
        .bind(&meta.player_id)
        .bind(&meta.challenge_id)
        .bind(&meta.algorithm_id)
        .bind(&meta.track_id)
        .bind(&meta.compute_type)
        .bind(stored_count(meta, "num_bundles", meta.num_bundles)?)
        .bind(opt("fuel_budget", meta.fuel_budget)?)
        .bind(&meta.hyperparameters)
        .bind(stored_count(
            meta,
            "precommit_block_confirmed",
            meta.precommit_block_confirmed,
        )?)
        .bind(opt("num_active_bundles", meta.num_active_bundles)?)
        .bind(serde_json::Value::Array(
            meta.average_quality_by_bundle.clone(),
        ))
        .bind(meta.stopped)
        .bind(stored_count(
            meta,
            "benchmark_block_confirmed",
            meta.benchmark_block_confirmed,
        )?)
        .bind(stored_count(meta, "fetched_at_height", fetched_at_height)?)
        .execute(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(())
    }
}
