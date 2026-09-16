//! The controller process (`architecture.md` §4): snapshot ingestion,
//! reconciliation and — in later slices — orchestration, in one process
//! that never holds the TIG API key.
//!
//! What runs is [`pool_controller::service::Service::tick`] on §11's poll
//! interval: see the latest block, take in any new one, reconcile every
//! workflow from it, then decide from the same snapshot (`architecture.md`
//! §5.1 step 4).
//!
//! A deployment decides only if it is configured with an
//! `[orchestration.bootstrap_offer]` — slice 1 has no members to offer
//! compute, and absent means it takes blocks in and proposes nothing, which is
//! `tig_integration.md` §13.5's posture.
//!
//! Every failure before the loop exits non-zero. A failure inside one poll
//! is logged and the next poll runs: a controller that stops on a transient
//! read error misses blocks, and §10 makes a missed block unrecoverable.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use pool_config::{Binary, Config};
use pool_controller::decide::Decided;
use pool_controller::propose::Offer;
use pool_controller::reconciler::Outcome;
use pool_controller::service::{self, Deciding, Policy, Service, Tick};
use pool_snapshot::TigSnapshotSource;
use pool_workflow::Guardrails;
use tig_client::{ReadPolicy, TigReadClient, TigReader};

/// The pinned integration baseline, embedded so the binary cannot disagree
/// with the file it was built from (`tig_integration.md` §2).
const PINNED: &str = include_str!("../../../config/tig_integration.json");

#[derive(Parser)]
#[command(name = "pool-controller", about = "TIG mining pool controller")]
struct Cli {
    /// Path to this binary's TOML configuration. Exactly one, always
    /// explicit (`architecture.md` §9).
    #[arg(long, value_name = "PATH")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll for blocks and reconcile from each, until stopped.
    Run,
    /// Take in the latest block, reconcile from it once, and exit. For an
    /// operator checking a deployment or recording evidence.
    Once,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let binary = Binary::PoolController;

    let config = match Config::load(&cli.config, binary) {
        Ok(config) => config,
        Err(e) => {
            // Telemetry is not up yet; this is the one bare stderr write.
            eprintln!("pool-controller: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Held for the rest of main: the root span carries service, deployment
    // and network onto every log line (`architecture.md` §10.1).
    let _telemetry = match pool_telemetry::init(&config, binary) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("pool-controller: {e}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(event = "runtime.failed", error = %e);
            return ExitCode::FAILURE;
        }
    };

    let once = matches!(cli.command, Command::Once);
    match runtime.block_on(run(&config, once)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(event = "controller.failed", error = %e);
            ExitCode::FAILURE
        }
    }
}

async fn run(config: &Config, once: bool) -> Result<(), String> {
    let tig = config
        .tig
        .as_ref()
        .ok_or("configuration has no [tig] section")?;
    let network = config
        .network
        .to_string()
        .parse::<pool_domain::Network>()
        .map_err(|e| e.to_string())?;
    let policy = ReadPolicy::from_config_json(PINNED)
        .map_err(|e| format!("pinned read policy does not load: {e}"))?;
    let guardrails = Guardrails::from_config_json(PINNED)
        .map_err(|e| format!("pinned guardrails do not load: {e}"))?;

    // The URL carries the password: built here, handed to the driver, never
    // logged or stored (`architecture.md` §2.2).
    let url = config
        .database_url()
        .map_err(|e| format!("cannot build database URL: {e}"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_millis(u64::from(
            config.database.connect_timeout_ms,
        )))
        .connect(&url)
        .await
        .map_err(|e| format!("cannot connect to database: {e}"))?;

    // Two clients on one host: the poll, bounded to the interval, and the
    // reader for everything else. They share the host's limiter, so
    // together they stay within the controller's share (ADR-0006). Both
    // are built once and kept for the process's life, which is what makes
    // the shared pacing and any Retry-After pause mean anything (§11).
    let poll = TigReadClient::new(
        &tig.base_url,
        &policy,
        policy.for_block_poll(TigReader::Controller),
    )?;
    let reader = TigReadClient::new(
        &tig.base_url,
        &policy,
        policy.for_reader(TigReader::Controller),
    )?;
    let source = TigSnapshotSource::new(reader, &tig.player_id);
    let orchestration = config
        .orchestration
        .as_ref()
        .ok_or("configuration has no [orchestration] section")?;
    // §5.1 step 4's inputs. The offer is checked against the pinned §3
    // compatibility table here rather than in `pool-config`, because the pin
    // lives with this binary: a compute type outside the table is "ineligible
    // rather than coerced", and refusing at startup beats discovering it on a
    // write whose fee is already paid.
    let offer = match &orchestration.bootstrap_offer {
        Some(configured) => Some(
            Offer::from_config(
                &configured.compute_class,
                configured.cpu_cores,
                &configured.tig_compute_type,
                &serde_json::from_str::<serde_json::Value>(PINNED)
                    .map_err(|e| format!("the pinned configuration does not parse: {e}"))?,
            )
            .map_err(|e| format!("orchestration.bootstrap_offer: {e}"))?,
        ),
        // No offer: this deployment decides nothing, which is what slice 1 is
        // until an operator says otherwise (`tig_integration.md` §13.5).
        None => None,
    };
    let deciding = Deciding {
        offer,
        unverified_limit: orchestration.internal_pool_unverified_limit,
        failure_charge_atoms: orchestration.precommit_failure_charge_atoms.clone(),
        reserve_policy_version: orchestration.reserve_policy_version.clone(),
        config_digest: config.decision_digest(),
    };
    tracing::info!(
        event = "controller.deciding",
        offers = deciding.offer.is_some(),
        compute_type = deciding
            .offer
            .as_ref()
            .map_or("-", |o| o.tig_compute_type.as_str()),
        unverified_limit = deciding.unverified_limit,
        "whether this deployment proposes work"
    );

    let mut service = Service::new(
        pool,
        source,
        poll,
        network,
        &tig.player_id,
        Policy {
            guardrails,
            cache_budget: orchestration.active_cache_fetches_per_poll as usize,
            deciding,
        },
    );

    let interval = policy.block_poll_interval();
    tracing::info!(
        event = "controller.started",
        network = %network,
        tig_host = tig.host().unwrap_or_default(),
        poll_interval_secs = interval.as_secs(),
        once,
    );

    service::run(&mut service, interval, once, report)
        .await
        .map_err(|e| e.to_string())
}

/// One poll's outcome, as log lines. Correlation ids and counts only — the
/// report carries workflow ids, which §10.1 allows, and nothing bulkier.
/// What §5.1 step 4 did with this block (`architecture.md` §10.1).
///
/// A committed decision is the pool's only money-costing effect and criterion
/// K3 records it as the live run's evidence, so it carries the ids §10.1
/// correlates by: the workflow, the intent, and the trace the intent was
/// admitted under. Without this the outcome the service deliberately carried
/// out on the `Tick` was discarded by its only caller, including under
/// `--once`, which is the mode K3 uses.
fn report_decision(ingested: &service::Ingested) {
    let Some(decided) = &ingested.decided else {
        // No offer. Logged nowhere on purpose: a deployment that decides
        // nothing would otherwise emit a line every block saying so, and
        // §10.3's rule is that a bucket filling on every pass is one nobody
        // reads. `controller.deciding` says it once, at startup.
        return;
    };
    match decided {
        Ok(Decided::Admitted(admitted)) => tracing::info!(
            event = "controller.decision.admitted",
            block_id = %ingested.block_id,
            workflow_id = %admitted.intent.workflow_id,
            intent_id = %admitted.intent.intent_id,
            trace_id = admitted
                .intent
                .trace_id
                .map_or_else(|| "-".to_string(), |t| t.to_hex()),
            decision_id = %admitted.decision_id,
            pool_unverified = admitted.pool_unverified,
            unverified_limit = admitted.unverified_limit,
            "a precommit intent was created"
        ),
        Ok(Decided::NoAction) => tracing::debug!(
            event = "controller.decision.no_action",
            block_id = %ingested.block_id,
            "nothing compute-compatible and eligible"
        ),
        Ok(Decided::SnapshotNotUsable(why)) => tracing::debug!(
            event = "controller.decision.snapshot_not_usable",
            block_id = %ingested.block_id,
            reason = %why,
        ),
        Ok(Decided::NotReconciled(why)) => tracing::debug!(
            event = "controller.decision.not_reconciled",
            block_id = %ingested.block_id,
            reason = %why,
            "the block was not reconciled from, so nothing was claimed from it"
        ),
        // §10.3: this is an operator condition, and it is the one the §10 stop
        // exists to surface. Warned rather than logged at info, and the
        // reconciliation line beside it names which workflows.
        Ok(Decided::BlockedForOperator) => tracing::warn!(
            event = "controller.decision.blocked",
            block_id = %ingested.block_id,
            "reconciliation stopped the claiming path; no decision was made"
        ),
        Err(error) => tracing::warn!(
            event = "controller.decision.failed",
            block_id = %ingested.block_id,
            error = %error,
            "the block was taken in; the deciding pass did not complete"
        ),
    }
}

fn report(tick: &Tick) {
    match tick {
        Tick::Unchanged { block_id } => {
            tracing::debug!(event = "controller.block.unchanged", block_id = %block_id);
        }
        Tick::AlreadyIngested { block_id } => {
            tracing::info!(
                event = "controller.block.already_ingested",
                block_id = %block_id,
                "a previous run took this block in; reconciling from the next one"
            );
        }
        Tick::CacheAdvanced {
            block_id,
            cache,
            now_usable,
        } => {
            tracing::info!(
                event = "controller.cache.advanced",
                block_id = %block_id,
                fetched = cache.fetched.len(),
                missing = cache.missing.len(),
                failed = cache.failed.len(),
                now_usable,
            );
            for (benchmark_id, error) in &cache.failed {
                tracing::warn!(
                    event = "controller.cache.fetch_failed",
                    benchmark_id = %benchmark_id,
                    error = %error,
                );
            }
        }
        Tick::Ingested(ingested) => {
            tracing::info!(
                event = "controller.block.ingested",
                block_id = %ingested.block_id,
                height = ingested.height,
                cache_fetched = ingested.cache.fetched.len(),
                cache_missing = ingested.cache.missing.len(),
                cache_ready = ingested.cache.covers_active_set(),
            );
            for (benchmark_id, error) in &ingested.cache.failed {
                tracing::warn!(
                    event = "controller.cache.fetch_failed",
                    benchmark_id = %benchmark_id,
                    error = %error,
                );
            }
            // §10.3: a recorded gap alerts. One line per height, so the
            // alert names what was lost.
            for height in &ingested.gaps_recorded {
                tracing::warn!(
                    event = "controller.block.gap_recorded",
                    height,
                    observed_block_id = %ingested.block_id,
                );
            }
            report_decision(ingested);
            match &ingested.outcome {
                Outcome::Blind { reason, .. } => {
                    tracing::warn!(
                        event = "controller.block.blind",
                        block_id = %ingested.block_id,
                        reason = %reason,
                        "reads incomplete; nothing reconciled"
                    );
                }
                Outcome::Reconciled(pass) => {
                    tracing::info!(
                        event = "controller.block.reconciled",
                        block_id = %pass.block_id,
                        height = pass.height,
                        bound = pass.bind.outcomes.len(),
                        advanced = pass.restart.advanced.len(),
                        unchanged = pass.restart.unchanged.len(),
                        expired = pass.expired.len(),
                        expiry_withheld = pass.expiry_withheld.len(),
                        blocks_claiming = pass.blocks_claiming(),
                    );
                    for item in pass.needs_attention() {
                        tracing::warn!(event = "controller.needs_attention", detail = %item);
                    }
                }
            }
        }
    }
}
