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

use crate::readiness::{
    ActiveChallengeRuntime, ApiKeyPlacement, FixtureOutcome, ModelValidation, OpenApiObservation,
};

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
    // Timed out, because §13 fails closed and a document that *hangs* rather
    // than failing would hold the gate open indefinitely — a gateway that
    // never starts is not the same as one that refuses to write, and only the
    // second is a decision.
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("building the client: {e}"))?;
    let body = http
        .get(url)
        .send()
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
///
/// §13.5 records this reading, the §5.1 activity filter, and that the
/// compute-path half is satisfied by construction rather than checked.
pub async fn active_challenge_runtimes(
    reader: &TigReadClient,
    block_id: &str,
    block_round: u64,
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
        let config = challenge
            .get("config")
            .ok_or_else(|| format!("challenge at index {index} has no config"))?;
        let compute = config
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("challenge at index {index} has no config.type"))?;

        // Scope first, then demand fields. The first version required `id`
        // and `config.name` before filtering, so a malformed challenge for a
        // compute type this deployment does not serve failed the whole check
        // and blocked writes — contradicting this function's own claim that a
        // gateway serving nothing has nothing to fail on.
        if !served.contains(compute) {
            continue;
        }

        // §5.1: active is `state.round_active <= block.details.round`. A
        // challenge that activates in a future round is not one the decision
        // engine considers, so requiring a pinned runtime for it would fail
        // the gate over work the pool could not take even if it wanted to.
        let round_active = challenge
            .get("state")
            .and_then(|state| state.get("round_active"))
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("challenge at index {index} has no state.round_active"))?;
        if round_active > block_round {
            continue;
        }

        let id = challenge
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("challenge at index {index} has no id"))?;
        let name = config
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("challenge {id} has no config.name"))?;
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

/// §13 check 7: the lossless-numeric and canonical-serialization fixtures
/// pass.
///
/// **Run here, in the process, rather than left to CI.** §13 is a gate on a
/// running binary — "at startup and after any deployment" — and a property
/// proven in CI is a property of a *tree*. A binary built from a tree whose
/// tests never ran is the deployment the gate exists to stop, and it would
/// pass a check that trusted CI. Both fixtures are compiled in, so the
/// process carries what it verifies.
///
/// The two properties are reported separately because `evaluate` fails them
/// separately: one flag for both would let either regress while the check
/// stayed green.
pub fn serialization_fixtures() -> Result<FixtureOutcome, String> {
    const LOSSLESS: &str = include_str!("../../../fixtures/serialization/v1/lossless.json");
    const BODY: &str = include_str!("../../../fixtures/serialization/v1/precommit-body.json");

    let mut detail = String::new();

    // §4: integers must survive the JSON boundary unchanged. The fixture's
    // values sit above 2^53, so a parser routing them through `f64` returns a
    // neighbour rather than the value — which is the failure, and it is
    // silent.
    let numeric = (|| -> Result<bool, String> {
        let doc: Value =
            serde_json::from_str(LOSSLESS).map_err(|e| format!("lossless fixture: {e}"))?;
        let values = doc
            .get("values")
            .and_then(Value::as_object)
            .ok_or_else(|| "lossless fixture has no values object".to_string())?;
        for (field, value) in values {
            let parsed = value
                .as_u64()
                .ok_or_else(|| format!("{field} did not parse as an integer"))?;
            // Round-tripped through the same serializer a request body uses,
            // because that is the path a value actually takes.
            let round_tripped: Value = serde_json::from_str(
                &serde_json::to_string(&parsed).map_err(|e| format!("{field}: {e}"))?,
            )
            .map_err(|e| format!("{field}: {e}"))?;
            if round_tripped.as_u64() != Some(parsed) {
                return Err(format!("{field} did not survive a round trip"));
            }
        }
        Ok(true)
    })();

    // §6.1's body, byte for byte — for what the documents establish, not for
    // a claim about how TIG hashes a request. §10 identifies a precommit by
    // its semantic fields, not by a body digest.
    //
    // What makes the bytes load-bearing is `architecture.md` §7.3: an
    // admitted intent is bound to its canonical payload digest and the
    // gateway refuses bytes that do not reproduce it, so changing the
    // rendering invalidates intents already recorded.
    let canonical = (|| -> Result<bool, String> {
        let doc: Value = serde_json::from_str(BODY).map_err(|e| format!("body fixture: {e}"))?;
        let expected = doc
            .get("expected_bytes")
            .and_then(Value::as_str)
            .ok_or_else(|| "body fixture has no expected_bytes".to_string())?;
        let input = doc
            .get("input")
            .ok_or_else(|| "body fixture has no input".to_string())?;
        let submission = submission_from(input)?;
        let rendered = serde_json::to_string(&pool_workflow::payload::precommit_body(&submission))
            .map_err(|e| format!("rendering the body: {e}"))?;
        if rendered != expected {
            return Err(format!("rendered {rendered}, fixture expects {expected}"));
        }
        Ok(true)
    })();

    for outcome in [&numeric, &canonical] {
        if let Err(e) = outcome {
            if !detail.is_empty() {
                detail.push_str("; ");
            }
            detail.push_str(e);
        }
    }

    Ok(FixtureOutcome {
        lossless_numeric_parsing: numeric.unwrap_or(false),
        canonical_request_serialization: canonical.unwrap_or(false),
        detail,
    })
}

/// The fixture's `input`, read into a submission by hand.
///
/// `PrecommitSubmission` deliberately derives no `Deserialize`: it is a
/// domain type the pool constructs from decisions, not something parsed from
/// arbitrary JSON, and widening its API so one fixture could be loaded more
/// briefly would make every future caller's mistake compile.
fn submission_from(input: &Value) -> Result<pool_workflow::PrecommitSubmission, String> {
    use std::collections::BTreeMap;

    let text = |key: &str| -> Result<String, String> {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("the fixture input has no {key}"))
    };

    let mut track_settings = BTreeMap::new();
    let tracks = input
        .get("track_settings")
        .and_then(Value::as_object)
        .ok_or_else(|| "the fixture input has no track_settings".to_string())?;
    for (track_id, settings) in tracks {
        let number = |key: &str| -> Result<u64, String> {
            settings
                .get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("track {track_id} has no {key}"))
        };
        let hyperparameters = settings
            .get("hyperparameters")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("track {track_id} has no hyperparameters"))?
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        track_settings.insert(
            track_id.clone(),
            pool_workflow::TrackSettings {
                num_bundles: number("num_bundles")?,
                fuel_budget: number("fuel_budget")?,
                hyperparameters,
            },
        );
    }

    Ok(pool_workflow::PrecommitSubmission {
        player_id: text("player_id")?,
        block_id: text("block_id")?,
        challenge_id: text("challenge_id")?,
        algorithm_id: text("algorithm_id")?,
        compute_type: text("compute_type")?,
        track_settings,
    })
}

/// The collections each §13 check-5 read must carry, as TIG serves them.
///
/// **Taken from what TIG serves, not from the fixtures.** `get-algorithms` is
/// why: `fixtures/tig/v1` gives it a top-level `algorithms` key, and the
/// envelope §14 records — verified live in spike S1 and reconfirmed
/// 2026-09-15 — carries `codes`, `binarys` and `advances` with no
/// `algorithms` at all (issue #28 owns the v2 fixture). Validating against
/// the fixture would make the fake the authority on the real API's shape,
/// which is the inversion check 5 exists to prevent — code that passes every
/// test and fails the moment it meets TIG.
const REQUIRED_COLLECTIONS: &[(&str, &[&str])] = &[
    ("get-block", &["block"]),
    ("get-challenges", &["challenges"]),
    ("get-algorithms", &["codes", "binarys", "advances"]),
    ("get-opow", &["opow"]),
    (
        "get-benchmarks",
        &["precommits", "benchmarks", "proofs", "frauds"],
    ),
];

/// §13 check 5: the required responses validate against required models.
///
/// §13.4 records what this validates and what it does not: collection
/// presence, not field-level model validation. In short — it catches TIG
/// renaming or removing a collection, and misses a field inside one changing
/// meaning, which no shape check catches at any depth.
///
/// A response that does not arrive is an error, not an absent validation —
/// `evaluate` fails an endpoint nobody validated, so a read that timed out
/// cannot pass as a read that succeeded.
pub async fn response_models(
    reader: &TigReadClient,
    block_id: &str,
    player_id: &str,
) -> Result<Vec<ModelValidation>, String> {
    let mut out = Vec::new();
    for (endpoint, required) in REQUIRED_COLLECTIONS {
        let query = match *endpoint {
            "get-block" => "/get-block?include_data=true".to_string(),
            "get-benchmarks" => {
                format!("/get-benchmarks?block_id={block_id}&player_id={player_id}")
            }
            other => format!("/{other}?block_id={block_id}"),
        };
        let (valid, detail) = match reader.get_json(&query).await {
            Err(e) => (false, format!("read failed: {e}")),
            Ok(body) => {
                let missing: Vec<&str> = required
                    .iter()
                    .filter(|key| body.get(**key).is_none())
                    .copied()
                    .collect();
                if missing.is_empty() {
                    (true, String::new())
                } else {
                    (false, format!("missing {}", missing.join(", ")))
                }
            }
        };
        out.push(ModelValidation {
            endpoint: (*endpoint).to_string(),
            valid,
            detail,
        });
    }
    Ok(out)
}
