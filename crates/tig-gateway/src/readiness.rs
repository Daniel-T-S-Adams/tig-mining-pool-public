//! §13: the nine compatibility checks that gate `WRITE_READY`.
//!
//! §13 owns the list; this implements it and does not restate the rules.
//! Two shapes matter here.
//!
//! **Evaluation is pure.** [`evaluate`] takes the pinned values and the
//! gathered observations and returns a verdict. Gathering — resolving a
//! registry digest, fetching the hosted OpenAPI document, replaying the
//! fixtures — is the caller's, and lives with the binary that has network
//! access. That split is what makes slice-1 criterion B2 possible: each of
//! the nine can be failed on its own, offline and deterministically, rather
//! than only against a live registry that happens to be misconfigured.
//!
//! **Every observation is a `Result`.** Evidence that could not be gathered
//! fails its check; it is never skipped. A gate whose checks quietly pass
//! when their input is unavailable is worse than no gate, because the
//! failure surfaces as a successful write against an incompatible API.

use std::collections::BTreeMap;

use pool_domain::Network;

/// The nine checks of §13, in the order §13 lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    /// 1. Project config parses and the network is exactly `testnet`.
    ConfigNetwork,
    /// 2. The acquired upstream source commit matches the pin.
    UpstreamCommit,
    /// 3. Every required container resolves to the pinned manifest digest
    ///    for the current platform.
    ContainerDigests,
    /// 4. The hosted OpenAPI checksum matches the reviewed checksum, or an
    ///    explicit reviewed local schema override is active.
    OpenApiChecksum,
    /// 5. The latest block, challenges, algorithms, OPoW and pool lifecycle
    ///    responses validate against required models.
    ResponseModels,
    /// 6. Every live active challenge the decision engine considers has a
    ///    pinned runtime and a supported compute path.
    ChallengeRuntimes,
    /// 7. Lossless numeric parsing and canonical request serialization
    ///    fixtures pass.
    SerializationFixtures,
    /// 8. The API key is present in the gateway without being readable by
    ///    member services.
    ApiKeyIsolation,
    /// 9. The pool player ID returned by confirmed data matches configured
    ///    identity.
    PlayerIdentity,
}

impl Check {
    /// All nine. Iterated rather than hand-listed at each use, so a tenth
    /// check cannot be added and silently left out of the gate.
    pub const ALL: &'static [Check] = &[
        Check::ConfigNetwork,
        Check::UpstreamCommit,
        Check::ContainerDigests,
        Check::OpenApiChecksum,
        Check::ResponseModels,
        Check::ChallengeRuntimes,
        Check::SerializationFixtures,
        Check::ApiKeyIsolation,
        Check::PlayerIdentity,
    ];

    /// §13's numbering, so an operator reading a failure can find the rule.
    pub fn number(self) -> u8 {
        match self {
            Check::ConfigNetwork => 1,
            Check::UpstreamCommit => 2,
            Check::ContainerDigests => 3,
            Check::OpenApiChecksum => 4,
            Check::ResponseModels => 5,
            Check::ChallengeRuntimes => 6,
            Check::SerializationFixtures => 7,
            Check::ApiKeyIsolation => 8,
            Check::PlayerIdentity => 9,
        }
    }
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "§13 check {} ({:?})", self.number(), self)
    }
}

/// Why one check did not pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{check} failed: {detail}")]
pub struct Failure {
    pub check: Check,
    pub detail: String,
}

/// The reviewed values a deployment is pinned to.
///
/// From `config/tig_integration.json` and the pool's own configuration. §13
/// compares observations against these; nothing here is discovered at
/// runtime, which is the point — "moving a Git branch or container tag is
/// never accepted automatically".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    pub network: Network,
    pub upstream_commit: String,
    /// Image reference to pinned manifest digest.
    pub image_digests: BTreeMap<String, String>,
    /// The platform those digests were reviewed for.
    pub platform: String,
    pub openapi_sha256: String,
    pub pool_player_id: String,
}

/// One container resolved against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub reference: String,
    pub manifest_digest: String,
    pub platform: String,
}

/// What was observed about the OpenAPI document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApiObservation {
    /// Checksum of the hosted document, when it could be fetched.
    pub hosted_sha256: Option<String>,
    /// An explicit, reviewed local schema override. §13 accepts this as an
    /// alternative to the hosted checksum — but only when it is marked
    /// reviewed, which is what stops "the fetch failed" from becoming a
    /// silent override.
    pub reviewed_local_override: Option<String>,
}

/// One required response model, and whether it validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelValidation {
    pub endpoint: String,
    pub valid: bool,
    pub detail: String,
}

/// One live active challenge and its runtime support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveChallengeRuntime {
    pub challenge_id: String,
    pub runtime_pinned: bool,
    pub compute_path_supported: bool,
}

/// The §7 fixture replays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureOutcome {
    pub lossless_numeric_parsing: bool,
    pub canonical_request_serialization: bool,
    pub detail: String,
}

/// Where the API key was found, and where it must not be.
///
/// Carries no key material — only the two facts §13 check 8 asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiKeyPlacement {
    pub present_in_gateway: bool,
    pub readable_by_member_services: bool,
}

/// The endpoints §13 check 5 names.
const REQUIRED_MODELS: &[&str] = &[
    "get-block",
    "get-challenges",
    "get-algorithms",
    "get-opow",
    "get-benchmarks",
];

/// Observations gathered for the gate.
///
/// Each is a `Result` so that a failure to gather fails its check rather
/// than skipping it (see the module doc).
#[derive(Debug, Clone)]
pub struct Evidence {
    pub config_network: Result<Network, String>,
    pub upstream_commit: Result<String, String>,
    pub resolved_images: Result<Vec<ResolvedImage>, String>,
    pub openapi: Result<OpenApiObservation, String>,
    pub response_models: Result<Vec<ModelValidation>, String>,
    pub active_challenges: Result<Vec<ActiveChallengeRuntime>, String>,
    pub fixtures: Result<FixtureOutcome, String>,
    pub api_key: Result<ApiKeyPlacement, String>,
    pub confirmed_pool_player_id: Result<String, String>,
}

/// Proof that all nine §13 checks passed.
///
/// No public constructor: only [`evaluate`] produces one. A write path that
/// takes this cannot be reached by code that skipped the gate, which is
/// what B1 means by "only after all nine".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReady {
    network: Network,
    upstream_commit: String,
}

impl WriteReady {
    pub fn network(&self) -> Network {
        self.network
    }

    /// The upstream commit the gate passed against, for the write-attempt
    /// ledger: a recorded write should say which pinned upstream it was made
    /// under.
    pub fn upstream_commit(&self) -> &str {
        &self.upstream_commit
    }
}

/// Run all nine checks.
///
/// Every check is evaluated even after one fails, and all failures are
/// returned. §13 makes a failure an operator task — "compare a newly pinned
/// upstream commit, update explicit models and fixtures, run the protocol
/// spike tests, then review the config change" — and doing that one
/// rediscovered failure at a time turns one round of work into nine.
pub fn evaluate(pins: &Pins, evidence: &Evidence) -> Result<WriteReady, Vec<Failure>> {
    let mut failures = Vec::new();
    let mut fail = |check: Check, detail: String| failures.push(Failure { check, detail });

    // 1. Config parses and the network is exactly testnet.
    match &evidence.config_network {
        Err(e) => fail(Check::ConfigNetwork, format!("config did not parse: {e}")),
        Ok(network) if *network != Network::Testnet => fail(
            Check::ConfigNetwork,
            format!("network is {network}, and §13 requires exactly testnet"),
        ),
        // The pins themselves must agree. A config that parsed as testnet
        // while the deployment was pinned to mainnet would otherwise pass a
        // check whose whole purpose is to keep slice 1 off mainnet.
        Ok(network) if *network != pins.network => fail(
            Check::ConfigNetwork,
            format!(
                "network {network} disagrees with the pinned {}",
                pins.network
            ),
        ),
        Ok(_) => {}
    }

    // 2. Upstream source commit matches the pin.
    match &evidence.upstream_commit {
        Err(e) => fail(
            Check::UpstreamCommit,
            format!("could not determine the acquired upstream commit: {e}"),
        ),
        Ok(commit) if *commit != pins.upstream_commit => fail(
            Check::UpstreamCommit,
            format!(
                "acquired {commit}, pinned {}; §13 never accepts a moved branch \
                 automatically",
                pins.upstream_commit
            ),
        ),
        Ok(_) => {}
    }

    // 3. Every required container resolves to its pinned digest, for the
    //    pinned platform.
    match &evidence.resolved_images {
        Err(e) => fail(
            Check::ContainerDigests,
            format!("could not resolve container digests: {e}"),
        ),
        Ok(resolved) => {
            let by_reference: BTreeMap<&str, &ResolvedImage> = resolved
                .iter()
                .map(|image| (image.reference.as_str(), image))
                .collect();
            // Driven by the pins, not by what was resolved: iterating the
            // observations would let an image that failed to resolve at all
            // vanish from the check instead of failing it.
            for (reference, pinned_digest) in &pins.image_digests {
                match by_reference.get(reference.as_str()) {
                    None => fail(
                        Check::ContainerDigests,
                        format!("{reference} did not resolve"),
                    ),
                    Some(image) if image.manifest_digest != *pinned_digest => fail(
                        Check::ContainerDigests,
                        format!(
                            "{reference} resolved to {}, pinned {pinned_digest}",
                            image.manifest_digest
                        ),
                    ),
                    Some(image) if image.platform != pins.platform => fail(
                        Check::ContainerDigests,
                        format!(
                            "{reference} resolved for platform {}, pinned {}",
                            image.platform, pins.platform
                        ),
                    ),
                    Some(_) => {}
                }
            }
        }
    }

    // 4. Hosted OpenAPI checksum matches, or a reviewed local override is
    //    active.
    match &evidence.openapi {
        Err(e) => fail(
            Check::OpenApiChecksum,
            format!("could not obtain the OpenAPI document: {e}"),
        ),
        Ok(observed) => match (&observed.reviewed_local_override, &observed.hosted_sha256) {
            // The override wins when present, because §13 offers it as an
            // alternative rather than a fallback — it has to be chosen, and
            // its own checksum still has to match the reviewed one.
            (Some(override_sha), _) if *override_sha == pins.openapi_sha256 => {}
            (Some(override_sha), _) => fail(
                Check::OpenApiChecksum,
                format!(
                    "the local schema override checksum {override_sha} is not the reviewed {}",
                    pins.openapi_sha256
                ),
            ),
            (None, Some(hosted)) if *hosted == pins.openapi_sha256 => {}
            (None, Some(hosted)) => fail(
                Check::OpenApiChecksum,
                format!(
                    "hosted OpenAPI checksum {hosted} is not the reviewed {}; §13 makes this \
                     an operator task, not an automatic re-pin",
                    pins.openapi_sha256
                ),
            ),
            (None, None) => fail(
                Check::OpenApiChecksum,
                "no hosted checksum and no reviewed local override".to_string(),
            ),
        },
    }

    // 5. The required responses validate against required models.
    match &evidence.response_models {
        Err(e) => fail(
            Check::ResponseModels,
            format!("could not validate the required responses: {e}"),
        ),
        Ok(validations) => {
            let by_endpoint: BTreeMap<&str, &ModelValidation> = validations
                .iter()
                .map(|v| (v.endpoint.as_str(), v))
                .collect();
            // Again driven by the required list: an endpoint nobody
            // validated must fail rather than be absent from the result.
            for endpoint in REQUIRED_MODELS {
                match by_endpoint.get(endpoint) {
                    None => fail(
                        Check::ResponseModels,
                        format!("{endpoint} was not validated"),
                    ),
                    Some(v) if !v.valid => fail(
                        Check::ResponseModels,
                        format!("{endpoint} did not validate: {}", v.detail),
                    ),
                    Some(_) => {}
                }
            }
        }
    }

    // 6. Active challenges have a pinned runtime and a supported compute
    //    path.
    match &evidence.active_challenges {
        Err(e) => fail(
            Check::ChallengeRuntimes,
            format!("could not determine the active challenges: {e}"),
        ),
        Ok(challenges) => {
            for challenge in challenges {
                if !challenge.runtime_pinned {
                    fail(
                        Check::ChallengeRuntimes,
                        format!("challenge {} has no pinned runtime", challenge.challenge_id),
                    );
                }
                if !challenge.compute_path_supported {
                    fail(
                        Check::ChallengeRuntimes,
                        format!(
                            "challenge {} has no supported compute path",
                            challenge.challenge_id
                        ),
                    );
                }
            }
        }
    }

    // 7. The fixture replays pass.
    match &evidence.fixtures {
        Err(e) => fail(
            Check::SerializationFixtures,
            format!("could not run the fixtures: {e}"),
        ),
        Ok(outcome) => {
            // Both, separately. A single combined flag would let one of the
            // two regress while the other kept the check green.
            if !outcome.lossless_numeric_parsing {
                fail(
                    Check::SerializationFixtures,
                    format!("lossless numeric parsing failed: {}", outcome.detail),
                );
            }
            if !outcome.canonical_request_serialization {
                fail(
                    Check::SerializationFixtures,
                    format!("canonical request serialization failed: {}", outcome.detail),
                );
            }
        }
    }

    // 8. The API key is in the gateway and nowhere a member service can
    //    read it.
    match &evidence.api_key {
        Err(e) => fail(
            Check::ApiKeyIsolation,
            format!("could not establish where the API key is: {e}"),
        ),
        Ok(placement) => {
            if !placement.present_in_gateway {
                fail(
                    Check::ApiKeyIsolation,
                    "the TIG API key is not present in the gateway".to_string(),
                );
            }
            if placement.readable_by_member_services {
                fail(
                    Check::ApiKeyIsolation,
                    "the TIG API key is readable by member services (architecture.md §2.2)"
                        .to_string(),
                );
            }
        }
    }

    // 9. Confirmed data agrees with configured identity.
    match &evidence.confirmed_pool_player_id {
        Err(e) => fail(
            Check::PlayerIdentity,
            format!("could not read the confirmed pool player ID: {e}"),
        ),
        Ok(observed) if *observed != pins.pool_player_id => fail(
            Check::PlayerIdentity,
            format!(
                "confirmed data reports player {observed}, configured identity is {}",
                pins.pool_player_id
            ),
        ),
        Ok(_) => {}
    }

    if failures.is_empty() {
        // Both fields are read out of evidence that has just been checked
        // against the pins, so they cannot disagree with what passed.
        let network = match &evidence.config_network {
            Ok(network) => *network,
            Err(_) => return Err(failures),
        };
        let upstream_commit = match &evidence.upstream_commit {
            Ok(commit) => commit.clone(),
            Err(_) => return Err(failures),
        };
        Ok(WriteReady {
            network,
            upstream_commit,
        })
    } else {
        failures.sort_by_key(|f| f.check);
        Err(failures)
    }
}
