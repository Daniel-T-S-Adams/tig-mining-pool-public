//! §13's observations, gathered against a real `fake-tig`.
//!
//! B2 covers each check failing in isolation. This is the other half: that
//! the evidence a check grades is actually gathered, and gathered from TIG
//! rather than from the configuration it is compared against.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use tig_client::{ReadPolicy, TigReadClient, TigReader};

use tig_gateway::evidence::{
    active_challenge_runtimes, api_key_placement, canonical_holds, confirmed_pool_player_id,
    lossless_holds, openapi_checksum, response_models, serialization_fixtures,
};

const PINNED: &str = include_str!("../../../config/tig_integration.json");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
const POOL_PLAYER: &str = "0xp00l00000000000000000000000000000000000";
/// The fixture anchor (`fixtures/tig/v1/README.md`). `c099` activates at 900,
/// so §5.1 makes it inactive here.
const BLOCK_ROUND: u64 = 834;

async fn fake_tig() -> String {
    let world = fake_tig::build_world(fake_tig::Config::new(FIXTURES)).expect("world loads");
    let app = fake_tig::router(world);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn reader(base: &str) -> TigReadClient {
    let policy = ReadPolicy::from_config_json(PINNED).unwrap();
    TigReadClient::new(base, &policy, policy.for_reader(TigReader::Gateway)).unwrap()
}

async fn latest_block(base: &str) -> String {
    let body: serde_json::Value = reqwest::get(format!("{base}/get-block"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["block"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn check_9_reads_the_identity_from_tig_and_not_from_the_configuration() {
    let base = fake_tig().await;
    let reader = reader(&base);
    let block = latest_block(&base).await;

    let observed = confirmed_pool_player_id(&reader, &block, POOL_PLAYER)
        .await
        .expect("the fixture pool exists");
    assert_eq!(observed, POOL_PLAYER);

    // The case that matters. `get-player-data` takes `player_id` as a
    // parameter, so a check trusting the echoed value would compare the
    // configured identity against itself and pass for an account that does
    // not exist — the circularity §13.1 rejects for check 2, and that a
    // reviewer found in check 1.
    //
    // Live testnet answers an unknown id with HTTP 200 and a null player
    // rather than a 404 (verified 2026-09-15), so "the request succeeded" is
    // not the test. "TIG holds a player" is.
    let err = confirmed_pool_player_id(
        &reader,
        &block,
        "0x0000000000000000000000000000000000000001",
    )
    .await
    .expect_err("TIG holds no such player");
    assert!(err.contains("holds no player"), "{err}");

    // The whole envelope, not just the null player. §14.3 records what TIG
    // sends, and the fake is the fidelity reference §13 check 5's model
    // validation will run against — so a reply that drops the sibling keys
    // would let a v0 model pass here and fail against testnet.
    //
    // It also must not answer with the pool's own data under a null player.
    // An earlier version built this reply by cloning the present-player
    // fixture, which would have done exactly that once a fixture carried
    // balances.
    let absent: serde_json::Value = reqwest::get(format!(
        "{base}/get-player-data?block_id={block}&player_id=0x0000000000000000000000000000000000000001"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        absent,
        serde_json::json!({
            "player": serde_json::Value::Null,
            "deposits": [],
            "round_earnings": [],
            "topups": [],
        }),
        "the absent-player envelope must be §14.3's, exactly"
    );
}

/// The runtimes the pinned file names, keyed the way check 6 looks them up.
fn pinned_images() -> BTreeSet<String> {
    serde_json::from_str::<serde_json::Value>(PINNED).unwrap()["images"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn check_6_considers_only_the_compute_this_deployment_serves() {
    let base = fake_tig().await;
    let reader = reader(&base);
    let block = latest_block(&base).await;
    let pinned = pinned_images();

    // A deployment serving nothing has nothing to consider. This is slice 1:
    // no members, nothing mined. It is true rather than acknowledged, and it
    // stops being true the moment compute is configured.
    let none = active_challenge_runtimes(&reader, &block, BLOCK_ROUND, &pinned, &BTreeSet::new())
        .await
        .unwrap();
    assert!(none.is_empty(), "{none:?}");

    // Serving CPU brings the live CPU challenges into scope, and each is
    // judged on whether its runtime is pinned. `evaluate` fails on any that
    // is not — which is how a stale pin stops a pool mining what it has not
    // reviewed.
    let cpu = active_challenge_runtimes(
        &reader,
        &block,
        BLOCK_ROUND,
        &pinned,
        &BTreeSet::from(["cpu".to_string()]),
    )
    .await
    .unwrap();
    assert!(!cpu.is_empty(), "the fixture must carry a CPU challenge");
    assert!(
        cpu.iter().all(|c| c.compute_path_supported),
        "anything in the list passed the compute filter: {cpu:?}"
    );
    // The assertion whose absence hid a bug: with the shipped pins, every
    // challenge reported must be pinned. Without it the list could carry
    // `c099` — which activates at round 900 against the fixture's 834, so §5.1
    // makes it inactive — and the gate would refuse to write over a challenge
    // the decision engine never considers.
    assert!(
        cpu.iter().all(|c| c.runtime_pinned),
        "every active served challenge must be pinned: {cpu:?}"
    );
    assert!(
        !cpu.iter().any(|c| c.challenge_id == "c099"),
        "c099 activates in a later round; §5.1 makes it inactive: {cpu:?}"
    );

    // An unpinned runtime is reported, not filtered out. A gatherer that
    // dropped it would answer check 6 by omission and the gate would never
    // see the thing it exists to refuse.
    let empty_pins = BTreeSet::new();
    let unpinned = active_challenge_runtimes(
        &reader,
        &block,
        BLOCK_ROUND,
        &empty_pins,
        &BTreeSet::from(["cpu".to_string()]),
    )
    .await
    .unwrap();
    assert!(unpinned.iter().all(|c| !c.runtime_pinned), "{unpinned:?}");
    assert_eq!(unpinned.len(), cpu.len());
}

#[tokio::test]
async fn check_6_does_not_fail_over_a_challenge_outside_the_served_set() {
    // The function's own claim is that a gateway serving nothing has nothing
    // to fail on. The first version broke that: it required `id` and
    // `config.name` on every challenge *before* filtering by served compute,
    // so one malformed entry for a compute type this deployment does not
    // touch failed the whole check and blocked writes.
    //
    // Scope first, then demand fields.
    let base = fake_tig().await;
    let reader = reader(&base);
    let block = latest_block(&base).await;
    let pinned = pinned_images();

    // Serving nothing: every challenge is out of scope, so nothing about
    // their shape can matter.
    assert!(
        active_challenge_runtimes(&reader, &block, BLOCK_ROUND, &pinned, &BTreeSet::new())
            .await
            .unwrap()
            .is_empty()
    );

    // A served type TIG has never heard of is refused rather than scoping
    // nothing quietly — the misspelling case ("CPU" for "cpu"), which would
    // otherwise pass check 6 with no runtime examined.
    //
    // Judged against the vocabulary the read declares, not against a match
    // count: a type TIG knows but has no active challenge for today is a
    // legitimate empty scope, and counting matches cannot tell those apart.
    let err = active_challenge_runtimes(
        &reader,
        &block,
        BLOCK_ROUND,
        &pinned,
        &BTreeSet::from(["CPU".to_string()]),
    )
    .await
    .expect_err("a misspelled compute type must not pass as an empty scope");
    assert!(
        err.contains("not a type any live challenge declares"),
        "{err}"
    );
}

#[tokio::test]
async fn check_4_hashes_what_was_served_and_fails_when_it_cannot_look() {
    // The checksum is taken over the bytes fetched, so a document that
    // changed under the pin produces a different answer rather than the
    // pinned one.
    let base = fake_tig().await;
    let served = openapi_checksum(&format!("{base}/get-block"))
        .await
        .unwrap();
    let sha = served
        .hosted_sha256
        .expect("a fetched document has a checksum");
    assert_eq!(sha.len(), 64, "{sha}");
    assert!(served.reviewed_local_override.is_none());

    // Unreachable is an error, never a silent "unchanged". §13's whole
    // posture is that evidence which could not be gathered fails its check.
    let err = openapi_checksum(&format!("{base}/does-not-exist"))
        .await
        .expect_err("a 404 is not a checksum");
    assert!(err.contains("does-not-exist"), "{err}");
}

#[test]
fn check_8_reads_who_can_open_the_key_file_from_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tig-ev-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("key");
    std::fs::write(&path, "k\n").unwrap();

    // Owner-only is the shape `architecture.md` §2.2 requires.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let placement = api_key_placement(true, &path).unwrap();
    assert!(placement.present_in_gateway);
    assert!(!placement.readable_by_member_services);

    // Group-readable is the finding, not world-readable only: a member
    // service in the same group reads it just as easily.
    for mode in [0o640, 0o604, 0o644, 0o660] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            api_key_placement(true, &path)
                .unwrap()
                .readable_by_member_services,
            "mode {mode:o} must be reported as reachable"
        );
    }

    // A key that is not loaded is reported as absent rather than as an error:
    // `evaluate` owns what absence means.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!api_key_placement(false, &path).unwrap().present_in_gateway);

    // A path that does not exist cannot be judged at all.
    assert!(api_key_placement(true, &dir.join("absent")).is_err());

    // Mode bits alone are not the question §2.2 asks. It asks that the file
    // be readable *only by the gateway identity* — so a 0600 file owned by
    // somebody else has no group or other bits set and its owner can still
    // read it. The first version stopped at the mode and passed this case.
    //
    // Only checkable when this process can chown, which is why it is guarded
    // rather than assumed: a test that silently skipped its own subject is
    // the failure this suite keeps finding.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let chowned = std::process::Command::new("chown")
        .arg("1:1")
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if chowned {
        assert!(
            api_key_placement(true, &path)
                .unwrap()
                .readable_by_member_services,
            "0600 owned by another identity is still readable by that identity"
        );
    } else {
        // Not silently skipped: say so, so a green run here is not mistaken
        // for the assertion having been made.
        eprintln!(
            "note: cannot chown in this environment; the owner half of check 8 was not exercised"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_7_runs_the_fixtures_rather_than_trusting_that_ci_did() {
    // §13 gates a running process — "at startup and after any deployment" —
    // and a property proven in CI is a property of a tree. A binary built
    // from a tree whose tests never ran is the deployment this stops, and it
    // would pass a check that trusted CI. Both fixtures are compiled in.
    let outcome = serialization_fixtures().expect("the fixtures are compiled in and readable");
    assert!(outcome.lossless_numeric_parsing, "{}", outcome.detail);
    assert!(
        outcome.canonical_request_serialization,
        "{}",
        outcome.detail
    );
    assert!(outcome.detail.is_empty(), "{}", outcome.detail);
}

#[test]
fn check_7_pins_the_bytes_a_body_renders_to() {
    // What the fixture is for: `architecture.md` §7.3 binds an admitted
    // intent to its canonical payload digest and the gateway refuses bytes
    // that do not reproduce it, so changing the rendering invalidates intents
    // already recorded. Not a claim about how TIG hashes a request — §10
    // identifies a precommit by its semantic fields.
    //
    // The expectation was taken from what the code renders, so it cannot
    // establish that the rendering is what TIG accepts. The spike confirmed
    // the envelope live, always with null hyperparameters; a typed one has
    // never been confirmed.
    const BODY: &str = include_str!("../../../fixtures/serialization/v1/precommit-body.json");
    let doc: serde_json::Value = serde_json::from_str(BODY).unwrap();
    let expected = doc["expected_bytes"].as_str().unwrap();

    // Key order is part of it, because §7.3's digest is over the bytes the
    // pool recorded — whatever TIG does with them.
    assert!(
        expected.starts_with(r#"{"compute_type":"#),
        "the fixture must pin an ordering, not just a value: {expected}"
    );
    // And the hyperparameters keep their own types — §6.6 copies the source
    // benchmark's values, and re-typing either is the failure this pins.
    assert!(expected.contains(r#""alpha":7,"beta":0.25"#), "{expected}");
}

#[tokio::test]
async fn check_5_validates_against_what_tig_serves_not_what_the_fixture_says() {
    // The four endpoints where the fixture and live TIG agree pass.
    //
    // `get-algorithms` does not, and that is deliberate. `fixtures/tig/v1`
    // gives it a top-level `algorithms` key; live testnet sends `codes`,
    // `binarys` and `advances` with no `algorithms` at all (issue #28).
    //
    // Check 5 is written against TIG, because validating against the fixture
    // would make the fake the authority on the real API's shape — code that
    // passes every test and fails the moment it meets TIG. So the fake fails
    // this one endpoint until the fixture is corrected, and this test says so
    // rather than letting the discrepancy sit invisible.
    let base = fake_tig().await;
    let reader = reader(&base);
    let block = latest_block(&base).await;

    let validations = response_models(&reader, &block, POOL_PLAYER).await.unwrap();
    assert_eq!(validations.len(), 5, "{validations:?}");

    for v in &validations {
        if v.endpoint == "get-algorithms" {
            assert!(
                !v.valid,
                "the v1 fixture has `algorithms` where TIG has `codes`/`binarys`/`advances`; \
                 if this now passes, issue #28 has been fixed and this test should be \
                 updated to expect it: {v:?}"
            );
            assert!(v.detail.contains("missing"), "{v:?}");
        } else {
            assert!(v.valid, "{v:?}");
            assert!(v.detail.is_empty(), "{v:?}");
        }
    }
}

#[tokio::test]
async fn check_5_reports_a_read_that_failed_as_invalid_not_as_absent() {
    // `evaluate` fails an endpoint nobody validated, so the danger is not an
    // endpoint reported invalid — it is one quietly missing from the list. A
    // read that could not be made must still produce a row.
    let base = fake_tig().await;
    let reader = reader(&format!("{base}/nowhere"));
    let validations = response_models(&reader, "block_100080", POOL_PLAYER)
        .await
        .unwrap();
    assert_eq!(
        validations.len(),
        5,
        "every endpoint reports: {validations:?}"
    );
    assert!(validations.iter().all(|v| !v.valid), "{validations:?}");
    assert!(
        validations.iter().all(|v| v.detail.contains("read failed")),
        "{validations:?}"
    );
}

#[test]
fn check_7_both_halves_of_the_lossless_rule_can_fail() {
    // `serialization_fixtures` reads two fixed files, so a test calling only
    // that can never see either judgement return an error. Two mutants
    // survived exactly that gap — one stopping the decimal-string check, one
    // dropping the key-file owner comparison. These take a document.
    use serde_json::json;

    let good = json!({
        "integers": { "fuel": 9007199254740993u64 },
        "precise_numbers": { "fee": "1000000000000000" }
    })
    .to_string();
    assert!(lossless_holds(&good).is_ok());

    // An integer that arrived as a float is the f64 failure §4 forbids.
    let floated = json!({
        "integers": { "fuel": 9007199254740992.0 },
        "precise_numbers": { "fee": "1000000000000000" }
    })
    .to_string();
    assert!(lossless_holds(&floated).is_err());

    // A PreciseNumber re-typed to a number is `accounting.md` §3's failure,
    // and it leaves the integer half untouched — which is why both halves are
    // checked rather than one flag standing for both.
    let retyped = json!({
        "integers": { "fuel": 9007199254740993u64 },
        "precise_numbers": { "fee": 1000000000000000u64 }
    })
    .to_string();
    let err = lossless_holds(&retyped).expect_err("a fee that is not a string");
    assert!(err.contains("not a decimal string"), "{err}");

    // A string that has been through a float arrives looking like one, even
    // though it is still a string — which a bare type check would accept.
    let exponent = json!({
        "integers": { "fuel": 9007199254740993u64 },
        "precise_numbers": { "fee": "1e15" }
    })
    .to_string();
    let err = lossless_holds(&exponent).expect_err("an exponent is not a decimal integer");
    assert!(err.contains("not a plain decimal integer string"), "{err}");

    // And a document missing a half fails rather than passing on the half it
    // has.
    assert!(lossless_holds(&json!({"integers": {}}).to_string()).is_err());
    assert!(lossless_holds(&json!({"precise_numbers": {}}).to_string()).is_err());
}

#[test]
fn check_7_a_rendering_that_drifted_from_the_fixture_fails() {
    use serde_json::json;
    const BODY: &str = include_str!("../../../fixtures/serialization/v1/precommit-body.json");
    let doc: serde_json::Value = serde_json::from_str(BODY).unwrap();

    assert!(canonical_holds(BODY).is_ok());

    // One byte different is a different set of recorded intents.
    let drifted = json!({
        "input": doc["input"],
        "expected_bytes": doc["expected_bytes"].as_str().unwrap().replace("aws_t4g", "aws_t3")
    })
    .to_string();
    let err = canonical_holds(&drifted).expect_err("the rendering no longer matches");
    assert!(err.contains("fixture expects"), "{err}");
}
