//! §6.2 compute compatibility and eligibility, §6.3 the projected raw
//! balancing factor and the recorded tie draw.

use std::collections::BTreeMap;

use pool_domain::DrawRank;

use crate::ratio::{Ratio, RatioError};

/// What a member offered (`mining_system.md` §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferedCompute {
    /// CPU offers carry a core count; §6.7's alignment rule needs it.
    Cpu {
        cores: u64,
    },
    Gpu,
}

impl OfferedCompute {
    fn matches(&self, challenge: ComputeType) -> bool {
        matches!(
            (self, challenge),
            (OfferedCompute::Cpu { .. }, ComputeType::Cpu)
                | (OfferedCompute::Gpu, ComputeType::Gpu)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeType {
    Cpu,
    Gpu,
}

/// One candidate challenge, reduced to what §6.2 and §6.3 read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub id: String,
    pub compute_type: ComputeType,
    /// The challenge's active tracks, from its own configuration.
    ///
    /// Carried explicitly rather than inferred from the qualifier map's keys.
    /// TIG's `num_qualifiers_by_track` is sparse — a track nobody has
    /// qualified on is simply absent — so inferring the set would drop that
    /// track from §6.2's "every active track" check *and* from §6.3's sums.
    /// The challenge would then be declared eligible without a source for it,
    /// and score zero under the zero-denominator branch, which makes the
    /// challenge that should have been excluded the most preferred one.
    pub active_tracks: Vec<String>,
    /// Network qualifiers per active track. Missing means zero, not absent.
    pub network_qualifiers_by_track: BTreeMap<String, u128>,
    /// Active tracks that have at least one valid §6.6 source benchmark.
    pub tracks_with_source: Vec<String>,
}

/// A confirmed in-flight benchmark contributing to §6.3's projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightBenchmark {
    pub benchmark_id: String,
    pub challenge_id: String,
    pub algorithm_id: String,
    /// `None` when TIG has not yet selected the track.
    ///
    /// §6.3 projects "each confirmed in-flight benchmark *whose selected
    /// algorithm and track are known*", and §2's in-flight definition says
    /// the same. Such a benchmark is excluded from the projection rather than
    /// contributing zero — the two have the same arithmetic effect and
    /// different meanings, and the decision record has to show which happened.
    pub track_id: Option<String>,
    pub num_bundles: u128,
}

/// `qualifiers[a,t]` and `active_bundles[a,t]` for the live rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackStats {
    pub qualifiers: u128,
    pub active_bundles: u128,
}

impl TrackStats {
    /// `qualifier_rate[a,t]`, or `None` when there are no active bundles.
    ///
    /// §6.3: "If there are no active bundles for that algorithm and track, its
    /// rate is unavailable and its expected addition is zero." Unavailable is
    /// not zero — a zero rate would still be a usable rate for §6.5's
    /// best-on-track comparison, and treating the two alike would make an
    /// algorithm that has never run beat one that has.
    pub fn rate(self) -> Result<Option<Ratio>, RatioError> {
        if self.active_bundles == 0 {
            return Ok(None);
        }
        Ratio::new(self.qualifiers, self.active_bundles).map(Some)
    }
}

/// Everything §6.2 and §6.3 need for one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeSelectionInput {
    pub offered_compute: OfferedCompute,
    pub challenges: Vec<Challenge>,
    /// Pool qualifiers per challenge per track.
    pub pool_qualifiers_by_challenge_by_track: BTreeMap<String, BTreeMap<String, u128>>,
    pub confirmed_in_flight_benchmarks: Vec<InFlightBenchmark>,
    /// `algorithm_track_stats[a][t]`, for the in-flight projection.
    pub algorithm_track_stats: BTreeMap<String, BTreeMap<String, TrackStats>>,
    /// One rank per compute-compatible eligible challenge, derived by the
    /// controller from the anchor block (`pool_domain::challenge_tie`).
    ///
    /// The engine derives no randomness of its own (`architecture.md` §3), so
    /// a missing rank for a tied challenge is an error, never a fallback.
    pub draw_ranks: BTreeMap<String, DrawRank>,
}

/// Why a challenge was not considered. Kept because §6.3's audit record has to
/// show which challenges were candidates at that instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Excluded {
    IncompatibleComputeType,
    /// An active track with no valid §6.6 source: §6.2 makes the challenge
    /// ineligible, before any factor is compared.
    TrackWithoutSource {
        track_id: String,
    },
}

/// Why an in-flight benchmark did not contribute to the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionExcluded {
    /// TIG has not selected the track yet.
    TrackUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeSelection {
    /// Compute-compatible and eligible, in id order.
    pub considered: Vec<String>,
    pub excluded: BTreeMap<String, Excluded>,
    /// `qualifier_rate[a][t]`, `None` where no active bundles make it usable.
    pub qualifier_rate: BTreeMap<String, BTreeMap<String, Option<Ratio>>>,
    /// `expected_q[j]` for each projected in-flight benchmark.
    pub expected_q: BTreeMap<String, Ratio>,
    pub projection_excluded: BTreeMap<String, ProjectionExcluded>,
    /// `expected_addition[c]`, for every considered challenge.
    pub expected_addition: BTreeMap<String, Ratio>,
    pub projected_raw_factor: BTreeMap<String, Ratio>,
    /// The challenges that tied on the lowest factor, when more than one did.
    pub tie_candidates: Option<Vec<String>>,
    /// `None` means §6.2's no-action outcome: nothing compatible and eligible.
    pub selected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChallengeError {
    #[error("arithmetic: {0}")]
    Ratio(#[from] RatioError),
    #[error("challenge {challenge_id} tied but the controller supplied no draw rank")]
    MissingDrawRank { challenge_id: String },
}

/// §6.2 then §6.3: pick the compatible eligible challenge with the lowest
/// projected raw balancing factor, resolving a tie by the supplied draw.
pub fn select_challenge(
    input: &ChallengeSelectionInput,
) -> Result<ChallengeSelection, ChallengeError> {
    let mut excluded = BTreeMap::new();
    let mut considered = Vec::new();

    for challenge in &input.challenges {
        if !input.offered_compute.matches(challenge.compute_type) {
            excluded.insert(challenge.id.clone(), Excluded::IncompatibleComputeType);
            continue;
        }
        // §6.2: eligible only when the pool can construct valid settings for
        // *every* active track. One track without a source disqualifies the
        // challenge, however good its factor.
        if let Some(track_id) = challenge
            .active_tracks
            .iter()
            .find(|track| !challenge.tracks_with_source.contains(track))
        {
            excluded.insert(
                challenge.id.clone(),
                Excluded::TrackWithoutSource {
                    track_id: track_id.clone(),
                },
            );
            continue;
        }
        considered.push(challenge);
    }

    let projection = project(input)?;

    let mut expected_addition = BTreeMap::new();
    let mut projected_raw_factor = BTreeMap::new();
    for challenge in &considered {
        let addition = projection
            .addition
            .get(&challenge.id)
            .copied()
            .unwrap_or(Ratio::ZERO);
        let factor = projected_raw_factor_of(
            challenge,
            input
                .pool_qualifiers_by_challenge_by_track
                .get(&challenge.id),
            addition,
        )?;
        expected_addition.insert(challenge.id.clone(), addition);
        projected_raw_factor.insert(challenge.id.clone(), factor);
    }

    let lowest = lowest_factor(&projected_raw_factor)?;
    let (tie_candidates, selected) = match lowest {
        None => (None, None),
        Some(tied) if tied.len() == 1 => (None, tied.into_iter().next()),
        Some(tied) => {
            let winner = resolve_tie(&tied, &input.draw_ranks)?;
            (Some(tied), Some(winner))
        }
    };

    Ok(ChallengeSelection {
        considered: considered.iter().map(|c| c.id.clone()).collect(),
        excluded,
        qualifier_rate: projection.rates,
        expected_q: projection.expected_q,
        projection_excluded: projection.excluded,
        expected_addition,
        projected_raw_factor,
        tie_candidates,
        selected,
    })
}

/// §6.3's projection, kept whole because the decision record stores every
/// intermediate: the rates used, each benchmark's expected qualifiers, and the
/// per-challenge sum.
struct Projection {
    rates: BTreeMap<String, BTreeMap<String, Option<Ratio>>>,
    expected_q: BTreeMap<String, Ratio>,
    excluded: BTreeMap<String, ProjectionExcluded>,
    addition: BTreeMap<String, Ratio>,
}

/// `expected_addition[c] = sum(num_bundles[j] * qualifier_rate[a,t])`.
fn project(input: &ChallengeSelectionInput) -> Result<Projection, ChallengeError> {
    let mut rates: BTreeMap<String, BTreeMap<String, Option<Ratio>>> = BTreeMap::new();
    let mut expected_q = BTreeMap::new();
    let mut excluded = BTreeMap::new();
    let mut addition: BTreeMap<String, Ratio> = BTreeMap::new();

    for benchmark in &input.confirmed_in_flight_benchmarks {
        let Some(track_id) = benchmark.track_id.as_ref() else {
            excluded.insert(
                benchmark.benchmark_id.clone(),
                ProjectionExcluded::TrackUnknown,
            );
            continue;
        };
        let stats = input
            .algorithm_track_stats
            .get(&benchmark.algorithm_id)
            .and_then(|by_track| by_track.get(track_id))
            .copied();
        // §6.3: a benchmark whose algorithm and track have no live rate — no
        // active bundles, or no stats at all — adds zero. It is still a real
        // in-flight benchmark; what is missing is a basis for projecting it.
        let rate = match stats {
            Some(stats) => stats.rate()?,
            None => None,
        };
        rates
            .entry(benchmark.algorithm_id.clone())
            .or_default()
            .insert(track_id.clone(), rate);
        let expected = match rate {
            Some(rate) => rate.checked_mul_int(benchmark.num_bundles)?,
            None => Ratio::ZERO,
        };
        expected_q.insert(benchmark.benchmark_id.clone(), expected);
        let entry = addition
            .entry(benchmark.challenge_id.clone())
            .or_insert(Ratio::ZERO);
        *entry = entry.checked_add(expected)?;
    }

    Ok(Projection {
        rates,
        expected_q,
        excluded,
        addition,
    })
}

fn projected_raw_factor_of(
    challenge: &Challenge,
    pool_qualifiers: Option<&BTreeMap<String, u128>>,
    expected_addition: Ratio,
) -> Result<Ratio, RatioError> {
    // Summed over the active-track set, not over whichever tracks happen to
    // appear in either sparse map. A missing entry is zero qualifiers.
    let mut pool_q: u128 = 0;
    let mut network_q: u128 = 0;
    for track in &challenge.active_tracks {
        let network = challenge
            .network_qualifiers_by_track
            .get(track)
            .copied()
            .unwrap_or(0);
        network_q = network_q
            .checked_add(network)
            .ok_or(RatioError::Overflow("network_q"))?;
        let pool = pool_qualifiers
            .and_then(|by_track| by_track.get(track))
            .copied()
            .unwrap_or(0);
        pool_q = pool_q
            .checked_add(pool)
            .ok_or(RatioError::Overflow("pool_q"))?;
    }

    let denominator = Ratio::whole(network_q).checked_add(expected_addition)?;
    // The explicit zero branch. It covers both the plain `network_q = 0` case
    // and a projection that adds nothing to it, exactly as §6.3 writes it.
    if denominator.is_zero() {
        return Ok(Ratio::ZERO);
    }
    let numerator = Ratio::whole(pool_q).checked_add(expected_addition)?;

    // numerator / denominator, where both are already rationals.
    Ratio::new(
        numerator
            .numerator()
            .checked_mul(denominator.denominator())
            .ok_or(RatioError::Overflow("factor numerator"))?,
        denominator
            .numerator()
            .checked_mul(numerator.denominator())
            .ok_or(RatioError::Overflow("factor denominator"))?,
    )
}

/// The challenge ids holding the lowest factor, in id order.
fn lowest_factor(factors: &BTreeMap<String, Ratio>) -> Result<Option<Vec<String>>, ChallengeError> {
    let mut best: Option<(Ratio, Vec<String>)> = None;
    for (id, factor) in factors {
        match &mut best {
            None => best = Some((*factor, vec![id.clone()])),
            Some((lowest, ids)) => match factor.checked_cmp(*lowest)? {
                std::cmp::Ordering::Less => {
                    *lowest = *factor;
                    *ids = vec![id.clone()];
                }
                std::cmp::Ordering::Equal => ids.push(id.clone()),
                std::cmp::Ordering::Greater => {}
            },
        }
    }
    Ok(best.map(|(_, ids)| ids))
}

/// Smallest supplied draw rank wins; equal ranks fall back to the smaller
/// `challenge_id` in byte order (§6.3).
fn resolve_tie(
    tied: &[String],
    draw_ranks: &BTreeMap<String, DrawRank>,
) -> Result<String, ChallengeError> {
    let mut best: Option<(&String, DrawRank)> = None;
    for id in tied {
        let rank = *draw_ranks
            .get(id)
            .ok_or_else(|| ChallengeError::MissingDrawRank {
                challenge_id: id.clone(),
            })?;
        best = match best {
            None => Some((id, rank)),
            Some((best_id, best_rank)) => {
                if rank < best_rank || (rank == best_rank && id.as_bytes() < best_id.as_bytes()) {
                    Some((id, rank))
                } else {
                    Some((best_id, best_rank))
                }
            }
        };
    }
    best.map(|(id, _)| id.clone())
        .ok_or_else(|| ChallengeError::MissingDrawRank {
            challenge_id: String::new(),
        })
}
