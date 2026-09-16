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
use serde_json::{Value, json};

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
}

/// A proposal, ready for the admission transaction to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub selected_challenge: String,
    pub selected_algorithm: String,
    pub compute_type: String,
    /// The decision record's `track_settings`, in the shape
    /// `PrecommitSubmission::from_decision` reads back.
    pub track_settings: Value,
    /// §6.3's audit evidence: the rank of every compute-compatible eligible
    /// challenge, not only the tied ones (criterion D2d).
    pub draw_ranks: BTreeMap<String, DrawRank>,
    pub tie_candidates: Option<Vec<String>>,
    pub tie_winner: Option<String>,
    /// Kept whole so the caller can record why, not only what.
    pub challenge_selection: ChallengeSelection,
    pub algorithm_selection: AlgorithmSelection,
    pub bundle_sizing: BundleSizing,
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

        configs.insert(
            id.to_string(),
            ChallengeConfig {
                compute_type,
                min_num_bundles,
                nonces_per_bundle,
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
    /// §6.3's numerator: the pool's own qualifiers, per challenge per track.
    pool_qualifiers_by_challenge_by_track: BTreeMap<String, BTreeMap<String, u128>>,
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
    player_id: &str,
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
        if binary
            .pointer("/details/compile_success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
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
                    // TIG addresses are hex; compared case-insensitively for
                    // the reason `tig_integration.md` §13 check 9 gives.
                    if player.eq_ignore_ascii_case(player_id) {
                        *evidence
                            .pool_qualifiers_by_challenge_by_track
                            .entry(challenge_id.to_string())
                            .or_default()
                            .entry(track_id.clone())
                            .or_default() += count;
                    }
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
/// In-flight is §2's definition: confirmed at TIG and not yet active. The
/// active set is the block's, so a benchmark in both is not in flight — it is
/// already contributing to the qualifier counts the projection adds to.
fn in_flight(
    snapshot: &Snapshot,
    player_id: &str,
    active_ids: &BTreeSet<String>,
) -> Result<Vec<InFlightBenchmark>, ProposeError> {
    const E: &str = "get-benchmarks";
    let body = read(snapshot, E)?;
    let benchmarks = body
        .get("benchmarks")
        .and_then(Value::as_array)
        .ok_or_else(|| shape(E, "/benchmarks", "a list"))?;

    let mut out = Vec::new();
    for benchmark in benchmarks {
        let id = benchmark
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(E, "/benchmarks[].id", "a string"))?;
        if active_ids.contains(id) {
            continue;
        }
        let owner = benchmark
            .pointer("/details/player_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !owner.eq_ignore_ascii_case(player_id) {
            continue;
        }
        // §7: a benchmark is confirmed when its block_confirmed is set.
        // Anything else is a write the pool sent and TIG has not recorded, and
        // §6.3 projects only confirmed work.
        if benchmark
            .pointer("/state/block_confirmed")
            .and_then(Value::as_u64)
            .is_none()
        {
            continue;
        }
        let challenge_id = benchmark
            .pointer("/details/challenge_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                shape(
                    E,
                    format!("/benchmarks[{id}].details.challenge_id"),
                    "a string",
                )
            })?;
        let algorithm_id = benchmark
            .pointer("/details/algorithm_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                shape(
                    E,
                    format!("/benchmarks[{id}].details.algorithm_id"),
                    "a string",
                )
            })?;
        let num_bundles = benchmark
            .pointer("/details/num_bundles")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                shape(
                    E,
                    format!("/benchmarks[{id}].details.num_bundles"),
                    "an unsigned integer",
                )
            })?;

        out.push(InFlightBenchmark {
            benchmark_id: id.to_string(),
            challenge_id: challenge_id.to_string(),
            algorithm_id: algorithm_id.to_string(),
            // Absent is `None`, not a guess. §6.3 excludes a benchmark whose
            // track TIG has not selected, and the record has to show that it
            // was excluded rather than that it contributed zero.
            track_id: benchmark
                .pointer("/details/track_id")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            num_bundles: u128::from(num_bundles),
        });
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
    network: Network,
    player_id: &str,
    offered_compute: OfferedCompute,
    active: &[ActiveBenchmarkMeta],
) -> Result<Proposed, ProposeError> {
    let round = block_round(snapshot)?;
    let (mut challenges, configs) = candidate_challenges(snapshot, round)?;
    let evidence = algorithm_evidence(snapshot, player_id, round)?;

    let active_ids: BTreeSet<String> = active.iter().map(|m| m.benchmark_id.clone()).collect();
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
    let seed = challenge_tie_seed(network, &snapshot.block_id);
    let ranks = draw_ranks(
        network,
        &snapshot.block_id,
        challenges.iter().map(|c| c.id.as_str()),
    );
    debug_assert_eq!(seed, challenge_tie_seed(network, &snapshot.block_id));

    let selection = pool_decision::challenge::select_challenge(&ChallengeSelectionInput {
        offered_compute: offered_compute.clone(),
        challenges: challenges.clone(),
        pool_qualifiers_by_challenge_by_track: evidence
            .pool_qualifiers_by_challenge_by_track
            .clone(),
        confirmed_in_flight_benchmarks: in_flight(snapshot, player_id, &active_ids)?,
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
        offered_compute,
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
        compute_type: match config.compute_type {
            ComputeType::Cpu => "cpu".to_string(),
            ComputeType::Gpu => "gpu".to_string(),
        },
        selected_challenge,
        selected_algorithm,
        track_settings: Value::Object(track_settings),
        draw_ranks: ranks,
        tie_candidates: selection.tie_candidates.clone(),
        tie_winner: selection.selected.clone(),
        challenge_selection: selection,
        algorithm_selection: decided.algorithm.clone(),
        bundle_sizing: sizing,
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
        benchmarks: Vec<Value>,
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
                benchmarks: vec![],
            }
        }

        fn snapshot(&self) -> Snapshot {
            Snapshot {
                block_id: "block-1".to_string(),
                height: 100_080,
                block: json!({
                    "block": {
                        "id": "block-1",
                        "details": {"round": ROUND},
                        "data": {"active_ids": {"benchmark": []}},
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
                    (
                        "get-benchmarks".to_string(),
                        json!({"benchmarks": self.benchmarks}),
                    ),
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

    fn binary(algorithm_id: &str, compile_success: bool) -> Value {
        json!({
            "algorithm_id": algorithm_id,
            "details": {"compile_success": compile_success},
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
            challenge_id: "c001".to_string(),
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

    fn cpu() -> OfferedCompute {
        OfferedCompute::Cpu { cores: 8 }
    }

    fn run(f: &Fixture, active: &[ActiveBenchmarkMeta]) -> Result<Proposed, ProposeError> {
        propose(&f.snapshot(), Network::Testnet, POOL, cpu(), active)
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
        // §6.3's numerator is what *the pool* holds; §6.5's is how the
        // algorithm performs network-wide. Reading one for the other is
        // invisible in a single-player fixture, so this one has two players.
        let f = Fixture {
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
        let evidence = algorithm_evidence(&snapshot, POOL, ROUND).unwrap();

        assert_eq!(
            evidence.pool_qualifiers_by_challenge_by_track["c001"]["t1"], 5,
            "§6.3 counts only the pool's own"
        );
        assert_eq!(
            evidence.qualifiers_by_algorithm_by_track["c001_a001"]["t1"], 25,
            "§6.5 counts every player's"
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
    fn the_draw_map_covers_every_candidate_not_only_the_tied_ones() {
        // Criterion D2d: the full rank map is the audit evidence, and which
        // challenges were candidates at this block is not recoverable later.
        // A map built only when a tie happened could never be reconstructed.
        let mut f = Fixture::workable();
        f.challenges = vec![
            challenge("c001", "cpu", 25, &[("t1", 10)], &[("t1", 30)]),
            challenge("c002", "cpu", 25, &[("t1", 10)], &[("t1", 1)]),
        ];
        // The pool already holds qualifiers on c001 and none on c002, so
        // §6.3's factors are 5/30 and 0 — genuinely different, which is what
        // makes "not tied, still ranked" the thing being asserted.
        f.codes = vec![
            code("c001_a001", "c001", 25, SOME_ADOPTION, &[("t1", POOL, 5)]),
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
    fn an_already_active_benchmark_is_not_also_in_flight() {
        // §2's definition: in flight is confirmed and *not yet active*. An
        // active benchmark already contributes to the qualifier counts §6.3
        // reads, so projecting it as well would count the same work twice —
        // in both the numerator and the denominator.
        let f = Fixture {
            benchmarks: vec![json!({
                "id": "b1",
                "details": {
                    "player_id": POOL,
                    "challenge_id": "c001",
                    "algorithm_id": "c001_a001",
                    "track_id": "t1",
                    "num_bundles": 7,
                },
                "state": {"block_confirmed": 10},
            })],
            ..Fixture::workable()
        };
        let snapshot = f.snapshot();

        let none_active = in_flight(&snapshot, POOL, &BTreeSet::new()).unwrap();
        assert_eq!(none_active.len(), 1, "confirmed and not active: in flight");
        assert_eq!(none_active[0].num_bundles, 7);

        let active_now = in_flight(&snapshot, POOL, &BTreeSet::from(["b1".to_string()])).unwrap();
        assert!(
            active_now.is_empty(),
            "active is not in flight: {active_now:?}"
        );
    }

    #[test]
    fn another_player_s_benchmark_is_not_the_pool_s_projection() {
        // §6.3 projects the pool's own in-flight work. Someone else's is
        // already in the network counts, and adding it to the numerator would
        // inflate the pool's projected share and steer the choice away from a
        // challenge it should have picked.
        let f = Fixture {
            benchmarks: vec![json!({
                "id": "b1",
                "details": {
                    "player_id": OTHER,
                    "challenge_id": "c001",
                    "algorithm_id": "c001_a001",
                    "track_id": "t1",
                    "num_bundles": 7,
                },
                "state": {"block_confirmed": 10},
            })],
            ..Fixture::workable()
        };
        assert!(
            in_flight(&f.snapshot(), POOL, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_unconfirmed_benchmark_is_not_projected() {
        // §7 makes confirmation a read. A write the pool sent and TIG has not
        // recorded is not evidence of anything, and projecting it would let a
        // failed submission steer the next decision.
        let f = Fixture {
            benchmarks: vec![json!({
                "id": "b1",
                "details": {
                    "player_id": POOL,
                    "challenge_id": "c001",
                    "algorithm_id": "c001_a001",
                    "track_id": "t1",
                    "num_bundles": 7,
                },
                "state": {},
            })],
            ..Fixture::workable()
        };
        assert!(
            in_flight(&f.snapshot(), POOL, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
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
            propose(&snapshot, Network::Testnet, POOL, cpu(), &[]),
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
        assert_eq!(p.compute_type, "cpu");
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
