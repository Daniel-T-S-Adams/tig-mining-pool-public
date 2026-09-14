//! Precommit reconciliation (`docs/tig_integration.md` §10, criterion E4).
//!
//! In `pool-workflow` rather than `tig-gateway`, for the reason `payload` is:
//! both sides need it. The gateway reconciles before it retries (§7.3), and
//! the controller reconciles to *bind* — a DECIDED workflow whose precommit
//! was sent has no `benchmark_id` until this search finds its confirmed
//! entry, and §6 makes advancing the workflow the controller's. The gateway
//! is the credential boundary and a leaf; the controller cannot depend on it.
//!
//! §10: "A lost precommit HTTP response is harder because the client may not
//! know the generated ID. The gateway permits only one unresolved precommit
//! request per serialized submission lane, searches newly confirmed
//! precommits for the exact player, decision block, challenge, algorithm,
//! compute type and selected-track settings, and stops for operator
//! resolution if more than one candidate matches. It never blindly
//! resubmits an ambiguous precommit."
//!
//! Everything here is a pure function over records already read. That is
//! deliberate: the decision this makes — whether a write reached TIG — is
//! the one that licenses or forbids a resend, and it should be testable
//! against constructed candidates rather than only against a live chain that
//! happens to be in the right state.
//!
//! The outcome deliberately distinguishes "nothing matches" from "something
//! matches but has not confirmed". Collapsing them would make an
//! unconfirmed precommit of our own look like a write that never arrived,
//! which is precisely the blind resubmission §10 forbids.

use std::collections::BTreeMap;

pub use crate::payload::{PrecommitSubmission, TrackSettings};

/// Whether two hyperparameter sets name the same values.
///
/// Both sides render to text before comparing, so `250` and `"250"` are the
/// same hyperparameter: TIG returns these as strings and the pool selects
/// them as values, and a type difference is not a different method. The
/// rendering exists only here; nothing transmits it.
fn hyperparameters_match(
    a: &BTreeMap<String, serde_json::Value>,
    b: &BTreeMap<String, serde_json::Value>,
) -> bool {
    fn render(map: &BTreeMap<String, serde_json::Value>) -> BTreeMap<&String, String> {
        map.iter()
            .map(|(k, v)| {
                let text = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k, text)
            })
            .collect()
    }
    render(a) == render(b)
}

/// What a search of confirmed precommits established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciliation {
    /// Nothing matches the tuple.
    ///
    /// Evidence that this write is not present in the window searched — not,
    /// on its own, a licence to resend: §10 step 3 fetches the latest
    /// 120-block window, and a precommit older than that window is absent
    /// from it for a reason that has nothing to do with whether it was
    /// accepted.
    NoCandidate,
    /// Exactly one candidate matches but has not confirmed yet.
    ///
    /// Never a licence to resend. Treating this as "nothing arrived" is the
    /// blind resubmission §10 forbids, and it is how a duplicate benchmark
    /// gets created from a write that was in fact fine.
    PendingConfirmation,
    /// Exactly one confirmed candidate. This is the pool's precommit, and
    /// the benchmark id the lost response would have carried.
    Confirmed { benchmark_id: String },
    /// More than one candidate matches. §10: stop for operator resolution.
    ///
    /// Deliberately not "pick the confirmed one" or "pick the newest": if
    /// two precommits match the exact tuple the pool chose, the pool cannot
    /// tell which is its own, and guessing attributes a benchmark — and its
    /// fees and rewards — to the wrong one.
    StopForOperator { candidates: Vec<String> },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReconcileError {
    /// A precommit record could not be read.
    ///
    /// Fatal rather than skipped. A record that cannot be parsed might be
    /// the pool's own, so skipping it could turn a match into `NoCandidate`
    /// and license the resend §10 forbids.
    #[error("precommit at index {index} is unusable: {reason}")]
    Shape { index: usize, reason: String },
}

/// One precommit as read from `get-benchmarks`: its id, the track TIG
/// selected, the settings it confirmed, and whether it has confirmed.
struct Candidate {
    benchmark_id: String,
    player_id: String,
    block_id: String,
    challenge_id: String,
    algorithm_id: String,
    compute_type: String,
    /// TIG's selection, not the pool's.
    track_id: String,
    settings: TrackSettings,
    confirmed: bool,
}

fn candidate_of(index: usize, record: &serde_json::Value) -> Result<Candidate, ReconcileError> {
    let shape = |reason: String| ReconcileError::Shape { index, reason };

    let field =
        |parent: &serde_json::Value, path: &str, key: &str| -> Result<String, ReconcileError> {
            parent
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| shape(format!("missing {path}.{key}")))
        };
    let number =
        |parent: &serde_json::Value, path: &str, key: &str| -> Result<u64, ReconcileError> {
            parent
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| shape(format!("missing or non-numeric {path}.{key}")))
        };

    let benchmark_id = field(record, "precommit", "benchmark_id")?;
    let settings = record
        .get("settings")
        .ok_or_else(|| shape("missing settings".to_string()))?;
    let details = record
        .get("details")
        .ok_or_else(|| shape("missing details".to_string()))?;

    // Kept as values; `hyperparameters_match` does the normalising, so
    // nothing downstream can mistake the comparison form for the wire one.
    let hyperparameters: BTreeMap<String, serde_json::Value> = match details.get("hyperparameters")
    {
        None | Some(serde_json::Value::Null) => BTreeMap::new(),
        Some(serde_json::Value::Object(map)) => {
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        Some(_) => {
            return Err(shape(
                "details.hyperparameters is not an object".to_string(),
            ));
        }
    };

    let confirmed = block_confirmed(record);

    Ok(Candidate {
        benchmark_id,
        player_id: field(settings, "settings", "player_id")?,
        block_id: field(settings, "settings", "block_id")?,
        challenge_id: field(settings, "settings", "challenge_id")?,
        algorithm_id: field(settings, "settings", "algorithm_id")?,
        compute_type: field(details, "details", "compute_type")?,
        track_id: field(settings, "settings", "track_id")?,
        settings: TrackSettings {
            num_bundles: number(details, "details", "num_bundles")?,
            fuel_budget: number(details, "details", "fuel_budget")?,
            hyperparameters,
        },
        confirmed,
    })
}

impl Candidate {
    /// Whether this confirmed record is the precommit `submitted` produced.
    ///
    /// The track comes from TIG, so the test is that its selection is one of
    /// the tracks the pool offered AND that what it confirmed for that track
    /// is what the pool submitted for it.
    fn matches(&self, submitted: &PrecommitSubmission) -> bool {
        self.player_id == submitted.player_id
            && self.block_id == submitted.block_id
            && self.challenge_id == submitted.challenge_id
            && self.algorithm_id == submitted.algorithm_id
            && self.compute_type == submitted.compute_type
            && submitted
                .track_settings
                .get(&self.track_id)
                .is_some_and(|offered| {
                    offered.num_bundles == self.settings.num_bundles
                        && offered.fuel_budget == self.settings.fuel_budget
                        && hyperparameters_match(
                            &offered.hyperparameters,
                            &self.settings.hyperparameters,
                        )
                })
    }
}

/// Search precommits for the one the pool submitted.
///
/// `precommits` is the `precommits` array of a `get-benchmarks` response —
/// §5's latest 120-block window, and §7's authority for confirmation.
/// §7's confirmation test, in one place.
///
/// `tig_integration.md` §7 maps a TIG record to confirmed by one rule: a
/// matching entry in the read **with a non-null `state.block_confirmed`**.
/// Presence in the collection is not the test — an entry can be there and
/// unconfirmed — and every caller that reduces a read to "these are
/// confirmed" is applying this and nothing else.
///
/// Stated here rather than at each reader because two readers that disagree
/// about what confirmation means is how a workflow gets advanced on evidence
/// TIG has not given.
pub fn block_confirmed(record: &serde_json::Value) -> bool {
    record
        .get("state")
        .and_then(|s| s.get("block_confirmed"))
        .is_some_and(|v| !v.is_null())
}

pub fn reconcile_precommit(
    precommits: &[serde_json::Value],
    submitted: &PrecommitSubmission,
) -> Result<Reconciliation, ReconcileError> {
    let mut matches: Vec<(String, bool)> = Vec::new();
    for (index, record) in precommits.iter().enumerate() {
        let candidate = candidate_of(index, record)?;
        if candidate.matches(submitted) {
            matches.push((candidate.benchmark_id, candidate.confirmed));
        }
    }

    // Counted across confirmed AND unconfirmed, and across tracks. Two
    // records matching what the pool submitted means the pool cannot tell
    // which is its own — including the case where TIG selected a different
    // track for each, which is precisely what a per-track search would have
    // hidden.
    match matches.len() {
        0 => Ok(Reconciliation::NoCandidate),
        1 => {
            let (benchmark_id, confirmed) = &matches[0];
            if *confirmed {
                Ok(Reconciliation::Confirmed {
                    benchmark_id: benchmark_id.clone(),
                })
            } else {
                Ok(Reconciliation::PendingConfirmation)
            }
        }
        _ => {
            let mut candidates: Vec<String> = matches.into_iter().map(|(id, _)| id).collect();
            candidates.sort();
            Ok(Reconciliation::StopForOperator { candidates })
        }
    }
}
