//! Running the gateway: the §13 gate, then the claim loop.
//!
//! **This lives in the library, not in `src/main.rs`, and the reason is not
//! taste.** A package's binary is a separate crate from its library, so
//! `credential::load` being crate-private keeps the key out of `main.rs` just
//! as firmly as it keeps it out of `pool-controller`. The key is loaded, used
//! and dropped inside this module; nothing outside the library can obtain a
//! `TigApiKey` at all, which is what `architecture.md` §2.2 asks for and what
//! `scripts/credential-boundary.sh` checks from the other direction.
//!
//! An earlier draft put this in the binary on the belief that
//! `pub(crate)` reached it. It does not, and discovering that is what moved
//! the loop here rather than widening `load` to `pub`.

use std::time::Duration;

use pool_config::Config;
use tig_client::{ReadPolicy, TigReadClient, TigReader};

use crate::drive::{self, Driver, Notability};
use crate::lane::PostLane;
use crate::readiness::{Evidence, Pins, WriteReady, evaluate};
use crate::transmit::PrecommitTransmitter;
use crate::write_gate::WriteGate;
use crate::write_policy::WritePolicy;
use crate::{credential, evidence};

/// The pinned integration baseline, embedded so the binary cannot disagree
/// with the file it was built from (`tig_integration.md` §2).
const PINNED: &str = include_str!("../../../config/tig_integration.json");

pub async fn run(config: &Config, check_only: bool) -> Result<(), String> {
    let tig = config
        .tig
        .as_ref()
        .ok_or("configuration has no [tig] section")?;
    let gateway = config
        .gateway
        .as_ref()
        .ok_or("configuration has no [gateway] section")?;
    let network = config
        .network
        .to_string()
        .parse::<pool_domain::Network>()
        .map_err(|e| e.to_string())?;

    let read_policy = ReadPolicy::from_config_json(PINNED)
        .map_err(|e| format!("pinned read policy does not load: {e}"))?;
    let write_policy = WritePolicy::from_config_json(PINNED)
        .map_err(|e| format!("pinned write policy does not load: {e}"))?;

    // The key, loaded once and held for the process's life. `credential::load`
    // is crate-private, which is why this binary lives inside the crate: no
    // other component can reach the loading path at all (§2.2).
    let key = credential::load(&gateway.api_key_file)
        .map_err(|e| format!("cannot load the TIG API key: {e}"))?;

    let reader = TigReadClient::new(
        &tig.base_url,
        &read_policy,
        read_policy.for_reader(TigReader::Gateway),
    )?;

    // §7.5's rule, checked here rather than discovered per pass. `run_once`
    // refuses a lease shorter than a write's call timeout, and both values are
    // known now — so a misconfigured gateway that only found out inside the
    // loop would log "write gate open" and then transmit nothing, for ever.
    // `architecture.md` §9 requires exiting before serving on a cross-field
    // error, and `main.rs`'s own doc names a process that started but cannot
    // write as the more dangerous of the two.
    // The same comparison `drive::check_lease` makes per pass, made once here
    // against the same pinned policy, so a lease that every pass would reject
    // is refused before the first one rather than for ever after it.
    drive::lease_outlasts_call(gateway.lease_secs, write_policy.call_timeout().as_secs())
        .map_err(|e| format!("{e} (architecture.md §7.5)"))?;

    // §13, before anything else. `WriteReady` has no public constructor, so
    // the gate below cannot be opened by a path that skipped this.
    let ready = gate_evidence(&reader, tig, gateway, network, &key).await?;

    if check_only {
        tracing::info!(
            event = "gateway.write_ready.checked",
            upstream_commit = ready.upstream_commit(),
            containers_unresolved = ready.containers_unresolved().unwrap_or("none"),
            "all nine §13 checks passed"
        );
        return Ok(());
    }

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

    let transmitter = PrecommitTransmitter::new(&tig.base_url, &write_policy)
        .map_err(|e| format!("cannot build the transmitter: {e}"))?;
    let lane = PostLane::new(write_policy.clone());
    let gate = WriteGate::open(ready);

    // Who holds a lease. Stable across this process's life and distinct
    // across processes, which is what §7.5's fence rests on.
    // Unique by construction. Falling back to `@unknown` when `/proc` is
    // unreadable let two containerised gateways — both PID 1, both without a
    // readable hostname — share an owner, and `work_lease`'s claim re-enters
    // on a matching owner: each could take over the other's unexpired lease
    // mid-call, which is precisely what the lease-outlasts-the-call rule
    // exists to prevent. The nonce is what makes the string distinct; the
    // host and pid are there to make it legible to a human reading the row.
    let lease_owner = format!(
        "tig-gateway/{}@{}/{}",
        std::process::id(),
        hostname().unwrap_or_else(|| "unknown".to_string()),
        startup_nonce()?
    );

    tracing::info!(
        event = "gateway.started",
        network = %network,
        lease_owner = %lease_owner,
        "write gate open"
    );

    loop {
        let driver = Driver {
            pool: &pool,
            network,
            player_id: &tig.player_id,
            lease_owner: &lease_owner,
            lease_secs: gateway.lease_secs,
            served_compute: &gateway.served_compute,
            transmitter: &transmitter,
            lane: &lane,
            // Slice 1 constructs no commitments: §3 keeps that out of the
            // gateway and the artifact worker that would supply one does not
            // exist yet. `decide_benchmark` reads `None` as "no bytes this
            // pass", which is the ordinary answer rather than a fault.
            commitment: None,
            gate: &gate,
            key: &key,
        };

        // The confirmed window the precommit path reconciles against. A read
        // that fails is logged and the pass is skipped: §10 makes the search
        // the recovery path, and a gateway that stopped on a transient read
        // error would stop transmitting entirely.
        match confirmed_precommits(&reader, tig).await {
            Ok(window) => match drive::run_once(&driver, &window).await {
                Ok(report) => {
                    for outcome in &report.outcomes {
                        report_outcome(outcome);
                    }
                }
                Err(e) => tracing::error!(event = "gateway.pass.failed", error = %e),
            },
            Err(e) => tracing::warn!(event = "gateway.window.unavailable", error = %e),
        }

        tokio::time::sleep(read_policy.block_poll_interval()).await;
    }
}

/// One line per intent, carrying the ids §10.1 requires to correlate it.
///
/// Per intent rather than per pass: the pass count that stood here said how
/// many things happened and nothing about which, so the pool's only
/// money-costing effect and its operator stops left no correlated record.
///
/// The level comes from [`Notability`] rather than from a judgement made here,
/// so what the report raises and what the log warns about are the same
/// question asked once. Routine contention goes to `debug` deliberately: it
/// occurs on every pass with a precommit in flight, and at `info` it would
/// bury the two events that matter (§10.3).
fn report_outcome(outcome: &drive::IntentOutcome) {
    let intent_id = outcome.intent_id.as_str();
    let workflow_id = outcome.workflow_id.as_str();
    let generation = outcome.generation;
    let attempt_id = outcome.attempt_id().unwrap_or("-");
    let benchmark_id = outcome.benchmark_id().unwrap_or("-");
    // §10.1's cross-restart correlation (criterion I3). The controller that
    // admitted this intent is a different process, and on the far side of a
    // crash it may no longer be running at all; this is what joins its
    // decision to this transmission.
    let trace_id = outcome
        .trace_id
        .map_or_else(|| "-".to_string(), |t| t.to_hex());
    // `decision` and `acted` carry their own payloads — a stop's reason, a
    // failure's error — so they are recorded whole rather than flattened to a
    // name that would drop exactly the part an operator needs.
    match outcome.notability() {
        Notability::Operator => tracing::warn!(
            event = "gateway.intent.outcome",
            intent_id,
            workflow_id,
            generation,
            attempt_id,
            benchmark_id,
            trace_id,
            decision = ?outcome.decision,
            acted = ?outcome.acted,
            "an intent needs an operator"
        ),
        Notability::Effect => tracing::info!(
            event = "gateway.intent.outcome",
            intent_id,
            workflow_id,
            generation,
            attempt_id,
            benchmark_id,
            trace_id,
            decision = ?outcome.decision,
            acted = ?outcome.acted,
            "an intent reached TIG"
        ),
        Notability::Routine => tracing::debug!(
            event = "gateway.intent.outcome",
            intent_id,
            workflow_id,
            generation,
            attempt_id,
            benchmark_id,
            trace_id,
            decision = ?outcome.decision,
            acted = ?outcome.acted,
            "an intent made no change"
        ),
    }
}

/// The nine observations, gathered and judged.
///
/// Every one is gathered before any is judged, so a failure reports *all* the
/// checks that did not pass rather than the first. An operator fixing a
/// deployment wants the list, not a sequence of single-item discoveries.
async fn gate_evidence(
    reader: &TigReadClient,
    tig: &pool_config::TigConfig,
    gateway: &pool_config::GatewayConfig,
    network: pool_domain::Network,
    key: &credential::TigApiKey,
) -> Result<WriteReady, String> {
    let pins = Pins::compiled_in(tig.player_id.clone(), gateway.platform.clone())?;

    // One block for every anchored read below. §9's discipline: reads that
    // disagree about which block they describe cannot be compared, and a gate
    // asking its questions at three heights judges a state that never was.
    let anchor = evidence::anchor_block(reader).await;

    let evidence = Evidence {
        config_network: Ok(network),
        upstream_commit: crate::readiness::acquired_upstream_commit(&tig.acquired_upstream_commit),
        resolved_images: Ok(evidence::unresolved_containers(
            tig.unresolved_containers_acknowledged.as_deref(),
        )),
        // A pinned file with no specification URL is a check-4 failure, not a
        // reason to stop gathering: `?` here would report one broken pin and
        // hide the other eight answers, which is what this function exists
        // not to do. `evaluate` compares `config_network` against the pin, so
        // a network mismatch reports as check 1 for the same reason.
        openapi: match evidence::pinned_openapi_url() {
            Ok(url) => evidence::openapi_checksum(&url).await,
            Err(e) => Err(e),
        },
        response_models: match &anchor {
            Ok((block, _)) => evidence::response_models(reader, block, &tig.player_id).await,
            Err(e) => Err(e.clone()),
        },
        active_challenges: match &anchor {
            Ok((block, round)) => {
                evidence::active_challenge_runtimes(
                    reader,
                    block,
                    *round,
                    &pins.image_names,
                    &gateway.served_compute.iter().cloned().collect(),
                )
                .await
            }
            Err(e) => Err(e.clone()),
        },
        fixtures: evidence::serialization_fixtures(),
        api_key: evidence::api_key_placement(key.is_present(), &gateway.api_key_file),
        confirmed_pool_player_id: match &anchor {
            Ok((block, _)) => {
                evidence::confirmed_pool_player_id(reader, block, &tig.player_id).await
            }
            Err(e) => Err(e.clone()),
        },
    };

    evaluate(&pins, &evidence).map_err(|failures| {
        for failure in &failures {
            tracing::error!(
                event = "gateway.write_ready.failed",
                check = failure.check.number(),
                detail = %failure.detail,
            );
        }
        let names: Vec<String> = failures.iter().map(|f| f.check.to_string()).collect();
        format!("§13 refused this deployment: {}", names.join(", "))
    })
}

/// The confirmed precommit window §10's search reconciles against.
///
/// The gateway's own read, not the controller's snapshot: §10 makes this the
/// search a *write* needs before retry, and a gateway that waited for another
/// process's view of the window could not answer "did my write land" while
/// that process was down.
async fn confirmed_precommits(
    reader: &TigReadClient,
    tig: &pool_config::TigConfig,
) -> Result<Vec<serde_json::Value>, String> {
    let (block, _) = evidence::anchor_block(reader).await?;
    let body = reader
        .get_json(&format!(
            "/get-benchmarks?block_id={block}&player_id={}",
            tig.player_id
        ))
        .await
        .map_err(|e| format!("get-benchmarks failed: {e}"))?;
    Ok(body
        .get("precommits")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default())
}

/// This host's name, for the lease owner.
///
/// Read from `/proc`, for the reason `evidence::effective_uid` gives: the one
/// crate holding the API key is the last place to add a dependency for a
/// string. An unreadable name is not fatal — the process id already makes the
/// owner unique on a host, and the name only helps a human reading the row.
fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// A value distinct for every start of this process.
///
/// Not randomness for its own sake: two gateways that cannot read a hostname
/// and share a pid would otherwise produce the same owner string, and §7.5's
/// fence rests on owners being distinct across processes.
///
/// Drawn from the OS rather than from the clock. A startup instant reads as
/// unique until you notice that the environment which produces the collision —
/// no readable `/proc`, no hostname — is the same stripped container likely to
/// have no working clock, and that a `SystemTime` before the epoch has to fall
/// back to *some* value, which every such process would share. Randomness has
/// no degraded case to share.
///
/// Fails rather than substitutes. There is no second-best value here: a
/// gateway that cannot distinguish itself from another cannot safely hold a
/// fenced lease, and §9 says to exit rather than serve.
fn startup_nonce() -> Result<u128, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| format!("cannot draw a startup nonce from the OS: {e}"))?;
    Ok(u128::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn two_starts_do_not_present_the_same_lease_owner() {
        // §7.5's fence rests on the owner distinguishing processes. The value
        // this asserts about is the nonce, because the other two components
        // are exactly what the degraded case makes identical: two containers
        // at PID 1 with no readable hostname agree on `tig-gateway/1@unknown`.
        let a = startup_nonce().expect("the OS has randomness");
        let b = startup_nonce().expect("the OS has randomness");
        assert_ne!(a, b, "two starts must not share an owner");
        assert_ne!(a, 0, "a fallback shared by every degraded process");
    }
}
