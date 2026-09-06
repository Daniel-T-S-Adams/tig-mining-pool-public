//! `fixtures/decision-engine/v1/hyperparameter-source.json` and
//! `bundle-sizing.json` (`mining_system.md` §6.6, §6.7).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pool_decision::{SourceExcluded, select_source, size_bundles};
use support::{bundle_input, cases, expected_ids, expected_str, source_input};

#[test]
fn every_hyperparameter_source_case_holds() {
    for case in cases(include_str!(
        "../../../fixtures/decision-engine/v1/hyperparameter-source.json"
    )) {
        let input = source_input(&case.input);
        let selection = select_source(&input);

        match expected_str(&case.expected, "source_benchmark") {
            Some(expected) => assert_eq!(
                selection.source_benchmark.as_deref(),
                Some(expected.as_str()),
                "{}: source benchmark",
                case.name
            ),
            None => assert_eq!(
                selection.source_benchmark, None,
                "{}: expected no source, which makes the challenge ineligible",
                case.name
            ),
        }

        // §6.6 step 3 copies the source's settings verbatim. A hyperparameter
        // that arrives as a number leaves as a number; the fixture's expected
        // object is compared at its own types for exactly that reason.
        if let Some(expected) = case.expected.get("hyperparameters") {
            let actual: serde_json::Value = selection
                .hyperparameters
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<serde_json::Map<_, _>>()
                .into();
            assert_eq!(&actual, expected, "{}: hyperparameters", case.name);
        }
        if let Some(expected) = case.expected.get("fuel_budget").and_then(|v| v.as_u64()) {
            assert_eq!(
                selection.fuel_budget,
                Some(expected),
                "{}: fuel budget",
                case.name
            );
        }

        if let Some(expected) = case
            .expected
            .get("highest_quality_bundle")
            .and_then(|v| v.as_object())
        {
            assert_eq!(
                selection.highest_quality,
                expected.get("quality").and_then(|q| q.as_i64()),
                "{}: highest quality bundle",
                case.name
            );
            assert_eq!(
                selection.source_benchmark.as_deref(),
                expected.get("benchmark_id").and_then(|b| b.as_str()),
                "{}: the benchmark holding the highest-quality bundle",
                case.name
            );
        }
        if let Some(expected) = case.expected.get("tied_quality").and_then(|q| q.as_i64()) {
            assert_eq!(
                selection.highest_quality,
                Some(expected),
                "{}: tied quality",
                case.name
            );
        }
        match expected_ids(&case.expected, "tie_candidates") {
            Some(expected) => assert_eq!(
                selection.tie_candidates.as_deref(),
                Some(expected.as_slice()),
                "{}: quality tie",
                case.name
            ),
            None => assert_eq!(
                selection.tie_candidates, None,
                "{}: expected no quality tie",
                case.name
            ),
        }

        if let Some(excluded) = case.expected.get("excluded").and_then(|v| v.as_object()) {
            for (id, reason) in excluded {
                let actual = selection
                    .excluded
                    .get(id)
                    .unwrap_or_else(|| panic!("{}: {id} was not excluded", case.name));
                let reason = reason.as_str().unwrap_or_default();
                let matches = match actual {
                    SourceExcluded::NotVerified => reason.contains("not verified"),
                    SourceExcluded::Fraudulent => reason.contains("fraudulent"),
                    SourceExcluded::DifferentAlgorithm { algorithm_id } => {
                        reason.contains("algorithm") && reason.contains(algorithm_id.as_str())
                    }
                    SourceExcluded::DifferentTrack { track_id } => {
                        reason.contains("track") && reason.contains(track_id.as_str())
                    }
                    SourceExcluded::NoActiveBundles => reason.contains("bundle"),
                };
                assert!(
                    matches,
                    "{}: {id} excluded as {actual:?} but the case says {reason:?}",
                    case.name
                );
            }
            assert_eq!(
                selection.excluded.len(),
                excluded.len(),
                "{}: exclusion count",
                case.name
            );
        }
    }
}

#[test]
fn every_bundle_sizing_case_holds() {
    for case in cases(include_str!(
        "../../../fixtures/decision-engine/v1/bundle-sizing.json"
    )) {
        let input = bundle_input(&case.input);
        let sizing = size_bundles(&input)
            .unwrap_or_else(|e| panic!("{}: size_bundles failed: {e}", case.name));

        let expected = case.expected["num_bundles"]
            .as_object()
            .expect("num_bundles");
        for (track, count) in expected {
            assert_eq!(
                sizing.num_bundles.get(track).copied(),
                count.as_u64(),
                "{}: num_bundles[{track}]",
                case.name
            );
        }
        assert_eq!(
            sizing.num_bundles.len(),
            expected.len(),
            "{}: every track is sized",
            case.name
        );

        if let Some(derivation) = case.expected.get("derivation").and_then(|v| v.as_object()) {
            for (track, expected) in derivation {
                let actual = sizing
                    .derivation
                    .get(track)
                    .unwrap_or_else(|| panic!("{}: no derivation for {track}", case.name));
                assert_eq!(
                    Some(actual.gcd_cores_nonces),
                    expected["gcd_c_n"].as_u64(),
                    "{}: gcd for {track}",
                    case.name
                );
                assert_eq!(
                    Some(actual.bundle_multiple),
                    expected["bundle_multiple"].as_u64(),
                    "{}: bundle multiple for {track}",
                    case.name
                );
            }
        }

        // The alignment property the formula exists to guarantee, checked
        // directly rather than only through the expected count.
        if let pool_decision::OfferedCompute::Cpu { cores } = input.offered_compute {
            for (track, count) in &sizing.num_bundles {
                let sizing_input = &input.tracks[track];
                if !sizing_input.selected_algorithm_best_on_track {
                    continue;
                }
                assert_eq!(
                    (count * sizing_input.num_nonces_per_bundle) % cores,
                    0,
                    "{}: ({count} * {}) mod {cores} must be 0 for {track}",
                    case.name,
                    sizing_input.num_nonces_per_bundle
                );
                assert!(
                    *count >= input.min_num_bundles,
                    "{}: {track} sized below the protocol minimum",
                    case.name
                );
            }
        }
    }
}
