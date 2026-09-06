//! §6.6 hyperparameter and fuel source selection.

use std::collections::BTreeMap;

/// An active benchmark that could serve as a settings source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBenchmark {
    pub benchmark_id: String,
    pub algorithm_id: String,
    pub track_id: String,
    pub verified: bool,
    pub fraudulent: bool,
    /// Read as the height at which the benchmark was confirmed; §6.6 step 4's
    /// "most recently confirmed" is the highest of these.
    pub block_confirmed: u64,
    pub average_quality_by_bundle: Vec<i64>,
    /// Copied verbatim to the precommit, at the types TIG published them.
    pub hyperparameters: BTreeMap<String, serde_json::Value>,
    pub fuel_budget: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceExcluded {
    NotVerified,
    Fraudulent,
    DifferentAlgorithm {
        algorithm_id: String,
    },
    DifferentTrack {
        track_id: String,
    },
    /// A benchmark with no bundles has no quality to compare.
    NoActiveBundles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSelectionInput {
    pub selected_algorithm: String,
    pub track: String,
    pub active_benchmarks: Vec<SourceBenchmark>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSelection {
    pub excluded: BTreeMap<String, SourceExcluded>,
    /// The winning bundle quality, and the benchmark holding it.
    pub highest_quality: Option<i64>,
    /// The benchmarks tied on that quality, when more than one was.
    pub tie_candidates: Option<Vec<String>>,
    /// `None` makes the challenge ineligible (§6.6, §6.2).
    pub source_benchmark: Option<String>,
    pub hyperparameters: BTreeMap<String, serde_json::Value>,
    pub fuel_budget: Option<u64>,
}

/// §6.6: the benchmark containing the highest-quality active bundle wins, with
/// most-recently-confirmed then lowest benchmark id breaking a tie.
///
/// Step 2 compares the *highest single bundle*, not the benchmark's average: a
/// benchmark whose best bundle is 50 beats one averaging 41 on bundles of 40
/// and 42, which is the fixture's `highest_single_bundle_beats_higher_average`
/// case.
pub fn select_source(input: &SourceSelectionInput) -> SourceSelection {
    let mut excluded = BTreeMap::new();
    let mut candidates: Vec<(&SourceBenchmark, i64)> = Vec::new();

    for benchmark in &input.active_benchmarks {
        // Step 1's filters all run before any quality is compared, so a
        // fraudulent benchmark with the best bundle in the set never wins.
        if benchmark.algorithm_id != input.selected_algorithm {
            excluded.insert(
                benchmark.benchmark_id.clone(),
                SourceExcluded::DifferentAlgorithm {
                    algorithm_id: benchmark.algorithm_id.clone(),
                },
            );
            continue;
        }
        if benchmark.track_id != input.track {
            excluded.insert(
                benchmark.benchmark_id.clone(),
                SourceExcluded::DifferentTrack {
                    track_id: benchmark.track_id.clone(),
                },
            );
            continue;
        }
        if benchmark.fraudulent {
            excluded.insert(benchmark.benchmark_id.clone(), SourceExcluded::Fraudulent);
            continue;
        }
        if !benchmark.verified {
            excluded.insert(benchmark.benchmark_id.clone(), SourceExcluded::NotVerified);
            continue;
        }
        let Some(best_bundle) = benchmark.average_quality_by_bundle.iter().copied().max() else {
            excluded.insert(
                benchmark.benchmark_id.clone(),
                SourceExcluded::NoActiveBundles,
            );
            continue;
        };
        candidates.push((benchmark, best_bundle));
    }

    let Some(highest) = candidates.iter().map(|(_, quality)| *quality).max() else {
        return SourceSelection {
            excluded,
            highest_quality: None,
            tie_candidates: None,
            source_benchmark: None,
            hyperparameters: BTreeMap::new(),
            fuel_budget: None,
        };
    };

    let mut tied: Vec<&SourceBenchmark> = candidates
        .iter()
        .filter(|(_, quality)| *quality == highest)
        .map(|(benchmark, _)| *benchmark)
        .collect();
    // Highest block_confirmed first, then lowest benchmark id.
    tied.sort_by(|a, b| {
        b.block_confirmed
            .cmp(&a.block_confirmed)
            .then_with(|| a.benchmark_id.cmp(&b.benchmark_id))
    });
    let tie_candidates = if tied.len() > 1 {
        let mut ids: Vec<String> = tied.iter().map(|b| b.benchmark_id.clone()).collect();
        ids.sort();
        Some(ids)
    } else {
        None
    };

    let winner = tied.first();
    SourceSelection {
        excluded,
        highest_quality: Some(highest),
        tie_candidates,
        source_benchmark: winner.map(|b| b.benchmark_id.clone()),
        hyperparameters: winner
            .map(|b| b.hyperparameters.clone())
            .unwrap_or_default(),
        fuel_budget: winner.map(|b| b.fuel_budget),
    }
}
