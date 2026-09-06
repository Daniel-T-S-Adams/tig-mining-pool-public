//! Reading `fixtures/decision-engine/v1` cases into engine inputs.
//!
//! The fixture files are the authority on the expected values; this module
//! only translates their reduced synthetic snapshots into the engine's types,
//! following the field mapping in the fixture set's README.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;

use pool_decision::{
    Algorithm, AlgorithmSelectionInput, BundleSizingInput, Challenge, ChallengeSelectionInput,
    ComputeType, InFlightBenchmark, OfferedCompute, Ratio, SourceBenchmark, SourceSelectionInput,
    TrackSizing, TrackStats,
};
use pool_domain::DrawRank;
use serde_json::Value;

pub struct Case {
    pub name: String,
    pub input: Value,
    pub expected: Value,
}

pub fn cases(raw: &str) -> Vec<Case> {
    let file: Value = serde_json::from_str(raw).expect("fixture parses");
    file["cases"]
        .as_array()
        .expect("cases array")
        .iter()
        .map(|case| Case {
            name: case["name"].as_str().expect("case name").to_string(),
            input: case["input"].clone(),
            expected: case["expected"].clone(),
        })
        .collect()
}

pub fn challenge_input(input: &Value) -> ChallengeSelectionInput {
    let sources = input["hyperparameter_sources_available"]
        .as_object()
        .cloned()
        .unwrap_or_default();

    let challenges: Vec<Challenge> = input["challenges"]
        .as_array()
        .expect("challenges")
        .iter()
        .map(|challenge| {
            let id = challenge["id"].as_str().expect("challenge id").to_string();
            Challenge {
                compute_type: compute_type(challenge["type"].as_str().expect("challenge type")),
                // The challenge's own configuration, which is the authority on
                // which tracks are active. The qualifier map is sparse.
                active_tracks: challenge["active_tracks"]
                    .as_object()
                    .expect("active_tracks")
                    .keys()
                    .cloned()
                    .collect(),
                network_qualifiers_by_track: counts(&challenge["network_qualifiers_by_track"]),
                tracks_with_source: sources
                    .get(&id)
                    .and_then(|v| v.as_array())
                    .map(|tracks| {
                        tracks
                            .iter()
                            .map(|t| t.as_str().expect("track id").to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                id,
            }
        })
        .collect();

    let pool_qualifiers_by_challenge_by_track = input["pool_qualifiers_by_challenge_by_track"]
        .as_object()
        .map(|by_challenge| {
            by_challenge
                .iter()
                .map(|(id, by_track)| (id.clone(), counts(by_track)))
                .collect()
        })
        .unwrap_or_default();

    let confirmed_in_flight_benchmarks = input["confirmed_in_flight_benchmarks"]
        .as_array()
        .map(|benchmarks| {
            benchmarks
                .iter()
                .map(|benchmark| InFlightBenchmark {
                    benchmark_id: benchmark["benchmark_id"]
                        .as_str()
                        .expect("benchmark id")
                        .to_string(),
                    challenge_id: benchmark["challenge_id"]
                        .as_str()
                        .expect("challenge id")
                        .to_string(),
                    algorithm_id: benchmark["algorithm_id"]
                        .as_str()
                        .expect("algorithm id")
                        .to_string(),
                    // `null` means TIG has not selected the track yet, which
                    // §6.3 excludes from the projection.
                    track_id: benchmark["track_id"].as_str().map(str::to_string),
                    num_bundles: u128::from(
                        benchmark["num_bundles"].as_u64().expect("num_bundles"),
                    ),
                })
                .collect()
        })
        .unwrap_or_default();

    ChallengeSelectionInput {
        offered_compute: offered_compute(&input["offered_compute"]),
        challenges,
        pool_qualifiers_by_challenge_by_track,
        confirmed_in_flight_benchmarks,
        algorithm_track_stats: track_stats(&input["algorithm_track_stats"]),
        draw_ranks: draw_ranks(&input["supplied_randomness"]),
    }
}

pub fn algorithm_input(input: &Value) -> AlgorithmSelectionInput {
    AlgorithmSelectionInput {
        algorithms: input["algorithms"]
            .as_array()
            .expect("algorithms")
            .iter()
            .map(|algorithm| Algorithm {
                id: algorithm["id"].as_str().expect("algorithm id").to_string(),
                banned: algorithm["banned"].as_bool().expect("banned"),
                has_successful_binary: algorithm["has_successful_binary"]
                    .as_bool()
                    .expect("has_successful_binary"),
                // An 18-decimal fixed-point integer string, compared as an
                // integer so no rounding enters the §6.4 comparison.
                adoption: algorithm["adoption"]
                    .as_str()
                    .expect("adoption")
                    .parse()
                    .expect("adoption is an unsigned integer"),
            })
            .collect(),
        algorithm_track_stats: track_stats(&input["algorithm_track_stats"]),
        // The algorithm-selection family carries no challenge, so the cases
        // that exercise §6.5 name their tracks through the stats alone.
        active_tracks: Vec::new(),
    }
}

pub fn source_input(input: &Value) -> SourceSelectionInput {
    SourceSelectionInput {
        selected_algorithm: input["selected_algorithm"]
            .as_str()
            .expect("selected_algorithm")
            .to_string(),
        track: input["track"].as_str().expect("track").to_string(),
        active_benchmarks: input["active_benchmarks"]
            .as_array()
            .expect("active_benchmarks")
            .iter()
            .map(|benchmark| SourceBenchmark {
                benchmark_id: benchmark["benchmark_id"]
                    .as_str()
                    .expect("benchmark_id")
                    .to_string(),
                algorithm_id: benchmark["algorithm_id"]
                    .as_str()
                    .expect("algorithm_id")
                    .to_string(),
                track_id: benchmark["track_id"]
                    .as_str()
                    .expect("track_id")
                    .to_string(),
                verified: benchmark["verified"].as_bool().expect("verified"),
                fraudulent: benchmark["fraudulent"].as_bool().expect("fraudulent"),
                block_confirmed: benchmark["block_confirmed"]
                    .as_u64()
                    .expect("block_confirmed"),
                average_quality_by_bundle: benchmark["average_quality_by_bundle"]
                    .as_array()
                    .expect("average_quality_by_bundle")
                    .iter()
                    .map(|q| q.as_i64().expect("quality"))
                    .collect(),
                hyperparameters: benchmark["hyperparameters"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                fuel_budget: benchmark["fuel_budget"].as_u64().expect("fuel_budget"),
            })
            .collect(),
    }
}

pub fn bundle_input(input: &Value) -> BundleSizingInput {
    BundleSizingInput {
        offered_compute: offered_compute(&input["offered_compute"]),
        min_num_bundles: input["min_num_bundles"].as_u64().expect("min_num_bundles"),
        tracks: input["tracks"]
            .as_object()
            .expect("tracks")
            .iter()
            .map(|(id, track)| {
                (
                    id.clone(),
                    TrackSizing {
                        num_nonces_per_bundle: track["num_nonces_per_bundle"]
                            .as_u64()
                            .expect("num_nonces_per_bundle"),
                        selected_algorithm_best_on_track: track["selected_algorithm_best_on_track"]
                            .as_bool()
                            .expect("selected_algorithm_best_on_track"),
                    },
                )
            })
            .collect(),
    }
}

pub fn expected_str(expected: &Value, field: &str) -> Option<String> {
    expected.get(field)?.as_str().map(str::to_string)
}

pub fn expected_ids(expected: &Value, field: &str) -> Option<Vec<String>> {
    expected.get(field)?.as_array().map(|ids| {
        ids.iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect()
    })
}

/// Fixture factors are exact fractions: `"7/42"`, or `"0"` for the zero branch.
/// The `decimal_approx` beside them is readability only and is never read.
pub fn expected_factors(expected: &Value, field: &str) -> Vec<(String, Ratio)> {
    expected
        .get(field)
        .and_then(|v| v.as_object())
        .map(|by_id| {
            by_id
                .iter()
                .map(|(id, value)| (id.clone(), fraction(value["fraction"].as_str().unwrap())))
                .collect()
        })
        .unwrap_or_default()
}

/// A map of expected exact values. Cases write a whole number as a bare
/// integer (`"expected_addition": {"c502": 0}`) and anything else as an exact
/// fraction, so both forms are accepted.
pub fn expected_ratios(expected: &Value, field: &str) -> Vec<(String, Ratio)> {
    expected
        .get(field)
        .and_then(|v| v.as_object())
        .map(|by_id| {
            by_id
                .iter()
                .map(|(id, value)| (id.clone(), ratio(value)))
                .collect()
        })
        .unwrap_or_default()
}

/// `qualifier_rate[a][t]`, where `null` means the rate is unavailable.
pub fn expected_rates(expected: &Value, field: &str) -> Vec<(String, String, Option<Ratio>)> {
    let mut rates = Vec::new();
    let Some(by_algorithm) = expected.get(field).and_then(|v| v.as_object()) else {
        return rates;
    };
    for (algorithm, by_track) in by_algorithm {
        let Some(by_track) = by_track.as_object() else {
            continue;
        };
        for (track, value) in by_track {
            let rate = if value.is_null() {
                None
            } else {
                Some(ratio(value))
            };
            rates.push((algorithm.clone(), track.clone(), rate));
        }
    }
    rates
}

pub fn ratio(value: &Value) -> Ratio {
    match value {
        Value::Object(map) => fraction(
            map.get("fraction")
                .and_then(|v| v.as_str())
                .expect("fraction"),
        ),
        Value::Number(number) => Ratio::whole(u128::from(number.as_u64().expect("whole number"))),
        other => panic!("unexpected exact value {other}"),
    }
}

pub fn fraction(text: &str) -> Ratio {
    match text.split_once('/') {
        Some((numerator, denominator)) => Ratio::new(
            numerator.trim().parse().expect("numerator"),
            denominator.trim().parse().expect("denominator"),
        )
        .expect("fixture fraction"),
        None => Ratio::whole(text.trim().parse().expect("whole number")),
    }
}

fn compute_type(text: &str) -> ComputeType {
    match text {
        "cpu" => ComputeType::Cpu,
        "gpu" => ComputeType::Gpu,
        other => panic!("unknown compute type {other}"),
    }
}

fn offered_compute(value: &Value) -> OfferedCompute {
    match value["type"].as_str().expect("offer type") {
        "cpu" => OfferedCompute::Cpu {
            cores: value["cpu_cores"].as_u64().expect("cpu_cores"),
        },
        "gpu" => OfferedCompute::Gpu,
        other => panic!("unknown offer type {other}"),
    }
}

fn counts(value: &Value) -> BTreeMap<String, u128> {
    value
        .as_object()
        .map(|by_key| {
            by_key
                .iter()
                .map(|(key, count)| (key.clone(), u128::from(count.as_u64().expect("count"))))
                .collect()
        })
        .unwrap_or_default()
}

fn track_stats(value: &Value) -> BTreeMap<String, BTreeMap<String, TrackStats>> {
    value
        .as_object()
        .map(|by_algorithm| {
            by_algorithm
                .iter()
                .map(|(algorithm, by_track)| {
                    let tracks = by_track
                        .as_object()
                        .expect("track stats")
                        .iter()
                        .map(|(track, stats)| {
                            (
                                track.clone(),
                                TrackStats {
                                    qualifiers: u128::from(
                                        stats["qualifiers"].as_u64().expect("qualifiers"),
                                    ),
                                    active_bundles: u128::from(
                                        stats["active_bundles"].as_u64().expect("active_bundles"),
                                    ),
                                },
                            )
                        })
                        .collect();
                    (algorithm.clone(), tracks)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The fixture supplies small integer ranks, which its README records as
/// standing in for derived 32-byte ranks *by order*. Widening them to 32-byte
/// big-endian preserves that order exactly, so the engine sees the production
/// type and the case still means what it says.
fn draw_ranks(value: &Value) -> BTreeMap<String, DrawRank> {
    value
        .get("challenge_tie_draw_ranks")
        .and_then(|v| v.as_object())
        .map(|ranks| {
            ranks
                .iter()
                .map(|(id, rank)| {
                    let mut bytes = [0u8; 32];
                    let value = rank.as_u64().expect("draw rank");
                    bytes[24..].copy_from_slice(&value.to_be_bytes());
                    (id.clone(), DrawRank::from_bytes(bytes))
                })
                .collect()
        })
        .unwrap_or_default()
}
