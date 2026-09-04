//! The `architecture.md` §10.3 alert raised when `WRITE_READY` is lost
//! (slice-1 criterion B3).
//!
//! ONE test, in its own binary, deliberately.
//!
//! `tracing` caches a callsite's interest the first time it is evaluated. A
//! `revoke` called with no subscriber installed — which every other test in
//! `write_gate.rs` does — caches the alert callsite as disabled, and a later
//! test that installs a capture subscriber then sees nothing. That is
//! exactly how the first version of this coverage failed: it passed locally
//! and found zero alerts in CI, because the outcome depended on which test
//! the harness happened to run first across threads.
//!
//! A single test in a separate binary removes the race rather than making it
//! less likely: nothing else in this process touches those callsites, and
//! the subscriber is installed before the first one is reached.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn losing_write_ready_alerts_once_and_recovery_is_recorded() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let gate = WriteGate::open(ready());
        let _permit = gate.begin_write().unwrap();

        // §10.3: "TIG writes are disabled by compatibility or authentication
        // failure" is one of the conditions that must page an operator.
        gate.revoke(Revocation::Authentication {
            detail: "TIG returned 401".to_string(),
        });
        // A credential rejected on every request must not page per request.
        gate.revoke(Revocation::Authentication {
            detail: "TIG returned 401".to_string(),
        });
        gate.revoke(Revocation::Authentication {
            detail: "TIG returned 401".to_string(),
        });

        gate.restore(ready());

        // A compatibility loss names the specific checks in the alert.
        let compat = WriteGate::open(ready());
        compat.revoke(Revocation::Compatibility {
            failed: vec![Check::OpenApiChecksum, Check::PlayerIdentity],
            detail: "re-evaluation failed".to_string(),
        });
    });

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();

    // Scoped to the authentication gate: the compatibility gate below
    // raises its own, and counting every alert in the process would make
    // this assertion about how many gates the test happens to open rather
    // than about one loss producing one alert.
    let alerts: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("gateway.write_ready.lost"))
        .filter(|line| line.contains("authentication"))
        .collect();
    assert_eq!(
        alerts.len(),
        1,
        "three revocations of one gate must alert once; output was: {output}"
    );

    let alert = alerts[0];
    assert!(alert.contains("\"level\":\"ERROR\""), "got: {alert}");
    assert!(alert.contains("authentication"), "got: {alert}");
    // The count an operator needs before deciding anything: that outcome is
    // still arriving.
    assert!(alert.contains("\"in_flight\":1"), "got: {alert}");

    assert!(
        output.contains("gateway.write_ready.restored"),
        "recovery must be recorded; output was: {output}"
    );

    // The compatibility alert carries which of the nine regressed, as data.
    let compat = output
        .lines()
        .filter(|line| line.contains("gateway.write_ready.lost"))
        .find(|line| line.contains("compatibility"))
        .unwrap_or_else(|| panic!("no compatibility alert; output was: {output}"));
    assert!(
        compat.contains("\"failed_checks\":\"4,9\""),
        "the alert should name the §13 check numbers, got: {compat}"
    );
}
