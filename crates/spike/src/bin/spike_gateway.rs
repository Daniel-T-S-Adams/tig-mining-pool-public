//! Spike gateway CLI. See `docs/plans/protocol-spike.md` §5 (phase S1).
//!
//! Subcommands: snapshot | plan | precommit | reconcile | status
//! Common flags: --base-url <url> --api-key-file <path> --player <address>
//!               --data-dir <dir> [--challenge <id> --algorithm <id>
//!               --compute-type <t>]

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use spike::{Gateway, Ledger, TigClient, fold_intents, plan_precommit};

struct Args {
    cmd: String,
    base_url: String,
    api_key_file: String,
    player: String,
    data_dir: String,
    challenge: Option<String>,
    algorithm: Option<String>,
    compute_type: String,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| anyhow!("missing subcommand"))?;
    let mut a = Args {
        cmd,
        base_url: "https://testnet-api.tig.foundation".to_owned(),
        api_key_file: "secrets/tig-testnet-api-key".to_owned(),
        player: String::new(),
        data_dir: "data/spike-gateway".to_owned(),
        challenge: None,
        algorithm: None,
        compute_type: "aws_t4g".to_owned(),
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
            _ => bail!("unknown flag {flag}"),
        }
    }
    if a.player.is_empty() {
        bail!("--player <address> is required");
    }
    Ok(a)
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
        "snapshot" => {
            let block = gw.client.latest_block()?;
            println!(
                "block {} height {} round {} active_challenges {:?}",
                block.get("id").and_then(Value::as_str).unwrap_or("?"),
                block
                    .pointer("/details/height")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                block
                    .pointer("/details/round")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                block
                    .pointer("/data/active_ids/challenge")
                    .and_then(Value::as_array)
                    .map(|v| v.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
        }
        "plan" | "precommit" => {
            let challenge = a.challenge.ok_or_else(|| anyhow!("--challenge required"))?;
            let algorithm = a.algorithm.ok_or_else(|| anyhow!("--algorithm required"))?;
            // Fetch the block immediately before planning/submitting so
            // settings.block_id honors the latest-or-second-latest rule.
            let block = gw.client.latest_block()?;
            let block_id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned();
            let challenges = gw.client.challenges(&block_id)?;
            let plan = plan_precommit(
                &block,
                &challenges,
                &a.player,
                &challenge,
                &algorithm,
                &a.compute_type,
            )?;
            println!("anchor block {block_id}");
            for (t, fb, fnn) in &plan.fee_by_track {
                println!("track {t}: fee(per-bundle-basis) {fb} fee(per-nonce-basis) {fnn}");
            }
            println!(
                "max collateral: per-bundle-basis {} per-nonce-basis {}",
                plan.max_collateral_per_bundle_basis, plan.max_collateral_per_nonce_basis
            );
            if a.cmd == "plan" {
                println!("{}", serde_json::to_string_pretty(&plan.body)?);
            } else {
                let intent_id = gw.submit_precommit(&plan)?;
                println!("intent {intent_id} recorded; run `reconcile` for confirmation");
            }
        }
        "reconcile" => {
            for line in gw.reconcile()? {
                println!("{line}");
            }
        }
        "status" => {
            for (id, rec) in fold_intents(&gw.ledger.read_all("intents")?) {
                println!(
                    "{id}: {} benchmark_id={}",
                    rec.get("state").and_then(Value::as_str).unwrap_or("?"),
                    rec.get("benchmark_id")
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                );
            }
        }
        other => bail!("unknown subcommand {other} (snapshot|plan|precommit|reconcile|status)"),
    }
    Ok(())
}
