//! §6.4 challenge-wide algorithm choice and §6.5 track-specific performance.

use std::collections::BTreeMap;

use crate::challenge::TrackStats;
use crate::ratio::{Ratio, RatioError};

/// One candidate algorithm within the selected challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Algorithm {
    pub id: String,
    pub banned: bool,
    pub has_successful_binary: bool,
    /// Challenge-wide adoption as TIG publishes it: an unsigned 18-decimal
    /// fixed-point integer. Compared as an integer, so no rounding enters.
    pub adoption: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlgorithmExcluded {
    Banned,
    NoSuccessfulBinary,
    /// §6.4 step 2 ignores zero adoption. If that empties the candidate set
    /// the challenge is ineligible — §6.2 requires an eligible algorithm, and
    /// no step of §6.4 says what to select from nothing.
    ZeroAdoption,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlgorithmSelectionInput {
    pub algorithms: Vec<Algorithm>,
    /// `algorithm_track_stats[a][t]` for §6.5. Empty when the caller only
    /// needs the challenge-wide choice.
    pub algorithm_track_stats: BTreeMap<String, BTreeMap<String, TrackStats>>,
    /// The selected challenge's active tracks.
    ///
    /// §6.7 needs a best-on-track answer for *every* active track, not only
    /// for the tracks that happen to appear in `algorithm_track_stats`: a
    /// track nobody has run yet has no stats at all, and its answer is "no
    /// algorithm is best", which is a different thing from no answer.
    /// Defaults to the stats' own track set when empty, so a caller that only
    /// wants §6.4's choice need not supply it.
    pub active_tracks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlgorithmSelection {
    /// Survivors of §6.4 steps 1 and 2, in id order.
    pub candidates_after_filters: Vec<String>,
    pub excluded: BTreeMap<String, AlgorithmExcluded>,
    /// The algorithms that tied on highest adoption, when more than one did.
    pub tie_candidates: Option<Vec<String>>,
    /// `None` means the challenge is ineligible for want of an algorithm.
    pub selected: Option<String>,
    /// `track_qualifier_rate[t][a]`; `None` where the rate is unavailable.
    pub track_qualifier_rate: BTreeMap<String, BTreeMap<String, Option<Ratio>>>,
    /// The best algorithm on each track, or `None` where no algorithm has a
    /// usable rate there.
    pub best_on_track: BTreeMap<String, Option<String>>,
    /// Per track, the algorithms that tied on the greatest usable rate, when
    /// more than one did. §6.5's tie is a separate event from §6.4's and the
    /// decision record has to be able to tell them apart.
    pub track_rate_tie_candidates: BTreeMap<String, Vec<String>>,
    /// Whether the *selected* algorithm is best on each track. This is what
    /// §6.7 reads; §6.5 is explicit that it does not change the selection.
    pub selected_is_best_on_track: BTreeMap<String, bool>,
}

/// §6.4 then §6.5.
pub fn select_algorithm(input: &AlgorithmSelectionInput) -> Result<AlgorithmSelection, RatioError> {
    let mut excluded = BTreeMap::new();
    // §6.5 compares "eligible algorithms" — step 1's keep-set. Zero adoption
    // is step 2's filter on the challenge-wide *selection*, and reading it
    // into §6.5 would let a zero-adoption algorithm that is genuinely fastest
    // on a track fail to deny best-on-track, changing §6.7's bundle count.
    let mut step_one_survivors: Vec<&Algorithm> = Vec::new();
    let mut candidates: Vec<&Algorithm> = Vec::new();

    for algorithm in &input.algorithms {
        // Step 1's filters run before step 2's, which is what the fixture's
        // banned_and_missing_binary case pins: a banned algorithm with the
        // highest adoption is gone before adoption is looked at.
        if algorithm.banned {
            excluded.insert(algorithm.id.clone(), AlgorithmExcluded::Banned);
            continue;
        }
        if !algorithm.has_successful_binary {
            excluded.insert(algorithm.id.clone(), AlgorithmExcluded::NoSuccessfulBinary);
            continue;
        }
        step_one_survivors.push(algorithm);
        if algorithm.adoption == 0 {
            excluded.insert(algorithm.id.clone(), AlgorithmExcluded::ZeroAdoption);
            continue;
        }
        candidates.push(algorithm);
    }
    step_one_survivors.sort_by(|a, b| a.id.cmp(&b.id));
    candidates.sort_by(|a, b| a.id.cmp(&b.id));

    let highest = candidates.iter().map(|a| a.adoption).max();
    let tied: Vec<String> = highest
        .map(|highest| {
            candidates
                .iter()
                .filter(|a| a.adoption == highest)
                .map(|a| a.id.clone())
                .collect()
        })
        .unwrap_or_default();
    // Step 4 resolves equal adoption "deterministically by algorithm_id";
    // smallest wins, the direction the fixture set records as its convention.
    let selected = tied.first().cloned();
    let tie_candidates = if tied.len() > 1 { Some(tied) } else { None };

    let (track_qualifier_rate, best_on_track, track_rate_tie_candidates) = track_rates(
        &step_one_survivors,
        &input.algorithm_track_stats,
        &input.active_tracks,
    )?;
    let selected_is_best_on_track = best_on_track
        .iter()
        .map(|(track, best)| {
            let is_best =
                matches!((best, &selected), (Some(best), Some(selected)) if best == selected);
            (track.clone(), is_best)
        })
        .collect();

    Ok(AlgorithmSelection {
        candidates_after_filters: candidates.iter().map(|a| a.id.clone()).collect(),
        excluded,
        tie_candidates,
        selected,
        track_qualifier_rate,
        best_on_track,
        track_rate_tie_candidates,
        selected_is_best_on_track,
    })
}

type TrackRates = (
    BTreeMap<String, BTreeMap<String, Option<Ratio>>>,
    BTreeMap<String, Option<String>>,
    BTreeMap<String, Vec<String>>,
);

fn track_rates(
    candidates: &[&Algorithm],
    stats: &BTreeMap<String, BTreeMap<String, TrackStats>>,
    active_tracks: &[String],
) -> Result<TrackRates, RatioError> {
    let mut rates: BTreeMap<String, BTreeMap<String, Option<Ratio>>> = BTreeMap::new();
    // Every active track gets an entry, even one with no stats at all, so
    // §6.7 receives a flag for each rather than only for the tracks some
    // algorithm has run. An empty list means the caller wants only §6.4, so
    // the stats' own tracks stand in.
    for track in active_tracks {
        rates.entry(track.clone()).or_default();
    }
    for algorithm in candidates {
        let Some(by_track) = stats.get(&algorithm.id) else {
            continue;
        };
        for (track, track_stats) in by_track {
            if !active_tracks.is_empty() && !active_tracks.contains(track) {
                continue;
            }
            rates
                .entry(track.clone())
                .or_default()
                .insert(algorithm.id.clone(), track_stats.rate()?);
        }
    }

    let mut best_on_track = BTreeMap::new();
    let mut ties = BTreeMap::new();
    for (track, by_algorithm) in &rates {
        let mut best: Option<(&String, Ratio)> = None;
        for (algorithm_id, rate) in by_algorithm {
            // An algorithm with no active bundles has no usable rate and
            // cannot be best on the track, however the others compare.
            let Some(rate) = rate else { continue };
            best = match best {
                None => Some((algorithm_id, *rate)),
                // Equal rates resolve by algorithm_id; the map is ordered, so
                // the first one seen is already the smallest.
                Some((best_id, best_rate)) => {
                    if rate.checked_cmp(best_rate)? == std::cmp::Ordering::Greater {
                        Some((algorithm_id, *rate))
                    } else {
                        Some((best_id, best_rate))
                    }
                }
            };
        }
        if let Some((_, best_rate)) = best {
            let mut tied = Vec::new();
            for (algorithm_id, rate) in by_algorithm {
                let Some(rate) = rate else { continue };
                if rate.checked_cmp(best_rate)? == std::cmp::Ordering::Equal {
                    tied.push(algorithm_id.clone());
                }
            }
            if tied.len() > 1 {
                ties.insert(track.clone(), tied);
            }
        }
        best_on_track.insert(track.clone(), best.map(|(id, _)| id.clone()));
    }

    Ok((rates, best_on_track, ties))
}
