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

/// One row, as the facts §5.2 retained.
///
/// A row that cannot be read is [`StoreError::Corrupt`], never a default: the
/// values here are denominators and source settings, and a zero substituted
/// for an unreadable count would change a decision rather than fail one.
///
/// Those paths are unreachable through the schema — `migrations/0014`
/// constrains the counts, the heights and the quality list, and the controller
/// cannot write the table at all. They are kept because an `i64` column has to
/// become a `u64` field somehow and reporting is the right answer if it ever
/// cannot; the schema's half of the guarantee is asserted by
/// `the_schema_refuses_every_shape_load_would_have_to_call_corrupt`, so a
/// later migration cannot drop it silently.
fn row_to_meta(row: &sqlx::postgres::PgRow) -> Result<ActiveBenchmarkMeta, StoreError> {
    use sqlx::Row;

    let benchmark_id: String = row.try_get("benchmark_id").map_err(unavailable)?;
    let corrupt = |reason: String| StoreError::Corrupt {
        benchmark_id: benchmark_id.clone(),
        reason,
    };
    let count = |field: &'static str, v: i64| {
        u64::try_from(v).map_err(|_| corrupt(format!("{field} {v} is negative")))
    };

    let num_bundles: i64 = row.try_get("num_bundles").map_err(unavailable)?;
    let fuel_budget: Option<i64> = row.try_get("fuel_budget").map_err(unavailable)?;
    let precommit_block_confirmed: i64 = row
        .try_get("precommit_block_confirmed")
        .map_err(unavailable)?;
    let num_active_bundles: Option<i64> = row.try_get("num_active_bundles").map_err(unavailable)?;
    let benchmark_block_confirmed: i64 = row
        .try_get("benchmark_block_confirmed")
        .map_err(unavailable)?;
    let qualities: serde_json::Value = row
        .try_get("average_quality_by_bundle")
        .map_err(unavailable)?;

    Ok(ActiveBenchmarkMeta {
        player_id: row.try_get("player_id").map_err(unavailable)?,
        challenge_id: row.try_get("challenge_id").map_err(unavailable)?,
        algorithm_id: row.try_get("algorithm_id").map_err(unavailable)?,
        track_id: row.try_get("track_id").map_err(unavailable)?,
        compute_type: row.try_get("compute_type").map_err(unavailable)?,
        num_bundles: count("num_bundles", num_bundles)?,
        fuel_budget: fuel_budget.map(|v| count("fuel_budget", v)).transpose()?,
        hyperparameters: row.try_get("hyperparameters").map_err(unavailable)?,
        precommit_block_confirmed: count("precommit_block_confirmed", precommit_block_confirmed)?,
        num_active_bundles: num_active_bundles
            .map(|v| count("num_active_bundles", v))
            .transpose()?,
        average_quality_by_bundle: qualities
            .as_array()
            .cloned()
            .ok_or_else(|| corrupt("average_quality_by_bundle is not a list".to_string()))?,
        stopped: row.try_get("stopped").map_err(unavailable)?,
        benchmark_block_confirmed: count("benchmark_block_confirmed", benchmark_block_confirmed)?,
        benchmark_id,
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

    async fn load(
        &self,
        network: Network,
        ids: &[String],
    ) -> Result<Vec<ActiveBenchmarkMeta>, StoreError> {
        let rows = sqlx::query(
            "SELECT benchmark_id, player_id, challenge_id, algorithm_id, track_id,
                    compute_type, num_bundles, fuel_budget, hyperparameters,
                    precommit_block_confirmed, num_active_bundles,
                    average_quality_by_bundle, stopped, benchmark_block_confirmed
               FROM pool.active_benchmark_meta
              WHERE network = $1 AND benchmark_id = ANY($2)
              ORDER BY benchmark_id",
        )
        .bind(network.as_str())
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        rows.iter().map(row_to_meta).collect()
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
