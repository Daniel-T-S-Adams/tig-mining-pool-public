//! The active-benchmark metadata cache (`tig_integration.md` §5.2, §9
//! step 4).
//!
//! §5.2 owns the rule; the shape here is what it leaves to implementation.
//! The controller reads the block's active id set, fetches
//! `get-benchmark-data` for every id it has not yet retained, keeps the
//! compact facts, and discards the rest. A snapshot is usable for a decision
//! only once every active id in its block is retained — until then the
//! orchestrator returns `no_action`, because a projection made over a
//! partial denominator, or a source hyperparameter guessed at, is a decision
//! the pool could not explain afterwards.
//!
//! **Why this runs after step 6 rather than between steps 3 and 5.** §9
//! lists the cache advance as step 4, inside the anchored section. The
//! constraint it states there is not about ordering but about mixing:
//! "without mixing its cached immutable facts with an incorrect active-ID
//! set". The facts are immutable per benchmark id — a confirmed precommit
//! and benchmark do not change — so *when* they are fetched does not affect
//! what they say. What must be block-consistent is the active set they are
//! read against, and that is the accepted snapshot's own block. Fetching
//! inside the anchored section would instead put a warm-up of hundreds of
//! reads between the opening and closing `get-block`, so the closing read
//! would disagree with the opening one on nearly every attempt and §9 step 6
//! would discard nearly every snapshot — the cache could never warm up at
//! all. So the reads happen once the block is accepted, the readiness flag
//! is computed against that block's active set, and the two are persisted
//! together (step 7).
//!
//! **Bounded per pass.** The reads share the controller's read budget with
//! the poll and the assembly, and a block must still be taken in every block.
//! The caller says how many fetches one pass may spend; what is left over is
//! reported as still missing, and the next pass continues. An initial
//! warm-up therefore "may span several blocks", exactly as §5.2 says.

use std::collections::BTreeSet;
use std::future::Future;

use pool_domain::Network;
use serde_json::Value;

use crate::SnapshotError;

/// The compact facts §5.2 retains for one active benchmark.
///
/// Read once from `get-benchmark-data` and never updated: a confirmed
/// precommit and benchmark are immutable. The proof and any fraud ruling
/// are deliberately absent — active membership, which is what those decide,
/// is the block's to say every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBenchmarkMeta {
    pub benchmark_id: String,
    pub player_id: String,
    pub challenge_id: String,
    pub algorithm_id: String,
    pub track_id: String,
    pub compute_type: Option<String>,
    pub num_bundles: u64,
    pub fuel_budget: Option<u64>,
    pub hyperparameters: Option<Value>,
    pub precommit_block_confirmed: u64,
    pub num_active_bundles: Option<u64>,
    /// One entry per bundle, in TIG's order. Kept as JSON: the quality type
    /// is the challenge's (`config.quality_type`), and §7 expands the list
    /// positionally rather than reading the values here.
    pub average_quality_by_bundle: Vec<Value>,
    pub stopped: bool,
    pub benchmark_block_confirmed: u64,
}

/// Why a `get-benchmark-data` body could not be retained.
///
/// A body the pool cannot read is not cached, and the id stays missing, so
/// the snapshot stays unusable — never a partial row a decision would then
/// divide by.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("get-benchmark-data for {benchmark_id} is unusable: {reason}")]
pub struct ShapeError {
    pub benchmark_id: String,
    pub reason: String,
}

/// Reduce one `get-benchmark-data` body to what §5.2 retains.
///
/// Requires a confirmed precommit and a confirmed benchmark. An id the block
/// lists as active has both — activation follows a confirmed proof, which
/// follows a confirmed benchmark — so a body without them contradicts the
/// block that named it, and is refused rather than cached with holes.
pub fn retain(benchmark_id: &str, body: &Value) -> Result<ActiveBenchmarkMeta, ShapeError> {
    let shape = |reason: String| ShapeError {
        benchmark_id: benchmark_id.to_string(),
        reason,
    };
    let str_at = |path: &[&str]| -> Result<String, ShapeError> {
        pointer(body, path)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| shape(format!("no string at {}", path.join("."))))
    };
    let u64_at = |path: &[&str]| -> Result<u64, ShapeError> {
        pointer(body, path)
            .and_then(Value::as_u64)
            .ok_or_else(|| shape(format!("no non-negative integer at {}", path.join("."))))
    };
    let opt_u64_at = |path: &[&str]| -> Result<Option<u64>, ShapeError> {
        match pointer(body, path) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| shape(format!("{} is not a non-negative integer", path.join(".")))),
        }
    };

    let precommit = pointer(body, &["precommit"])
        .filter(|v| v.is_object())
        .ok_or_else(|| shape("no precommit".to_string()))?;
    if !precommit.get("settings").is_some_and(Value::is_object) {
        return Err(shape("precommit has no settings".to_string()));
    }
    let listed = str_at(&["precommit", "benchmark_id"])?;
    if listed != benchmark_id {
        return Err(shape(format!("body describes {listed}")));
    }
    let benchmark = pointer(body, &["benchmark"])
        .filter(|v| v.is_object())
        .ok_or_else(|| shape("no confirmed benchmark for an active id".to_string()))?;
    let qualities = pointer(benchmark, &["details", "average_quality_by_bundle"])
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            shape("benchmark.details.average_quality_by_bundle is not a list".to_string())
        })?;
    let stopped = pointer(benchmark, &["details", "stopped"])
        .and_then(Value::as_bool)
        .ok_or_else(|| shape("benchmark.details.stopped is not a boolean".to_string()))?;
    let hyperparameters = match pointer(body, &["precommit", "details", "hyperparameters"]) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.clone()),
    };
    let compute_type =
        match pointer(body, &["precommit", "details", "compute_type"]) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_str().map(str::to_string).ok_or_else(|| {
                shape("precommit.details.compute_type is not a string".to_string())
            })?),
        };

    Ok(ActiveBenchmarkMeta {
        benchmark_id: benchmark_id.to_string(),
        player_id: str_at(&["precommit", "settings", "player_id"])?,
        challenge_id: str_at(&["precommit", "settings", "challenge_id"])?,
        algorithm_id: str_at(&["precommit", "settings", "algorithm_id"])?,
        track_id: str_at(&["precommit", "settings", "track_id"])?,
        compute_type,
        num_bundles: u64_at(&["precommit", "details", "num_bundles"])?,
        fuel_budget: opt_u64_at(&["precommit", "details", "fuel_budget"])?,
        hyperparameters,
        // Non-null `state.block_confirmed` is §7's confirmation, for both.
        precommit_block_confirmed: u64_at(&["precommit", "state", "block_confirmed"])?,
        num_active_bundles: opt_u64_at(&["benchmark", "details", "num_active_bundles"])?,
        average_quality_by_bundle: qualities,
        stopped,
        benchmark_block_confirmed: u64_at(&["benchmark", "state", "block_confirmed"])?,
    })
}

fn pointer<'v>(value: &'v Value, path: &[&str]) -> Option<&'v Value> {
    path.iter().try_fold(value, |v, key| v.get(key))
}

/// `GET /get-benchmark-data`, for one id.
///
/// Not block-anchored: the endpoint takes none, and the facts it returns
/// for a confirmed benchmark do not depend on one.
pub trait BenchmarkDataSource {
    fn get_benchmark_data(
        &self,
        benchmark_id: &str,
    ) -> impl Future<Output = Result<Value, SnapshotError>> + Send;
}

/// Where retained facts live.
pub trait ActiveBenchmarkStore {
    /// Which of `ids` are already retained.
    fn retained(
        &self,
        network: Network,
        ids: &[String],
    ) -> impl Future<Output = Result<BTreeSet<String>, StoreError>> + Send;

    /// Retain one benchmark's facts, recording the height they were read
    /// at. Idempotent: a second insert of the same id is a no-op, since the
    /// facts are immutable.
    fn retain(
        &self,
        network: Network,
        meta: &ActiveBenchmarkMeta,
        fetched_at_height: u64,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("active-benchmark cache unavailable: {0}")]
    Unavailable(String),
    #[error("active-benchmark cache row for {benchmark_id} is unreadable: {reason}")]
    Corrupt {
        benchmark_id: String,
        reason: String,
    },
}

/// What one pass over the active set did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Advance {
    /// Retained by this pass.
    pub fetched: Vec<String>,
    /// Active ids not retained after this pass: the budget ran out before
    /// them, or their fetch failed. Non-empty means the snapshot is not
    /// usable for a decision.
    pub missing: Vec<String>,
    /// Fetches this pass attempted and could not retain, with why. A
    /// subset of `missing`; separate so an operator can tell a warm-up
    /// still in progress from a body the pool cannot read.
    pub failed: Vec<(String, String)>,
}

impl Advance {
    /// §9 / criterion C5: every active id in the block is retained.
    pub fn covers_active_set(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Advance the cache for one block's active set, spending at most `budget`
/// fetches.
///
/// `active_ids` is `block.data.active_ids.benchmark` of the block whose
/// snapshot the readiness answer will be persisted with — the block-
/// consistency §9 step 4 asks for lives in the caller passing that set and
/// no other.
pub async fn advance<S, T>(
    source: &S,
    store: &T,
    network: Network,
    active_ids: &[String],
    height: u64,
    budget: usize,
) -> Result<Advance, StoreError>
where
    S: BenchmarkDataSource,
    T: ActiveBenchmarkStore,
{
    let known = store.retained(network, active_ids).await?;
    let mut report = Advance::default();
    let mut spent = 0;
    // Sorted and deduplicated, so a pass spends its budget on the same
    // ids in the same order across restarts rather than on whatever order
    // the block listed them in.
    let wanted: BTreeSet<&String> = active_ids
        .iter()
        .filter(|id| !known.contains(*id))
        .collect();
    for id in wanted {
        if spent >= budget {
            report.missing.push(id.clone());
            continue;
        }
        spent += 1;
        match source.get_benchmark_data(id).await {
            Ok(body) => match retain(id, &body) {
                Ok(meta) => {
                    store.retain(network, &meta, height).await?;
                    report.fetched.push(id.clone());
                }
                Err(e) => {
                    report.failed.push((id.clone(), e.to_string()));
                    report.missing.push(id.clone());
                }
            },
            // A read that failed is a fact not retained, whatever the
            // reason; the store itself failing is the caller's problem.
            Err(e) => {
                report.failed.push((id.clone(), e.to_string()));
                report.missing.push(id.clone());
            }
        }
    }
    Ok(report)
}
