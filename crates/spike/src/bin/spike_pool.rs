//! Spike pool CLI. See `docs/plans/protocol-spike.md` §5 (phase S3).
//!
//! Runs the member→pool package handoff locally against the real S2 package:
//! resumable chunked upload into quarantine, ordered durable-acceptance saga,
//! immutable receipt, and slot re-offer — capturing the §7 measurements
//! (upload wall time/throughput, ingestion/verification time, peak temporary
//! disk use, receipt round-trip). Never performs a TIG protocol write; the
//! optional `--live-read` flag issues one public `get-benchmarks` read as
//! evidence that the benchmark is still non-terminal at re-offer time.
//!
//! Usage:
//!   spike-pool run --package-dir <S2 package dir> --pool-root <dir> \
//!     [--chunk-size 4096] [--live-read]
//!   spike-pool status --pool-root <dir> --upload-id <id>

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use spike::member::sha256_hex;
use spike::pool::Pool;

struct RunArgs {
    package_dir: PathBuf,
    pool_root: PathBuf,
    chunk_size: usize,
    live_read: bool,
}

fn parse_run_args(mut it: impl Iterator<Item = String>) -> Result<RunArgs> {
    let mut package_dir = None;
    let mut pool_root = None;
    let mut chunk_size = 4096usize;
    let mut live_read = false;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--package-dir" => package_dir = it.next().map(PathBuf::from),
            "--pool-root" => pool_root = it.next().map(PathBuf::from),
            "--chunk-size" => {
                chunk_size = it
                    .next()
                    .ok_or_else(|| anyhow!("--chunk-size needs a value"))?
                    .parse()
                    .context("--chunk-size")?;
            }
            "--live-read" => live_read = true,
            other => bail!("unknown flag {other}"),
        }
    }
    Ok(RunArgs {
        package_dir: package_dir.ok_or_else(|| anyhow!("--package-dir is required"))?,
        pool_root: pool_root.ok_or_else(|| anyhow!("--pool-root is required"))?,
        chunk_size: chunk_size.max(1),
        live_read,
    })
}

fn read_json(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn str_field<'v>(v: &'v Value, ptr: &str) -> Result<&'v str> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing field {ptr}"))
}

/// Assignment context for the pool, built from the S2 package directory's
/// `assignment-identity.json` plus the declaration's assignment digest.
fn assignment_doc(identity: &Value, declaration: &Value) -> Result<Value> {
    Ok(json!({
        "assignment_digest": str_field(declaration, "/assignment_digest")?,
        "assignment_id": str_field(identity, "/assignment_id")?,
        "benchmark_id": str_field(identity, "/confirmed_precommit/benchmark_id")?,
        "slot_id": str_field(identity, "/slot_id")?,
        "slot_generation": identity
            .pointer("/slot_generation")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("missing slot_generation"))?,
        "network": str_field(identity, "/network")?,
    }))
}

/// One public, non-mutating `get-benchmarks` read for slot-re-offer evidence
/// (no API key, no TIG write).
fn live_benchmark_state(base_url: &str, player_id: &str, benchmark_id: &str) -> Result<Value> {
    let http = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let block: Value = http
        .get(format!("{base_url}/get-block"))
        .send()?
        .error_for_status()?
        .json()?;
    let block_id = str_field(&block, "/block/id")?;
    let benches: Value = http
        .get(format!(
            "{base_url}/get-benchmarks?block_id={block_id}&player_id={player_id}"
        ))
        .send()?
        .error_for_status()?
        .json()?;
    let find = |key: &str| -> Option<Value> {
        benches.get(key).and_then(Value::as_array).and_then(|arr| {
            arr.iter()
                .find(|b| b.get("benchmark_id").and_then(Value::as_str) == Some(benchmark_id))
                .cloned()
        })
    };
    Ok(json!({
        "observed_block_id": block_id,
        "precommit": find("precommits"),
        "benchmark": find("benchmarks"),
        "proof": find("proofs"),
        "fraud": find("frauds"),
    }))
}

fn run(args: RunArgs) -> Result<()> {
    let declaration = read_json(&args.package_dir.join("upload-declaration.json"))?;
    let identity = read_json(&args.package_dir.join("assignment-identity.json"))?;
    let package = std::fs::read(args.package_dir.join("package.tar.zst"))
        .context("reading package.tar.zst")?;
    let package_id = str_field(&declaration, "/package_id")?.to_owned();
    let slot_id = str_field(&identity, "/slot_id")?.to_owned();
    let benchmark_id = str_field(&identity, "/confirmed_precommit/benchmark_id")?.to_owned();

    let pool = Pool::open(&args.pool_root).map_err(|e| anyhow!("{e}"))?;
    let assignment = assignment_doc(&identity, &declaration)?;
    pool.register_assignment(&assignment)
        .map_err(|e| anyhow!("register assignment: {e}"))?;

    // Resumable chunked upload (member_protocol §11).
    let session = pool
        .create_upload(&declaration)
        .map_err(|e| anyhow!("create upload: {e}"))?;
    let t_upload = Instant::now();
    let mut offset = session.committed_offset;
    let mut chunks = 0u64;
    while (offset as usize) < package.len() {
        let end = (offset as usize + args.chunk_size).min(package.len());
        let chunk = &package[offset as usize..end];
        offset = pool
            .put_chunk(&session.upload_id, offset, &sha256_hex(chunk), chunk)
            .map_err(|e| anyhow!("chunk at {offset}: {e}"))?;
        chunks += 1;
    }
    let upload_ms = t_upload.elapsed().as_millis();
    let throughput = (package.len() as u128 * 1000)
        .checked_div(upload_ms)
        .unwrap_or(0);

    // Ordered acceptance saga.
    let outcome = pool
        .finalize(&package_id)
        .map_err(|e| anyhow!("finalize: {e}"))?;

    // Receipt round-trip: an idempotent retry returning the stored receipt.
    let t_retry = Instant::now();
    let retry = pool
        .finalize(&package_id)
        .map_err(|e| anyhow!("finalize retry: {e}"))?;
    let receipt_rtt_us = t_retry.elapsed().as_micros();
    if retry.receipt_json != outcome.receipt_json {
        bail!("retried finalization returned a different receipt");
    }

    // Slot re-offer immediately after durable acceptance.
    let released = pool.slot_view(&slot_id).map_err(|e| anyhow!("{e}"))?;
    let next_generation = pool
        .offer_slot(&slot_id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .map_err(|e| anyhow!("re-offer: {e}"))?;

    let live = if args.live_read {
        let base = str_field(&identity, "/tig_api_base_url")?;
        let player = str_field(&identity, "/pool_player_id")?;
        match live_benchmark_state(base, player, &benchmark_id) {
            Ok(v) => v,
            Err(e) => json!({ "error": format!("live read failed: {e}") }),
        }
    } else {
        Value::Null
    };

    let receipt: Value = serde_json::from_str(&outcome.receipt_json)?;
    let report = json!({
        "phase": "S3",
        "package_id": package_id,
        "benchmark_id": benchmark_id,
        "upload_id": session.upload_id,
        "measurements": {
            "package_compressed_bytes": package.len(),
            "chunk_size_bytes": args.chunk_size,
            "chunks": chunks,
            "upload_wall_ms": upload_ms,
            "upload_throughput_bytes_per_s": throughput,
            "ingestion_verify_us": outcome.verify_us,
            "publish_us": outcome.publish_us,
            "state_commit_us": outcome.commit_us,
            "peak_temporary_bytes_under_pool_root": outcome.peak_bytes_under_root,
            "receipt_round_trip_us": receipt_rtt_us,
        },
        "receipt": receipt,
        "slot": {
            "slot_id": slot_id,
            "released_generation": released.generation,
            "released": !released.occupied,
            "reoffered_generation": next_generation,
        },
        "live_benchmark_state_at_reoffer": live,
        "notes": [
            "package identities remain deterministic spike stand-ins carried from S2 (no pool-side member protocol yet)",
            "compressed package digest treated as run-scoped; compression recipe recorded in the S2 run report (zstd level 19, single frame, checksum, pledged size)",
        ],
    });
    let path = args.pool_root.join("run-report.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report)?).context("writing run report")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    println!("run report: {}", path.display());
    Ok(())
}

fn status(mut it: impl Iterator<Item = String>) -> Result<()> {
    let mut pool_root = None;
    let mut upload_id = None;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--pool-root" => pool_root = it.next().map(PathBuf::from),
            "--upload-id" => upload_id = it.next(),
            other => bail!("unknown flag {other}"),
        }
    }
    let pool = Pool::open(pool_root.ok_or_else(|| anyhow!("--pool-root is required"))?)
        .map_err(|e| anyhow!("{e}"))?;
    let upload_id = upload_id.ok_or_else(|| anyhow!("--upload-id is required"))?;
    let s = pool.upload_status(&upload_id).map_err(|e| anyhow!("{e}"))?;
    println!("{}", serde_json::to_string_pretty(&s)?);
    Ok(())
}

fn main() -> Result<()> {
    let mut it = std::env::args().skip(1);
    match it.next().as_deref() {
        Some("run") => run(parse_run_args(it)?),
        Some("status") => status(it),
        other => bail!("usage: spike-pool run|status [flags]; got {other:?}"),
    }
}
