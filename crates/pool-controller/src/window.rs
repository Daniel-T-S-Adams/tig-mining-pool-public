//! `tig_integration.md` §7's confirmation mapping, in one place.
//!
//! §7 opens with the rule the rest of the pool is built on: "The controller
//! advances local state only from confirmed reads." Its table then says, for
//! each local event, exactly which read is the authority. Every entry has the
//! same shape — a matching entry with a **non-null `state.block_confirmed`**,
//! or membership of a block set — and that shape is the whole of it: a
//! recorded attempt, an HTTP 200, a synchronous `verified` response are all
//! listed as *not* confirmation.
//!
//! [`ConfirmedWindow`] is the typed form of that table, and
//! `pool_workflow::workflow`'s transitions consume nothing else. So this
//! module is the only place a TIG read becomes evidence, and the only place
//! §7's mapping is expressed. Anything that re-derived it — a second parser, a
//! caller reaching into the JSON for one field — would be the table restated,
//! free to drift from the one the documents own.
//!
//! It lives in the controller because `architecture.md` §6 gives "advance
//! confirmed TIG lifecycle" to the controller reconciler. `pool-snapshot`
//! assembles the reads and `tig-client` fetches them; neither decides what a
//! read *means*.

use std::collections::BTreeMap;

use pool_workflow::restart::ConfirmedWindow;
use pool_workflow::workflow::{
    ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof,
};
use serde_json::Value;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WindowError {
    /// A record could not be read.
    ///
    /// Fatal rather than skipped, for the same reason `tig-gateway`'s §10
    /// reconciliation refuses to skip a malformed precommit: a record the pool
    /// cannot parse might be the one that matters, and dropping it turns
    /// evidence into absence. Absence is what licenses the pool to act as
    /// though nothing happened.
    #[error("{collection}[{index}] is unusable: {reason}")]
    Shape {
        collection: &'static str,
        index: usize,
        reason: String,
    },
    /// The block carries no height, so nothing read against it can be dated.
    #[error("the block names no height: {reason}")]
    Block { reason: String },
}

/// Build the confirmed window from a `get-benchmarks` body and a block.
///
/// `benchmarks` is the `get-benchmarks` response — §5's latest 120-block
/// window — and `block` is a `get-block` response with `include_data`.
pub fn confirmed_window(benchmarks: &Value, block: &Value) -> Result<ConfirmedWindow, WindowError> {
    // A block body may arrive wrapped (`{"block": {...}}`) or bare. Reading
    // both is not laxity: `get-block` returns the wrapper and a caller holding
    // an already-unwrapped block is the ordinary case for a snapshot layer
    // that stored one.
    let block = block.get("block").unwrap_or(block);
    let at_block = block
        .get("details")
        .and_then(|d| d.get("height"))
        .and_then(Value::as_i64)
        .ok_or_else(|| WindowError::Block {
            reason: "details.height is missing or not a number".to_string(),
        })?;

    let data = block.get("data");
    let ids = |set: &str, key: &str| -> Vec<String> {
        data.and_then(|d| d.get(set))
            .and_then(|s| s.get(key))
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    Ok(ConfirmedWindow {
        precommits: precommits(benchmarks)?,
        benchmarks: benchmark_entries(benchmarks)?,
        proofs: proofs(benchmarks)?,
        frauds: frauds(benchmarks)?,
        // §7: "Benchmark ID in `block.data.confirmed_ids.verified`, when
        // published." An absent set is an absent event, not an error — the
        // qualifier is the document's.
        verified: ids("confirmed_ids", "verified"),
        active: ids("active_ids", "benchmark"),
        at_block,
    })
}

/// The one test §7 applies to every collection entry.
///
/// Returns `Ok(None)` for an entry that exists but has not confirmed. That is
/// the distinction the whole table turns on, and collapsing it is how an HTTP
/// 200 becomes a confirmation: `get-benchmarks` lists the pool's *submitted*
/// precommits too, with a null `state.block_confirmed`, so an implementation
/// that took presence for confirmation would advance a workflow the moment the
/// write was accepted.
fn block_confirmed(
    entry: &Value,
    collection: &'static str,
    index: usize,
) -> Result<Option<i64>, WindowError> {
    // The boolean half is `pool_workflow::block_confirmed`, not a second
    // reading of it. That function is what `tig-gateway` applies to the same
    // collections, and two readers that disagree about what §7 confirms is
    // the failure both doc comments claim to prevent — so the disagreement is
    // removed rather than described.
    if !pool_workflow::block_confirmed(entry) {
        return Ok(None);
    }
    // Confirmed, so §7 says there is a height. One that will not read as an
    // integer is a shape error and not a silent "unconfirmed": treating it as
    // unconfirmed is exactly the divergence — the gateway would settle a
    // write the controller would leave waiting forever.
    entry
        .get("state")
        .and_then(|s| s.get("block_confirmed"))
        .and_then(Value::as_i64)
        .map(Some)
        .ok_or_else(|| WindowError::Shape {
            collection,
            index,
            reason: "state.block_confirmed is confirmed but not an integer height".to_string(),
        })
}

fn entries<'a>(body: &'a Value, collection: &'static str) -> &'a [Value] {
    body.get(collection)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn text(
    entry: &Value,
    path: &[&str],
    collection: &'static str,
    index: usize,
) -> Result<String, WindowError> {
    let mut cursor = entry;
    for key in path {
        cursor = cursor.get(key).unwrap_or(&Value::Null);
    }
    cursor
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| WindowError::Shape {
            collection,
            index,
            reason: format!("{} is missing or not a string", path.join(".")),
        })
}

fn precommits(body: &Value) -> Result<BTreeMap<String, ConfirmedPrecommit>, WindowError> {
    let mut out = BTreeMap::new();
    for (index, entry) in entries(body, "precommits").iter().enumerate() {
        let Some(block_confirmed) = block_confirmed(entry, "precommits", index)? else {
            continue;
        };
        let benchmark_id = text(entry, &["benchmark_id"], "precommits", index)?;
        // §8's guardrails are all ages from `details.block_started`, so a
        // confirmed precommit without one cannot be aged and is a shape error
        // rather than a workflow the pool silently cannot expire.
        let block_started = entry
            .get("details")
            .and_then(|d| d.get("block_started"))
            .and_then(Value::as_i64)
            .ok_or_else(|| WindowError::Shape {
                collection: "precommits",
                index,
                reason: "details.block_started is missing or not a number".to_string(),
            })?;
        out.insert(
            benchmark_id.clone(),
            ConfirmedPrecommit {
                benchmark_id,
                block_confirmed,
                block_started,
                // §6.1: the pool submits every live track and TIG selects one,
                // so this is TIG's answer and never the pool's proposal.
                track_id: text(entry, &["settings", "track_id"], "precommits", index)?,
                // §6.2's commitment is built to exactly this length. It is
                // a *detail*, so `settings` below does not contain it and it
                // is carried across explicitly, as `block_started` is.
                num_nonces: entry
                    .get("details")
                    .and_then(|d| d.get("num_nonces"))
                    .and_then(Value::as_i64),
                // §7: "Confirmed settings/details replace proposed values."
                // Carried whole rather than field by field, because the pool
                // has no business deciding which of TIG's values matter.
                settings: entry.get("settings").cloned().unwrap_or(Value::Null),
            },
        );
    }
    Ok(out)
}

fn benchmark_entries(body: &Value) -> Result<BTreeMap<String, ConfirmedBenchmark>, WindowError> {
    let mut out = BTreeMap::new();
    for (index, entry) in entries(body, "benchmarks").iter().enumerate() {
        let Some(block_confirmed) = block_confirmed(entry, "benchmarks", index)? else {
            continue;
        };
        // The benchmark collection keys on `id`, not `benchmark_id`; the
        // proofs and frauds collections key on `benchmark_id`. Getting this
        // wrong reads as an empty window rather than an error.
        let benchmark_id = text(entry, &["id"], "benchmarks", index)?;
        // §7: "If `details.stopped` is true, no proof is sent." A missing
        // flag is a shape error and not a false: reading it as "not stopped"
        // would have the pool build and submit a proof for a benchmark TIG had
        // stopped, and pay for it.
        let stopped = entry
            .get("details")
            .and_then(|d| d.get("stopped"))
            .and_then(Value::as_bool)
            .ok_or_else(|| WindowError::Shape {
                collection: "benchmarks",
                index,
                reason: "details.stopped is missing or not a boolean".to_string(),
            })?;
        out.insert(
            benchmark_id.clone(),
            ConfirmedBenchmark {
                benchmark_id,
                block_confirmed,
                stopped,
            },
        );
    }
    Ok(out)
}

fn proofs(body: &Value) -> Result<BTreeMap<String, ConfirmedProof>, WindowError> {
    let mut out = BTreeMap::new();
    for (index, entry) in entries(body, "proofs").iter().enumerate() {
        let Some(block_confirmed) = block_confirmed(entry, "proofs", index)? else {
            continue;
        };
        let benchmark_id = text(entry, &["benchmark_id"], "proofs", index)?;
        out.insert(
            benchmark_id.clone(),
            ConfirmedProof {
                benchmark_id,
                block_confirmed,
            },
        );
    }
    Ok(out)
}

fn frauds(body: &Value) -> Result<BTreeMap<String, ConfirmedFraud>, WindowError> {
    let mut out = BTreeMap::new();
    for (index, entry) in entries(body, "frauds").iter().enumerate() {
        let Some(block_confirmed) = block_confirmed(entry, "frauds", index)? else {
            continue;
        };
        let benchmark_id = text(entry, &["benchmark_id"], "frauds", index)?;
        out.insert(
            benchmark_id.clone(),
            ConfirmedFraud {
                benchmark_id,
                block_confirmed,
            },
        );
    }
    Ok(out)
}
