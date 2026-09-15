//! §13.2's visibility rule: a gate granted on an acknowledgement says so.
//!
//! ONE test, in its own binary, for the reason `write_gate_alerts.rs` sets
//! out and this does not restate: `tracing` caches a callsite's interest the
//! first time it is evaluated, so one emitting call reached with no
//! subscriber installed disables that callsite for every later test in the
//! same process.
//!
//! This could have lived beside the alert test — every emitting call there is
//! inside a subscriber too — but that file's own history is the argument
//! against reasoning it through: an earlier version of it passed locally and
//! found zero alerts in CI, because the outcome turned on which test the
//! harness ran first across threads. A second binary removes the question
//! rather than answering it. The harness below is copied for that, and for
//! nothing else.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pool_domain::Network;
use tig_gateway::readiness::{
    ActiveChallengeRuntime, ApiKeyPlacement, Evidence, FixtureOutcome, ImageObservation,
    ModelValidation, OpenApiObservation, Pins, ResolvedImage, WriteReady, evaluate,
};
use tig_gateway::write_gate::{Revocation, WriteGate};

fn pins() -> Pins {
    Pins {
        network: Network::Testnet,
        upstream_commit: "ad08d1ea001a73ff5aab3b556d7f59246fece14e".to_string(),
        image_digests: BTreeMap::from([("img".to_string(), "sha256:aa".to_string())]),
        platform: "linux/arm64".to_string(),
        openapi_sha256: "abc".to_string(),
        pool_player_id: "0xpool".to_string(),
    }
}

fn ready_with(acknowledgement: Option<&str>) -> WriteReady {
    let pins = pins();
    let evidence = Evidence {
        config_network: Ok(Network::Testnet),
        upstream_commit: Ok(pins.upstream_commit.clone()),
        resolved_images: Ok(match acknowledgement {
            None => ImageObservation {
                resolved: Some(vec![ResolvedImage {
                    reference: "img".to_string(),
                    manifest_digest: "sha256:aa".to_string(),
                    platform: "linux/arm64".to_string(),
                }]),
                reviewed_unresolved: None,
            },
            Some(reason) => ImageObservation {
                resolved: None,
                reviewed_unresolved: Some(reason.to_string()),
            },
        }),
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
fn a_gate_opened_on_an_acknowledgement_says_so_every_time() {
    // `tig_integration.md` §13.2 permits check 3 to pass unperformed on one
    // ground: the acknowledgement stays visible, so a pass granted without
    // resolving a digest never looks like one granted with.
    //
    // An accessor nobody calls does not make it visible. The first version
    // shipped exactly that while §13.2 asserted the gateway reported it, and
    // a reviewer caught the document describing a safety property the code
    // did not have. This is that property, asserted.
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let gate = WriteGate::open(ready_with(Some(
            "slice 1 runs no pinned container; issue #20",
        )));
        // Both entry points, because a gate lost and restored is still a gate
        // granting permission on an acknowledgement.
        gate.revoke(Revocation::Authentication {
            detail: "TIG returned 401".to_string(),
        });
        assert!(gate.restore(ready_with(Some(
            "slice 1 runs no pinned container; issue #20"
        ))));
    });

    let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let reported: Vec<&str> = logged
        .lines()
        .filter(|l| l.contains("container_digests"))
        .collect();
    assert_eq!(
        reported.len(),
        2,
        "once per gate opening, at both entry points: {logged}"
    );
    for line in &reported {
        assert!(line.contains("issue #20"), "the reason must travel: {line}");
        assert!(
            line.contains("WARN"),
            "not below an operator's threshold: {line}"
        );
        assert!(
            line.contains("gateway.write_ready.containers_unresolved"),
            "its own event name: reusing the restore's put two records under one \
             name, so a dashboard counting restorations double-counted in exactly \
             the deviation case — {line}"
        );
    }

    // And a restore is still one record. The bug this guards was the
    // deviation warning sharing `gateway.write_ready.restored`.
    let restores = logged
        .lines()
        .filter(|l| l.contains("gateway.write_ready.restored"))
        .count();
    assert_eq!(restores, 1, "one restore, one record: {logged}");

    // And a gate opened on resolved digests says nothing, or the signal means
    // nothing.
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let _gate = WriteGate::open(ready_with(None));
    });
    let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logged.contains("container_digests"),
        "a check actually performed must not report a deviation: {logged}"
    );
}
