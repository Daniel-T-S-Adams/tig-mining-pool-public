//! Operator CLI.
//!
//! Slice 1 ships one subcommand: `migrate`, the one-shot database migration
//! job of `docs/architecture.md` §4 and §7.1. Normal services own no DDL
//! privilege and never auto-migrate at startup; this binary is the only
//! thing that applies schema changes, and it runs as `pool_migration`.
//!
//! Every failure path exits non-zero before doing partial work.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pool_config::{Binary, Config};

/// The migration job is the only caller entitled to the DDL-bearing
/// credential (`architecture.md` §9). Other subcommands, when they exist,
/// go through the controller's private endpoint (§4) and need no database
/// credential at all.
const MIGRATE_BINARY: Binary = Binary::PoolAdminMigrate;

/// Embedded at compile time from the repository's single ordered migration
/// directory, so the binary that runs a migration cannot disagree with the
/// files it was built from.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// PostgreSQL `undefined_table`, the one error that legitimately means the
/// migration ledger has not been created yet.
const UNDEFINED_TABLE: &str = "42P01";

/// Advisory lock serialising the whole migrate operation.
///
/// Distinct from the lock SQLx takes inside `run`: that one serialises the
/// applying, this one serialises read-plan-apply-confirm as a unit, so the
/// before and after snapshots bracket exactly this process's work. Without
/// it the plan is read outside any lock and a losing concurrent invocation
/// reports the winner's migrations as its own.
const MIGRATE_LOCK_KEY: i64 = 0x706f_6f6c_6d69_6772;

#[derive(Parser)]
#[command(name = "pool-admin", about = "TIG mining pool operator CLI")]
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
    /// Apply every pending migration under the migration lock.
    Migrate {
        /// Report what would be applied and exit without applying it.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Which role the config must name depends on the operation, not on the
    // binary.
    let binary = match cli.command {
        Command::Migrate { .. } => MIGRATE_BINARY,
    };

    let config = match Config::load(&cli.config, binary) {
        Ok(config) => config,
        Err(e) => {
            // Telemetry is not up yet, and this is the one place a bare
            // stderr write is the only option.
            eprintln!("pool-admin: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Held for the rest of main: the root span carries service, deployment
    // and network onto every log line (`architecture.md` §10.1).
    let _telemetry = match pool_telemetry::init(&config, binary) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("pool-admin: {e}");
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

    match cli.command {
        Command::Migrate { dry_run } => match runtime.block_on(migrate(&config, dry_run)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(event = "migrate.failed", error = %e);
                ExitCode::FAILURE
            }
        },
    }
}

async fn migrate(config: &Config, dry_run: bool) -> Result<(), String> {
    // The URL carries the password: built here, handed to the driver, never
    // logged or stored (`architecture.md` §2.2).
    let url = config
        .database_url()
        .map_err(|e| format!("cannot build database URL: {e}"))?;

    // Two connections: one holds the operation lock for the duration, the
    // other does the work.
    //
    // The acquire timeout is set explicitly because sqlx retries a refused
    // connection until it expires, and its default is thirty seconds. A1 says
    // a job that cannot reach its database must fail closed; doing so half a
    // minute late is a worse answer than doing so promptly, and the retry
    // decision belongs to whatever invoked this — which cannot make it until
    // this process returns.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_millis(u64::from(
            config.database.connect_timeout_ms,
        )))
        .connect(&url)
        .await
        .map_err(|e| format!("cannot connect to database: {e}"))?;

    // Session-scoped, so it is released even if this process dies mid-run.
    let mut lock_conn = pool
        .acquire()
        .await
        .map_err(|e| format!("cannot acquire a connection: {e}"))?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await
        .map_err(|e| format!("cannot take the migrate lock: {e}"))?;

    // Only "the ledger table does not exist yet" means nothing is applied.
    // Treating every error that way would let a permission failure or a lost
    // connection render a dry-run plan claiming the whole schema is pending
    // on an already-migrated database.
    let applied: Vec<(i64, Vec<u8>, bool)> = match sqlx::query_as(
        "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    {
        Ok(rows) => rows,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNDEFINED_TABLE) => Vec::new(),
        Err(e) => return Err(format!("cannot read applied migrations: {e}")),
    };
    let applied_versions: Vec<i64> = applied.iter().map(|(v, _, _)| *v).collect();

    // Verify checksums here, not only inside `run`. A dry run is the
    // operator's pre-deploy evidence for a mutation they own
    // (`architecture.md` §6), so it has to refuse everything the real run
    // would refuse. Comparing versions alone would report a clean plan for a
    // repository whose already-applied migration file had been edited —
    // exactly the forward-only violation §7.1 exists to stop.
    for (version, applied_checksum, success) in &applied {
        // A row left dirty by a migration that died partway. `run` refuses
        // it with "migration N is partially applied", so the pre-check must
        // too — otherwise a dry run reports a clean plan and exit 0 for a
        // database the real run cannot migrate, which is the opposite of
        // what a pre-deploy check is for.
        if !success {
            return Err(format!(
                "migration {version} is partially applied and needs manual repair; \
                 `run` will refuse it. Fix the schema and remove its row from \
                 `_sqlx_migrations`"
            ));
        }
        match MIGRATOR.iter().find(|m| m.version == *version) {
            Some(migration) if migration.checksum.as_ref() != applied_checksum.as_slice() => {
                return Err(format!(
                    "migration {version} ({}) was previously applied but has been modified; \
                     migrations are forward-only",
                    migration.description
                ));
            }
            Some(_) => {}
            None => {
                return Err(format!(
                    "migration {version} is recorded as applied but is absent from this build; \
                     the binary and the database disagree"
                ));
            }
        }
    }

    let pending: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| !applied_versions.contains(&m.version))
        .collect();

    tracing::info!(
        event = "migrate.plan",
        applied = applied_versions.len(),
        pending = pending.len(),
        dry_run,
    );
    for migration in &pending {
        tracing::info!(
            event = "migrate.pending",
            version = migration.version,
            description = %migration.description,
        );
    }

    if dry_run {
        tracing::info!(event = "migrate.dry_run_complete", applied_now = 0);
        drop(lock_conn);
        return Ok(());
    }

    // `run` takes the advisory migration lock, applies each pending
    // migration in version order, and records version and checksum. A
    // checksum mismatch against an already-applied version is an error, not
    // a silent re-apply — that is what makes the directory forward-only in
    // practice and not only by convention.
    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| format!("migration failed: {e}"))?;

    // Report what THIS process applied. Both snapshots are taken inside the
    // operation lock, so no other invocation can apply anything between
    // them: the difference is this process's work and nobody else's.
    // `architecture.md` §6 makes this an owned mutation and §13 invariant 6
    // requires an auditable result, so the number has to be a fact rather
    // than an intention — and, since the lock brackets it, a fact about the
    // right process.
    let after: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .map_err(|e| format!("cannot confirm applied migrations: {e}"))?;
    let applied_now = after
        .iter()
        .filter(|v| !applied_versions.contains(v))
        .count();

    tracing::info!(
        event = "migrate.complete",
        applied_now,
        planned = pending.len(),
        total_applied = after.len(),
    );

    drop(lock_conn);
    Ok(())
}
