//! Spike S4 CLI: drives the pool-owned tail of the live chain — benchmark
//! commitment after durable acceptance, sampled nonces from confirmed state,
//! proofs solely from the retained accepted package, idempotent proof
//! submission, ACTIVE observation, and retention-conditioned deletion.
//! See `docs/plans/protocol-spike.md` §5 (phase S4, issue #13).
//!
//! Subcommands (each resumable; all state is in durable ledgers):
//!   precommit        --challenge <id> --algorithm <id> [--compute-type t]
//!   await-assignment [--poll-secs 15] [--max-secs 900]
//!   commit           --package-id <id> --pool-root <dir>
//!   await-sampled    --benchmark-id <id> [--poll-secs 15] [--max-secs 1800]
//!   prove            --package-id <id> --pool-root <dir>
//!   submit-proof     --benchmark-id <id>
//!   await-active     --benchmark-id <id> [--poll-secs 20] [--max-secs 3900]
//!   retire           --package-id <id> --pool-root <dir>
//!   status
//!
//! Common flags: --base-url <url> --api-key-file <path> --player <address>
//!               --data-dir <dir>
//!
//! Rate limits honored (`tig_integration.md` §11): polling loops sleep at
//! least the configured interval between iterations; protocol writes go
//! through a serialized lane with >= 5 seconds between POSTs (persisted in
//! `post-lane.json` so spacing holds across invocations). Every invocation
//! appends a step record with its per-endpoint API call counts to
//! `steps.jsonl`.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use spike::active::{
    apply_retention, build_proof_payload, commitment_body, load_accepted_package, vm_hwm_kib,
};
use spike::member::rfc3339_utc;
use spike::pool::Pool;
use spike::{Gateway, Ledger, TigClient, fold_intents, plan_precommit};

const MIN_POST_SPACING_SECS: u64 = 5;

struct Args {
    cmd: String,
    base_url: String,
    api_key_file: String,
    player: String,
    data_dir: String,
    challenge: Option<String>,
    algorithm: Option<String>,
    compute_type: String,
    package_id: Option<String>,
    benchmark_id: Option<String>,
    pool_root: Option<PathBuf>,
    poll_secs: Option<u64>,
    max_secs: Option<u64>,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| anyhow!("missing subcommand"))?;
    let mut a = Args {
        cmd,
        base_url: "https://testnet-api.tig.foundation".to_owned(),
        api_key_file: "secrets/tig-testnet-api-key".to_owned(),
        player: String::new(),
        data_dir: "data/spike-active/gateway".to_owned(),
        challenge: None,
        algorithm: None,
        compute_type: "aws_t4g".to_owned(),
        package_id: None,
        benchmark_id: None,
        pool_root: None,
        poll_secs: None,
        max_secs: None,
    };
    while let Some(flag) = it.next() {
        let v = it
            .next()
            .ok_or_else(|| anyhow!("flag {flag} needs a value"))?;
        match flag.as_str() {
            "--base-url" => a.base_url = v,
            "--api-key-file" => a.api_key_file = v,
            "--player" => a.player = v,
            "--data-dir" => a.data_dir = v,
            "--challenge" => a.challenge = Some(v),
            "--algorithm" => a.algorithm = Some(v),
            "--compute-type" => a.compute_type = v,
            "--package-id" => a.package_id = Some(v),
            "--benchmark-id" => a.benchmark_id = Some(v),
            "--pool-root" => a.pool_root = Some(v.into()),
            "--poll-secs" => a.poll_secs = Some(v.parse().context("--poll-secs")?),
            "--max-secs" => a.max_secs = Some(v.parse().context("--max-secs")?),
            _ => bail!("unknown flag {flag}"),
        }
    }
    if a.player.is_empty() {
        bail!("--player <address> is required");
    }
    Ok(a)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serialized POST lane spacing across invocations (`tig_integration.md`
/// §11): sleep until at least `MIN_POST_SPACING_SECS` since the last
/// recorded POST, then record this one.
fn pace_post_lane(ledger: &Ledger) -> Result<()> {
    if let Some(doc) = ledger.read_doc("post-lane.json")? {
        let last = doc
            .get("last_post_unix")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let now = unix_now();
        if now < last + MIN_POST_SPACING_SECS {
            std::thread::sleep(std::time::Duration::from_secs(
                last + MIN_POST_SPACING_SECS - now,
            ));
        }
    }
    ledger.write_doc("post-lane.json", &json!({ "last_post_unix": unix_now() }))?;
    Ok(())
}

/// Append one step record with timestamps and per-endpoint API call counts.
fn record_step(gw: &Gateway, step: &str, started_unix: u64, detail: &Value) -> Result<()> {
    gw.ledger.append(
        "steps",
        &json!({
            "step": step,
            "started_at": rfc3339_utc(started_unix),
            "finished_at": rfc3339_utc(unix_now()),
            "wall_secs": unix_now().saturating_sub(started_unix),
            "api_calls": gw.client.call_counts(),
            "detail": detail,
        }),
    )
}

fn require<'a>(v: &'a Option<String>, name: &str) -> Result<&'a str> {
    v.as_deref().ok_or_else(|| anyhow!("{name} is required"))
}

fn pool_of(a: &Args) -> Result<Pool> {
    let root = a
        .pool_root
        .clone()
        .ok_or_else(|| anyhow!("--pool-root is required"))?;
    Pool::open(root).map_err(|e| anyhow!("{e}"))
}

fn latest_height(gw: &Gateway) -> Result<(String, u64)> {
    let block = gw.client.latest_block()?;
    Ok((
        block
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned(),
        block
            .pointer("/details/height")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    ))
}

fn cmd_precommit(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let challenge = require(&a.challenge, "--challenge")?;
    let algorithm = require(&a.algorithm, "--algorithm")?;
    // Fetch the block immediately before submitting so settings.block_id
    // honors the latest-or-second-latest rule.
    let block = gw.client.latest_block()?;
    let block_id = block
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("block missing id"))?
        .to_owned();
    let height = block
        .pointer("/details/height")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let challenges = gw.client.challenges(&block_id)?;
    let plan = plan_precommit(
        &block,
        &challenges,
        &gw.player_id,
        challenge,
        algorithm,
        &a.compute_type,
    )?;
    pace_post_lane(&gw.ledger)?;
    let intent_id = gw.submit_precommit(&plan)?;
    println!("precommit intent {intent_id} (anchor block {block_id} height {height})");
    record_step(
        gw,
        "precommit",
        started,
        &json!({ "intent_id": intent_id, "anchor_block_id": block_id, "anchor_height": height,
                 "challenge": challenge, "algorithm": algorithm }),
    )
}

fn cmd_await_assignment(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let poll = a.poll_secs.unwrap_or(15).max(1);
    let max = a.max_secs.unwrap_or(900);
    loop {
        for line in gw.reconcile()? {
            println!("{line}");
        }
        let confirmed: Vec<(String, String)> = fold_intents(&gw.ledger.read_all("intents")?)
            .into_iter()
            .filter(|(_, rec)| {
                rec.get("write_kind").and_then(Value::as_str) != Some("BENCHMARK")
                    && rec.get("write_kind").and_then(Value::as_str) != Some("PROOF")
                    && rec.get("state").and_then(Value::as_str) == Some("CONFIRMED")
            })
            .filter_map(|(id, rec)| {
                rec.get("benchmark_id")
                    .and_then(Value::as_str)
                    .map(|b| (id, b.to_owned()))
            })
            .collect();
        if let Some((intent_id, benchmark_id)) = confirmed.last() {
            let (block_id, height) = latest_height(gw)?;
            println!("assignment confirmed: benchmark {benchmark_id} (intent {intent_id})");
            record_step(
                gw,
                "await-assignment",
                started,
                &json!({ "benchmark_id": benchmark_id, "intent_id": intent_id,
                         "observed_block_id": block_id, "observed_height": height }),
            )?;
            return Ok(());
        }
        if unix_now().saturating_sub(started) > max {
            record_step(
                gw,
                "await-assignment",
                started,
                &json!({ "timed_out": true }),
            )?;
            bail!("no confirmed assignment within {max} seconds");
        }
        std::thread::sleep(std::time::Duration::from_secs(poll));
    }
}

fn cmd_commit(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let package_id = require(&a.package_id, "--package-id")?;
    let pool = pool_of(a)?;
    // Invariant 4 chain: load refuses without a committed acceptance record,
    // and the gateway re-verifies the acceptance before creating the intent.
    let pkg = load_accepted_package(&pool, package_id)?;
    let body = commitment_body(&pkg)?;
    let (block_id, height) = latest_height(gw)?;
    pace_post_lane(&gw.ledger)?;
    let result = gw.submit_benchmark_commitment(&pkg.acceptance, &body)?;
    println!(
        "commitment for benchmark {}: intent {} state {} (sent: {}) at height {height}",
        pkg.benchmark_id, result.intent_id, result.state, result.sent
    );
    record_step(
        gw,
        "commit",
        started,
        &json!({ "benchmark_id": pkg.benchmark_id, "package_id": package_id,
                 "package_sha256_recomputed": pkg.package_sha256,
                 "merkle_root": pkg.root_hex, "num_nonces": pkg.num_nonces,
                 "intent_id": result.intent_id, "state": result.state, "sent": result.sent,
                 "block_id": block_id, "height": height }),
    )
}

fn cmd_await_sampled(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let benchmark_id = require(&a.benchmark_id, "--benchmark-id")?;
    let poll = a.poll_secs.unwrap_or(15).max(1);
    let max = a.max_secs.unwrap_or(1800);
    loop {
        for line in gw.reconcile()? {
            println!("{line}");
        }
        if let Some(doc) = gw
            .ledger
            .read_doc(&format!("benchmark-confirmed-{benchmark_id}.json"))?
        {
            let sampled = doc
                .pointer("/entry/details/sampled_nonces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !sampled.is_empty() {
                println!(
                    "benchmark {benchmark_id} confirmed at block {} with sampled nonces {sampled:?}",
                    doc.get("confirmed_at_block")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                );
                record_step(
                    gw,
                    "await-sampled",
                    started,
                    &json!({ "benchmark_id": benchmark_id,
                             "confirmed_at_block": doc.get("confirmed_at_block"),
                             "sampled_nonces": sampled,
                             "observed_via": doc.get("observed_via") }),
                )?;
                return Ok(());
            }
            println!("benchmark confirmed but sampled_nonces not yet published");
        }
        if unix_now().saturating_sub(started) > max {
            record_step(gw, "await-sampled", started, &json!({ "timed_out": true }))?;
            bail!("no confirmed sampled nonces within {max} seconds");
        }
        std::thread::sleep(std::time::Duration::from_secs(poll));
    }
}

fn cmd_prove(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let package_id = require(&a.package_id, "--package-id")?;
    let pool = pool_of(a)?;
    let pkg = load_accepted_package(&pool, package_id)?;
    let benchmark_id = pkg.benchmark_id.clone();
    // Sampled nonces come ONLY from the confirmed benchmark entry recorded by
    // reconcile (tig_integration §7) — never from a write response.
    let confirmed = gw
        .ledger
        .read_doc(&format!("benchmark-confirmed-{benchmark_id}.json"))?
        .ok_or_else(|| {
            anyhow!("no confirmed benchmark entry for {benchmark_id}; run await-sampled first")
        })?;
    let sampled: Vec<u64> = confirmed
        .pointer("/entry/details/sampled_nonces")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("confirmed benchmark entry has no sampled_nonces"))?
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    if sampled.is_empty() {
        bail!("confirmed benchmark entry has an empty sampled_nonces set");
    }
    let hwm_before = vm_hwm_kib();
    let t0 = std::time::Instant::now();
    let payload = build_proof_payload(&pkg, &sampled)?;
    let construction_us = t0.elapsed().as_micros();
    let hwm_after = vm_hwm_kib();
    // Persist the canonical payload durably BEFORE any proof intent can
    // exist (architecture invariant 5; enforced by Gateway::submit_proof).
    let path = gw
        .ledger
        .write_doc(&format!("proof-payload-{benchmark_id}.json"), &payload)?;
    let provenance = json!({
        "benchmark_id": benchmark_id,
        "package_id": package_id,
        "package_sha256_recomputed": pkg.package_sha256,
        "sampled_nonces": sampled,
        "merkle_root": pkg.root_hex,
        "construction_us": u64::try_from(construction_us).unwrap_or(u64::MAX),
        "vm_hwm_kib_before": hwm_before,
        "vm_hwm_kib_after": hwm_after,
        "created_at": rfc3339_utc(unix_now()),
    });
    gw.ledger.write_doc(
        &format!("proof-provenance-{benchmark_id}.json"),
        &provenance,
    )?;
    println!(
        "proof payload for {benchmark_id}: {} proofs in {construction_us} us -> {}",
        sampled.len(),
        path.display()
    );
    record_step(gw, "prove", started, &provenance)
}

fn cmd_submit_proof(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let benchmark_id = require(&a.benchmark_id, "--benchmark-id")?;
    let (block_id, height) = latest_height(gw)?;
    pace_post_lane(&gw.ledger)?;
    let result = gw.submit_proof(benchmark_id)?;
    println!(
        "proof for {benchmark_id}: intent {} state {} (sent: {}) at height {height}",
        result.intent_id, result.state, result.sent
    );
    record_step(
        gw,
        "submit-proof",
        started,
        &json!({ "benchmark_id": benchmark_id, "intent_id": result.intent_id,
                 "state": result.state, "sent": result.sent,
                 "block_id": block_id, "height": height }),
    )
}

fn cmd_await_active(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let benchmark_id = require(&a.benchmark_id, "--benchmark-id")?;
    let poll = a.poll_secs.unwrap_or(20).max(1);
    let max = a.max_secs.unwrap_or(3900);
    // Phase 1: reconcile until the proof is confirmed.
    loop {
        if gw
            .ledger
            .read_doc(&format!("proof-confirmed-{benchmark_id}.json"))?
            .is_some()
        {
            break;
        }
        for line in gw.reconcile()? {
            println!("{line}");
        }
        if gw
            .ledger
            .read_doc(&format!("proof-confirmed-{benchmark_id}.json"))?
            .is_some()
        {
            break;
        }
        if unix_now().saturating_sub(started) > max {
            record_step(
                gw,
                "await-active",
                started,
                &json!({ "timed_out": true, "phase": "proof-confirmation" }),
            )?;
            bail!("proof not confirmed within {max} seconds");
        }
        std::thread::sleep(std::time::Duration::from_secs(poll));
    }
    let proof_doc = gw
        .ledger
        .read_doc(&format!("proof-confirmed-{benchmark_id}.json"))?
        .ok_or_else(|| anyhow!("proof-confirmed doc vanished"))?;
    println!(
        "proof confirmed at block {}; awaiting ACTIVE",
        proof_doc
            .get("confirmed_at_block")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    );
    // Phase 2: poll get-block until the id appears in active_ids.benchmark.
    loop {
        let block = gw.client.latest_block()?;
        let height = block
            .pointer("/details/height")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let active = block
            .pointer("/data/active_ids/benchmark")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .any(|id| id == benchmark_id)
            })
            .unwrap_or(false);
        if active {
            let block_id = block.get("id").and_then(Value::as_str).unwrap_or("?");
            let evidence = json!({
                "benchmark_id": benchmark_id,
                "block_id": block_id,
                "block_height": height,
                "observed_at": rfc3339_utc(unix_now()),
                "proof_confirmed_at_block": proof_doc.get("confirmed_at_block"),
                "proof_details": proof_doc.pointer("/entry/details"),
            });
            let path = gw
                .ledger
                .write_doc(&format!("active-evidence-{benchmark_id}.json"), &evidence)?;
            println!("benchmark {benchmark_id} ACTIVE at block {block_id} height {height}");
            record_step(gw, "await-active", started, &evidence)?;
            println!("active evidence: {}", path.display());
            return Ok(());
        }
        println!("height {height}: not yet in active_ids.benchmark");
        if unix_now().saturating_sub(started) > max {
            record_step(
                gw,
                "await-active",
                started,
                &json!({ "timed_out": true, "phase": "active-set", "last_height": height }),
            )?;
            bail!("benchmark not ACTIVE within {max} seconds");
        }
        std::thread::sleep(std::time::Duration::from_secs(poll));
    }
}

fn cmd_retire(gw: &Gateway, a: &Args) -> Result<()> {
    let started = unix_now();
    let package_id = require(&a.package_id, "--package-id")?;
    let pool = pool_of(a)?;
    // Fresh confirmed observation: the retention condition is checked against
    // the current block's active set, not a cached claim.
    let block = gw.client.latest_block()?;
    let record = apply_retention(&pool, package_id, &block)?;
    gw.ledger.append("retention", &record)?;
    println!("{}", serde_json::to_string_pretty(&record)?);
    record_step(gw, "retire", started, &record)
}

fn cmd_status(gw: &Gateway) -> Result<()> {
    for (id, rec) in fold_intents(&gw.ledger.read_all("intents")?) {
        println!(
            "{id}: {} kind={} benchmark_id={}",
            rec.get("state").and_then(Value::as_str).unwrap_or("?"),
            rec.get("write_kind").and_then(Value::as_str).unwrap_or("?"),
            rec.get("benchmark_id")
                .and_then(Value::as_str)
                .unwrap_or("-"),
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let a = parse_args()?;
    let api_key = std::fs::read_to_string(&a.api_key_file)
        .with_context(|| format!("reading API key file {}", a.api_key_file))?
        .trim()
        .to_owned();
    let gw = Gateway {
        client: TigClient::new(&a.base_url, api_key)?,
        ledger: Ledger::open(&a.data_dir)?,
        player_id: a.player.clone(),
    };
    match a.cmd.as_str() {
        "precommit" => cmd_precommit(&gw, &a),
        "await-assignment" => cmd_await_assignment(&gw, &a),
        "commit" => cmd_commit(&gw, &a),
        "await-sampled" => cmd_await_sampled(&gw, &a),
        "prove" => cmd_prove(&gw, &a),
        "submit-proof" => cmd_submit_proof(&gw, &a),
        "await-active" => cmd_await_active(&gw, &a),
        "retire" => cmd_retire(&gw, &a),
        "status" => cmd_status(&gw),
        other => bail!(
            "unknown subcommand {other} (precommit|await-assignment|commit|await-sampled|\
             prove|submit-proof|await-active|retire|status)"
        ),
    }
}
