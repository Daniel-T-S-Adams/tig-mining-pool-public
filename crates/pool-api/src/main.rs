//! The member-facing process.
//!
//! It starts from exactly one explicit TOML path, like every other binary
//! here, and every failure before the server is up exits non-zero
//! (`architecture.md` §9, slice-2 criterion A1).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pool_config::{Binary, Config};

#[derive(Parser)]
#[command(name = "pool-api", about = "TIG mining pool member API")]
struct Cli {
    /// Path to this binary's TOML configuration. Exactly one, always explicit
    /// (`architecture.md` §9).
    #[arg(long, value_name = "PATH")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the member protocol until stopped.
    Run,
    /// Load and validate the configuration, then exit.
    ///
    /// For an operator asking "would this deployment start" without a process
    /// staying up to answer it — the same question `tig-gateway check` asks of
    /// its own preconditions.
    Check,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let binary = Binary::PoolApi;

    let config = match Config::load(&cli.config, binary) {
        Ok(config) => config,
        Err(e) => {
            // Telemetry is not up yet; this is the one bare stderr write.
            eprintln!("pool-api: {e}");
            return ExitCode::FAILURE;
        }
    };

    let _telemetry = match pool_telemetry::init(&config, binary) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("pool-api: {e}");
            return ExitCode::FAILURE;
        }
    };

    if matches!(cli.command, Command::Check) {
        // The same question `run` asks, without binding a port: would this
        // deployment start? A configuration that loads but names an
        // unreadable ticket key is not a deployment that starts.
        let Some(api) = config.member_api.as_ref() else {
            tracing::error!(
                event = "api.config.failed",
                "pool-api requires [member_api]"
            );
            return ExitCode::FAILURE;
        };
        if let Err(e) = pool_api::service::preflight(api) {
            tracing::error!(event = "api.preflight.failed", error = %e);
            return ExitCode::FAILURE;
        }
        tracing::info!(
            event = "api.config.ok",
            "configuration loads and preflight passes"
        );
        return ExitCode::SUCCESS;
    }

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

    match runtime.block_on(pool_api::service::run(&config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(event = "api.failed", error = %e);
            ExitCode::FAILURE
        }
    }
}
