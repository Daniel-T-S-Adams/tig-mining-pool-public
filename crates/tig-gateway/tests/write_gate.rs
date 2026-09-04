//! Slice-1 criterion B3: losing `WRITE_READY` at runtime blocks new writes
//! and raises the `architecture.md` §10.3 alert, without corrupting
//! in-flight intents.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pool_domain::Network;
use tig_gateway::readiness::{
    ActiveChallengeRuntime, ApiKeyPlacement, Check, Evidence, FixtureOutcome, ModelValidation,
    OpenApiObservation, Pins, ResolvedImage, WriteReady, evaluate,
};
use tig_gateway::write_gate::{Revocation, WriteGate};

fn pins() -> Pins {
    Pins {
        network: Network::Testnet,
        upstream_commit: "ad08d1ea001a73ff5aab3b556d7f59246fece14e".to_string(),
        image_digests: BTreeMap::from([("img".to_string(), "sha256:aa".to_string())]),
        platform: "arm64".to_string(),
        openapi_sha256: "abc".to_string(),
        pool_player_id: "0xpool".to_string(),
    }
}

/// The only way to get a `WriteReady`: all nine checks passing.
fn ready() -> WriteReady {
    let pins = pins();
    let evidence = Evidence {
        config_network: Ok(Network::Testnet),
        upstream_commit: Ok(pins.upstream_commit.clone()),
        resolved_images: Ok(vec![ResolvedImage {
            reference: "img".to_string(),
            manifest_digest: "sha256:aa".to_string(),
            platform: "arm64".to_string(),
        }]),
        openapi: Ok(OpenApiObservation {
            hosted_sha256: Some("abc".to_string()),
            reviewed_local_override: None,
        }),
        response_models: Ok([
            "get-block",
            "get-challenges",
            "get-algorithms",
            "get-opow",
            "get-benchmarks",
        ]
        .iter()
        .map(|e| ModelValidation {
            endpoint: (*e).to_string(),
            valid: true,
            detail: String::new(),
        })
        .collect()),
        active_challenges: Ok(vec![ActiveChallengeRuntime {
            challenge_id: "c001".to_string(),
            runtime_pinned: true,
            compute_path_supported: true,
        }]),
        fixtures: Ok(FixtureOutcome {
            lossless_numeric_parsing: true,
            canonical_request_serialization: true,
            detail: String::new(),
        }),
        api_key: Ok(ApiKeyPlacement {
            present_in_gateway: true,
            readable_by_member_services: false,
        }),
        confirmed_pool_player_id: Ok(pins.pool_player_id.clone()),
    };
    evaluate(&pins, &evidence).expect("the evidence passes all nine checks")
}

fn auth_failure() -> Revocation {
    Revocation::Authentication {
        detail: "TIG returned 401".to_string(),
    }
}

#[test]
fn losing_write_ready_blocks_new_writes() {
    let gate = WriteGate::open(ready());
    assert!(gate.begin_write().is_ok());

    assert!(gate.revoke(auth_failure()), "this call lost WRITE_READY");
    assert!(!gate.is_ready());

    let blocked = gate.begin_write().expect_err("new writes must be refused");
    assert_eq!(blocked.revocation, auth_failure());
    assert!(
        format!("{blocked}").contains("authentication"),
        "the refusal should name the §10.3 category, got: {blocked}"
    );
}

#[test]
fn an_in_flight_write_survives_revocation() {
    // The heart of B3. Revocation is not cancellation: §7 advances a
    // workflow from confirmed evidence, so abandoning a request already sent
    // to TIG would not undo it — it would only lose track of an intent whose
    // outcome is now unknown.
    let gate = WriteGate::open(ready());
    let permit = gate.begin_write().expect("admitted while ready");

    gate.revoke(Revocation::Compatibility {
        failed: vec![Check::OpenApiChecksum],
        detail: "hosted checksum changed".to_string(),
    });

    // Still valid, still counted, and still carrying what it was admitted
    // under.
    assert_eq!(gate.in_flight(), 1, "the in-flight write is still tracked");
    assert_eq!(
        permit.admitted_under().upstream_commit(),
        pins().upstream_commit
    );

    // And no new one may start alongside it.
    assert!(gate.begin_write().is_err());

    drop(permit);
    assert_eq!(gate.in_flight(), 0, "releasing the permit clears the slot");
}

#[test]
fn the_permit_records_the_decision_it_was_admitted_under() {
    // Not read back off the gate: by the time an attempt is written down the
    // gate may have been revoked and restored against a different pin.
    let gate = WriteGate::open(ready());
    let permit = gate.begin_write().unwrap();
    gate.revoke(auth_failure());
    gate.restore(ready());

    assert_eq!(permit.admitted_under().network(), Network::Testnet);
    assert_eq!(
        permit.admitted_under().upstream_commit(),
        pins().upstream_commit
    );
}

#[test]
fn a_compatibility_refusal_names_which_checks_regressed() {
    // §13 makes a failed check an operator task — compare the pinned
    // upstream, update models and fixtures, re-run the spike. Which checks
    // failed is the part that says where to start, so it has to survive as
    // data rather than be flattened into the detail string.
    let gate = WriteGate::open(ready());
    gate.revoke(Revocation::Compatibility {
        failed: vec![Check::OpenApiChecksum, Check::PlayerIdentity],
        detail: "re-evaluation failed".to_string(),
    });

    let blocked = gate.begin_write().expect_err("writes are disabled");
    assert_eq!(
        blocked.revocation.failed_checks(),
        &[Check::OpenApiChecksum, Check::PlayerIdentity]
    );
    let message = format!("{blocked}");
    assert!(
        message.contains("4,9"),
        "the refusal should name the §13 check numbers, got: {message}"
    );
}

#[test]
fn an_authentication_refusal_names_no_checks() {
    // Nothing regressed, so there is nothing to name; the caller can print
    // the list unconditionally without inventing a check that failed.
    let gate = WriteGate::open(ready());
    gate.revoke(auth_failure());
    let blocked = gate.begin_write().expect_err("writes are disabled");
    assert!(blocked.revocation.failed_checks().is_empty());
    assert!(
        !format!("{blocked}").contains("§13 checks"),
        "got: {blocked}"
    );
}

#[test]
fn revoking_twice_keeps_the_first_cause() {
    // A failing credential produces one alert, not one per rejected
    // request, and the first cause is the one that explains the rest.
    let gate = WriteGate::open(ready());
    assert!(gate.revoke(auth_failure()));
    assert!(
        !gate.revoke(Revocation::Compatibility {
            failed: vec![Check::ResponseModels],
            detail: "later, and a consequence".to_string(),
        }),
        "a second revocation must report that it changed nothing"
    );
    assert_eq!(gate.revocation(), Some(auth_failure()));
}

#[test]
fn recovery_requires_passing_all_nine_checks_again() {
    // `restore` takes a WriteReady, which has no public constructor — a
    // credential that started working again says nothing about whether the
    // schema still matches, so recovery re-runs the whole gate rather than
    // clearing a flag.
    let gate = WriteGate::open(ready());
    gate.revoke(auth_failure());
    assert!(gate.begin_write().is_err());

    assert!(gate.restore(ready()));
    assert!(gate.is_ready());
    assert!(gate.revocation().is_none());
    assert!(gate.begin_write().is_ok());

    assert!(
        !gate.restore(ready()),
        "restoring an already-ready gate must report that it changed nothing"
    );
}

#[test]
fn in_flight_counts_only_live_permits() {
    let gate = WriteGate::open(ready());
    let a = gate.begin_write().unwrap();
    let b = gate.begin_write().unwrap();
    assert_eq!(gate.in_flight(), 2);
    drop(a);
    assert_eq!(gate.in_flight(), 1);
    drop(b);
    assert_eq!(gate.in_flight(), 0);
}

#[test]
fn a_clone_of_the_gate_shares_one_decision() {
    // The gate is handed to whatever issues writes; two holders must not
    // disagree about whether writes are enabled.
    let gate = WriteGate::open(ready());
    let other = gate.clone();
    other.revoke(auth_failure());
    assert!(
        !gate.is_ready(),
        "revocation is visible through every handle"
    );
}
