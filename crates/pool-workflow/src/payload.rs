//! The §6.1 precommit payload: one definition, both sides render it.
//!
//! The controller computes an intent's `payload_digest` at admission and the
//! gateway must transmit the exact bytes that digest was taken over — `claim`
//! refuses anything else, on the reconcile path as well as the send. So the
//! two components need one rendering, and it cannot live in `tig-gateway`:
//! that crate is the credential boundary (`architecture.md` §2.2) and is
//! meant to be a leaf, not something the controller depends on.
//!
//! What lives here is the vocabulary and the rendering. Reconciliation and
//! transmission stay in the gateway.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// What the pool actually submitted (`tig_integration.md` §6.1).
///
/// The pool does NOT choose the track. §6.1: "The submitted `track_id` is
/// empty because TIG selects it; the confirmed precommit's
/// `settings.track_id`, `details.rand_hash`, counts, fuel, hyperparameters
/// and fee are authoritative." The pool submits `track_settings` for every
/// live active track of the chosen challenge, and TIG picks one during
/// precommit processing.
///
/// So §10's "selected-track settings" is a comparison against the settings
/// the pool submitted *for whichever track TIG selected* — not against a
/// track the pool picked, because it picked none.
///
/// Getting this wrong is not a near miss. Matching a single guessed track
/// makes a confirmed precommit on any other track report `NoCandidate` — an
/// accepted write reported as absent, which is the precondition for the
/// blind resubmission §10 forbids and a second fee paid. Probing per track
/// to work around that would defeat the multi-candidate rule too: two
/// duplicates landing on different tracks would each match a separate call
/// and never surface as `StopForOperator`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrecommitSubmission {
    pub player_id: String,
    /// The decision block the precommit was anchored to.
    pub block_id: String,
    pub challenge_id: String,
    pub algorithm_id: String,
    pub compute_type: String,
    /// Every live active track of the chosen challenge, as submitted.
    pub track_settings: BTreeMap<String, TrackSettings>,
}

/// The per-track settings submitted for one track.
///
/// `num_nonces` is deliberately absent: §14 pins it as TIG-derived
/// (`num_nonces = num_bundles * num_nonces_per_bundle`) and authoritative
/// only on the confirmed precommit. Matching on it would be matching on a
/// value the pool did not pick — and a caller deriving it from a different
/// block's `num_nonces_per_bundle` would turn a real match into
/// `NoCandidate`, the fail-open direction that licenses a resend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSettings {
    pub num_bundles: u64,
    pub fuel_budget: u64,
    /// Selected hyperparameters, as values.
    ///
    /// `mining_system.md` §6.6 copies the source benchmark's hyperparameters
    /// and `tig_integration.md` §4 requires lossless numeric handling on the
    /// §6.1 write body, so a numeric hyperparameter has to reach TIG as a
    /// number. Comparison normalises instead — see `hyperparameters_match`.
    /// Storing the normalised form here would transmit a re-typed method.
    pub hyperparameters: BTreeMap<String, serde_json::Value>,
}

/// Build §6.1's request body from what the pool decided.
///
/// The submitted `track_id` is empty because TIG selects it (§6.1), and
/// `track_settings` carries every live active track of the chosen challenge.
/// That is the same asymmetry `reconcile` matches on afterwards, which is why
/// both take the one type.
pub fn precommit_body(submission: &PrecommitSubmission) -> Value {
    let mut track_settings = serde_json::Map::new();
    for (track_id, settings) in &submission.track_settings {
        // The values the pool chose, at their own types. §4 requires lossless
        // numeric handling on this body and §6.6 copies the source
        // benchmark's hyperparameters, so a number must leave as a number.
        let hyperparameters: serde_json::Map<String, Value> = settings
            .hyperparameters
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        track_settings.insert(
            track_id.clone(),
            json!({
                "hyperparameters": Value::Object(hyperparameters),
                "fuel_budget": settings.fuel_budget,
                "num_bundles": settings.num_bundles,
            }),
        );
    }

    json!({
        "settings": {
            "player_id": submission.player_id,
            "block_id": submission.block_id,
            "challenge_id": submission.challenge_id,
            "algorithm_id": submission.algorithm_id,
            "track_id": "",
        },
        "track_settings": Value::Object(track_settings),
        "compute_type": submission.compute_type,
    })
}

/// The §7.3 `payload_digest` of a precommit submission.
///
/// Taken over the exact bytes [`PrecommitTransmitter::send`] puts on the
/// socket, which is the point: the intent is recorded with this digest and
/// the send refuses anything that does not reproduce it, so the recorded
/// payload and the transmitted one cannot diverge. `serde_json` orders object
/// keys, so a given submission always renders the same bytes.
pub fn precommit_digest(submission: &PrecommitSubmission) -> [u8; 32] {
    // `Display` for `Value` is serde_json's compact form — the same bytes
    // `to_vec` produces — and needs no error path, so the digest cannot fail
    // and the send has no branch that skips the check.
    Sha256::digest(precommit_body(submission).to_string()).into()
}

/// What a recorded decision contributes to the §6.1 body.
///
/// Everything in the body except `player_id`, which is the pool's identity on
/// the endpoint and lives in configuration rather than on the decision — the
/// same pool has a different id on a different network, and a decision does
/// not decide who the pool is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionPayloadInputs {
    pub anchor_block_id: String,
    pub selected_challenge: String,
    pub selected_algorithm: String,
    pub compute_type: String,
    /// The decision record's `track_settings` column, verbatim.
    pub track_settings: Value,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PayloadError {
    /// The recorded `track_settings` do not have the shape §6.1 needs.
    ///
    /// Fatal rather than defaulted. A track with a defaulted bundle count
    /// would render a body that digests to something the intent never
    /// recorded — which `claim` then refuses — and a body that silently
    /// dropped a track would submit fewer tracks than the decision chose.
    #[error("track_settings[{track}].{field} is missing or not a {expected}")]
    Shape {
        track: String,
        field: &'static str,
        expected: &'static str,
    },
    #[error("track_settings is not an object")]
    NotAnObject,
}

impl PrecommitSubmission {
    /// Rebuild the submission a decision describes.
    ///
    /// **The** reconstruction: the controller uses it to digest the payload at
    /// admission and the gateway uses it to produce the bytes it sends and the
    /// tuple it reconciles by. One function, so the two cannot render
    /// differently — and so a decision record that cannot be rebuilt fails
    /// here, loudly, rather than at a send that then refuses its own bytes.
    pub fn from_decision(
        player_id: &str,
        inputs: &DecisionPayloadInputs,
    ) -> Result<Self, PayloadError> {
        let tracks = inputs
            .track_settings
            .as_object()
            .ok_or(PayloadError::NotAnObject)?;

        let mut track_settings = BTreeMap::new();
        for (track_id, settings) in tracks {
            let number = |field: &'static str| -> Result<u64, PayloadError> {
                settings
                    .get(field)
                    .and_then(Value::as_u64)
                    .ok_or_else(|| PayloadError::Shape {
                        track: track_id.clone(),
                        field,
                        expected: "non-negative integer",
                    })
            };
            // Hyperparameters keep the types the pool chose them at
            // (`mining_system.md` §6.6, `tig_integration.md` §4): copied as
            // values, never re-typed.
            let hyperparameters = match settings.get("hyperparameters") {
                None | Some(Value::Null) => BTreeMap::new(),
                Some(Value::Object(map)) => {
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                }
                Some(_) => {
                    return Err(PayloadError::Shape {
                        track: track_id.clone(),
                        field: "hyperparameters",
                        expected: "object or null",
                    });
                }
            };
            track_settings.insert(
                track_id.clone(),
                TrackSettings {
                    num_bundles: number("num_bundles")?,
                    fuel_budget: number("fuel_budget")?,
                    hyperparameters,
                },
            );
        }

        Ok(PrecommitSubmission {
            player_id: player_id.to_string(),
            block_id: inputs.anchor_block_id.clone(),
            challenge_id: inputs.selected_challenge.clone(),
            algorithm_id: inputs.selected_algorithm.clone(),
            compute_type: inputs.compute_type.clone(),
            track_settings,
        })
    }
}
