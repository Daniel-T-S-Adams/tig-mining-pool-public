//! Structured logging for pool binaries.
//!
//! `docs/architecture.md` §10.1 owns the rules: structured JSON carrying
//! timestamp, severity, service, deployment, network, event name, and opaque
//! correlation IDs. Sensitive and bulky fields are **allow-listed, not
//! deny-listed** — nothing here ever formats a secret, a signature, a
//! solution body, or a full TIG payload, and new fields are added
//! deliberately rather than by deriving a log line from a whole struct.

use pool_config::{Binary, Config, LogFormat};
use tracing_subscriber::EnvFilter;

/// Initialise the global subscriber for `binary`.
///
/// Call once, before any work. Returns an error string rather than panicking
/// so the caller can exit non-zero with a readable message.
///
/// **The caller must hold the returned guard for the life of the process.**
/// `architecture.md` §10.1 requires `service`, `deployment`, and `network` on
/// every line, and the only way to get them there is a root span that stays
/// entered — attaching them to a startup event puts them on exactly one line
/// and leaves every later line bare.
///
/// One limit worth knowing: an entered span is thread-local, so a task
/// spawned onto another thread carries the fields only if it is
/// `.instrument()`-ed with a span descended from this one. Slice 1 logs from
/// no such task. The slice that first spawns one must either instrument it
/// or replace this mechanism with a field-injecting layer; the assertion in
/// `crates/pool-admin/tests/cli.rs` is what will catch the omission.
#[must_use = "dropping the guard removes service/deployment/network from every \
              subsequent log line, which architecture.md §10.1 requires"]
pub fn init(config: &Config, binary: Binary) -> Result<tracing::span::EnteredSpan, String> {
    let filter = EnvFilter::try_new(&config.telemetry.level).map_err(|e| {
        format!(
            "telemetry.level \"{}\" is invalid: {e}",
            config.telemetry.level
        )
    })?;

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    // JSON unconditionally: `architecture.md` §10.1 makes structured JSON
    // the contract for every process, and `LogFormat` has one variant so it
    // cannot be configured away.
    match config.telemetry.format {
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            // `with_span_list(true)` is load-bearing, not cosmetic.
            // `with_current_span` alone serialises only the INNERMOST span,
            // so once this slice adds the request/assignment/intent spans
            // §10.1 also mandates, the root span's service/deployment/network
            // would silently vanish from every line emitted inside them. The
            // span list keeps the whole stack, so the root's fields survive
            // nesting.
            .with_span_list(true)
            .try_init()
            .map_err(|e| format!("cannot install JSON log subscriber: {e}")),
    }?;

    // Entered for the process lifetime, so a log search can separate two
    // deployments of the same binary without parsing messages.
    let root = tracing::info_span!(
        "service",
        service = binary.as_str(),
        deployment = %config.telemetry.deployment,
        network = %config.network,
    )
    .entered();

    tracing::info!(
        event = "telemetry.started",
        config_digest = %config.decision_digest_hex(),
        "telemetry initialised"
    );
    Ok(root)
}
