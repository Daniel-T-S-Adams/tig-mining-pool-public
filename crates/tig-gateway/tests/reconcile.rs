//! Slice-1 criterion E4: precommit reconciliation matches on the full §10
//! tuple and stops for operator resolution when more than one candidate
//! matches.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use serde_json::json;

/// Changes one field of a precommit record, for the per-field match test.
type MutateField = Box<dyn Fn(&mut serde_json::Value)>;
use tig_gateway::reconcile::{
    PrecommitSubmission, ReconcileError, Reconciliation, TrackSettings, reconcile_precommit,
};

/// The shape `get-benchmarks` returns, taken from `fixtures/tig/v1`. The
/// `track_id` is TIG's selection, not the pool's — §6.1 submits an empty one.
fn precommit_on(benchmark_id: &str, track_id: &str, confirmed: Option<u64>) -> serde_json::Value {
    json!({
        "benchmark_id": benchmark_id,
        "settings": {
            "player_id": "0xpool",
            "block_id": "block_100020",
            "challenge_id": "c001",
            "algorithm_id": "a011",
            "track_id": track_id
        },
        "details": {
            "block_started": 100020,
            "num_nonces": 120,
            "num_bundles": 3,
            "rand_hash": "rh_0001",
            "fee_paid": "130000000000000000",
            "fuel_budget": 1500000,
            "hyperparameters": { "noise": "0.15", "restart_period": 250 },
            "compute_type": "aws_c7g"
        },
        "state": { "block_confirmed": confirmed }
    })
}

/// The common case: TIG selected t001.
fn precommit(benchmark_id: &str, confirmed: Option<u64>) -> serde_json::Value {
    precommit_on(benchmark_id, "t001", confirmed)
}

fn track(num_bundles: u64, fuel_budget: u64) -> TrackSettings {
    TrackSettings {
        num_bundles,
        fuel_budget,
        // Values, not text: the pool chose a float and an integer, and
        // transmits them at those types. The fixture record returns noise as
        // a string and restart_period as a number, so the match crosses the
        // type boundary in both directions.
        hyperparameters: BTreeMap::from([
            ("noise".to_string(), json!(0.15)),
            ("restart_period".to_string(), json!(250)),
        ]),
    }
}

/// What the pool submitted: every live active track of the challenge, with
/// the settings offered for each. TIG picks one of them.
fn expected() -> PrecommitSubmission {
    PrecommitSubmission {
        player_id: "0xpool".to_string(),
        block_id: "block_100020".to_string(),
        challenge_id: "c001".to_string(),
        algorithm_id: "a011".to_string(),
        compute_type: "aws_c7g".to_string(),
        track_settings: BTreeMap::from([
            ("t001".to_string(), track(3, 1_500_000)),
            ("t002".to_string(), track(5, 2_000_000)),
        ]),
    }
}

#[test]
fn exactly_one_confirmed_candidate_is_the_pools_precommit() {
    let found = reconcile_precommit(&[precommit("bench_a", Some(100_021))], &expected()).unwrap();
    assert_eq!(
        found,
        Reconciliation::Confirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
}

#[test]
fn two_candidates_stop_for_operator_resolution() {
    // E4's named case. §10: "stops for operator resolution if more than one
    // candidate matches". Not "pick the confirmed one" and not "pick the
    // newest" — if two precommits match the exact tuple the pool chose, the
    // pool cannot tell which is its own, and guessing attributes a
    // benchmark, its fees and its rewards to the wrong one.
    let found = reconcile_precommit(
        &[
            precommit("bench_a", Some(100_021)),
            precommit("bench_b", Some(100_022)),
        ],
        &expected(),
    )
    .unwrap();
    assert_eq!(
        found,
        Reconciliation::StopForOperator {
            candidates: vec!["bench_a".to_string(), "bench_b".to_string()]
        }
    );
}

#[test]
fn a_confirmed_and_an_unconfirmed_candidate_also_stop() {
    // The likelier real duplicate: one confirmed, one still pending.
    // Counting only confirmed matches would hide exactly the case worth
    // stopping for.
    let found = reconcile_precommit(
        &[
            precommit("bench_a", Some(100_021)),
            precommit("bench_b", None),
        ],
        &expected(),
    )
    .unwrap();
    assert!(matches!(found, Reconciliation::StopForOperator { .. }));
}

#[test]
fn a_single_unconfirmed_candidate_is_not_absence() {
    // The distinction that keeps §10's "never blindly resubmits" true.
    // Collapsing this into NoCandidate makes the pool's own unconfirmed
    // precommit look like a write that never arrived.
    let found = reconcile_precommit(&[precommit("bench_a", None)], &expected()).unwrap();
    assert_eq!(found, Reconciliation::PendingConfirmation);
}

#[test]
fn nothing_matching_is_no_candidate() {
    assert_eq!(
        reconcile_precommit(&[], &expected()).unwrap(),
        Reconciliation::NoCandidate
    );
}

#[test]
fn every_field_of_the_tuple_is_matched() {
    // §10 says "exact". Each field is changed on its own, so a matcher that
    // ignored any one of them fails here rather than in production, where
    // the symptom is a benchmark attributed to the wrong precommit.
    let mutations: Vec<(&str, MutateField)> = vec![
        (
            "player_id",
            Box::new(|v: &mut serde_json::Value| v["settings"]["player_id"] = json!("0xother")),
        ),
        (
            "block_id",
            Box::new(|v: &mut serde_json::Value| v["settings"]["block_id"] = json!("block_999")),
        ),
        (
            "challenge_id",
            Box::new(|v: &mut serde_json::Value| v["settings"]["challenge_id"] = json!("c003")),
        ),
        (
            "algorithm_id",
            Box::new(|v: &mut serde_json::Value| v["settings"]["algorithm_id"] = json!("a999")),
        ),
        (
            "track_id outside the submitted set",
            Box::new(|v: &mut serde_json::Value| v["settings"]["track_id"] = json!("t999")),
        ),
        (
            "compute_type",
            Box::new(|v: &mut serde_json::Value| v["details"]["compute_type"] = json!("gpu_x")),
        ),
        (
            "num_bundles",
            Box::new(|v: &mut serde_json::Value| v["details"]["num_bundles"] = json!(4)),
        ),
        (
            "fuel_budget",
            Box::new(|v: &mut serde_json::Value| v["details"]["fuel_budget"] = json!(1_500_001)),
        ),
        (
            "hyperparameters",
            Box::new(|v: &mut serde_json::Value| {
                v["details"]["hyperparameters"]["noise"] = json!("0.16")
            }),
        ),
    ];

    for (field, mutate) in mutations {
        let mut record = precommit("bench_a", Some(100_021));
        mutate(&mut record);
        assert_eq!(
            reconcile_precommit(&[record], &expected()).unwrap(),
            Reconciliation::NoCandidate,
            "a differing {field} must not match"
        );
    }
}

#[test]
fn tig_generated_fields_are_not_part_of_the_match() {
    // rand_hash, the fee actually charged, block_started and num_nonces are
    // TIG's or derived (§14: num_nonces = num_bundles * num_nonces_per_bundle);
    // matching on a value the pool did not choose cannot identify the pool's
    // own write, and would make a correct candidate miss.
    let mut record = precommit("bench_a", Some(100_021));
    record["details"]["rand_hash"] = json!("something-else");
    record["details"]["fee_paid"] = json!("999");
    record["details"]["block_started"] = json!(999_999);
    record["details"]["num_nonces"] = json!(999);
    assert_eq!(
        reconcile_precommit(&[record], &expected()).unwrap(),
        Reconciliation::Confirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
}

#[test]
fn a_hyperparameter_the_pool_chose_as_text_still_matches() {
    // The normalising runs on the pool's side too, not only TIG's.
    let mut expected = expected();
    for settings in expected.track_settings.values_mut() {
        settings
            .hyperparameters
            .insert("restart_period".to_string(), json!("250"));
    }
    assert_eq!(
        reconcile_precommit(&[precommit("bench_a", Some(100_021))], &expected).unwrap(),
        Reconciliation::Confirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
}

#[test]
fn a_hyperparameter_written_as_a_number_matches_the_same_value() {
    // TIG returns these as strings and the pool selects them as values; a
    // type difference is not a different hyperparameter.
    let mut record = precommit("bench_a", Some(100_021));
    record["details"]["hyperparameters"]["restart_period"] = json!(250);
    assert_eq!(
        reconcile_precommit(&[record], &expected()).unwrap(),
        Reconciliation::Confirmed {
            benchmark_id: "bench_a".to_string()
        }
    );
}

#[test]
fn an_unreadable_record_is_fatal_rather_than_skipped() {
    // A record that cannot be parsed might be the pool's own. Skipping it
    // could turn a match into NoCandidate and license the resend §10
    // forbids — the fail-open shape, in the one place it licenses a
    // duplicate write.
    let mut broken = precommit("bench_a", Some(100_021));
    broken["settings"]
        .as_object_mut()
        .unwrap()
        .remove("track_id");

    match reconcile_precommit(&[precommit("bench_b", None), broken], &expected()) {
        Err(ReconcileError::Shape { index, reason }) => {
            assert_eq!(index, 1);
            assert!(reason.contains("track_id"), "got: {reason}");
        }
        other => panic!("expected Shape, got {other:?}"),
    }
}

#[test]
fn a_record_without_a_benchmark_id_is_refused() {
    let mut broken = precommit("bench_a", Some(100_021));
    broken.as_object_mut().unwrap().remove("benchmark_id");
    assert!(reconcile_precommit(&[broken], &expected()).is_err());
}

#[test]
fn tig_may_select_any_track_the_pool_submitted() {
    // §6.1: the pool submits track_settings for every live active track and
    // TIG selects one. A confirmed precommit on the second track is the
    // pool's own — reporting NoCandidate here is an accepted write reported
    // as absent, the precondition for a blind resubmission and a second fee.
    let mut on_t002 = precommit_on("bench_b", "t002", Some(100_021));
    on_t002["details"]["num_bundles"] = json!(5);
    on_t002["details"]["fuel_budget"] = json!(2_000_000);

    assert_eq!(
        reconcile_precommit(&[on_t002], &expected()).unwrap(),
        Reconciliation::Confirmed {
            benchmark_id: "bench_b".to_string()
        }
    );
}

#[test]
fn duplicates_on_different_tracks_still_stop_for_operator() {
    // The case a per-track search would hide: two precommits the pool cannot
    // tell apart, each landing on a different TIG-selected track. Searching
    // one track at a time would return exactly one candidate per call and
    // never surface the duplicate.
    let mut on_t002 = precommit_on("bench_b", "t002", Some(100_022));
    on_t002["details"]["num_bundles"] = json!(5);
    on_t002["details"]["fuel_budget"] = json!(2_000_000);

    let found =
        reconcile_precommit(&[precommit("bench_a", Some(100_021)), on_t002], &expected()).unwrap();
    assert_eq!(
        found,
        Reconciliation::StopForOperator {
            candidates: vec!["bench_a".to_string(), "bench_b".to_string()]
        }
    );
}

#[test]
fn settings_confirmed_for_the_selected_track_must_be_the_ones_submitted() {
    // TIG selects the track; it does not invent the settings. A record whose
    // confirmed settings are not what the pool offered for that track is
    // somebody else's precommit.
    let mut wrong_bundles = precommit("bench_a", Some(100_021));
    wrong_bundles["details"]["num_bundles"] = json!(4);
    assert_eq!(
        reconcile_precommit(&[wrong_bundles], &expected()).unwrap(),
        Reconciliation::NoCandidate
    );

    // And the settings of a DIFFERENT submitted track do not qualify either.
    let mut t001_with_t002_settings = precommit("bench_a", Some(100_021));
    t001_with_t002_settings["details"]["num_bundles"] = json!(5);
    t001_with_t002_settings["details"]["fuel_budget"] = json!(2_000_000);
    assert_eq!(
        reconcile_precommit(&[t001_with_t002_settings], &expected()).unwrap(),
        Reconciliation::NoCandidate
    );
}
