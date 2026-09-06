//! What the `fixtures/decision-engine/v1` cases do not contain.
//!
//! The fixtures are dense: every active track appears in every map. TIG's own
//! shapes are not — `num_qualifiers_by_track` omits a track nobody has
//! qualified on, and `algorithm_track_stats` omits a track nobody has run —
//! so these are the cases the constructed fixtures could not cover.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pool_decision::{
    Algorithm, AlgorithmSelectionInput, BundleSizingInput, Challenge, ChallengeSelectionInput,
    ComputeType, Excluded, OfferedCompute, TrackSizing, TrackStats, select_algorithm,
    select_challenge, size_bundles,
};

fn challenge(
    id: &str,
    tracks: &[&str],
    qualifiers: &[(&str, u128)],
    sources: &[&str],
) -> Challenge {
    Challenge {
        id: id.to_string(),
        compute_type: ComputeType::Cpu,
        active_tracks: tracks.iter().map(|t| t.to_string()).collect(),
        network_qualifiers_by_track: qualifiers
            .iter()
            .map(|(t, n)| (t.to_string(), *n))
            .collect(),
        tracks_with_source: sources.iter().map(|t| t.to_string()).collect(),
    }
}

fn input(
    challenges: Vec<Challenge>,
    pool: BTreeMap<String, BTreeMap<String, u128>>,
) -> ChallengeSelectionInput {
    ChallengeSelectionInput {
        offered_compute: OfferedCompute::Cpu { cores: 8 },
        challenges,
        pool_qualifiers_by_challenge_by_track: pool,
        confirmed_in_flight_benchmarks: Vec::new(),
        algorithm_track_stats: BTreeMap::new(),
        draw_ranks: BTreeMap::new(),
    }
}

#[test]
fn an_active_track_missing_from_the_qualifier_map_is_still_checked_for_a_source() {
    // The failure this prevents is the worst shape available: inferring the
    // active-track set from the sparse qualifier map drops t2 from §6.2's
    // "every active track" check, so c1 is declared eligible without a source
    // for t2 — and its factor is 0 under the zero-denominator branch, which
    // makes the challenge that should have been excluded the winner.
    let selection = select_challenge(&input(
        vec![
            challenge("c1", &["t1", "t2"], &[("t1", 0)], &["t1"]),
            challenge("c2", &["t3"], &[("t3", 20)], &["t3"]),
        ],
        BTreeMap::from([("c2".to_string(), BTreeMap::from([("t3".to_string(), 5)]))]),
    ))
    .unwrap();

    assert_eq!(
        selection.excluded.get("c1"),
        Some(&Excluded::TrackWithoutSource {
            track_id: "t2".to_string()
        }),
        "t2 is active and has no source, whatever the qualifier map says"
    );
    assert_eq!(selection.selected.as_deref(), Some("c2"));
}

#[test]
fn pool_qualifiers_on_a_track_the_network_map_omits_still_count() {
    // The pool can hold qualifiers on a track the network map omits — that is
    // what a sparse map means. Summing over the map's keys instead of the
    // active set drops those from the numerator only, understating the
    // pool's own share and making the challenge look emptier than it is.
    //
    // c1 over its active set is (2 + 3) / 10 = 1/2. Over the network map's
    // keys it would be 2/10 = 1/5, which is *lower* than c2's 3/10 — so the
    // wrong sum both misreports the factor and flips the selection.
    let selection = select_challenge(&input(
        vec![
            challenge("c1", &["t1", "t2"], &[("t1", 10)], &["t1", "t2"]),
            challenge("c2", &["t3"], &[("t3", 10)], &["t3"]),
        ],
        BTreeMap::from([
            (
                "c1".to_string(),
                BTreeMap::from([("t1".to_string(), 2), ("t2".to_string(), 3)]),
            ),
            ("c2".to_string(), BTreeMap::from([("t3".to_string(), 3)])),
        ]),
    ))
    .unwrap();

    let c1 = selection.projected_raw_factor["c1"];
    assert_eq!(
        (c1.numerator(), c1.denominator()),
        (1, 2),
        "c1 is (2 + 3) / 10, not 2 / 10"
    );
    assert_eq!(
        selection.selected.as_deref(),
        Some("c2"),
        "c2's 3/10 is lower than c1's 1/2"
    );
}

#[test]
fn a_zero_adoption_algorithm_can_still_deny_best_on_track() {
    // §6.5 compares "eligible algorithms" — §6.4 step 1's keep-set. Zero
    // adoption is step 2's filter on the challenge-wide *selection*. Reading
    // it into §6.5 would let a201 be best on t1 at 1/2 while a202, which is
    // genuinely faster at 3/4, was invisible — and §6.7 would then align a
    // bundle count on a false premise.
    let selection = select_algorithm(&AlgorithmSelectionInput {
        algorithms: vec![
            Algorithm {
                id: "a201".to_string(),
                banned: false,
                has_successful_binary: true,
                adoption: 600_000_000_000_000_000,
            },
            Algorithm {
                id: "a202".to_string(),
                banned: false,
                has_successful_binary: true,
                adoption: 0,
            },
        ],
        algorithm_track_stats: BTreeMap::from([
            (
                "a201".to_string(),
                BTreeMap::from([(
                    "t1".to_string(),
                    TrackStats {
                        qualifiers: 6,
                        active_bundles: 12,
                    },
                )]),
            ),
            (
                "a202".to_string(),
                BTreeMap::from([(
                    "t1".to_string(),
                    TrackStats {
                        qualifiers: 3,
                        active_bundles: 4,
                    },
                )]),
            ),
        ]),
        active_tracks: vec!["t1".to_string()],
    })
    .unwrap();

    assert_eq!(
        selection.selected.as_deref(),
        Some("a201"),
        "zero adoption is still excluded from the challenge-wide selection"
    );
    assert_eq!(
        selection.best_on_track["t1"].as_deref(),
        Some("a202"),
        "but it is an eligible algorithm for §6.5's comparison"
    );
    assert!(!selection.selected_is_best_on_track["t1"]);
}

#[test]
fn every_active_track_gets_a_best_on_track_answer() {
    // A track nobody has run has no stats at all. "No algorithm is best" is a
    // different answer from no answer: §6.7 needs a flag for every active
    // track, and a missing entry would leave the CPU branch unable to decide.
    let selection = select_algorithm(&AlgorithmSelectionInput {
        algorithms: vec![Algorithm {
            id: "a201".to_string(),
            banned: false,
            has_successful_binary: true,
            adoption: 600_000_000_000_000_000,
        }],
        algorithm_track_stats: BTreeMap::from([(
            "a201".to_string(),
            BTreeMap::from([(
                "t1".to_string(),
                TrackStats {
                    qualifiers: 6,
                    active_bundles: 12,
                },
            )]),
        )]),
        active_tracks: vec!["t1".to_string(), "t2".to_string()],
    })
    .unwrap();

    assert_eq!(selection.best_on_track["t1"].as_deref(), Some("a201"));
    assert_eq!(
        selection.best_on_track.get("t2"),
        Some(&None),
        "t2 is answered, and the answer is that nobody is best"
    );
    assert!(!selection.selected_is_best_on_track["t2"]);
}

#[test]
fn a_zero_minimum_sizes_to_zero_on_every_branch() {
    // §6.7's formula is ceil(m / bundle_multiple) * bundle_multiple. A guard
    // that floored it at one multiple would make the CPU-best branch return
    // `bundle_multiple` for m = 0 while the GPU and not-best branches
    // returned 0 — a silent divergence from the rule for the same input.
    let tracks = BTreeMap::from([(
        "t1".to_string(),
        TrackSizing {
            num_nonces_per_bundle: 25,
            selected_algorithm_best_on_track: true,
        },
    )]);

    let cpu = size_bundles(&BundleSizingInput {
        offered_compute: OfferedCompute::Cpu { cores: 8 },
        min_num_bundles: 0,
        tracks: tracks.clone(),
    })
    .unwrap();
    let gpu = size_bundles(&BundleSizingInput {
        offered_compute: OfferedCompute::Gpu,
        min_num_bundles: 0,
        tracks,
    })
    .unwrap();

    assert_eq!(cpu.num_bundles["t1"], 0);
    assert_eq!(gpu.num_bundles["t1"], 0);
    // The alignment derivation still holds: 8 / gcd(8, 25) = 8.
    assert_eq!(cpu.derivation["t1"].bundle_multiple, 8);
}
