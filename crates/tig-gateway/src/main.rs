//! The gateway process (`architecture.md` §4): the only holder of the TIG API
//! key and the only transmitter of protocol writes.
//!
//! What runs is [`tig_gateway::drive::run_once`] on §11's pacing — claim each
//! prepared intent, decide what it is owed, act, record. Nothing here chooses
//! work: `architecture.md` §3 keeps that with the controller, and this process
//! could not do it without the reads it deliberately does not make.
//!
//! **Writes are gated before the loop starts.** §13's nine checks run once, at
//! startup, and a gateway that fails any of them exits rather than serving.
//! That is the fail-closed posture §13 asks for: a process that started but
//! cannot write is more dangerous than one that did not start, because the
//! first looks healthy.
//!
//! Every failure before the loop exits non-zero. A failure *inside* one pass
//! is logged and the next pass runs, for the reason the controller gives: a
//! transient read error is not a reason to stop transmitting, and §10 makes
//! recovery the normal path rather than an exception.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pool_config::{Binary, Config};

#[derive(Parser)]
#[command(name = "tig-gateway", about = "TIG mining pool gateway")]
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
    /// Evaluate §13's checks and transmit claimable writes, until stopped.
    Run,
    /// Evaluate §13's checks, report each one, and exit without writing.
    ///
    /// For an operator asking "may this deployment write, and if not, which
    /// check says no" without the answer depending on a process staying up.
    Check,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let binary = Binary::TigGateway;

    let config = match Config::load(&cli.config, binary) {
        Ok(config) => config,
        Err(e) => {
            // Telemetry is not up yet; this is the one bare stderr write.
            eprintln!("tig-gateway: {e}");
            return ExitCode::FAILURE;
        }
    };

    let _telemetry = match pool_telemetry::init(&config, binary) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("tig-gateway: {e}");
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

    let check_only = matches!(cli.command, Command::Check);
    match runtime.block_on(tig_gateway::service::run(&config, check_only)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(event = "gateway.failed", error = %e);
            ExitCode::FAILURE
        }
    }
}
