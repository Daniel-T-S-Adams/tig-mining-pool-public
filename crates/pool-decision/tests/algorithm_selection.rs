//! `fixtures/decision-engine/v1/algorithm-selection.json` (`mining_system.md`
//! §6.4 challenge-wide choice, §6.5 track-specific performance).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pool_decision::{AlgorithmExcluded, select_algorithm};
use support::{algorithm_input, cases, expected_ids, expected_rates, expected_str};

#[test]
fn every_algorithm_selection_case_holds() {
    for case in cases(include_str!(
        "../../../fixtures/decision-engine/v1/algorithm-selection.json"
    )) {
        let input = algorithm_input(&case.input);
        let selection = select_algorithm(&input)
            .unwrap_or_else(|e| panic!("{}: select_algorithm failed: {e}", case.name));

        match expected_str(&case.expected, "selected_algorithm") {
            Some(expected) => assert_eq!(
                selection.selected.as_deref(),
                Some(expected.as_str()),
                "{}: selected algorithm",
                case.name
            ),
            None => assert_eq!(
                selection.selected, None,
                "{}: expected no algorithm, which makes the challenge ineligible",
                case.name
            ),
        }

        if let Some(expected) = expected_ids(&case.expected, "candidates_after_filters") {
            assert_eq!(
                selection.candidates_after_filters, expected,
                "{}: candidates after §6.4 steps 1 and 2",
                case.name
            );
        }

        // `tie_candidates` names §6.4's adoption tie in the cases that have no
        // track rates, and §6.5's track-rate tie in the ones that do. They are
        // different ties and the engine reports them separately.
        let track_family = case.expected.get("track_qualifier_rate").is_some();
        match (expected_ids(&case.expected, "tie_candidates"), track_family) {
            (Some(expected), false) => assert_eq!(
                selection.tie_candidates.as_deref(),
                Some(expected.as_slice()),
                "{}: adoption tie",
                case.name
            ),
            (Some(expected), true) => {
                let tied: Vec<String> = selection
                    .track_rate_tie_candidates
                    .values()
                    .flatten()
                    .cloned()
                    .collect();
                assert_eq!(tied, expected, "{}: track-rate tie", case.name);
            }
            (None, _) => {
                assert_eq!(
                    selection.tie_candidates, None,
                    "{}: expected no adoption tie",
                    case.name
                );
                assert!(
                    selection.track_rate_tie_candidates.is_empty(),
                    "{}: expected no track-rate tie",
                    case.name
                );
            }
        }

        for (track, algorithm, expected) in expected_rates(&case.expected, "track_qualifier_rate") {
            // This family writes the rate map as track -> algorithm, the
            // transpose of the projection family's algorithm -> track.
            let actual = selection
                .track_qualifier_rate
                .get(&track)
                .and_then(|by_algorithm| by_algorithm.get(&algorithm))
                .copied();
            match (actual, expected) {
                (Some(Some(actual)), Some(expected)) => assert_eq!(
                    actual.checked_cmp(expected).unwrap(),
                    std::cmp::Ordering::Equal,
                    "{}: track_qualifier_rate[{track}][{algorithm}]",
                    case.name
                ),
                (Some(None), None) => {}
                (actual, expected) => panic!(
                    "{}: track_qualifier_rate[{track}][{algorithm}] was {actual:?}, expected {expected:?}",
                    case.name
                ),
            }
        }

        if let Some(best) = case
            .expected
            .get("best_on_track")
            .and_then(|v| v.as_object())
        {
            for (track, expected) in best {
                assert_eq!(
                    selection
                        .best_on_track
                        .get(track)
                        .and_then(|b| b.as_deref()),
                    expected.as_str(),
                    "{}: best_on_track[{track}]",
                    case.name
                );
            }
        }

        if let Some(is_best) = case
            .expected
            .get("selected_algorithm_is_best_on_track")
            .and_then(|v| v.as_object())
        {
            for (track, expected) in is_best {
                assert_eq!(
                    selection.selected_is_best_on_track.get(track).copied(),
                    expected.as_bool(),
                    "{}: selected_algorithm_is_best_on_track[{track}]",
                    case.name
                );
            }
        }

        if let Some(excluded) = case.expected.get("excluded").and_then(|v| v.as_object()) {
            for (id, reason) in excluded {
                let actual = selection
                    .excluded
                    .get(id)
                    .unwrap_or_else(|| panic!("{}: {id} was not excluded", case.name));
                let reason = reason.as_str().unwrap_or_default();
                let matches = match actual {
                    AlgorithmExcluded::Banned => reason.contains("banned"),
                    AlgorithmExcluded::NoSuccessfulBinary => reason.contains("binary"),
                    AlgorithmExcluded::ZeroAdoption => reason.contains("adoption is zero"),
                };
                assert!(
                    matches,
                    "{}: {id} excluded as {actual:?} but the case says {reason:?}",
                    case.name
                );
            }
        }
    }
}
