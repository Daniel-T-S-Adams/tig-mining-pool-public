//! Slice-1 criteria F4a, F4c and F4d: the stand-in for facts slice 1 cannot
//! produce honestly.
//!
//! `architecture.md` §13 invariants 4 and 5 forbid a benchmark write without a
//! durable package acceptance and a proof write without a canonical payload
//! for the confirmed sample. `migrations/0011` enforces both. Slice 1 has no
//! members, no uploads and no artifact worker, so it cannot produce either
//! fact — and the plan is explicit that this does **not** license skipping the
//! check: "simulated bytes are fine, a missing precondition is not". Driving
//! the lifecycle with the guard removed would let slice 1 enshrine an
//! unguarded commitment path in a passing test.
//!
//! So this fabricates the rows, and everything about how it is reachable is
//! the point.
//!
//! **F4c — it is not in a real build.** The whole module sits behind the
//! `stub-acceptance` cargo feature, and `scripts/feature-gate.sh` compiles a
//! probe outside the workspace to prove a default `pool-controller` build
//! cannot name it. Criterion K3 runs these binaries against live testnet;
//! something that can assert "this package was durably accepted" must be
//! absent from that binary, not merely refused inside it.
//!
//! **F4d — and even then, only against the fake.** Defence in depth behind
//! F4c: a build that *does* carry the feature still refuses unless the
//! configured TIG endpoint is a local `fake-tig`. The two guards fail
//! independently, which is the reason for having both — a test binary is
//! exactly the thing someone eventually points at a real endpoint.
//!
//! The check reads the **endpoint**, not `network`. A2 pins `network` to
//! exactly `testnet`, so it cannot tell the real testnet API from a local
//! fake, and a guard keyed to it would pass in the one case it exists to
//! prevent.
//!
//! **Both halves of F4d now exist, in different places.** The criterion
//! refuses "creating **or accepting**" a stub record against a live endpoint.
//! This module is the creating half, and it is compiled out of a default
//! build (F4c). The accepting half is `crate::commit`'s, on the path that
//! turns a fabricated row into a TIG write, and it is in *every* build —
//! because the row it refuses may have been written by a build that had the
//! stub (a developer's database promoted, a dump restored) and the endpoint
//! is what decides whether acting on it is safe.
//!
//! `stub_origin` on each row is what makes the accepting half possible: a
//! fabricated row written without it is indistinguishable from a real
//! acceptance for the rest of the database's life, and no later migration
//! recovers what it was.

use pool_config::TigConfig;
use pool_domain::Network;
use pool_workflow::{
    AcceptanceError, CanonicalPayload, CommitmentPayload, PackageAcceptance, record_acceptance,
    record_canonical_payload, record_commitment_payload,
};

#[derive(Debug, thiserror::Error)]
pub enum StubError {
    /// The configured TIG endpoint is not a local `fake-tig`.
    ///
    /// Carries the **host** so an operator reading the log sees what the
    /// process was pointed at, which is the fact that decided this.
    ///
    /// The host and not the URL: `Config::load` refuses userinfo, but a
    /// `TigConfig` built directly in a feature-carrying build need not have
    /// come through it, and §9 keeps credentials out of logs whatever route
    /// they arrived by.
    #[error(
        "refusing to fabricate {what}: the TIG endpoint host {host} is not a local fake-tig, \
         and a stub acceptance is only ever a fake-tig fixture (slice-1 F4d)"
    )]
    NotFakeTig { what: &'static str, host: String },
    #[error(transparent)]
    Acceptance(#[from] AcceptanceError),
}

/// The bytes a stub stands in for.
///
/// Fixed and obviously synthetic. A stub that invented plausible-looking
/// digests would produce rows indistinguishable from real ones in a database
/// someone later looks at; these are recognisably a fixture.
const STUB_PACKAGE_SHA256: [u8; 32] = [0x5b; 32];
const STUB_SAMPLE_DIGEST: [u8; 32] = [0x5c; 32];

/// Record a stub durable package acceptance, satisfying invariant 4.
///
/// Fails unless `tig` names a local `fake-tig`.
pub async fn stub_acceptance<'e, E>(
    executor: E,
    tig: &TigConfig,
    network: Network,
    workflow_id: &str,
    benchmark_id: &str,
) -> Result<(), StubError>
where
    E: sqlx::PgExecutor<'e>,
{
    guard(tig, "a package acceptance")?;
    record_acceptance(
        executor,
        &PackageAcceptance {
            network,
            workflow_id: workflow_id.to_string(),
            benchmark_id: benchmark_id.to_string(),
            package_sha256: STUB_PACKAGE_SHA256,
            // What makes F4d's accepting half possible at all.
            stub_origin: true,
        },
    )
    .await?;
    Ok(())
}

/// Record a stub canonical proof payload, satisfying invariant 5.
///
/// `payload_digest` is the caller's, not a fixture: `migrations/0011` requires
/// it to equal the digest on the proof intent, and a stub that chose its own
/// would make every proof-intent test pass for the wrong reason.
pub async fn stub_canonical_payload<'e, E>(
    executor: E,
    tig: &TigConfig,
    network: Network,
    artifact_id: &str,
    workflow_id: &str,
    benchmark_id: &str,
    payload_digest: [u8; 32],
) -> Result<(), StubError>
where
    E: sqlx::PgExecutor<'e>,
{
    guard(tig, "a canonical payload")?;
    record_canonical_payload(
        executor,
        &CanonicalPayload {
            network,
            artifact_id: artifact_id.to_string(),
            workflow_id: workflow_id.to_string(),
            benchmark_id: benchmark_id.to_string(),
            sample_digest: STUB_SAMPLE_DIGEST,
            payload_digest,
            stub_origin: true,
        },
    )
    .await?;
    Ok(())
}

/// Record a stub commitment payload, satisfying `migrations/0015`.
///
/// `payload_digest` is the caller's, for the reason
/// [`stub_canonical_payload`]'s is: the trigger requires it to equal the
/// intent's, and a stub that chose its own would make every commitment test
/// pass without the body ever being the one submitted.
pub async fn stub_commitment_payload<'e, E>(
    executor: E,
    tig: &TigConfig,
    network: Network,
    artifact_id: &str,
    workflow_id: &str,
    benchmark_id: &str,
    payload_digest: [u8; 32],
) -> Result<(), StubError>
where
    E: sqlx::PgExecutor<'e>,
{
    guard(tig, "a commitment payload")?;
    record_commitment_payload(
        executor,
        &CommitmentPayload {
            network,
            artifact_id: artifact_id.to_string(),
            workflow_id: workflow_id.to_string(),
            benchmark_id: benchmark_id.to_string(),
            payload_digest,
            stub_origin: true,
        },
    )
    .await?;
    Ok(())
}

/// F4d, in one place so both entry points cannot disagree about it.
fn guard(tig: &TigConfig, what: &'static str) -> Result<(), StubError> {
    if tig.is_local_fake_tig() {
        return Ok(());
    }
    Err(StubError::NotFakeTig {
        what,
        host: tig.host().unwrap_or_else(|| "<unparseable>".to_string()),
    })
}
