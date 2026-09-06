//! `fixtures/decision-engine/v1/challenge-selection.json` and
//! `projected-qualifiers.json`, run case by case (`mining_system.md` §6.2,
//! §6.3, §6.6's eligibility effect).
//!
//! The fixtures are constructed, not captured: every expected value was
//! derived by hand from the cited section independently of any
//! implementation. Running them here is what makes them a test of the rules
//! rather than a record of what the code happens to do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pool_decision::{Excluded, Ratio, select_challenge};
use support::{
    Case, cases, challenge_input, expected_ids, expected_rates, expected_ratios, expected_str,
};

#[test]
fn every_challenge_selection_case_holds() {
    for case in cases(include_str!(
        "../../../fixtures/decision-engine/v1/challenge-selection.json"
    )) {
        run(&case);
    }
}

#[test]
fn every_projected_qualifier_case_holds() {
    for case in cases(include_str!(
        "../../../fixtures/decision-engine/v1/projected-qualifiers.json"
    )) {
        run(&case);
    }
}

/// Named because `docs/plans/slice-1-gateway.md` D2d cites this case: a tie is
/// resolved by the supplied rank, not by anything the engine chose.
#[test]
fn two_way_tie_resolved_by_supplied_draw_ranks() {
    run(&named(
        "challenge-selection",
        "two_way_tie_resolved_by_supplied_draw_ranks",
    ));
}

/// D2d: two challenges both at factor zero still tie, and still draw.
#[test]
fn all_zero_counts_tie_at_factor_zero() {
    run(&named(
        "challenge-selection",
        "all_zero_counts_tie_at_factor_zero",
    ));
}

/// D2d: the projection can create a tie that the raw factors did not have.
#[test]
fn projected_tie_resolved_by_supplied_draw_ranks() {
    run(&named(
        "projected-qualifiers",
        "projected_tie_resolved_by_supplied_draw_ranks",
    ));
}

/// `architecture.md` §3: the engine derives no randomness. A tie it cannot
/// resolve from supplied ranks is an error, never a fallback ordering — a
/// silent default would be the engine choosing, which is the one thing §6.3's
/// seed design exists to prevent.
#[test]
fn a_tie_without_a_supplied_rank_is_refused() {
    let mut case = named(
        "challenge-selection",
        "two_way_tie_resolved_by_supplied_draw_ranks",
    );
    let mut input = challenge_input(&case.input);
    input.draw_ranks.remove("c102");
    let error = select_challenge(&input).expect_err("a tie needs its ranks");
    assert!(matches!(
        error,
        pool_decision::ChallengeError::MissingDrawRank { ref challenge_id } if challenge_id == "c102"
    ));

    // With the rank present the same input selects, so the refusal is about
    // the missing rank and not about the case.
    case.input["supplied_randomness"]["challenge_tie_draw_ranks"]["c102"] = serde_json::json!(17);
    let selection = select_challenge(&challenge_input(&case.input)).unwrap();
    assert_eq!(selection.selected.as_deref(), Some("c102"));
}

/// §6.3's last clause: "If two tied challenges produce equal ranks, the
/// smaller `challenge_id` in byte order wins." No fixture case produces equal
/// ranks — BLAKE3 will not oblige — so the collision fallback is only ever
/// exercised here, and without it a collision would resolve by whichever
/// challenge the map happened to yield first.
#[test]
fn colliding_ranks_fall_back_to_the_smaller_challenge_id() {
    let case = named(
        "challenge-selection",
        "two_way_tie_resolved_by_supplied_draw_ranks",
    );
    let mut input = challenge_input(&case.input);
    let collision = pool_domain::DrawRank::from_bytes([7u8; 32]);
    input.draw_ranks.insert("c101".to_string(), collision);
    input.draw_ranks.insert("c102".to_string(), collision);

    let selection = select_challenge(&input).unwrap();
    assert_eq!(
        selection.tie_candidates.as_deref(),
        Some(["c101".to_string(), "c102".to_string()].as_slice()),
        "the same two challenges still tie on factor"
    );
    assert_eq!(
        selection.selected.as_deref(),
        Some("c101"),
        "c101 < c102 in byte order"
    );
}

fn named(family: &str, name: &str) -> Case {
    let raw = match family {
        "challenge-selection" => {
            include_str!("../../../fixtures/decision-engine/v1/challenge-selection.json")
        }
        "projected-qualifiers" => {
            include_str!("../../../fixtures/decision-engine/v1/projected-qualifiers.json")
        }
        other => panic!("unknown fixture family {other}"),
    };
    cases(raw)
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("fixture case {name} is missing from {family}"))
}

fn run(case: &Case) {
    let input = challenge_input(&case.input);
    let selection = select_challenge(&input)
        .unwrap_or_else(|e| panic!("{}: select_challenge failed: {e}", case.name));

    if let Some(expected) = expected_str(&case.expected, "selected_challenge") {
        assert_eq!(
            selection.selected.as_deref(),
            Some(expected.as_str()),
            "{}: selected challenge",
            case.name
        );
    } else {
        assert_eq!(
            selection.selected, None,
            "{}: expected no selection",
            case.name
        );
    }

    for (id, expected) in expected_ratios(&case.expected, "projected_raw_factor") {
        let actual = selection
            .projected_raw_factor
            .get(&id)
            .unwrap_or_else(|| panic!("{}: no factor computed for {id}", case.name));
        assert_exact(
            &case.name,
            &format!("projected_raw_factor[{id}]"),
            *actual,
            expected,
        );
    }

    // The projection's intermediates. §6.3 defines the factor through them and
    // the decision record stores them, so a case that agrees on the final
    // selection while disagreeing on a rate is still a failure.
    for (algorithm, track, expected) in expected_rates(&case.expected, "qualifier_rate") {
        let actual = selection
            .qualifier_rate
            .get(&algorithm)
            .and_then(|by_track| by_track.get(&track))
            .copied();
        match (actual, expected) {
            (Some(Some(actual)), Some(expected)) => assert_exact(
                &case.name,
                &format!("qualifier_rate[{algorithm}][{track}]"),
                actual,
                expected,
            ),
            (Some(None), None) => {}
            (actual, expected) => panic!(
                "{}: qualifier_rate[{algorithm}][{track}] was {actual:?}, expected {expected:?}",
                case.name
            ),
        }
    }

    for (id, expected) in expected_ratios(&case.expected, "expected_q") {
        let actual = selection
            .expected_q
            .get(&id)
            .unwrap_or_else(|| panic!("{}: no expected_q for {id}", case.name));
        assert_exact(&case.name, &format!("expected_q[{id}]"), *actual, expected);
    }

    for (id, expected) in expected_ratios(&case.expected, "expected_addition") {
        let actual = selection
            .expected_addition
            .get(&id)
            .unwrap_or_else(|| panic!("{}: no expected_addition for {id}", case.name));
        assert_exact(
            &case.name,
            &format!("expected_addition[{id}]"),
            *actual,
            expected,
        );
    }

    match expected_ids(&case.expected, "tie_candidates") {
        Some(expected) => assert_eq!(
            selection.tie_candidates.as_deref(),
            Some(expected.as_slice()),
            "{}: tie candidates",
            case.name
        ),
        None => assert_eq!(
            selection.tie_candidates, None,
            "{}: expected no tie",
            case.name
        ),
    }

    if let Some(expected) = expected_ids(&case.expected, "considered_challenges") {
        assert_eq!(
            selection.considered, expected,
            "{}: considered challenges",
            case.name
        );
    }

    // The `excluded` map in a case names why each challenge was dropped. The
    // reason strings are prose, so the assertion is on the id set and on the
    // kind of exclusion the prose cites.
    // A case's `excluded` map names challenges in the selection family and
    // in-flight benchmarks in the projection family. The reasons are prose, so
    // the assertion is on the id and on the kind of exclusion the prose cites.
    if let Some(excluded) = case.expected.get("excluded").and_then(|v| v.as_object()) {
        for (id, reason) in excluded {
            let reason = reason.as_str().unwrap_or_default();
            match (
                selection.excluded.get(id),
                selection.projection_excluded.get(id),
            ) {
                (Some(Excluded::IncompatibleComputeType), _) => assert!(
                    reason.contains("incompatible"),
                    "{}: {id} excluded as incompatible but the case says {reason:?}",
                    case.name
                ),
                (Some(Excluded::TrackWithoutSource { track_id }), _) => assert!(
                    reason.contains("source") && reason.contains(track_id.as_str()),
                    "{}: {id} excluded for track {track_id} but the case says {reason:?}",
                    case.name
                ),
                (_, Some(_)) => assert!(
                    reason.contains("track unknown"),
                    "{}: {id} excluded from the projection but the case says {reason:?}",
                    case.name
                ),
                (None, None) => panic!("{}: {id} was not excluded", case.name),
            }
        }
        assert_eq!(
            selection.excluded.len() + selection.projection_excluded.len(),
            excluded.len(),
            "{}: exclusion count",
            case.name
        );
    }
}

fn assert_exact(case: &str, what: &str, actual: Ratio, expected: Ratio) {
    assert_eq!(
        actual.checked_cmp(expected).unwrap(),
        std::cmp::Ordering::Equal,
        "{case}: {what} was {}/{}, expected {}/{}",
        actual.numerator(),
        actual.denominator(),
        expected.numerator(),
        expected.denominator()
    );
}
