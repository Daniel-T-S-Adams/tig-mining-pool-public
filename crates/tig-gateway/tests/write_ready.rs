//! Slice-1 criteria B1 and B2: `tig_integration.md` §13's compatibility gate.
//!
//! B2 requires each of the nine checks to have a test that fails it in
//! isolation. "In isolation" is asserted literally: every case below
//! asserts the failure set is *exactly* the check it mutated, so a check
//! that fired for the wrong reason — or a mutation that broke two things at
//! once — is a test failure rather than a green run.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::Network;
use serde_json::json;
use tig_gateway::readiness::{
    ActiveChallengeRuntime, ApiKeyPlacement, Check, Evidence, FixtureOutcome, ModelValidation,
    OpenApiObservation, Pins, ResolvedImage, acquired_upstream_commit, evaluate, pinned_network,
};

/// Makes one piece of evidence ungatherable, for the fail-closed cases.
type BreakEvidence = Box<dyn Fn(&mut Evidence)>;

/// The pins a deployment actually ships with.
///
/// The pins a deployment ships with, built by the **production**
/// constructor rather than re-derived here.
///
/// It used to re-read `config/tig_integration.json` by hand, which meant the
/// thing under test and the thing the binary uses were two readings of one
/// file that could drift apart — the duplication §13's own checks exist to
/// prevent, one level up. `Pins::compiled_in` is now the only reader, so a
/// shipped config that lost a pinned digest fails here too.
///
/// The platform is the test's because it is the deployment's everywhere: §2
/// records the pinned manifests as multi-platform, so the file names the
/// reviewed architecture set and the host answers which of them it is. The
/// network is *not* a parameter — it comes from the pin, so check 1 has
/// something to disagree with.
fn pins() -> Pins {
    let pins = Pins::compiled_in(
        // Test data, not a pin source: the gate compares observed identity
        // against whatever a deployment configures, and these pins are
        // self-consistent. The real slice-1 testnet identity is recorded in
        // docs/plans/slice-1-gateway.md.
        "0x1111111111111111111111111111111111111111".to_string(),
        "linux/arm64".to_string(),
    )
    .expect("the shipped config must parse into pins");
    assert!(
        !pins.image_digests.is_empty(),
        "the shipped config must pin at least one container"
    );
    assert_eq!(
        pins.upstream_commit.len(),
        40,
        "the pin must be a full commit, since check 2 compares it byte for byte"
    );
    pins
}

/// Observations that satisfy all nine checks.
fn passing(pins: &Pins) -> Evidence {
    Evidence {
        config_network: Ok(Network::Testnet),
        upstream_commit: Ok(pins.upstream_commit.clone()),
        resolved_images: Ok(pins
            .image_digests
            .iter()
            .map(|(reference, digest)| ResolvedImage {
                reference: reference.clone(),
                manifest_digest: digest.clone(),
                platform: pins.platform.clone(),
            })
            .collect()),
        openapi: Ok(OpenApiObservation {
            hosted_sha256: Some(pins.openapi_sha256.clone()),
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
        .map(|endpoint| ModelValidation {
            endpoint: (*endpoint).to_string(),
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
    }
}

/// Assert the evidence fails `check` and no other check.
///
/// One check may report more than one failure — §13 check 7 covers two
/// fixture replays, check 3 every pinned container — so this compares the
/// distinct checks that failed rather than the failure count. What it does
/// not permit is a second *check* firing: that would mean the mutation
/// broke something else too, and the case would no longer show the check
/// failing in isolation.
fn only(evidence: Evidence, check: Check) {
    let pins = pins();
    let failures = evaluate(&pins, &evidence).expect_err("this evidence must not be WRITE_READY");
    let mut failed: Vec<Check> = failures.iter().map(|f| f.check).collect();
    failed.dedup();
    assert_eq!(
        failed,
        vec![check],
        "expected only {check} to fail; got {failures:?}"
    );
}

#[test]
fn all_nine_passing_is_the_only_route_to_write_ready() {
    // B1. The token has no public constructor, so this is the only way one
    // exists at all.
    let pins = pins();
    let ready = evaluate(&pins, &passing(&pins)).expect("complete evidence must be WRITE_READY");
    assert_eq!(ready.network(), Network::Testnet);
    assert_eq!(ready.upstream_commit(), pins.upstream_commit);
}

#[test]
fn check_1_a_network_that_is_not_testnet() {
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.config_network = Ok(Network::Mainnet);
    only(evidence, Check::ConfigNetwork);
}

#[test]
fn check_1_a_config_that_disagrees_with_the_pinned_network() {
    // A testnet config under mainnet pins passes "is it testnet" and would
    // otherwise sail through the one check that exists to keep slice 1 off
    // mainnet.
    let mut pins = pins();
    pins.network = Network::Mainnet;
    let evidence = passing(&pins);
    let failures = evaluate(&pins, &evidence).expect_err("the pins and the config disagree");
    let failed: Vec<Check> = failures.iter().map(|f| f.check).collect();
    assert_eq!(failed, vec![Check::ConfigNetwork], "got {failures:?}");
}

#[test]
fn check_2_an_upstream_commit_that_moved() {
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.upstream_commit = Ok("0000000000000000000000000000000000000000".to_string());
    only(evidence, Check::UpstreamCommit);
}

#[test]
fn the_pinned_network_comes_from_the_pin_so_check_1_can_disagree() {
    // Check 1 has two halves: the network must be `testnet` absolutely, and
    // it must agree with the pin. The second is worth something only if the
    // pin is a different artifact from the thing being checked — otherwise it
    // compares a deployment's TOML value against itself, which is the
    // circularity §13.1 rejects for check 2.
    //
    // Tested through `pinned_network` rather than through `compiled_in`,
    // because `compiled_in` reads one fixed document. Asserting that its
    // network equals the shipped `network.name` looks like a test and is not:
    // a body returning `Testnet` outright satisfies it too, since the shipped
    // name is testnet. That version was written first and a mutant walked
    // straight through it.
    assert_eq!(
        pinned_network(&json!({ "network": { "name": "mainnet" } })).unwrap(),
        Network::Mainnet,
        "the network must come from the file, not from what this build expects"
    );
    assert_eq!(
        pinned_network(&json!({ "network": { "name": "testnet" } })).unwrap(),
        Network::Testnet
    );

    // A build that does not know the network it is pinned to cannot check
    // anything about it, so an unknown or absent name fails rather than
    // defaulting.
    assert!(pinned_network(&json!({ "network": { "name": "devnet" } })).is_err());
    assert!(pinned_network(&json!({})).is_err());

    // And the constructor uses it, so the two cannot drift apart.
    let shipped: serde_json::Value =
        serde_json::from_str(include_str!("../../../config/tig_integration.json")).unwrap();
    assert_eq!(pins().network, pinned_network(&shipped).unwrap());
}

#[test]
fn check_2_compares_the_deployments_declaration_against_the_compiled_in_pin() {
    // What check 2 can actually verify here, and what it cannot.
    //
    // §13 asks for "the acquired upstream source commit", but nothing in this
    // repository acquires TIG's source — §15 makes the upgrade an eight-step
    // human review ending in an edit to `config/tig_integration.json`. So the
    // observation is what the deployment *declares* was acquired, and the pin
    // is compiled into the binary from that same file.
    //
    // Those are two different artifacts: the pin is fixed when the binary is
    // built, the declaration is written when it is deployed. Comparing them
    // catches a binary deployed beside a configuration that moved on. It does
    // not catch a declaration nobody reviewed — only automated acquisition
    // would, which is why that gap is named rather than papered over.
    let pins = pins();

    // The deployment agreeing with the binary it was shipped with.
    let mut evidence = passing(&pins);
    evidence.upstream_commit = acquired_upstream_commit(&pins.upstream_commit);
    evaluate(&pins, &evidence).expect("a deployment that agrees with its binary may write");

    // A deployment that declares a different snapshot. This is the real
    // failure: the same binary, deployed beside a config that has moved.
    let mut evidence = passing(&pins);
    evidence.upstream_commit = acquired_upstream_commit("00112233445566778899aabbccddeeff00112233");
    only(evidence, Check::UpstreamCommit);

    // An empty declaration is an ungatherable observation, not a mismatch.
    // `pool-config` refuses one at load, so this only pins that the public
    // function does not quietly turn it into a comparison against "".
    assert!(acquired_upstream_commit("").is_err());
}

#[test]
fn check_3_a_container_that_resolves_to_a_different_digest() {
    let pins = pins();
    let mut evidence = passing(&pins);
    let mut images = evidence.resolved_images.unwrap();
    images[0].manifest_digest =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string();
    evidence.resolved_images = Ok(images);
    only(evidence, Check::ContainerDigests);
}

#[test]
fn check_3_a_container_resolved_for_the_wrong_platform() {
    // A digest can be right for an image that was built for another
    // architecture; §13 pins the platform as well for that reason.
    let pins = pins();
    let mut evidence = passing(&pins);
    let mut images = evidence.resolved_images.unwrap();
    images[0].platform = "amd64-but-pinned-elsewhere".to_string();
    evidence.resolved_images = Ok(images);
    only(evidence, Check::ContainerDigests);
}

#[test]
fn check_3_a_container_that_did_not_resolve_at_all() {
    // Driven by the pins rather than the observations: an image missing
    // from the resolved set must fail, not vanish from the check.
    let pins = pins();
    let mut evidence = passing(&pins);
    let mut images = evidence.resolved_images.unwrap();
    images.remove(0);
    evidence.resolved_images = Ok(images);
    only(evidence, Check::ContainerDigests);
}

#[test]
fn check_4_a_mutated_openapi_checksum() {
    // Named explicitly by criterion B2.
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.openapi = Ok(OpenApiObservation {
        hosted_sha256: Some("f".repeat(pins.openapi_sha256.len())),
        reviewed_local_override: None,
    });
    only(evidence, Check::OpenApiChecksum);
}

#[test]
fn check_4_an_unreviewed_local_override_does_not_rescue_a_changed_schema() {
    // §13 offers the override as an alternative, not a fallback: it still
    // has to match the reviewed checksum. Otherwise "the hosted document
    // changed" could be answered by pointing at any local file.
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.openapi = Ok(OpenApiObservation {
        hosted_sha256: Some("f".repeat(pins.openapi_sha256.len())),
        reviewed_local_override: Some("a".repeat(pins.openapi_sha256.len())),
    });
    only(evidence, Check::OpenApiChecksum);
}

#[test]
fn check_5_a_required_response_that_does_not_validate() {
    let pins = pins();
    let mut evidence = passing(&pins);
    let mut models = evidence.response_models.unwrap();
    models[0].valid = false;
    models[0].detail = "unexpected shape".to_string();
    evidence.response_models = Ok(models);
    only(evidence, Check::ResponseModels);
}

#[test]
fn check_5_a_required_response_nobody_validated() {
    let pins = pins();
    let mut evidence = passing(&pins);
    let mut models = evidence.response_models.unwrap();
    models.retain(|m| m.endpoint != "get-opow");
    evidence.response_models = Ok(models);
    only(evidence, Check::ResponseModels);
}

#[test]
fn check_6_an_active_challenge_without_a_pinned_runtime() {
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.active_challenges = Ok(vec![ActiveChallengeRuntime {
        challenge_id: "c002".to_string(),
        runtime_pinned: false,
        compute_path_supported: true,
    }]);
    only(evidence, Check::ChallengeRuntimes);
}

#[test]
fn check_6_an_active_challenge_without_a_supported_compute_path() {
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.active_challenges = Ok(vec![ActiveChallengeRuntime {
        challenge_id: "c003".to_string(),
        runtime_pinned: true,
        compute_path_supported: false,
    }]);
    only(evidence, Check::ChallengeRuntimes);
}

#[test]
fn check_7_each_fixture_half_fails_the_gate_on_its_own() {
    // Asserted separately: a single combined flag would let one of the two
    // regress while the other kept the check green.
    let pins = pins();
    for (numeric, canonical) in [(false, true), (true, false), (false, false)] {
        let mut evidence = passing(&pins);
        evidence.fixtures = Ok(FixtureOutcome {
            lossless_numeric_parsing: numeric,
            canonical_request_serialization: canonical,
            detail: "replay mismatch".to_string(),
        });
        only(evidence, Check::SerializationFixtures);
    }
}

#[test]
fn check_8_the_api_key_missing_or_readable_by_members() {
    let pins = pins();
    for placement in [
        ApiKeyPlacement {
            present_in_gateway: false,
            readable_by_member_services: false,
        },
        ApiKeyPlacement {
            present_in_gateway: true,
            readable_by_member_services: true,
        },
    ] {
        let mut evidence = passing(&pins);
        evidence.api_key = Ok(placement);
        only(evidence, Check::ApiKeyIsolation);
    }
}

#[test]
fn check_9_a_player_id_that_disagrees_with_configured_identity() {
    // Named explicitly by criterion B2.
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.confirmed_pool_player_id = Ok("0xdeadbeef".to_string());
    only(evidence, Check::PlayerIdentity);
}

#[test]
fn evidence_that_could_not_be_gathered_fails_its_check() {
    // Fail-closed, for all nine. A gate whose checks quietly pass when
    // their input is unavailable is worse than no gate: the failure then
    // surfaces as a successful write against an incompatible API.
    let pins = pins();
    let cases: Vec<(Check, BreakEvidence)> = vec![
        (
            Check::ConfigNetwork,
            Box::new(|e: &mut Evidence| e.config_network = Err("probe failed".into())),
        ),
        (
            Check::UpstreamCommit,
            Box::new(|e: &mut Evidence| e.upstream_commit = Err("probe failed".into())),
        ),
        (
            Check::ContainerDigests,
            Box::new(|e: &mut Evidence| e.resolved_images = Err("probe failed".into())),
        ),
        (
            Check::OpenApiChecksum,
            Box::new(|e: &mut Evidence| e.openapi = Err("probe failed".into())),
        ),
        (
            Check::ResponseModels,
            Box::new(|e: &mut Evidence| e.response_models = Err("probe failed".into())),
        ),
        (
            Check::ChallengeRuntimes,
            Box::new(|e: &mut Evidence| e.active_challenges = Err("probe failed".into())),
        ),
        (
            Check::SerializationFixtures,
            Box::new(|e: &mut Evidence| e.fixtures = Err("probe failed".into())),
        ),
        (
            Check::ApiKeyIsolation,
            Box::new(|e: &mut Evidence| e.api_key = Err("probe failed".into())),
        ),
        (
            Check::PlayerIdentity,
            Box::new(|e: &mut Evidence| e.confirmed_pool_player_id = Err("probe failed".into())),
        ),
    ];
    assert_eq!(
        cases.len(),
        Check::ALL.len(),
        "every check needs an unavailable-evidence case"
    );
    for (check, break_it) in cases {
        let mut evidence = passing(&pins);
        break_it(&mut evidence);
        only(evidence, check);
    }
}

#[test]
fn every_failure_is_reported_not_just_the_first() {
    // §13 makes a failure an operator task — compare the upstream commit,
    // update models and fixtures, re-run the spike, review the config. Doing
    // that one rediscovered failure at a time turns one round of work into
    // nine.
    let pins = pins();
    let mut evidence = passing(&pins);
    evidence.config_network = Ok(Network::Mainnet);
    evidence.confirmed_pool_player_id = Ok("0xdeadbeef".to_string());
    evidence.fixtures = Err("no fixture runner".to_string());

    let failures = evaluate(&pins, &evidence).expect_err("three checks failed");
    let failed: Vec<Check> = failures.iter().map(|f| f.check).collect();
    assert_eq!(
        failed,
        vec![
            Check::ConfigNetwork,
            Check::SerializationFixtures,
            Check::PlayerIdentity
        ],
        "all failures, in §13 order"
    );
}
