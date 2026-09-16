//! What the pool would precommit, from one block-consistent snapshot.
//!
//! `mining_system.md` §6 is written as stages and `pool-decision` implements
//! each as a pure function. This module is the part §6 leaves out: turning a
//! persisted snapshot and the active-benchmark cache into those functions'
//! inputs, running them in order, and producing a proposal the admission
//! transaction can record.
//!
//! Pure, and deliberately. It performs no I/O, reads no clock and derives no
//! randomness — the §6.3 draw arrives as ranks derived from the anchor block
//! (ADR-0005), for the reason `pool-decision`'s own doc gives: a draw the
//! engine could generate is a draw the pool could re-roll. Everything it needs
//! is an argument, so a case that is hard to reach live can be handed to it.
//!
//! **The stage order has one knot.** §6.2 makes a challenge eligible only when
//! the pool can construct settings for every active track, §6.6 defines those
//! settings *for the selected algorithm*, and §6.4 selects the algorithm
//! *within the selected challenge*. Read literally that is circular. It
//! resolves by computing, for each candidate challenge, the algorithm §6.4
//! would choose there and the §6.6 sources under it — eligibility is then a
//! fact about the challenge, and §6.3 chooses among challenges that are
//! genuinely constructible. The winner's algorithm and sources are the ones
//! already computed, so no stage runs twice on different evidence.

use std::collections::{BTreeMap, BTreeSet};

use pool_decision::algorithm::{Algorithm, AlgorithmSelection, AlgorithmSelectionInput};
use pool_decision::bundles::{BundleSizing, BundleSizingInput, TrackSizing};
use pool_decision::challenge::{
    Challenge, ChallengeSelection, ChallengeSelectionInput, ComputeType, InFlightBenchmark,
    OfferedCompute, TrackStats,
};
use pool_decision::source::{SourceBenchmark, SourceSelection, SourceSelectionInput};
use pool_domain::{DrawRank, Network, challenge_tie_seed, draw_ranks};
use pool_snapshot::Snapshot;
use pool_snapshot::active_cache::ActiveBenchmarkMeta;
use pool_workflow::restart::ConfirmedWindow;
use serde_json::{Value, json};

/// What the pool is deciding *for*.
///
/// Two compute vocabularies, deliberately both present. `compute` is
/// `mining_system.md` §6.2's CPU/GPU class, which selects challenges and
/// drives §6.7's alignment rule. `tig_compute_type` is `tig_integration.md`
/// §3's protocol type — `aws_t4g`, `aws_c7i` — which is what the §6.1 body
/// carries and what the decision record stores. §3 requires it be detected per
/// worker and is explicit that a mismatch is "ineligible rather than coerced",
/// so it is never derived from the class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub compute: OfferedCompute,
    pub tig_compute_type: String,
}

/// Why a configured offer is not one the pool may decide for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfferError {
    #[error("compute_class {0:?} is not \"cpu\" or \"gpu\" (mining_system.md §6.2)")]
    UnknownClass(String),
    #[error("a cpu offer needs cpu_cores (mining_system.md §6.7)")]
    NoCores,
    #[error(
        "compute type {compute_type:?} is not a {compute_class} type in the pinned \
         compatibility table (tig_integration.md §3); that class has {known:?}"
    )]
    UnknownComputeType {
        compute_type: String,
        compute_class: String,
        known: Vec<String>,
    },
    #[error("the pinned configuration has no compute_compatibility.compute_types_by_vendor")]
    PinUnreadable,
}

impl Offer {
    /// Build an offer from configuration, against the pinned compatibility
    /// table.
    ///
    /// `tig_integration.md` §3 requires the compute type be detected per
    /// worker and is explicit that a mismatch is "ineligible rather than
    /// coerced", so a type the pin does not know is refused here rather than
    /// sent and rejected — the fee is paid on submission. `pool-config`
    /// checks the shape; this checks it against the vocabulary, because the
    /// pin lives with the binary and not with the configuration.
    pub fn from_config(
        compute_class: &str,
        cpu_cores: Option<u64>,
        tig_compute_type: &str,
        pinned: &Value,
    ) -> Result<Self, OfferError> {
        let compute = match compute_class {
            "cpu" => OfferedCompute::Cpu {
                cores: cpu_cores.ok_or(OfferError::NoCores)?,
            },
            "gpu" => OfferedCompute::Gpu,
            other => return Err(OfferError::UnknownClass(other.to_string())),
        };

        let by_vendor = pinned
            .pointer("/compute_compatibility/compute_types_by_vendor")
            .and_then(Value::as_object)
            .ok_or(OfferError::PinUnreadable)?;
        let class_by_vendor = pinned
            .pointer("/compute_compatibility/compute_class_by_vendor")
            .and_then(Value::as_object)
            .ok_or(OfferError::PinUnreadable)?;

        // Only the vendors of the offered class. Flattening every vendor list
        // into one set accepted `compute_class = "cpu"` with
        // `tig_compute_type = "aws_g4dn"`: `propose` would then select CPU
        // challenges and the §6.1 body would carry the GPU protocol type. §3
        // is explicit that compatibility is detected and "ineligible rather
        // than coerced", and a label that crosses classes is exactly the
        // coercion it forbids.
        let known: Vec<String> = by_vendor
            .iter()
            .filter(|(vendor, _)| {
                class_by_vendor
                    .get(*vendor)
                    .and_then(Value::as_str)
                    .is_some_and(|class| class == compute_class)
            })
            .filter_map(|(_, types)| types.as_array())
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if known.is_empty() {
            // A class the pin names no vendor for. Refused rather than
            // accepted, for the same reason an unreadable table is.
            return Err(OfferError::PinUnreadable);
        }
        if !known.iter().any(|t| t == tig_compute_type) {
            return Err(OfferError::UnknownComputeType {
                compute_type: tig_compute_type.to_string(),
                compute_class: compute_class.to_string(),
                known,
            });
        }

        Ok(Self {
            compute,
            tig_compute_type: tig_compute_type.to_string(),
        })
    }
}

/// One challenge's configuration, as §6.7 and §6.1's body need it.
///
/// Kept beside the decision engine's reduced [`Challenge`] rather than folded
/// into it: the engine reads only what §6.2 and §6.3 compare, and widening its
/// input with fields no rule mentions would invite one to be compared.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChallengeConfig {
    compute_type: ComputeType,
    min_num_bundles: u64,
    /// `num_nonces_per_bundle` per active track.
    nonces_per_bundle: BTreeMap<String, u64>,
    /// §6.8's two fee inputs, as TIG publishes them: decimal atom strings.
    base_fee: u128,
    per_nonce_fee: u128,
}

/// A proposal, ready for the admission transaction to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub selected_challenge: String,
    pub selected_algorithm: String,
    /// TIG's protocol compute type (`tig_integration.md` §3's `aws_*` set),
    /// which is what the §6.1 body carries and what the decision record
    /// stores. A property of the offer, not of the challenge.
    pub compute_type: String,
    /// The challenge's CPU/GPU class. Recorded beside the type because §6.2
    /// matched on it, and because the two are easy to confuse — `spike` in
    /// `config/tig_integration.json` carries both, for the same reason.
    pub compute_class: String,
    /// The decision record's `track_settings`, in the shape
    /// `PrecommitSubmission::from_decision` reads back.
    pub track_settings: Value,
    /// §6.3's audit evidence: the rank of every compute-compatible eligible
    /// challenge, not only the tied ones (criterion D2d).
    pub draw_ranks: BTreeMap<String, DrawRank>,
    /// The seed the ranks were derived from, for the recorder. Derived once
    /// here so the decision record and the ranks cannot come from two
    /// derivations (criterion D2d).
    pub tie_seed: [u8; 32],
    pub tie_candidates: Option<Vec<String>>,
    pub tie_winner: Option<String>,
    /// Kept whole so the caller can record why, not only what.
    pub challenge_selection: ChallengeSelection,
    pub algorithm_selection: AlgorithmSelection,
    pub bundle_sizing: BundleSizing,
    /// §6.8's fee for each proposed track, in atoms.
    ///
    /// `base_fee + per_nonce_fee * num_bundles` — with **bundles**, despite
    /// the name. Settled during the protocol spike against the pinned upstream
    /// commit; `mining_system.md` §6.8 records that the per-nonce derivation
    /// in `fixtures/tig/v1/expected.json` is the refuted side.
    ///
    /// Per track because TIG selects the track during precommit processing and
    /// the counts differ (§6.7), so the pool must hold balance for the largest
    /// of them rather than for an average.
    pub fee_by_track: BTreeMap<String, u128>,
}

/// Why no proposal was made.
///
/// §6.2's "no compatible eligible challenge" is **not** here: it is
/// [`Proposed::NoAction`], an ordinary outcome of a correct pass. An error
/// means the snapshot could not be read as the protocol describes, which is a
/// different thing and must not be logged as "nothing to do".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProposeError {
    #[error("{endpoint} is missing from the snapshot")]
    MissingRead { endpoint: &'static str },
    #[error("{endpoint}{path} is not {expected}")]
    Shape {
        endpoint: &'static str,
        path: String,
        expected: &'static str,
    },
    #[error("challenge {challenge_id} was selected but is not among the candidates")]
    SelectedChallengeUnknown { challenge_id: String },
    #[error("challenge {challenge_id} was selected but no algorithm was chosen for it")]
    SelectedAlgorithmUnknown { challenge_id: String },
    #[error("{0}")]
    Challenge(#[from] pool_decision::challenge::ChallengeError),
    #[error("{0}")]
    Ratio(#[from] pool_decision::ratio::RatioError),
    #[error("{0}")]
    Bundles(#[from] pool_decision::bundles::BundleSizingError),
    #[error("the §6.8 fee for {challenge_id} track {track_id} overflows an atom count")]
    FeeOverflow {
        challenge_id: String,
        track_id: String,
    },
}

/// The outcome of one deciding pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proposed {
    /// §6.2's no-action outcome: nothing compute-compatible and eligible.
    ///
    /// Carries the selection so a pass that chose nothing can still say which
    /// challenges it considered and why each was excluded — otherwise "no
    /// action" is indistinguishable from "did not look".
    NoAction(Box<ChallengeSelection>),
    Precommit(Box<Proposal>),
}

fn read<'a>(snapshot: &'a Snapshot, endpoint: &'static str) -> Result<&'a Value, ProposeError> {
    snapshot
        .reads
        .get(endpoint)
        .ok_or(ProposeError::MissingRead { endpoint })
}

fn shape(endpoint: &'static str, path: impl Into<String>, expected: &'static str) -> ProposeError {
    ProposeError::Shape {
        endpoint,
        path: path.into(),
        expected,
    }
}

/// TIG publishes counts as JSON numbers and amounts as decimal strings.
///
/// Both are read, and neither is coerced from the other: a count arriving as a
/// string means the shape changed, and §13's gate is what should catch that,
/// not a parser that quietly accepts either.
fn as_u128(v: &Value) -> Option<u128> {
    v.as_u64().map(u128::from)
}

/// `state.round_active <= block.round` — `tig_integration.md` §5.1.
///
/// A challenge that activates in a later round is not one the engine
/// considers. Absent `round_active` is *not* active: an absent value read as
/// active would put a challenge the pool cannot mine into the candidate set,
/// and §6.3 would then be free to select it.
fn is_live_and_active(state: &Value, block_round: u64) -> bool {
    state
        .get("round_active")
        .and_then(Value::as_u64)
        .is_some_and(|round_active| round_active <= block_round)
}

/// The round the snapshot's block is in.
fn block_round(snapshot: &Snapshot) -> Result<u64, ProposeError> {
    let block = snapshot.block.get("block").unwrap_or(&snapshot.block);
    block
        .pointer("/details/round")
        .and_then(Value::as_u64)
        .ok_or_else(|| shape("get-block", "/details/round", "an unsigned integer"))
}

/// Compute-compatible candidates, with the configuration §6.7 and §6.1 need.
///
/// `tracks_with_source` is left empty here and filled by the caller once the
/// §6.6 sources are known — see this module's doc on the stage knot.
fn candidate_challenges(
    snapshot: &Snapshot,
    block_round: u64,
) -> Result<(Vec<Challenge>, BTreeMap<String, ChallengeConfig>), ProposeError> {
    const E: &str = "get-challenges";
    let body = read(snapshot, E)?;
    let challenges = body
        .get("challenges")
        .and_then(Value::as_array)
        .ok_or_else(|| shape(E, "/challenges", "a list"))?;

    let mut candidates = Vec::new();
    let mut configs = BTreeMap::new();

    for challenge in challenges {
        let id = challenge
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(E, "/challenges[].id", "a string"))?;
        let state = challenge
            .get("state")
            .ok_or_else(|| shape(E, format!("/challenges[{id}].state"), "an object"))?;
        if !is_live_and_active(state, block_round) {
            continue;
        }

        let config = challenge
            .get("config")
            .ok_or_else(|| shape(E, format!("/challenges[{id}].config"), "an object"))?;
        let compute_type = match config.get("type").and_then(Value::as_str) {
            Some("cpu") => ComputeType::Cpu,
            Some("gpu") => ComputeType::Gpu,
            // Not an error. A compute type this build does not know is a
            // challenge it cannot serve, and §13 check 6 is where an unknown
            // vocabulary blocks writes — dropping it here would be silent, so
            // it is dropped only from *candidates*, with the gate unchanged.
            _ => continue,
        };

        let active_tracks = config
            .get("active_tracks")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                shape(
                    E,
                    format!("/challenges[{id}].config.active_tracks"),
                    "an object",
                )
            })?;

        // §6.2's eligibility is "settings for **every** active track", and
        // `all()` over an empty set is true — so a challenge with no active
        // tracks would be vacuously eligible, and §6.3 would score it
        // `0/0 = 0`, the lowest factor there is. It would then win every
        // decision, and the precommit would carry no track settings at all.
        // A challenge with nothing to mine is not a candidate.
        if active_tracks.is_empty() {
            continue;
        }

        let mut nonces_per_bundle = BTreeMap::new();
        for (track_id, track) in active_tracks {
            let nonces = track
                .get("num_nonces_per_bundle")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    shape(
                        E,
                        format!("/challenges[{id}].config.active_tracks.{track_id}.num_nonces_per_bundle"),
                        "an unsigned integer",
                    )
                })?;
            nonces_per_bundle.insert(track_id.clone(), nonces);
        }

        let min_num_bundles = config
            .get("min_num_bundles")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                shape(
                    E,
                    format!("/challenges[{id}].config.min_num_bundles"),
                    "an unsigned integer",
                )
            })?;

        // Sparse by design: a track nobody has qualified on is simply absent,
        // and §6.3 reads a missing entry as zero. The active-track set comes
        // from the configuration above for exactly this reason.
        let mut network_qualifiers_by_track = BTreeMap::new();
        if let Some(by_track) = challenge
            .pointer("/block_data/num_qualifiers_by_track")
            .and_then(Value::as_object)
        {
            for (track_id, count) in by_track {
                let count = as_u128(count).ok_or_else(|| {
                    shape(
                        E,
                        format!("/challenges[{id}].block_data.num_qualifiers_by_track.{track_id}"),
                        "an unsigned integer",
                    )
                })?;
                network_qualifiers_by_track.insert(track_id.clone(), count);
            }
        }

        // §6.8's inputs. Read rather than defaulted: the fee is what the
        // pool must hold balance for, and a missing component silently read as
        // zero would under-state it on exactly the challenge whose
        // configuration the pool could not read.
        let fee = |field: &'static str| -> Result<u128, ProposeError> {
            config
                .get(field)
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<u128>().ok())
                .ok_or_else(|| {
                    shape(
                        E,
                        format!("/challenges[{id}].config.{field}"),
                        "a decimal atom string",
                    )
                })
        };

        configs.insert(
            id.to_string(),
            ChallengeConfig {
                compute_type,
                min_num_bundles,
                nonces_per_bundle,
                base_fee: fee("base_fee")?,
                per_nonce_fee: fee("per_nonce_fee")?,
            },
        );
        candidates.push(Challenge {
            id: id.to_string(),
            compute_type,
            active_tracks: active_tracks.keys().cloned().collect(),
            network_qualifiers_by_track,
            tracks_with_source: Vec::new(),
        });
    }

    Ok((candidates, configs))
}

/// What `get-algorithms` supplies: the candidates per challenge, and the
/// qualifier counts two different rules divide by.
#[derive(Debug, Default)]
struct AlgorithmEvidence {
    /// §6.4's candidates, per challenge.
    by_challenge: BTreeMap<String, Vec<Algorithm>>,
    /// §6.5's numerator: every player's qualifiers, per algorithm per track.
    ///
    /// Network-wide on purpose. §6.5 compares how an *algorithm* performs on a
    /// track, not how the pool does; scoping it to the pool would make an
    /// algorithm the pool has never run indistinguishable from one that does
    /// not work, and §6.7's alignment branch turns on that answer.
    qualifiers_by_algorithm_by_track: BTreeMap<String, BTreeMap<String, u128>>,
}

/// Read `get-algorithms`.
///
/// The collections are TIG's: `codes` are the algorithms, `binarys` say which
/// have a usable successful binary (§6.4 step 1). They are separate documents
/// because a code can exist with no binary, which is exactly the case step 1
/// excludes.
fn algorithm_evidence(
    snapshot: &Snapshot,
    block_round: u64,
) -> Result<AlgorithmEvidence, ProposeError> {
    const E: &str = "get-algorithms";
    let body = read(snapshot, E)?;

    // A code with a compiled binary. `compile_success` false is a binary that
    // exists and does not work, which step 1 excludes just as firmly as an
    // absent one.
    let mut compiled: BTreeSet<&str> = BTreeSet::new();
    let binarys = body
        .get("binarys")
        .and_then(Value::as_array)
        .ok_or_else(|| shape(E, "/binarys", "a list"))?;
    for binary in binarys {
        let id = binary
            .get("algorithm_id")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(E, "/binarys[].algorithm_id", "a string"))?;
        // `tig_integration.md` §5.1: usable means *confirmed*, compiled, and
        // fetchable. Compilation alone is not the test — an unconfirmed binary
        // is not yet part of the block's view, and one with no download URL is
        // a binary no runtime can obtain. Either would let §6.4 step 1 select
        // an algorithm the pool cannot actually run, and the precommit fee is
        // paid before anyone finds out.
        let confirmed = binary
            .pointer("/state/block_confirmed")
            .and_then(Value::as_u64)
            .is_some();
        let compiles = binary
            .pointer("/details/compile_success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let fetchable = binary
            .pointer("/details/download_url")
            .and_then(Value::as_str)
            .is_some_and(|url| !url.is_empty());
        if confirmed && compiles && fetchable {
            compiled.insert(id);
        }
    }

    let codes = body
        .get("codes")
        .and_then(Value::as_array)
        .ok_or_else(|| shape(E, "/codes", "a list"))?;

    let mut evidence = AlgorithmEvidence::default();
    for code in codes {
        let id = code
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(E, "/codes[].id", "a string"))?;
        let challenge_id = code
            .pointer("/details/challenge_id")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(E, format!("/codes[{id}].details.challenge_id"), "a string"))?;
        let state = code
            .get("state")
            .ok_or_else(|| shape(E, format!("/codes[{id}].state"), "an object"))?;

        // §6.4 step 1's "active": the same §5.1 test the challenges take, for
        // the same reason — an algorithm that activates in a later round is
        // not one the pool may select now.
        if !is_live_and_active(state, block_round) {
            continue;
        }

        // Adoption is an unsigned 18-decimal fixed-point integer as a decimal
        // string. Parsed as an integer and compared as one: §6.4 step 3 is a
        // comparison, and converting to a float to compare would let two
        // adoptions that differ in their last digits compare equal.
        let adoption_text = code
            .pointer("/block_data/adoption")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                shape(
                    E,
                    format!("/codes[{id}].block_data.adoption"),
                    "a decimal string",
                )
            })?;
        let adoption = adoption_text.parse::<u128>().map_err(|_| {
            shape(
                E,
                format!("/codes[{id}].block_data.adoption"),
                "a decimal string",
            )
        })?;

        if let Some(by_track) = code
            .pointer("/block_data/num_qualifiers_by_track_by_player")
            .and_then(Value::as_object)
        {
            for (track_id, by_player) in by_track {
                let by_player = by_player.as_object().ok_or_else(|| {
                    shape(
                        E,
                        format!(
                            "/codes[{id}].block_data.num_qualifiers_by_track_by_player.{track_id}"
                        ),
                        "an object",
                    )
                })?;
                for (player, count) in by_player {
                    let count = as_u128(count).ok_or_else(|| {
                        shape(
                            E,
                            format!("/codes[{id}].block_data.num_qualifiers_by_track_by_player.{track_id}.{player}"),
                            "an unsigned integer",
                        )
                    })?;
                    *evidence
                        .qualifiers_by_algorithm_by_track
                        .entry(id.to_string())
                        .or_default()
                        .entry(track_id.clone())
                        .or_default() += count;
                }
            }
        }

        evidence
            .by_challenge
            .entry(challenge_id.to_string())
            .or_default()
            .push(Algorithm {
                id: id.to_string(),
                banned: state
                    .get("banned")
                    .and_then(Value::as_bool)
                    // Absent is not "not banned". A ban the pool failed to
                    // read is a ban it would mine through, so the shape is
                    // required rather than defaulted.
                    .ok_or_else(|| shape(E, format!("/codes[{id}].state.banned"), "a boolean"))?,
                has_successful_binary: compiled.contains(id),
                adoption,
            });
    }

    Ok(evidence)
}

/// `active_bundles[a][t]`, summed over the block's active benchmarks.
///
/// From the cache rather than from a TIG field, because §5.2 is what retains
/// the per-benchmark facts and `for_decision` already refuses a snapshot whose
/// cache does not cover the active set — so this sum is over all of it or the
/// caller never got here.
fn active_bundles_by_algorithm_by_track(
    active: &[ActiveBenchmarkMeta],
) -> BTreeMap<String, BTreeMap<String, u128>> {
    let mut out: BTreeMap<String, BTreeMap<String, u128>> = BTreeMap::new();
    for meta in active {
        // `None` is "TIG has not published a count", which is not zero: a
        // benchmark contributing an unknown number of bundles must not lower
        // the denominator §6.5 divides by.
        let Some(bundles) = meta.num_active_bundles else {
            continue;
        };
        *out.entry(meta.algorithm_id.clone())
            .or_default()
            .entry(meta.track_id.clone())
            .or_default() += u128::from(bundles);
    }
    out
}

/// §6.3 and §6.5's `TrackStats`, joined from the two sources above.
fn track_stats(
    qualifiers: &BTreeMap<String, BTreeMap<String, u128>>,
    active_bundles: &BTreeMap<String, BTreeMap<String, u128>>,
) -> BTreeMap<String, BTreeMap<String, TrackStats>> {
    let mut out: BTreeMap<String, BTreeMap<String, TrackStats>> = BTreeMap::new();
    // Every (algorithm, track) either side knows about. A pair present in only
    // one is still a pair: qualifiers with no active bundles is §6.5's
    // "unavailable rate", and active bundles with no qualifiers is a rate of
    // zero — two different answers, and dropping either would silently make
    // one of them the other.
    for (algorithm, by_track) in qualifiers.iter().chain(active_bundles.iter()) {
        for track in by_track.keys() {
            out.entry(algorithm.clone()).or_default().insert(
                track.clone(),
                TrackStats {
                    qualifiers: qualifiers
                        .get(algorithm)
                        .and_then(|t| t.get(track))
                        .copied()
                        .unwrap_or(0),
                    active_bundles: active_bundles
                        .get(algorithm)
                        .and_then(|t| t.get(track))
                        .copied()
                        .unwrap_or(0),
                },
            );
        }
    }
    out
}

/// The pool's confirmed in-flight benchmarks, for §6.3's projection.
///
/// `mining_system.md` §2: a benchmark is in flight when its **precommit** is
/// confirmed, TIG has selected its track, and it is not yet active or
/// terminal. All four halves matter and each was got wrong once:
///
/// - the identity is on `precommits[].settings`, not on `benchmarks[]`. The
///   first version of this read `benchmarks[].details.player_id`, a field live
///   TIG does not publish there, so every entry failed the owner test and the
///   projection was permanently empty — §6.3 silently degraded to the current
///   raw factor, which piles successive decisions onto one challenge.
/// - a confirmed *benchmark* is not the test. Requiring one excludes the
///   principal in-flight state, a precommit confirmed and still computing.
/// - stopped and fraud-confirmed are terminal (§7: a stopped benchmark never
///   produces a proof). They never become active, so they never leave the
///   non-active set, and projecting them inflates `expected_addition` on every
///   decision from then on.
/// - active work already contributes to the qualifier counts §6.3 adds to.
///
/// Takes the typed [`ConfirmedWindow`] rather than the raw body. `window.rs`
/// says it is "the only place a TIG read becomes evidence, and the only place
/// §7's mapping is expressed"; the raw reading this replaces was a second
/// parser of the same document, and it had already drifted.
fn in_flight(window: &ConfirmedWindow, player_id: &str) -> Vec<InFlightBenchmark> {
    let active: BTreeSet<&str> = window.active.iter().map(String::as_str).collect();
    let stopped: BTreeSet<&str> = window
        .benchmarks
        .values()
        .filter(|b| b.stopped)
        .map(|b| b.benchmark_id.as_str())
        .collect();

    window
        .precommits
        .values()
        .filter(|p| {
            !active.contains(p.benchmark_id.as_str())
                && !stopped.contains(p.benchmark_id.as_str())
                && !window.frauds.contains_key(&p.benchmark_id)
        })
        .filter(|p| {
            p.settings
                .get("player_id")
                .and_then(Value::as_str)
                .is_some_and(|owner| owner.eq_ignore_ascii_case(player_id))
        })
        .filter_map(|p| {
            // A count TIG did not publish is not zero. §6.3 multiplies by it,
            // and a benchmark contributing an invented count would move the
            // factor that chooses the next challenge.
            let num_bundles = u128::try_from(p.num_bundles?).ok()?;
            Some(InFlightBenchmark {
                benchmark_id: p.benchmark_id.clone(),
                challenge_id: p
                    .settings
                    .get("challenge_id")
                    .and_then(Value::as_str)?
                    .to_string(),
                algorithm_id: p
                    .settings
                    .get("algorithm_id")
                    .and_then(Value::as_str)?
                    .to_string(),
                // §6.3 projects only a benchmark "whose selected algorithm and
                // track are known", and excludes the rest rather than counting
                // them as zero — the record has to show which happened.
                track_id: Some(p.track_id.clone()).filter(|t| !t.is_empty()),
                num_bundles,
            })
        })
        .collect()
}

/// §6.3's `pool_q[c][t]`, from OPoW.
///
/// The authoritative published count, which is the same quantity
/// `mining_system.md` §7 step 1 and invariant 13 read. An earlier version
/// summed `get-algorithms`' per-algorithm per-player counts instead: a
/// different endpoint, a different aggregation, and no document saying the two
/// are equal. If they ever differ, a decision made from one and attributed
/// from the other cannot be reconciled.
///
/// An absent player is an empty map, not an error: a pool that holds no
/// qualifiers yet is the ordinary first case, and §6.3 reads a missing entry
/// as zero.
fn pool_qualifiers(
    snapshot: &Snapshot,
    player_id: &str,
) -> Result<BTreeMap<String, BTreeMap<String, u128>>, ProposeError> {
    const E: &str = "get-opow";
    let body = read(snapshot, E)?;
    // Both envelopes, because the pin and the API disagree about this one
    // (`tig_integration.md` §14.5): testnet answers a list of players, and
    // `fixtures/tig/v1/get-opow.json` — which `fake-tig` serves verbatim —
    // holds a single unwrapped player object. Reading only one shape means the
    // pool cannot read §6.3's `pool_q[c][t]` against the other, and both are
    // endpoints it has to work against today. Narrow deliberately: an envelope
    // that is neither is a shape error, not a third case to guess at.
    let entries: Vec<&Value> = match body.get("opow") {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(one @ Value::Object(_)) => vec![one],
        _ => return Err(shape(E, "/opow", "a list of players or one player")),
    };

    let mut out: BTreeMap<String, BTreeMap<String, u128>> = BTreeMap::new();
    for entry in entries {
        let owner = entry.get("player_id").and_then(Value::as_str).unwrap_or("");
        if !owner.eq_ignore_ascii_case(player_id) {
            continue;
        }
        let Some(by_challenge) = entry
            .pointer("/block_data/num_qualifiers_by_challenge_by_track")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (challenge_id, by_track) in by_challenge {
            let by_track = by_track.as_object().ok_or_else(|| {
                shape(
                    E,
                    format!("/opow[{owner}].block_data.num_qualifiers_by_challenge_by_track.{challenge_id}"),
                    "an object",
                )
            })?;
            for (track_id, count) in by_track {
                let count = as_u128(count).ok_or_else(|| {
                    shape(
                        E,
                        format!("/opow[{owner}].block_data.num_qualifiers_by_challenge_by_track.{challenge_id}.{track_id}"),
                        "an unsigned integer",
                    )
                })?;
                out.entry(challenge_id.clone())
                    .or_default()
                    .insert(track_id.clone(), count);
            }
        }
    }
    Ok(out)
}

/// The cache's active benchmarks, as §6.6 candidates.
///
/// `verified` and `fraudulent` are not fields of the cache: an id in the
/// block's active set reached active, which follows a confirmed proof, and
/// §5.2 retains it only with a confirmed precommit and benchmark. A separate
/// flag would be a second opinion about the same fact.
fn source_candidates(active: &[ActiveBenchmarkMeta]) -> Vec<SourceBenchmark> {
    active
        .iter()
        // §6.6 step 3 *copies* the source's fuel budget. The cache keeps it
        // optional because TIG does not always publish one, and a `None`
        // admitted here with `unwrap_or_default` became a source offering a
        // budget of 0 — which `select_source` would return and the precommit
        // would submit. §6.6's last paragraph says a track with no valid
        // source makes the challenge ineligible; that is the correct outcome,
        // and a fabricated zero is not.
        .filter(|meta| meta.fuel_budget.is_some())
        .map(|meta| SourceBenchmark {
            benchmark_id: meta.benchmark_id.clone(),
            algorithm_id: meta.algorithm_id.clone(),
            track_id: meta.track_id.clone(),
            verified: true,
            fraudulent: false,
            block_confirmed: meta.benchmark_block_confirmed,
            // §5.2 keeps the qualities as TIG published them and the quality
            // type is the challenge's. Anything that is not an integer quality
            // cannot be compared by §6.6 step 2, and is dropped rather than
            // coerced — a coerced quality would win or lose a comparison the
            // protocol never defined.
            average_quality_by_bundle: meta
                .average_quality_by_bundle
                .iter()
                .filter_map(Value::as_i64)
                .collect(),
            hyperparameters: meta
                .hyperparameters
                .as_ref()
                .and_then(Value::as_object)
                .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            // Safe by the filter above, and written as a fallback only
            // because `SourceBenchmark` takes a bare `u64`.
            fuel_budget: meta.fuel_budget.unwrap_or_default(),
        })
        .collect()
}

/// What §6.4 and §6.6 conclude for one candidate challenge.
struct PerChallenge {
    algorithm: AlgorithmSelection,
    /// §6.6's source per active track, for the algorithm §6.4 chose here.
    sources: BTreeMap<String, SourceSelection>,
}

/// §6.4 then §6.6, for one candidate.
///
/// Run for *every* candidate rather than only the winner, because §6.2's
/// eligibility is defined in terms of its result — see the module doc.
fn per_challenge(
    challenge: &Challenge,
    config: &ChallengeConfig,
    algorithms: &[Algorithm],
    stats: &BTreeMap<String, BTreeMap<String, TrackStats>>,
    candidates: &[SourceBenchmark],
) -> Result<PerChallenge, ProposeError> {
    let algorithm = pool_decision::algorithm::select_algorithm(&AlgorithmSelectionInput {
        algorithms: algorithms.to_vec(),
        algorithm_track_stats: stats.clone(),
        active_tracks: challenge.active_tracks.clone(),
    })?;

    let mut sources = BTreeMap::new();
    if let Some(selected) = &algorithm.selected {
        for track in config.nonces_per_bundle.keys() {
            sources.insert(
                track.clone(),
                pool_decision::source::select_source(&SourceSelectionInput {
                    selected_algorithm: selected.clone(),
                    track: track.clone(),
                    active_benchmarks: candidates.to_vec(),
                }),
            );
        }
    }

    Ok(PerChallenge { algorithm, sources })
}

/// One deciding pass over one snapshot.
///
/// `active` is the block's active-benchmark cache. The caller reaches this
/// only through `PersistedSnapshot::for_decision`, which is what enforces
/// criterion C5 — so the cache here covers the block's whole active set and
/// the denominators below are complete.
pub fn propose(
    snapshot: &Snapshot,
    window: &ConfirmedWindow,
    network: Network,
    player_id: &str,
    offer: &Offer,
    active: &[ActiveBenchmarkMeta],
) -> Result<Proposed, ProposeError> {
    let round = block_round(snapshot)?;
    let (mut challenges, configs) = candidate_challenges(snapshot, round)?;
    let evidence = algorithm_evidence(snapshot, round)?;

    let stats = track_stats(
        &evidence.qualifiers_by_algorithm_by_track,
        &active_bundles_by_algorithm_by_track(active),
    );
    let candidates = source_candidates(active);

    // §6.4 and §6.6 per candidate, which is what makes §6.2's eligibility
    // answerable. A challenge whose chosen algorithm leaves any active track
    // without a source is excluded by `select_challenge` reading the
    // `tracks_with_source` filled in here.
    let mut per: BTreeMap<String, PerChallenge> = BTreeMap::new();
    for challenge in &mut challenges {
        let Some(config) = configs.get(&challenge.id) else {
            continue;
        };
        let decided = per_challenge(
            challenge,
            config,
            evidence
                .by_challenge
                .get(&challenge.id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            &stats,
            &candidates,
        )?;
        challenge.tracks_with_source = decided
            .sources
            .iter()
            .filter(|(_, selection)| selection.source_benchmark.is_some())
            .map(|(track, _)| track.clone())
            .collect();
        per.insert(challenge.id.clone(), decided);
    }

    // §6.3's draw, over every compute-compatible eligible challenge — the full
    // map, because criterion D2d makes it the audit evidence and which
    // challenges were candidates at this block is not recoverable later.
    // Exactly the set §6.3 chooses among: compute-compatible, and with every
    // active track sourced. A superset would put challenges that were never
    // candidates into an immutable audit record, and the record's whole job is
    // to say which ones were.
    let eligible: Vec<&Challenge> = challenges
        .iter()
        .filter(|c| offer.compute.matches_challenge(c.compute_type))
        .filter(|c| {
            c.active_tracks
                .iter()
                .all(|t| c.tracks_with_source.contains(t))
        })
        .collect();
    let seed = challenge_tie_seed(network, &snapshot.block_id);
    let ranks = draw_ranks(
        network,
        &snapshot.block_id,
        eligible.iter().map(|c| c.id.as_str()),
    );

    let selection = pool_decision::challenge::select_challenge(&ChallengeSelectionInput {
        offered_compute: offer.compute.clone(),
        challenges: challenges.clone(),
        pool_qualifiers_by_challenge_by_track: pool_qualifiers(snapshot, player_id)?,
        confirmed_in_flight_benchmarks: in_flight(window, player_id),
        algorithm_track_stats: stats.clone(),
        draw_ranks: ranks.clone(),
    })?;

    let Some(selected_challenge) = selection.selected.clone() else {
        return Ok(Proposed::NoAction(Box::new(selection)));
    };

    let config =
        configs
            .get(&selected_challenge)
            .ok_or_else(|| ProposeError::SelectedChallengeUnknown {
                challenge_id: selected_challenge.clone(),
            })?;
    let decided =
        per.get(&selected_challenge)
            .ok_or_else(|| ProposeError::SelectedChallengeUnknown {
                challenge_id: selected_challenge.clone(),
            })?;
    let selected_algorithm = decided.algorithm.selected.clone().ok_or_else(|| {
        // Unreachable: a challenge with no algorithm has no source for any
        // track, so `select_challenge` excluded it. Reported rather than
        // unwrapped, because the alternative to an error here is a precommit
        // built around an empty algorithm id.
        ProposeError::SelectedAlgorithmUnknown {
            challenge_id: selected_challenge.clone(),
        }
    })?;

    // §6.7, for every active track: TIG chooses the final one during precommit
    // processing, so all of them are sized and submitted.
    let sizing = pool_decision::bundles::size_bundles(&BundleSizingInput {
        offered_compute: offer.compute.clone(),
        min_num_bundles: config.min_num_bundles,
        tracks: config
            .nonces_per_bundle
            .iter()
            .map(|(track, nonces)| {
                (
                    track.clone(),
                    TrackSizing {
                        num_nonces_per_bundle: *nonces,
                        selected_algorithm_best_on_track: decided
                            .algorithm
                            .selected_is_best_on_track
                            .get(track)
                            .copied()
                            .unwrap_or(false),
                    },
                )
            })
            .collect(),
    })?;

    let mut fee_by_track = BTreeMap::new();
    for (track, num_bundles) in &sizing.num_bundles {
        let fee = config
            .per_nonce_fee
            .checked_mul(u128::from(*num_bundles))
            .and_then(|scaled| scaled.checked_add(config.base_fee))
            .ok_or_else(|| ProposeError::FeeOverflow {
                challenge_id: selected_challenge.clone(),
                track_id: track.clone(),
            })?;
        fee_by_track.insert(track.clone(), fee);
    }

    let mut track_settings = serde_json::Map::new();
    for (track, num_bundles) in &sizing.num_bundles {
        let source = decided.sources.get(track);
        track_settings.insert(
            track.clone(),
            json!({
                "num_bundles": num_bundles,
                // §6.6 copies the source's budget and hyperparameters. A track
                // that reached here has a source — `tracks_with_source` is what
                // made the challenge eligible — so an absent one is a defect,
                // not a default; it is left absent so `from_decision` refuses
                // the record rather than sending a fabricated budget.
                "fuel_budget": source.and_then(|s| s.fuel_budget),
                "hyperparameters": source
                    .map(|s| Value::Object(s.hyperparameters.clone().into_iter().collect()))
                    .unwrap_or(Value::Null),
            }),
        );
    }

    Ok(Proposed::Precommit(Box::new(Proposal {
        // The protocol type the §6.1 body sends (`tig_integration.md` §3's
        // `aws_*` set), which is the offer's and not the challenge's. The
        // challenge publishes a *class*; the two are different vocabularies,
        // and `config/tig_integration.json` keeps them as separate fields for
        // that reason. Sending a class here would send `"cpu"` where TIG
        // expects `"aws_c7i"`.
        compute_type: offer.tig_compute_type.clone(),
        compute_class: match config.compute_type {
            ComputeType::Cpu => "cpu".to_string(),
            ComputeType::Gpu => "gpu".to_string(),
        },
        selected_challenge,
        selected_algorithm,
        track_settings: Value::Object(track_settings),
        draw_ranks: ranks,
        tie_seed: seed,
        // §6.3 records a draw outcome only when a draw happened. Set
        // unconditionally it would assert, on every untied pass, that the
        // winner came from a tiebreak it never entered — and invariant 25 is
        // about a record an auditor can reproduce.
        tie_winner: selection
            .tie_candidates
            .as_ref()
            .and_then(|_| selection.selected.clone()),
        tie_candidates: selection.tie_candidates.clone(),
        challenge_selection: selection,
        algorithm_selection: decided.algorithm.clone(),
        bundle_sizing: sizing,
        fee_by_track,
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const POOL: &str = "0x2935a721068da756b28cba896efdb64e8909dfae";
    /// Any non-zero adoption. Deliberately not the 18-decimal scale TIG
    /// happens to publish: §12 forbids compiling an observed value in, and a
    /// test that used the real one would be doing exactly that for the sake of
    /// looking realistic. Every comparison here is between integers, so the
    /// magnitude is irrelevant.
    const SOME_ADOPTION: &str = "7";
    const OTHER: &str = "0xd354d4f76dd4747acd4763dc083d3fb506eaed5e";
    const ROUND: u64 = 133;

    /// A snapshot in the shape live TIG returns, so a test that passes here is
    /// a test against the real document and not against a convenient one.
    struct Fixture {
        challenges: Vec<Value>,
        codes: Vec<Value>,
        binarys: Vec<Value>,
        /// `get-benchmarks` in TIG's real shape: identity and counts live on
        /// `precommits[]`, and `benchmarks[]` carries only the bundle facts.
        /// Getting this wrong is what made the first version of `in_flight`
        /// read fields that do not exist, so the harness holds the collections
        /// separately rather than letting a test invent one entry shape.
        precommits: Vec<Value>,
        benchmark_entries: Vec<Value>,
        frauds: Vec<Value>,
        /// `get-opow[].block_data.num_qualifiers_by_challenge_by_track` for
        /// the pool: §6.3's `pool_q[c][t]`.
        pool_qualifiers: Vec<(&'static str, &'static str, u64)>,
        active_ids: Vec<String>,
    }

    impl Fixture {
        /// One CPU challenge with one track, one algorithm with a binary and
        /// non-zero adoption, and nothing in flight. Every test below starts
        /// from a case that *works* and breaks one thing, so a test that stops
        /// proposing proves the thing it broke mattered.
        fn workable() -> Self {
            Self {
                challenges: vec![challenge("c001", "cpu", 25, &[("t1", 10)], &[("t1", 30)])],
                codes: vec![code("c001_a001", "c001", 25, SOME_ADOPTION, &[])],
                binarys: vec![binary("c001_a001", true)],
                precommits: vec![],
                benchmark_entries: vec![],
                frauds: vec![],
                pool_qualifiers: vec![],
                active_ids: vec![],
            }
        }

        fn benchmarks_body(&self) -> Value {
            json!({
                "precommits": self.precommits,
                "benchmarks": self.benchmark_entries,
                "proofs": [],
                "frauds": self.frauds,
            })
        }

        fn opow_body(&self) -> Value {
            let mut by_challenge: serde_json::Map<String, Value> = serde_json::Map::new();
            for (challenge, track, count) in &self.pool_qualifiers {
                by_challenge
                    .entry((*challenge).to_string())
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .unwrap()
                    .insert((*track).to_string(), json!(count));
            }
            json!({
                "opow": [{
                    "player_id": POOL,
                    "block_data": {"num_qualifiers_by_challenge_by_track": by_challenge},
                }],
            })
        }

        /// The confirmed window, built by the module that owns §7's mapping.
        ///
        /// Not hand-assembled: routing the fixture through `confirmed_window`
        /// is what makes these tests measure the real document. The shape bug
        /// this replaced survived precisely because the test built its own.
        fn window(&self) -> ConfirmedWindow {
            let snapshot = self.snapshot();
            crate::window::confirmed_window(&self.benchmarks_body(), &snapshot.block)
                .expect("the fixture is a readable get-benchmarks body")
        }

        fn snapshot(&self) -> Snapshot {
            Snapshot {
                block_id: "block-1".to_string(),
                height: 100_080,
                block: json!({
                    "block": {
                        "id": "block-1",
                        "details": {"round": ROUND, "height": 100_080},
                        "data": {"active_ids": {"benchmark": self.active_ids}},
                    }
                }),
                reads: BTreeMap::from([
                    (
                        "get-challenges".to_string(),
                        json!({"challenges": self.challenges}),
                    ),
                    (
                        "get-algorithms".to_string(),
                        json!({
                            "codes": self.codes,
                            "binarys": self.binarys,
                            "advances": [],
                        }),
                    ),
                    ("get-benchmarks".to_string(), self.benchmarks_body()),
                    ("get-opow".to_string(), self.opow_body()),
                ]),
                tracks: BTreeMap::new(),
                reads_complete: true,
                active_cache_ready: true,
            }
        }
    }

    fn challenge(
        id: &str,
        compute: &str,
        round_active: u64,
        tracks: &[(&str, u64)],
        network_qualifiers: &[(&str, u64)],
    ) -> Value {
        let active_tracks: serde_json::Map<String, Value> = tracks
            .iter()
            .map(|(t, nonces)| {
                (
                    (*t).to_string(),
                    json!({"num_nonces_per_bundle": nonces, "min_active_quality": 0}),
                )
            })
            .collect();
        let by_track: serde_json::Map<String, Value> = network_qualifiers
            .iter()
            .map(|(t, n)| ((*t).to_string(), json!(n)))
            .collect();
        json!({
            "id": id,
            "config": {
                "type": compute,
                "active_tracks": active_tracks,
                "min_num_bundles": 1,
                // §6.8's inputs, as TIG publishes them: decimal atom strings.
                // Small arbitrary values — the real 10^16/10^15 scale is an
                // observed constant §12 forbids compiling in, and every
                // assertion here is exact integer arithmetic.
                "base_fee": "100",
                "per_nonce_fee": "7",
            },
            "state": {"round_active": round_active},
            "block_data": {"num_qualifiers_by_track": by_track},
        })
    }

    fn code(
        id: &str,
        challenge_id: &str,
        round_active: u64,
        adoption: &str,
        qualifiers: &[(&str, &str, u64)],
    ) -> Value {
        let mut by_track: serde_json::Map<String, Value> = serde_json::Map::new();
        for (track, player, count) in qualifiers {
            by_track
                .entry((*track).to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert((*player).to_string(), json!(count));
        }
        json!({
            "id": id,
            "details": {"challenge_id": challenge_id},
            "state": {"round_active": round_active, "banned": false},
            "block_data": {
                "adoption": adoption,
                "num_qualifiers_by_track_by_player": by_track,
            },
        })
    }

    /// A binary in TIG's shape: confirmed, compiled, with a download URL.
    ///
    /// All three matter (`tig_integration.md` §5.1), so the helper carries all
    /// three and the tests below remove them one at a time.
    fn binary(algorithm_id: &str, compile_success: bool) -> Value {
        json!({
            "algorithm_id": algorithm_id,
            "details": {
                "compile_success": compile_success,
                "download_url": format!("https://example.invalid/get-binary-blob?algorithm_id={algorithm_id}"),
            },
            "state": {"block_confirmed": 473_764},
        })
    }

    fn active(
        benchmark_id: &str,
        algorithm: &str,
        track: &str,
        bundles: Option<u64>,
    ) -> ActiveBenchmarkMeta {
        ActiveBenchmarkMeta {
            benchmark_id: benchmark_id.to_string(),
            player_id: OTHER.to_string(),
            // The algorithm id encodes its challenge in TIG's scheme
            // (`c008_a001`), so this stays consistent with the algorithm
            // rather than being a third opinion about which challenge it is.
            challenge_id: algorithm.split('_').next().unwrap_or(algorithm).to_string(),
            algorithm_id: algorithm.to_string(),
            track_id: track.to_string(),
            compute_type: Some("cpu".to_string()),
            num_bundles: 1,
            fuel_budget: Some(5_000_000),
            hyperparameters: Some(json!({"alpha": 3})),
            precommit_block_confirmed: 1,
            num_active_bundles: bundles,
            average_quality_by_bundle: vec![json!(50)],
            stopped: false,
            benchmark_block_confirmed: 2,
        }
    }

    /// A CPU offer on an arm instance. The class and the protocol type are
    /// different vocabularies and this fixture keeps them visibly different,
    /// because conflating them is how `"cpu"` reaches a field TIG reads as
    /// `"aws_t4g"`.
    fn cpu_offer() -> Offer {
        Offer {
            compute: OfferedCompute::Cpu { cores: 8 },
            tig_compute_type: "aws_t4g".to_string(),
        }
    }

    fn run(f: &Fixture, active: &[ActiveBenchmarkMeta]) -> Result<Proposed, ProposeError> {
        propose(
            &f.snapshot(),
            &f.window(),
            Network::Testnet,
            POOL,
            &cpu_offer(),
            active,
        )
    }

    /// A `precommits[]` entry in TIG's shape.
    fn precommit_entry(
        benchmark_id: &str,
        player: &str,
        challenge: &str,
        algorithm: &str,
        track: &str,
        num_bundles: Option<u64>,
        confirmed: bool,
    ) -> Value {
        let mut details = serde_json::Map::new();
        details.insert("block_started".to_string(), json!(100));
        details.insert("num_nonces".to_string(), json!(10));
        if let Some(n) = num_bundles {
            details.insert("num_bundles".to_string(), json!(n));
        }
        json!({
            "benchmark_id": benchmark_id,
            "details": details,
            "settings": {
                "player_id": player,
                "challenge_id": challenge,
                "algorithm_id": algorithm,
                "track_id": track,
            },
            "state": {"block_confirmed": if confirmed { json!(101) } else { Value::Null }},
        })
    }

    /// A `benchmarks[]` entry in TIG's shape: bundle facts only.
    fn benchmark_entry(benchmark_id: &str, stopped: bool) -> Value {
        json!({
            "id": benchmark_id,
            "details": {
                "stopped": stopped,
                "num_active_bundles": 1,
                "average_quality_by_bundle": [50],
            },
            "state": {"block_confirmed": 102},
        })
    }

    fn precommit(proposed: Proposed) -> Proposal {
        match proposed {
            Proposed::Precommit(p) => *p,
            Proposed::NoAction(s) => panic!("expected a proposal, got no action: {s:?}"),
        }
    }

    #[test]
    fn a_challenge_that_activates_in_a_later_round_is_not_a_candidate() {
        // `tig_integration.md` §5.1. On live testnet most challenges sit at
        // `round_active` 200 against a block in round 133, so a pass that read
        // them as candidates would propose work the pool cannot mine — and
        // §13 check 6 would not catch it, since the check scopes itself by the
        // same test.
        let mut f = Fixture::workable();
        f.challenges = vec![challenge(
            "c001",
            "cpu",
            ROUND + 1,
            &[("t1", 10)],
            &[("t1", 30)],
        )];
        let Proposed::NoAction(selection) =
            run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap()
        else {
            panic!("a challenge active in a later round must not be proposed");
        };
        assert!(
            selection.considered.is_empty(),
            "it must not even be considered: {selection:?}"
        );
    }

    #[test]
    fn an_absent_round_active_is_not_active() {
        // The claim `is_live_and_active` makes, tested because absence is the
        // case the §5.1 comparison never sees: a missing field read as active
        // puts a challenge the pool cannot mine into the candidate set, and
        // §13 check 6 scopes itself by the same test, so nothing downstream
        // would refuse it either.
        assert!(is_live_and_active(&json!({"round_active": ROUND}), ROUND));
        assert!(is_live_and_active(
            &json!({"round_active": ROUND - 1}),
            ROUND
        ));
        assert!(!is_live_and_active(
            &json!({"round_active": ROUND + 1}),
            ROUND
        ));
        assert!(
            !is_live_and_active(&json!({}), ROUND),
            "absent is not active"
        );
        assert!(
            !is_live_and_active(&json!({"round_active": null}), ROUND),
            "null is not active either — `advances` carries nulls in this position"
        );
        assert!(
            !is_live_and_active(&json!({"round_active": "25"}), ROUND),
            "a round that arrived as a string is a shape change, not an activation"
        );
    }

    #[test]
    fn a_challenge_with_no_round_active_is_not_a_candidate() {
        // The same rule reached through `propose`, so the filter is wired in
        // and not merely correct in isolation.
        let mut f = Fixture::workable();
        f.challenges = vec![json!({
            "id": "c001",
            "config": {
                "type": "cpu",
                "active_tracks": {"t1": {"num_nonces_per_bundle": 10}},
                "min_num_bundles": 1,
            },
            "state": {},
            "block_data": {"num_qualifiers_by_track": {"t1": 30}},
        })];
        let Proposed::NoAction(selection) =
            run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap()
        else {
            panic!("a challenge with no round_active must not be proposed");
        };
        assert!(selection.considered.is_empty(), "{selection:?}");
    }

    #[test]
    fn an_algorithm_with_no_round_active_is_not_selectable() {
        // §6.4 step 1's "active" takes the same test. Asserted separately
        // because the challenge and the code are different documents and a
        // filter applied to one is not applied to the other.
        let mut f = Fixture::workable();
        f.codes = vec![json!({
            "id": "c001_a001",
            "details": {"challenge_id": "c001"},
            "state": {"banned": false},
            "block_data": {"adoption": SOME_ADOPTION, "num_qualifiers_by_track_by_player": {}},
        })];
        assert!(matches!(
            run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap(),
            Proposed::NoAction(_)
        ));
    }

    #[test]
    fn an_algorithm_whose_binary_does_not_compile_is_not_selectable() {
        // §6.4 step 1 wants "a usable successful binary". A binary that exists
        // and failed to compile is not one, and is a different record from an
        // absent binary — both must exclude, so both are asserted.
        for (binarys, why) in [
            (vec![binary("c001_a001", false)], "compile_success false"),
            (vec![], "no binary at all"),
        ] {
            let mut f = Fixture::workable();
            f.binarys = binarys;
            let proposed = run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap();
            assert!(
                matches!(proposed, Proposed::NoAction(_)),
                "{why} must exclude the algorithm, leaving the challenge ineligible"
            );
        }
    }

    #[test]
    fn a_banned_algorithm_is_excluded_and_an_unreadable_ban_is_an_error() {
        // §6.4 step 1. The two halves are different failures: a ban that is
        // present excludes, and a ban that cannot be read must stop the pass
        // rather than default to "not banned" — the pool would otherwise mine
        // through exactly the ban it failed to read.
        let mut banned = Fixture::workable();
        banned.codes = vec![json!({
            "id": "c001_a001",
            "details": {"challenge_id": "c001"},
            "state": {"round_active": 25, "banned": true},
            "block_data": {"adoption": SOME_ADOPTION, "num_qualifiers_by_track_by_player": {}},
        })];
        assert!(matches!(
            run(&banned, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap(),
            Proposed::NoAction(_)
        ));

        let mut unreadable = Fixture::workable();
        unreadable.codes = vec![json!({
            "id": "c001_a001",
            "details": {"challenge_id": "c001"},
            "state": {"round_active": 25},
            "block_data": {"adoption": SOME_ADOPTION, "num_qualifiers_by_track_by_player": {}},
        })];
        assert_eq!(
            run(&unreadable, &[active("b1", "c001_a001", "t1", Some(4))]),
            Err(ProposeError::Shape {
                endpoint: "get-algorithms",
                path: "/codes[c001_a001].state.banned".to_string(),
                expected: "a boolean",
            }),
            "an unreadable ban must stop the pass, not default to unbanned"
        );
    }

    #[test]
    fn an_offer_is_checked_against_the_pinned_compute_vocabulary() {
        // `tig_integration.md` §3: a compute type outside the compatibility
        // table is "ineligible rather than coerced". Refused here rather than
        // sent and rejected, because the precommit fee is paid on submission.
        //
        // Against the shipped pin, not a fabricated table — the point is that
        // the value an operator writes is checked against what this build
        // actually knows.
        let pinned: Value =
            serde_json::from_str(include_str!("../../../config/tig_integration.json")).unwrap();

        let cpu = Offer::from_config("cpu", Some(8), "aws_t4g", &pinned).unwrap();
        assert_eq!(cpu.compute, OfferedCompute::Cpu { cores: 8 });
        assert_eq!(cpu.tig_compute_type, "aws_t4g");

        let gpu = Offer::from_config("gpu", None, "aws_g4dn", &pinned).unwrap();
        assert_eq!(gpu.compute, OfferedCompute::Gpu);

        // A type the table does not carry at all.
        let err = Offer::from_config("cpu", Some(8), "aws_z9x", &pinned).unwrap_err();
        assert!(
            matches!(err, OfferError::UnknownComputeType { ref compute_type, .. }
                     if compute_type == "aws_z9x"),
            "{err}"
        );

        // And a type that is real but belongs to the *other* class. This is
        // the case the first version accepted: it flattened every vendor list
        // into one set, so a cpu offer carrying `aws_g4dn` passed, `propose`
        // then selected CPU challenges, and the §6.1 body would have carried
        // the GPU protocol type. §3 calls that coercion and forbids it.
        //
        // An earlier version of this test *claimed* to cover it in a comment
        // and asserted nothing, which is worse than not testing it.
        let err = Offer::from_config("cpu", Some(8), "aws_g4dn", &pinned).unwrap_err();
        assert!(
            matches!(err, OfferError::UnknownComputeType { ref compute_class, .. }
                     if compute_class == "cpu"),
            "a gpu type is not a cpu offer's: {err}"
        );
        let err = Offer::from_config("gpu", None, "aws_t4g", &pinned).unwrap_err();
        assert!(
            matches!(err, OfferError::UnknownComputeType { ref compute_class, .. }
                     if compute_class == "gpu"),
            "and the reverse: {err}"
        );

        assert_eq!(
            Offer::from_config("cpu", None, "aws_t4g", &pinned).unwrap_err(),
            OfferError::NoCores,
            "§6.7 divides by the core count"
        );
        assert!(matches!(
            Offer::from_config("quantum", Some(8), "aws_t4g", &pinned).unwrap_err(),
            OfferError::UnknownClass(_)
        ));

        // A pin with no table refuses everything rather than accepting
        // anything: an unreadable vocabulary is not an empty one.
        assert_eq!(
            Offer::from_config("cpu", Some(8), "aws_t4g", &json!({})).unwrap_err(),
            OfferError::PinUnreadable
        );
        // A pin that names types but not their classes cannot answer the
        // question this function asks, so it is unreadable too rather than
        // falling back to the flattened set it used to use.
        assert_eq!(
            Offer::from_config(
                "cpu",
                Some(8),
                "aws_t4g",
                &json!({"compute_compatibility": {
                    "compute_types_by_vendor": {"arm": ["aws_t4g"]},
                }}),
            )
            .unwrap_err(),
            OfferError::PinUnreadable
        );
    }

    #[test]
    fn the_fee_scales_with_bundles_and_is_computed_per_track() {
        // §6.8: `base_fee + per_nonce_fee * num_bundles`, with **bundles**,
        // despite the name — settled during the spike against the pinned
        // upstream commit, where `fixtures/tig/v1/expected.json`'s per-nonce
        // rule is the refuted side. `fake-tig` had it backwards until PR #35,
        // which is why this is asserted from the pool's side too.
        //
        // Two tracks with different nonce counts, so a fee that multiplied by
        // nonces would differ between them and be caught, and so the
        // per-track requirement (§6.7 sizes each separately, TIG picks one)
        // is exercised rather than assumed.
        let mut f = Fixture::workable();
        f.challenges = vec![challenge(
            "c001",
            "cpu",
            25,
            &[("t1", 10), ("t2", 40)],
            &[("t1", 30), ("t2", 30)],
        )];
        let p = precommit(
            run(
                &f,
                &[
                    active("b1", "c001_a001", "t1", Some(4)),
                    active("b2", "c001_a001", "t2", Some(4)),
                ],
            )
            .unwrap(),
        );

        // base 100, per-nonce 7. A CPU offer of 8 cores over 10 nonces per
        // bundle aligns to 4 bundles (§6.7), and over 40 nonces to 1.
        let bundles = &p.bundle_sizing.num_bundles;
        assert_eq!(bundles["t1"], 4, "8 cores, 10 nonces: aligned to 4 bundles");
        assert_eq!(
            bundles["t2"], 1,
            "8 cores, 40 nonces: the minimum already aligns"
        );

        // Written as `base + per * bundles` so the §6.8 rule is visible in the
        // assertion and not only in the total it comes to.
        let fee = |bundles: u128| 100 + 7 * bundles;
        assert_eq!(p.fee_by_track["t1"], fee(4), "4 bundles, not 10 nonces");
        assert_eq!(p.fee_by_track["t2"], fee(1), "1 bundle, not 40 nonces");
        assert_ne!(
            p.fee_by_track["t1"], p.fee_by_track["t2"],
            "the two tracks differ, so a single fee for the challenge would be wrong"
        );
    }

    #[test]
    fn a_challenge_with_no_active_tracks_is_not_a_candidate() {
        // §6.2's eligibility is "settings for **every** active track", and
        // `all()` over an empty set is true — so a track-less challenge would
        // be vacuously eligible and §6.3 would score it 0/0 = 0, the lowest
        // factor there is. It would win every decision from then on, and the
        // precommit would carry no track settings at all.
        //
        // Paired with a challenge that *is* minable and scores worse, so the
        // assertion is that the empty one lost rather than that nothing was
        // proposed.
        let mut f = Fixture::workable();
        f.pool_qualifiers = vec![("c001", "t1", 5)];
        f.challenges = vec![
            challenge("c001", "cpu", 25, &[("t1", 10)], &[("t1", 30)]),
            challenge("c002", "cpu", 25, &[], &[]),
        ];
        f.codes = vec![
            code("c001_a001", "c001", 25, SOME_ADOPTION, &[]),
            code("c002_a001", "c002", 25, SOME_ADOPTION, &[]),
        ];
        f.binarys = vec![binary("c001_a001", true), binary("c002_a001", true)];

        let p = precommit(run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap());
        assert_eq!(
            p.selected_challenge, "c001",
            "a challenge with nothing to mine must not out-score one that can be mined"
        );
        assert!(
            !p.draw_ranks.contains_key("c002"),
            "and it was never a candidate: {:?}",
            p.draw_ranks.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_opow_envelope_is_read_in_both_shapes_the_pool_meets() {
        // §14.5: testnet answers `opow` as a list of players and the pinned
        // fixture — which `fake-tig` serves verbatim — holds one unwrapped
        // player object. The pool has to read §6.3's `pool_q[c][t]` against
        // both endpoints today, so it reads both envelopes and nothing else.
        let expected = BTreeMap::from([(
            "c001".to_string(),
            BTreeMap::from([("t1".to_string(), 5u128)]),
        )]);
        let listed = Fixture {
            pool_qualifiers: vec![("c001", "t1", 5)],
            ..Fixture::workable()
        };
        assert_eq!(pool_qualifiers(&listed.snapshot(), POOL).unwrap(), expected);

        // The fixture's shape: one player, unwrapped.
        let mut snapshot = listed.snapshot();
        snapshot.reads.insert(
            "get-opow".to_string(),
            json!({
                "opow": {
                    "player_id": POOL,
                    "block_data": {
                        "num_qualifiers_by_challenge_by_track": {"c001": {"t1": 5}},
                    },
                },
            }),
        );
        assert_eq!(pool_qualifiers(&snapshot, POOL).unwrap(), expected);

        // A third shape is a shape error, not a case to guess at.
        snapshot
            .reads
            .insert("get-opow".to_string(), json!({"opow": "unexpected"}));
        assert_eq!(
            pool_qualifiers(&snapshot, POOL),
            Err(ProposeError::Shape {
                endpoint: "get-opow",
                path: "/opow".to_string(),
                expected: "a list of players or one player",
            })
        );
    }

    #[test]
    fn a_binary_that_is_unconfirmed_or_unfetchable_is_not_usable() {
        // `tig_integration.md` §5.1: usable means confirmed, compiled *and*
        // fetchable. Compilation alone would let §6.4 step 1 select an
        // algorithm that is not yet in the block's view, or one no runtime can
        // obtain — and the precommit fee is paid before anyone finds out.
        for (b, why) in [
            (
                json!({
                    "algorithm_id": "c001_a001",
                    "details": {
                        "compile_success": true,
                        "download_url": "https://example.invalid/b",
                    },
                    "state": {},
                }),
                "unconfirmed",
            ),
            (
                json!({
                    "algorithm_id": "c001_a001",
                    "details": {"compile_success": true},
                    "state": {"block_confirmed": 1},
                }),
                "no download url",
            ),
            (
                json!({
                    "algorithm_id": "c001_a001",
                    "details": {"compile_success": true, "download_url": ""},
                    "state": {"block_confirmed": 1},
                }),
                "empty download url",
            ),
        ] {
            let mut f = Fixture::workable();
            f.binarys = vec![b];
            assert!(
                matches!(
                    run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap(),
                    Proposed::NoAction(_)
                ),
                "a binary that is {why} must leave the algorithm unselectable"
            );
        }
    }

    #[test]
    fn a_source_whose_fuel_budget_tig_did_not_publish_is_not_a_source() {
        // §6.6 step 3 *copies* the source's fuel budget. The cache keeps it
        // optional because TIG does not always publish one; admitting such an
        // entry produced a source offering a budget of 0, which the precommit
        // would then submit. §6.6's last paragraph says the track has no valid
        // source and the challenge is ineligible — a fabricated zero is not
        // the same answer.
        let mut meta = active("b1", "c001_a001", "t1", Some(4));
        meta.fuel_budget = None;
        let f = Fixture::workable();

        assert!(
            source_candidates(&[meta.clone()]).is_empty(),
            "an unpublished budget is not a source"
        );
        let Proposed::NoAction(selection) = run(&f, &[meta]).unwrap() else {
            panic!("a track with no valid source must not be precommitted");
        };
        assert!(selection.excluded.contains_key("c001"), "{selection:?}");
    }

    #[test]
    fn adoption_is_compared_as_an_integer() {
        // TIG publishes adoption as an 18-decimal fixed-point integer in a
        // decimal string. Two values that differ only in their last digits are
        // distinguishable as integers and identical as f64, and §6.4 step 3
        // picks the highest — so a float comparison would silently make this a
        // tie and hand the choice to the id ordering instead.
        let low = "3000000000000000001";
        let high = "3000000000000000002";
        assert_eq!(
            low.parse::<f64>().unwrap(),
            high.parse::<f64>().unwrap(),
            "the premise: these are equal as f64"
        );

        let mut f = Fixture::workable();
        // `a002` sorts after `a001`, so an id-ordered tiebreak would pick
        // `a001` and the assertion below would fail.
        f.codes = vec![
            code("c001_a001", "c001", 25, low, &[]),
            code("c001_a002", "c001", 25, high, &[]),
        ];
        f.binarys = vec![binary("c001_a001", true), binary("c001_a002", true)];
        let p = precommit(
            run(
                &f,
                &[
                    active("b1", "c001_a001", "t1", Some(4)),
                    active("b2", "c001_a002", "t1", Some(4)),
                ],
            )
            .unwrap(),
        );
        assert_eq!(
            p.selected_algorithm, "c001_a002",
            "the higher adoption must win, and it differs from the lower only in the last digit"
        );
    }

    #[test]
    fn a_track_with_no_source_benchmark_makes_the_challenge_ineligible() {
        // §6.6's last paragraph, which §6.2 reads as eligibility. The
        // challenge has two tracks and the cache covers only one, so settings
        // for the other cannot be constructed.
        let mut f = Fixture::workable();
        f.challenges = vec![challenge(
            "c001",
            "cpu",
            25,
            &[("t1", 10), ("t2", 10)],
            &[("t1", 30), ("t2", 30)],
        )];
        let Proposed::NoAction(selection) =
            run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap()
        else {
            panic!("a challenge with an unsourced track must not be proposed");
        };
        assert!(
            selection.excluded.contains_key("c001"),
            "and the record must say why: {selection:?}"
        );

        // The same fixture with both tracks sourced does propose, so the
        // exclusion above is the missing source and not the second track.
        let both = [
            active("b1", "c001_a001", "t1", Some(4)),
            active("b2", "c001_a001", "t2", Some(4)),
        ];
        assert_eq!(
            precommit(run(&f, &both).unwrap()).selected_challenge,
            "c001"
        );
    }

    #[test]
    fn the_pool_s_qualifiers_are_its_own_and_an_algorithm_s_are_everyone_s() {
        // §6.3's numerator is what *the pool* holds, published by OPoW — the
        // same quantity §7 step 1 and invariant 13 read. §6.5's is how the
        // algorithm performs network-wide, which is on the algorithm code.
        // Two endpoints, two aggregations; an earlier version derived the
        // first from the second, which no document says are equal.
        let f = Fixture {
            pool_qualifiers: vec![("c001", "t1", 5)],
            codes: vec![code(
                "c001_a001",
                "c001",
                25,
                SOME_ADOPTION,
                &[("t1", POOL, 5), ("t1", OTHER, 20)],
            )],
            ..Fixture::workable()
        };
        let snapshot = f.snapshot();

        assert_eq!(
            pool_qualifiers(&snapshot, POOL).unwrap()["c001"]["t1"],
            5,
            "§6.3 reads the pool's own count from OPoW"
        );
        assert_eq!(
            algorithm_evidence(&snapshot, ROUND)
                .unwrap()
                .qualifiers_by_algorithm_by_track["c001_a001"]["t1"],
            25,
            "§6.5 counts every player's"
        );
        assert!(
            pool_qualifiers(&snapshot, OTHER).unwrap().is_empty(),
            "another player's OPoW entry is not the pool's"
        );
    }

    #[test]
    fn an_unknown_bundle_count_does_not_lower_the_denominator() {
        // §6.5 divides by active bundles. A benchmark whose count TIG has not
        // published contributes nothing, which is not the same as contributing
        // zero — counting it as zero would leave the denominator unchanged but
        // is arithmetic by accident; the record has to be a sum over what is
        // known.
        let known = active_bundles_by_algorithm_by_track(&[
            active("b1", "a1", "t1", Some(4)),
            active("b2", "a1", "t1", None),
        ]);
        assert_eq!(known["a1"]["t1"], 4);

        let none = active_bundles_by_algorithm_by_track(&[active("b2", "a1", "t1", None)]);
        assert!(
            !none.contains_key("a1"),
            "a pair known only through an unpublished count is not a pair: {none:?}"
        );
    }

    #[test]
    fn no_qualifiers_and_no_bundles_stay_different_answers() {
        // §6.5: "no active bundles" makes the rate *unavailable*, and an
        // unavailable rate is not a zero rate — a zero rate is still usable
        // for the best-on-track comparison, so collapsing the two would let an
        // algorithm that has never run beat one that has.
        let qualifiers = BTreeMap::from([(
            "a1".to_string(),
            BTreeMap::from([("t1".to_string(), 9u128)]),
        )]);
        let bundles = BTreeMap::from([(
            "a2".to_string(),
            BTreeMap::from([("t1".to_string(), 3u128)]),
        )]);
        let stats = track_stats(&qualifiers, &bundles);

        let never_ran = stats["a1"]["t1"];
        assert_eq!(never_ran.active_bundles, 0);
        assert_eq!(
            never_ran.rate().unwrap(),
            None,
            "qualifiers with no bundles is an unavailable rate"
        );

        let ran_badly = stats["a2"]["t1"];
        assert_eq!(ran_badly.qualifiers, 0);
        assert!(
            ran_badly.rate().unwrap().is_some(),
            "bundles with no qualifiers is a usable rate of zero"
        );
    }

    #[test]
    fn the_draw_map_covers_every_eligible_challenge_not_only_the_tied_ones() {
        // Criterion D2d: the rank map is the audit evidence, and which
        // challenges were eligible at this block is not recoverable later. A
        // map built only when a tie happened could never be reconstructed.
        // Both challenges here are eligible; the *scope* of the set is
        // `the_draw_map_holds_only_challenges_that_were_actually_candidates`.
        let mut f = Fixture::workable();
        f.challenges = vec![
            challenge("c001", "cpu", 25, &[("t1", 10)], &[("t1", 30)]),
            challenge("c002", "cpu", 25, &[("t1", 10)], &[("t1", 1)]),
        ];
        // The pool already holds qualifiers on c001 and none on c002, so
        // §6.3's factors are 5/30 and 0 — genuinely different, which is what
        // makes "not tied, still ranked" the thing being asserted.
        f.pool_qualifiers = vec![("c001", "t1", 5)];
        f.codes = vec![
            code("c001_a001", "c001", 25, SOME_ADOPTION, &[]),
            code("c002_a001", "c002", 25, SOME_ADOPTION, &[]),
        ];
        f.binarys = vec![binary("c001_a001", true), binary("c002_a001", true)];
        let p = precommit(
            run(
                &f,
                &[
                    active("b1", "c001_a001", "t1", Some(4)),
                    active("b2", "c002_a001", "t1", Some(4)),
                ],
            )
            .unwrap(),
        );
        assert_eq!(
            p.draw_ranks.keys().collect::<Vec<_>>(),
            vec!["c001", "c002"],
            "both candidates need a rank, whether or not they tied"
        );
        assert!(
            p.tie_candidates.is_none(),
            "these do not tie, which is what makes the full map the point"
        );
        assert_eq!(
            p.selected_challenge, "c002",
            "the lower projected factor wins on its own, not by the draw"
        );
    }

    #[test]
    fn the_draw_map_holds_only_challenges_that_were_actually_candidates() {
        // Criterion D2d scopes the map to "every compute-compatible eligible
        // challenge". A superset is not merely untidy: the rank map is written
        // once into an immutable decision record whose job is to say which
        // challenges were candidates at this block, and a GPU challenge or one
        // missing a track source was never among them.
        let mut f = Fixture::workable();
        f.challenges = vec![
            challenge("c001", "cpu", 25, &[("t1", 10)], &[("t1", 30)]),
            // Wrong compute type for a CPU offer.
            challenge("c009", "gpu", 25, &[("t1", 10)], &[("t1", 30)]),
            // Right type, but one of its two tracks has no source.
            challenge("c002", "cpu", 25, &[("t1", 10), ("t2", 10)], &[("t1", 30)]),
        ];
        f.codes = vec![
            code("c001_a001", "c001", 25, SOME_ADOPTION, &[]),
            code("c009_a001", "c009", 25, SOME_ADOPTION, &[]),
            code("c002_a001", "c002", 25, SOME_ADOPTION, &[]),
        ];
        f.binarys = vec![
            binary("c001_a001", true),
            binary("c009_a001", true),
            binary("c002_a001", true),
        ];
        let p = precommit(
            run(
                &f,
                &[
                    active("b1", "c001_a001", "t1", Some(4)),
                    active("b2", "c009_a001", "t1", Some(4)),
                    active("b3", "c002_a001", "t1", Some(4)),
                ],
            )
            .unwrap(),
        );
        assert_eq!(
            p.draw_ranks.keys().collect::<Vec<_>>(),
            vec!["c001"],
            "the GPU challenge and the partially-sourced one were never candidates"
        );
        assert_eq!(
            p.draw_ranks.keys().collect::<Vec<_>>(),
            p.challenge_selection.considered.iter().collect::<Vec<_>>(),
            "the ranked set and the considered set are the same set"
        );
    }

    #[test]
    fn a_pass_with_no_tie_records_no_draw_outcome() {
        // §6.3 and D2d record a tied set and its winner "when a tie occurred".
        // Set unconditionally, `tie_winner` asserts on every untied pass that
        // the winner came from a tiebreak it never entered — and invariant 25
        // is about a record an auditor can reproduce.
        let mut f = Fixture::workable();
        f.pool_qualifiers = vec![("c001", "t1", 5)];
        f.challenges = vec![
            challenge("c001", "cpu", 25, &[("t1", 10)], &[("t1", 30)]),
            challenge("c002", "cpu", 25, &[("t1", 10)], &[("t1", 1)]),
        ];
        f.codes = vec![
            code("c001_a001", "c001", 25, SOME_ADOPTION, &[]),
            code("c002_a001", "c002", 25, SOME_ADOPTION, &[]),
        ];
        f.binarys = vec![binary("c001_a001", true), binary("c002_a001", true)];
        let untied = precommit(
            run(
                &f,
                &[
                    active("b1", "c001_a001", "t1", Some(4)),
                    active("b2", "c002_a001", "t1", Some(4)),
                ],
            )
            .unwrap(),
        );
        assert!(untied.tie_candidates.is_none());
        assert_eq!(untied.tie_winner, None, "no tie, so no draw outcome");

        // The same fixture with both factors at zero does tie, and then the
        // winner is the draw's — so the field is populated exactly when it
        // means something.
        f.pool_qualifiers = vec![];
        let tied = precommit(
            run(
                &f,
                &[
                    active("b1", "c001_a001", "t1", Some(4)),
                    active("b2", "c002_a001", "t1", Some(4)),
                ],
            )
            .unwrap(),
        );
        assert_eq!(
            tied.tie_candidates.as_deref(),
            Some(["c001".to_string(), "c002".to_string()].as_slice()),
        );
        assert_eq!(
            tied.tie_winner.as_deref(),
            Some(tied.selected_challenge.as_str())
        );
    }

    /// Every in-flight case over one fixture, so the §2 definition is tested
    /// as the four-part test it is rather than four unrelated assertions.
    ///
    /// The table is the point: each row differs from the projected one in
    /// exactly one respect, and the shapes are TIG's own — the bug this
    /// replaced survived because the test invented a `benchmarks[].details`
    /// that carries the identity, which live TIG puts on `precommits[].settings`.
    #[test]
    fn only_a_confirmed_unterminated_inactive_precommit_of_the_pool_is_projected() {
        let f = Fixture {
            precommits: vec![
                precommit_entry("b-flight", POOL, "c001", "c001_a001", "t1", Some(7), true),
                precommit_entry("b-active", POOL, "c001", "c001_a001", "t1", Some(7), true),
                precommit_entry("b-stopped", POOL, "c001", "c001_a001", "t1", Some(7), true),
                precommit_entry("b-fraud", POOL, "c001", "c001_a001", "t1", Some(7), true),
                precommit_entry(
                    "b-unconfirmed",
                    POOL,
                    "c001",
                    "c001_a001",
                    "t1",
                    Some(7),
                    false,
                ),
                precommit_entry("b-other", OTHER, "c001", "c001_a001", "t1", Some(7), true),
                precommit_entry("b-no-track", POOL, "c001", "c001_a001", "", Some(7), true),
                precommit_entry("b-no-bundles", POOL, "c001", "c001_a001", "t1", None, true),
            ],
            benchmark_entries: vec![
                benchmark_entry("b-stopped", true),
                benchmark_entry("b-flight", false),
            ],
            frauds: vec![json!({
                "benchmark_id": "b-fraud",
                "state": {"block_confirmed": 103},
            })],
            active_ids: vec!["b-active".to_string()],
            ..Fixture::workable()
        };

        let flights = in_flight(&f.window(), POOL);
        let projected: Vec<&str> = flights.iter().map(|b| b.benchmark_id.as_str()).collect();

        // `b-no-track` is in flight but carries no track, so §6.3 records it
        // as excluded from the projection rather than contributing zero — it
        // is present with `track_id: None`, which is a different answer from
        // being absent.
        assert_eq!(
            projected,
            vec!["b-flight", "b-no-track"],
            "one per §2 reason: active, stopped, fraud-confirmed, unconfirmed, \
             another player's, and a count TIG did not publish are all out"
        );

        let no_track = flights
            .iter()
            .find(|b| b.benchmark_id == "b-no-track")
            .expect("present");
        assert_eq!(
            no_track.track_id, None,
            "excluded from the projection, not zeroed"
        );

        let flight = flights
            .iter()
            .find(|b| b.benchmark_id == "b-flight")
            .expect("present");
        assert_eq!(
            flight.num_bundles, 7,
            "from precommits[].details.num_bundles"
        );
        assert_eq!(flight.challenge_id, "c001", "from precommits[].settings");
        assert_eq!(
            flight.algorithm_id, "c001_a001",
            "from precommits[].settings"
        );
        assert_eq!(flight.track_id.as_deref(), Some("t1"));
    }

    #[test]
    fn a_precommit_confirmed_and_still_computing_is_the_principal_in_flight_case() {
        // Called out separately because an earlier version required a
        // confirmed `benchmarks[]` entry, which excludes exactly this — the
        // state most in-flight work is in. Nothing here has a benchmarks[]
        // entry at all.
        let f = Fixture {
            precommits: vec![precommit_entry(
                "b1",
                POOL,
                "c001",
                "c001_a001",
                "t1",
                Some(3),
                true,
            )],
            ..Fixture::workable()
        };
        let projected = in_flight(&f.window(), POOL);
        assert_eq!(projected.len(), 1, "{projected:?}");
        assert_eq!(projected[0].num_bundles, 3);
    }

    #[test]
    fn a_missing_read_is_an_error_and_not_a_quiet_no_action() {
        // The distinction the error type exists for: "nothing to mine" and
        // "the snapshot could not be read" must not log the same way, or a
        // pool that stopped deciding looks like a pool with nothing to decide.
        let f = Fixture::workable();
        let mut snapshot = f.snapshot();
        snapshot.reads.remove("get-algorithms");
        assert_eq!(
            propose(
                &snapshot,
                &f.window(),
                Network::Testnet,
                POOL,
                &cpu_offer(),
                &[]
            ),
            Err(ProposeError::MissingRead {
                endpoint: "get-algorithms"
            })
        );
    }

    #[test]
    fn the_workable_case_proposes() {
        // The baseline every other test perturbs. Without it a test that
        // asserts "no action" proves nothing — the fixture might never have
        // been proposable at all.
        let f = Fixture::workable();
        let p = precommit(run(&f, &[active("b1", "c001_a001", "t1", Some(4))]).unwrap());
        assert_eq!(p.selected_challenge, "c001");
        assert_eq!(p.selected_algorithm, "c001_a001");
        assert_eq!(
            p.compute_type, "aws_t4g",
            "the record carries TIG's protocol type, which is the offer's"
        );
        assert_eq!(
            p.compute_class, "cpu",
            "and the challenge's class beside it, which §6.2 matched on"
        );
        assert_eq!(
            p.track_settings.pointer("/t1/fuel_budget"),
            Some(&json!(5_000_000)),
            "§6.6 copies the source's budget: {:?}",
            p.track_settings
        );
        assert_eq!(
            p.track_settings.pointer("/t1/hyperparameters/alpha"),
            Some(&json!(3)),
            "§6.6 copies the source's hyperparameters at TIG's types"
        );
    }
}
