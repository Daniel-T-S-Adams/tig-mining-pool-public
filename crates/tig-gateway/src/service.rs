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

use crate::drive::{self, Driver};
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
    let lease_owner = format!(
        "tig-gateway/{}@{}",
        std::process::id(),
        hostname().unwrap_or_else(|| "unknown".to_string())
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
                    if report.needs_operator() {
                        tracing::warn!(
                            event = "gateway.pass.needs_operator",
                            outcomes = report.outcomes.len(),
                            "a claim stopped for an operator"
                        );
                    }
                }
                Err(e) => tracing::error!(event = "gateway.pass.failed", error = %e),
            },
            Err(e) => tracing::warn!(event = "gateway.window.unavailable", error = %e),
        }

        tokio::time::sleep(read_policy.block_poll_interval()).await;
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
    if pins.network != network {
        return Err(format!(
            "configured network {network} is not the pinned {}",
            pins.network
        ));
    }

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
        openapi: evidence::openapi_checksum(&evidence::pinned_openapi_url()?).await,
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
