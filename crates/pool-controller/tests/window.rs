//! `tig_integration.md` §7's mapping, tested against the shapes it names.
//!
//! These are pure over constructed reads. That is the point: the decision
//! "does this read confirm anything" is what licenses every workflow
//! transition, and it should be testable against a record the test wrote
//! rather than only against a chain that happened to be in the right state.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_controller::window::{WindowError, confirmed_window};
use serde_json::{Value, json};

fn block(height: i64, verified: &[&str], active: &[&str]) -> Value {
    json!({
        "block": {
            "id": format!("block_{height}"),
            "details": { "height": height },
            "data": {
                "confirmed_ids": { "verified": verified },
                "active_ids": { "benchmark": active },
            }
        }
    })
}

fn precommit(id: &str, confirmed: Option<i64>) -> Value {
    json!({
        "benchmark_id": id,
        "settings": { "track_id": "t002", "challenge_id": "c001" },
        "details": { "block_started": 98, "num_nonces": 80 },
        "state": { "block_confirmed": confirmed },
    })
}

#[test]
fn only_a_non_null_block_confirmed_is_confirmation() {
    // The distinction the whole of §7 turns on. `get-benchmarks` lists the
    // pool's *submitted* precommits too, with a null `state.block_confirmed`,
    // so an implementation that took presence for confirmation would advance a
    // workflow the moment its write was accepted — which §7 lists explicitly
    // as not confirmation.
    let body = json!({
        "precommits": [precommit("bench_confirmed", Some(100)), precommit("bench_pending", None)],
        "benchmarks": [],
        "proofs": [],
        "frauds": [],
    });
    let window = confirmed_window(&body, &block(120, &[], &[])).unwrap();

    assert!(window.precommits.contains_key("bench_confirmed"));
    assert!(
        !window.precommits.contains_key("bench_pending"),
        "an entry with a null block_confirmed is not evidence"
    );
    assert_eq!(window.at_block, 120);
}

#[test]
fn the_confirmed_precommit_carries_what_the_guardrails_and_transitions_need() {
    // §7: "Confirmed settings/details replace proposed values", and §8's
    // deadlines are ages from `details.block_started`.
    let body = json!({
        "precommits": [precommit("bench_a", Some(100))],
        "benchmarks": [], "proofs": [], "frauds": [],
    });
    let window = confirmed_window(&body, &block(120, &[], &[])).unwrap();
    let p = &window.precommits["bench_a"];

    assert_eq!(p.block_confirmed, 100);
    assert_eq!(p.block_started, 98);
    // TIG selected the track; the pool proposed every live one and chose none.
    assert_eq!(p.track_id, "t002");
    // Settings arrive whole. The pool has no business deciding which of TIG's
    // values matter, and a field-by-field copy would silently drop whatever a
    // later TIG release added.
    assert_eq!(p.settings["challenge_id"], json!("c001"));
    // §6.2's commitment is built to exactly `details.num_nonces`, which is a
    // *detail* and so is not inside `settings`. Carried explicitly, and
    // asserted here because the field currently has no reader: a consumer
    // that later confirms from the window would find `None` and — treating
    // absence as permission, the way the first version of the commitment
    // path did — build a body against a length nobody checked.
    assert_eq!(p.num_nonces, Some(80));
    assert!(
        p.settings.get("num_nonces").is_none(),
        "settings and details are disjoint objects"
    );
}

#[test]
fn a_confirmed_precommit_with_no_start_block_is_a_shape_error() {
    // Every §8 guardrail is an age from `details.block_started`. A confirmed
    // precommit without one cannot be aged, so the pool could never expire
    // that workflow and §6.1's interval would never close. Refusing loudly
    // beats admitting a workflow nothing can time out.
    let mut entry = precommit("bench_a", Some(100));
    entry["details"]["block_started"] = Value::Null;
    let body = json!({ "precommits": [entry], "benchmarks": [], "proofs": [], "frauds": [] });

    let error = confirmed_window(&body, &block(120, &[], &[])).unwrap_err();
    assert!(
        matches!(
            error,
            WindowError::Shape {
                collection: "precommits",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_confirmed_benchmark_without_stopped_is_a_shape_error() {
    // §7: "If `details.stopped` is true, no proof is sent." Reading a missing
    // flag as false would have the pool build and submit a proof for a
    // benchmark TIG had stopped — and pay the fee for it.
    let body = json!({
        "precommits": [],
        "benchmarks": [{ "id": "bench_a", "details": {}, "state": { "block_confirmed": 110 } }],
        "proofs": [], "frauds": [],
    });
    let error = confirmed_window(&body, &block(120, &[], &[])).unwrap_err();
    assert!(
        matches!(
            error,
            WindowError::Shape {
                collection: "benchmarks",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn the_benchmark_collection_keys_on_id_and_the_others_on_benchmark_id() {
    // A real difference in TIG's shapes, and one that fails quietly: keying
    // the benchmark collection on `benchmark_id` yields an empty map rather
    // than an error, so every benchmark confirmation would simply never
    // arrive.
    let body = json!({
        "precommits": [],
        "benchmarks": [{ "id": "bench_a", "details": { "stopped": false },
                         "state": { "block_confirmed": 110 } }],
        "proofs": [{ "benchmark_id": "bench_a", "state": { "block_confirmed": 115 } }],
        "frauds": [{ "benchmark_id": "bench_b", "state": { "block_confirmed": 118 } }],
    });
    let window = confirmed_window(&body, &block(120, &[], &[])).unwrap();

    assert_eq!(window.benchmarks["bench_a"].block_confirmed, 110);
    assert!(!window.benchmarks["bench_a"].stopped);
    assert_eq!(window.proofs["bench_a"].block_confirmed, 115);
    assert_eq!(window.frauds["bench_b"].block_confirmed, 118);
}

#[test]
fn the_two_block_sets_come_from_the_block_and_are_optional() {
    // §7 reads verification from `block.data.confirmed_ids.verified` "when
    // published", and the active set from `block.data.active_ids.benchmark`.
    // The qualifier is the document's, so an absent set is an absent event
    // rather than an error — unlike a malformed collection entry, an absent
    // set says something true.
    let body = json!({ "precommits": [], "benchmarks": [], "proofs": [], "frauds": [] });

    let window = confirmed_window(&body, &block(120, &["bench_a"], &["bench_b"])).unwrap();
    assert_eq!(window.verified, vec!["bench_a".to_string()]);
    assert_eq!(window.active, vec!["bench_b".to_string()]);

    let bare = json!({ "block": { "details": { "height": 120 } } });
    let window = confirmed_window(&body, &bare).unwrap();
    assert!(window.verified.is_empty());
    assert!(window.active.is_empty());
    assert_eq!(window.at_block, 120);
}

#[test]
fn a_block_with_no_height_is_refused() {
    // Everything in the window is dated against it — §8's ages, §6.1's
    // interval close, the restart pass's report. A window with no height
    // would date them all against nothing.
    let body = json!({ "precommits": [], "benchmarks": [], "proofs": [], "frauds": [] });
    let error = confirmed_window(&body, &json!({ "block": { "details": {} } })).unwrap_err();
    assert!(matches!(error, WindowError::Block { .. }), "{error:?}");
}

#[test]
fn an_absent_collection_is_empty_and_not_an_error() {
    // A body that omits a collection entirely says the pool has none of that
    // kind, which is the ordinary state of `frauds`.
    let window = confirmed_window(&json!({}), &block(120, &[], &[])).unwrap();
    assert!(window.precommits.is_empty());
    assert!(window.frauds.is_empty());
}

#[test]
fn the_pinned_fixture_reads_as_an_empty_window() {
    // `fixtures/tig/v1` is a real recorded response for a player with no
    // benchmarks. It should produce a window with nothing in it and no error —
    // which is a weaker claim than it sounds, because it is the shape check:
    // a parser expecting different key names would also produce an empty
    // window, so this pairs with the keying test above rather than standing
    // alone.
    let benchmarks: Value =
        serde_json::from_str(include_str!("../../../fixtures/tig/v1/get-benchmarks.json")).unwrap();
    let block: Value =
        serde_json::from_str(include_str!("../../../fixtures/tig/v1/get-block.json")).unwrap();

    let window = confirmed_window(&benchmarks, &block).unwrap();
    assert!(window.precommits.is_empty());
    assert!(window.benchmarks.is_empty());
    assert!(window.proofs.is_empty());
    assert!(window.frauds.is_empty());
    assert!(window.verified.is_empty());
    assert!(
        window.at_block > 0,
        "the pinned block dates the window: {}",
        window.at_block
    );
}
