//! The controller process (`architecture.md` §4): snapshot ingestion,
//! reconciliation and — in later slices — orchestration, in one process
//! that never holds the TIG API key.
//!
//! What runs is [`pool_controller::service::Service::tick`] on §11's poll
//! interval: see the latest block, take in any new one, reconcile every
//! workflow from it. Slice 1 stops there; the decision path lands with the
//! active-benchmark cache it depends on (`tig_integration.md` §5.2, §9
//! step 4).
//!
//! Every failure before the loop exits non-zero. A failure inside one poll
//! is logged and the next poll runs: a controller that stops on a transient
//! read error misses blocks, and §10 makes a missed block unrecoverable.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use pool_config::{Binary, Config};
use pool_controller::reconciler::Outcome;
use pool_controller::service::{self, Service, Tick};
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
    let mut service = Service::new(pool, source, poll, network, &tig.player_id, guardrails);

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
fn report(tick: &Tick) {
    match tick {
        Tick::Unchanged { block_id } => {
            tracing::debug!(event = "controller.block.unchanged", block_id = %block_id);
        }
        Tick::Ingested(ingested) => {
            tracing::info!(
                event = "controller.block.ingested",
                block_id = %ingested.block_id,
                height = ingested.height,
            );
            // §10.3: a recorded gap alerts. One line per height, so the
            // alert names what was lost.
            for height in &ingested.gaps_recorded {
                tracing::warn!(
                    event = "controller.block.gap_recorded",
                    height,
                    observed_block_id = %ingested.block_id,
                );
            }
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
