//! Gathering the observations §13's gate judges.
//!
//! [`crate::readiness::evaluate`] decides; this produces what it decides on.
//! The split is the point: a check whose evidence is gathered by the same
//! code that grades it can only ever agree with itself, which is how three
//! separate reviews of this module's siblings found circular comparisons.
//!
//! Every function returns a `Result`, and an observation that could not be
//! made stays an `Err`. `evaluate` fails a check on one — it never skips it —
//! so an endpoint that was unreachable reads as "not verified" rather than
//! as "fine".
//!
//! The gateway reads TIG here even though `architecture.md` §4 gives it no
//! read responsibilities. §13 is why: checks 5, 6 and 9 cannot be answered
//! without reading, and `tig_client`'s `TigReader::Gateway` share exists for
//! exactly this. Choosing work stays the controller's.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use tig_client::TigReadClient;

use crate::readiness::{ActiveChallengeRuntime, ApiKeyPlacement, OpenApiObservation};

/// §13 check 9: the pool player ID returned by confirmed data matches the
/// configured identity.
///
/// **What is verified is that TIG holds a player for this id** (§13.3), not
/// that the reply repeated it. `get-player-data` takes `player_id` as a
/// parameter, so comparing the echoed id against the configured one would
/// compare a value with itself — the circularity §13.1 rejects for check 2
/// and a reviewer caught in check 1.
///
/// Existence is not circular. A typo, a mainnet identity against a testnet
/// endpoint, or an account that was retired all produce a reply with no
/// player, and the deployment cannot write until its identity is one TIG
/// actually serves.
///
/// **A missing player is HTTP 200, not 404** (§14.3, observed on live testnet
/// 2026-09-15). A check written as "did the request succeed" passes for an
/// identity that does not exist, which is the case this exists to catch.
///
/// §13.3 records what this establishes and what it does not — in particular
/// that it says nothing about whether the loaded API key belongs to the
/// player, which §10's byte-for-byte `settings.player_id` comparison catches
/// later and by different means.
pub async fn confirmed_pool_player_id(
    reader: &TigReadClient,
    block_id: &str,
    configured: &str,
) -> Result<String, String> {
    let body = reader
        .get_json(&format!(
            "/get-player-data?block_id={block_id}&player_id={configured}"
        ))
        .await
        .map_err(|e| format!("get-player-data failed: {e}"))?;

    match body.get("player") {
        None | Some(Value::Null) => Err(format!(
            "TIG holds no player {configured} on this endpoint; the identity or the \
             endpoint is wrong"
        )),
        Some(player) => player
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "the player TIG returned carries no id".to_string()),
    }
}

/// §13 check 4: the hosted OpenAPI checksum matches the reviewed one.
///
/// Fetched rather than assumed. TIG publishes the document at a stable URL and
/// the pin records its SHA-256; a change means the described API moved, which
/// §13 makes an operator's task and never an automatic re-pin.
///
/// A fetch that fails is an `Err` and fails the check. That is deliberate and
/// it is the expensive choice — a gateway restarting while the document is
/// unreachable cannot write — but the alternative is treating "we could not
/// look" as "nothing changed", which is the reading this whole gate exists to
/// refuse. `reviewed_local_override` is §13's own escape for the case where
/// the hosted document is not reachable by design.
pub async fn openapi_checksum(url: &str) -> Result<OpenApiObservation, String> {
    let body = reqwest::get(url)
        .await
        .map_err(|e| format!("fetching {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("fetching {url}: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("reading {url}: {e}"))?;

    use sha2::{Digest, Sha256};
    let hosted = format!("{:x}", Sha256::digest(&body));
    Ok(OpenApiObservation {
        hosted_sha256: Some(hosted),
        reviewed_local_override: None,
    })
}

/// §13 check 8: the API key is in the gateway and nowhere a member service
/// can read it.
///
/// `present_in_gateway` is whether a key was loaded at all. The build-time
/// half of `architecture.md` §2.2 — that no other crate can even reach the
/// loading path — is `scripts/credential-boundary.sh`, and it runs in CI where
/// it can inspect the whole tree. This is the half a running process can see.
///
/// `readable_by_member_services` is read off the file's mode. Anything beyond
/// owner-readable is the finding: a member service running as another user, or
/// in the same group, can read a group- or world-readable file whatever the
/// process boundaries say. Ownership is deliberately not checked — a
/// deployment may legitimately run the gateway as a user that is not the
/// file's owner, and the mode is what decides who can read it.
pub fn api_key_placement(present: bool, key_path: &Path) -> Result<ApiKeyPlacement, String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(key_path)
        .map_err(|e| format!("cannot stat the key file {}: {e}", key_path.display()))?
        .permissions()
        .mode();
    Ok(ApiKeyPlacement {
        present_in_gateway: present,
        // 0o077: any group or other permission bit at all.
        readable_by_member_services: mode & 0o077 != 0,
    })
}

/// §13 check 6: every live active challenge this deployment could mine has a
/// pinned runtime.
///
/// **What "considered by the decision engine" resolves to.** The engine is
/// handed a candidate list; what bounds that list is the compute this
/// deployment serves, because `select_challenge` excludes a challenge whose
/// compute type no offer matches before looking at anything else. So the
/// challenges that reach it are the live ones whose type is served here, and
/// those are the ones that need a runtime.
///
/// `served` is therefore a fact about the deployment, not about TIG. A slice-1
/// gateway serves none — there are no members and nothing is mined — so it has
/// no challenges to consider and this check has nothing to fail on. That is
/// not an acknowledgement in the sense of §13.2: it is true, and it stops
/// being true the moment compute is configured, at which point the check
/// starts biting without anyone having to remember to enable it.
///
/// It bites hard when it does. At the time of writing, live testnet carries
/// `zk_optimization` — a CPU challenge with no pinned runtime — so a
/// deployment serving CPU cannot write until the pin is brought forward
/// (`tig_integration.md` §15). That is the check working: the pool cannot mine
/// what it has not reviewed.
pub async fn active_challenge_runtimes(
    reader: &TigReadClient,
    block_id: &str,
    pinned_images: &BTreeSet<String>,
    served: &BTreeSet<String>,
) -> Result<Vec<ActiveChallengeRuntime>, String> {
    let body = reader
        .get_json(&format!("/get-challenges?block_id={block_id}"))
        .await
        .map_err(|e| format!("get-challenges failed: {e}"))?;

    let challenges = body
        .get("challenges")
        .and_then(Value::as_array)
        .ok_or_else(|| "get-challenges returned no challenges array".to_string())?;
    if challenges.is_empty() {
        // Not "nothing to check": TIG always has active challenges, so an
        // empty list means the read answered something other than the
        // question asked.
        return Err("get-challenges returned an empty list".to_string());
    }

    let mut out = Vec::new();
    for (index, challenge) in challenges.iter().enumerate() {
        let id = challenge
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("challenge at index {index} has no id"))?;
        let config = challenge
            .get("config")
            .ok_or_else(|| format!("challenge {id} has no config"))?;
        let name = config
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("challenge {id} has no config.name"))?;
        let compute = config
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("challenge {id} has no config.type"))?;

        if !served.contains(compute) {
            continue;
        }
        out.push(ActiveChallengeRuntime {
            challenge_id: id.to_string(),
            runtime_pinned: pinned_images.contains(&format!("{name}_runtime")),
            // Served by construction: anything here passed the filter above.
            // The field stays because `evaluate` owns the rule and a gatherer
            // that answered it by omission would put the rule in two places.
            compute_path_supported: true,
        });
    }
    Ok(out)
}
