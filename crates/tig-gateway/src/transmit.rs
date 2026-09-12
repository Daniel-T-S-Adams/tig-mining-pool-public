//! Transmitting a precommit (`docs/tig_integration.md` §6.1, criterion E3).
//!
//! The rule this module exists to keep is §11's: **a write is never retried
//! on a timer.** §11 retries read-only calls on network failure, 408, 429 and
//! 5xx; for writes it says instead "before retrying any write, query
//! confirmed state and the local attempt ledger", and §10 adds that the
//! gateway "never blindly resubmits an ambiguous precommit".
//!
//! So [`PrecommitTransmitter::send`] makes **exactly one HTTP attempt** and
//! returns what it learned. There is no retry loop here and there is
//! deliberately no way to ask for one: a caller that wants to try again must
//! go through reconciliation first, which is a different module and a
//! different decision.
//!
//! `send` also refuses to transmit a payload the intent does not describe.
//! §7.3 records the payload digest before the send; taking that digest over
//! the very bytes this module puts on the socket is what makes the record
//! binding, rather than a note of what the pool meant to send.
//!
//! An outcome the transport could not resolve — a timeout, a dropped
//! connection, a 5xx — is [`AttemptOutcome::Ambiguous`], not a failure. The
//! request may have been applied. Treating it as a failure and resending is
//! the duplicate precommit, and a second fee, that §10 exists to prevent.

use pool_workflow::{AttemptOutcome, WriteAttempt, WriteIntent, WriteKind};
use serde_json::Value;

use crate::credential::TigApiKey;
use crate::write_gate::WritePermit;
use crate::write_policy::WritePolicy;
use pool_workflow::payload::PrecommitSubmission;
pub use pool_workflow::payload::{precommit_body, precommit_digest};

/// What one transmission established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmitted {
    pub outcome: AttemptOutcome,
    /// The HTTP status, when one arrived at all.
    pub http_status: Option<i32>,
    /// A short classification for the attempt ledger — never response bytes,
    /// which `accounting.md` bounds and `architecture.md` §9 keeps out of
    /// database values.
    pub detail: String,
    /// The benchmark ID TIG assigned, on an accepted precommit only.
    pub benchmark_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransmitError {
    #[error("cannot build the HTTP client: {0}")]
    Client(String),
    /// The attempt handed in already carries an outcome.
    ///
    /// §7.3 records a response once. Sending against an attempt that already
    /// has one would either overwrite that record or leave the new request
    /// with no row of its own — both lose the evidence §10 reconciles from.
    ///
    /// This deliberately refuses an `AMBIGUOUS` attempt as well. `AMBIGUOUS`
    /// counts as *unresolved* for §10's lane, because the lane must stay
    /// closed until reconciliation settles it — but it is emphatically not a
    /// row to send against: §11 forbids issuing a replacement precommit
    /// while the previous outcome is ambiguous, and the ledger could not
    /// record the second response anyway, since §7.3's resolve path and the
    /// database trigger both refuse to restate an ambiguous row. The
    /// duplicate would exist at TIG, cost a second fee, and be invisible.
    #[error("attempt {attempt_id} already carries an outcome; a new send needs a new attempt")]
    AttemptAlreadyAnswered { attempt_id: String },
    /// The attempt belongs to a different intent.
    ///
    /// The attempt row is the evidence §10 reconciles from, and it is
    /// reconciled against its intent's recorded payload. An attempt filed
    /// under one intent while the bytes describe another leaves both wrong.
    #[error("attempt {attempt_id} belongs to intent {attempt_intent_id}, not {intent_id}")]
    AttemptNotForIntent {
        attempt_id: String,
        attempt_intent_id: String,
        intent_id: String,
    },
    /// The intent is not a precommit.
    #[error("intent {intent_id} is a {write_kind:?} intent; this path sends precommits")]
    NotAPrecommitIntent {
        intent_id: String,
        write_kind: WriteKind,
    },
    /// The body does not hash to the intent's recorded payload.
    ///
    /// §7.3 records the payload digest *before* the send and requires an
    /// explicit new generation for a changed payload. Without this check the
    /// digest records what the pool meant to send while the socket carries
    /// something else, so reconciliation would compare TIG's record against
    /// a payload that was never transmitted.
    #[error("submission does not hash to intent {intent_id}'s recorded payload digest")]
    PayloadNotTheRecordedOne { intent_id: String },
}

/// Sends one precommit, once.
pub struct PrecommitTransmitter {
    base_url: String,
    http: reqwest::Client,
}

impl PrecommitTransmitter {
    pub fn new(base_url: impl Into<String>, policy: &WritePolicy) -> Result<Self, TransmitError> {
        let http = reqwest::Client::builder()
            .connect_timeout(policy.connect_timeout())
            .timeout(policy.call_timeout())
            // No redirects, for the reason §2.1 gives the read client:
            // failure to reach the configured network must not silently
            // become a request somewhere else — and this one carries the
            // API key.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TransmitError::Client(e.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    /// Send one precommit and classify what came back.
    ///
    /// Exactly one attempt. See the module doc: retrying here would be the
    /// blind resubmission §10 forbids, and the caller cannot ask for one.
    /// `attempt` is the ledger row for this send, and taking it is the
    /// point: `architecture.md` §7.3 and criterion E1 require the attempt to
    /// be recorded **before** the request goes out, and a `send` callable
    /// without one would let a future caller transmit a precommit that §10
    /// reconciliation can never see — the "sent and never answered" write
    /// with no trace.
    ///
    /// The row must still be unresolved, so one attempt covers one request.
    ///
    /// This makes the ordering visible and checkable rather than provable:
    /// `WriteAttempt`'s fields are public, so the type is forgeable in
    /// principle. Closing that would mean making them private with a
    /// crate-private constructor in `pool-workflow`, the way
    /// `PersistedSnapshot` and `WriteReady` are built — worth doing, and
    /// deliberately not done here, where it would rewrite the assertions of
    /// a merged crate's test suite for a signature this PR is introducing.
    /// `permit` is the gate's proof that `WRITE_READY` was held when this
    /// write was admitted. It has no public constructor — only
    /// `WriteGate::begin_write` produces one — so a send cannot happen while
    /// writes are disabled, which is what criterion B3 and
    /// `architecture.md` §10.3 require. Taking the attempt row while leaving
    /// the gate to a caller's memory would have closed one
    /// forgeable-by-omission gap and left the other open.
    pub async fn send(
        &self,
        permit: &WritePermit,
        key: &TigApiKey,
        intent: &WriteIntent,
        attempt: &WriteAttempt,
        submission: &PrecommitSubmission,
    ) -> Result<Transmitted, TransmitError> {
        // Read so the permit is a live requirement rather than an unused
        // parameter: a write records the pin it was admitted under.
        let _admitted_under = permit.admitted_under();
        if attempt.intent_id != intent.intent_id {
            return Err(TransmitError::AttemptNotForIntent {
                attempt_id: attempt.attempt_id.clone(),
                attempt_intent_id: attempt.intent_id.clone(),
                intent_id: intent.intent_id.clone(),
            });
        }
        if intent.write_kind != WriteKind::Precommit {
            return Err(TransmitError::NotAPrecommitIntent {
                intent_id: intent.intent_id.clone(),
                write_kind: intent.write_kind,
            });
        }
        // §7.3's digest is recorded before the send; this is where it becomes
        // binding. Checked before the attempt's own state so a mismatched
        // payload is never sent, whatever the lane says.
        if precommit_digest(submission) != intent.payload_digest {
            return Err(TransmitError::PayloadNotTheRecordedOne {
                intent_id: intent.intent_id.clone(),
            });
        }
        if attempt.has_response() {
            return Err(TransmitError::AttemptAlreadyAnswered {
                attempt_id: attempt.attempt_id.clone(),
            });
        }
        let url = format!("{}/submit-precommit", self.base_url);
        let response = self
            .http
            .post(&url)
            .header("X-Api-Key", key.expose())
            .json(&precommit_body(submission))
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            // No usable answer. The request may have reached TIG and been
            // applied, so this is ambiguous rather than failed — the whole
            // reason §10 reconciles instead of resending.
            Err(e) => {
                return Ok(Transmitted {
                    outcome: AttemptOutcome::Ambiguous,
                    http_status: None,
                    detail: classify_transport_error(&e),
                    benchmark_id: None,
                });
            }
        };

        let status = response.status();
        let http_status = Some(i32::from(status.as_u16()));

        if status.is_success() {
            let body: Value = match response.json().await {
                Ok(body) => body,
                // The status said accepted and the body did not arrive. TIG
                // may well have applied it, so this is ambiguous too.
                Err(e) => {
                    return Ok(Transmitted {
                        outcome: AttemptOutcome::Ambiguous,
                        http_status,
                        detail: classify_transport_error(&e),
                        benchmark_id: None,
                    });
                }
            };
            let Some(benchmark_id) = body.get("benchmark_id").and_then(|v| v.as_str()) else {
                // A success status is TIG accepting the request for
                // processing (§6.1, §7), so the precommit may exist and its
                // fee may be paid. That the body did not carry the id back
                // says nothing about whether the write landed — it only
                // means this attempt cannot learn the id from the response.
                // Calling it an error would licence the resend §10 forbids;
                // the identical situation one branch above, a body that will
                // not decode, is already ambiguous.
                return Ok(Transmitted {
                    outcome: AttemptOutcome::Ambiguous,
                    http_status,
                    detail: "accepted without a benchmark_id".to_string(),
                    benchmark_id: None,
                });
            };
            return Ok(Transmitted {
                outcome: AttemptOutcome::Accepted,
                http_status,
                detail: "accepted".to_string(),
                benchmark_id: Some(benchmark_id.to_string()),
            });
        }

        // A 5xx, 408 or 429 says nothing about whether the write was applied
        // before the failure. §11 retries READS on exactly these; a write
        // must reconcile instead, so they are ambiguous rather than rejected.
        if status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Ok(Transmitted {
                outcome: AttemptOutcome::Ambiguous,
                http_status,
                detail: format!("HTTP {status}"),
                benchmark_id: None,
            });
        }

        // Any other 4xx is TIG deciding, and it decided no.
        Ok(Transmitted {
            outcome: AttemptOutcome::Rejected,
            http_status,
            detail: format!("HTTP {status}"),
            benchmark_id: None,
        })
    }
}

/// A short, bounded classification of a transport failure.
///
/// Deliberately not `e.to_string()`: a reqwest error renders the URL, which
/// for this client is the TIG endpoint, and the value is written to the
/// attempt ledger. `accounting.md` bounds that column and states it carries a
/// classification, never response bytes.
fn classify_transport_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connect failed".to_string()
    } else if e.is_body() || e.is_decode() {
        "response body unreadable".to_string()
    } else if e.is_request() {
        "request failed".to_string()
    } else {
        "transport failure".to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::credential;
    use crate::reconcile::{Reconciliation, TrackSettings, reconcile_precommit};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    /// These tests live inside the crate rather than in `tests/` because
    /// `TigApiKey` has no public constructor and `expose` is crate-private
    /// (`architecture.md` §2.2). An integration test could not build a key —
    /// which is the credential boundary working, not an inconvenience.
    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
    const POOL_PLAYER: &str = "0xp00l00000000000000000000000000000000000";

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

    /// A synthetic key in a private file — never a real credential, and the
    /// only way to obtain a `TigApiKey` at all.
    fn synthetic_key(label: &str) -> (TigApiKey, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tig-tx-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, format!("{}\n", fake_tig::DEFAULT_API_KEY)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let key = credential::load(&path).expect("synthetic key loads");
        (key, path)
    }

    /// A permit from a gate opened on complete §13 evidence — the only way
    /// one exists, since `WritePermit` has no public constructor.
    fn permit() -> crate::write_gate::WritePermit {
        use crate::readiness::{
            ActiveChallengeRuntime, ApiKeyPlacement, Evidence, FixtureOutcome, ModelValidation,
            OpenApiObservation, Pins, ResolvedImage, evaluate,
        };
        use pool_domain::Network;

        let pins = Pins {
            network: Network::Testnet,
            upstream_commit: "c".to_string(),
            image_digests: BTreeMap::from([("img".to_string(), "sha256:aa".to_string())]),
            platform: "arm64".to_string(),
            openapi_sha256: "abc".to_string(),
            pool_player_id: POOL_PLAYER.to_string(),
        };
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
        let ready = evaluate(&pins, &evidence).expect("complete evidence");
        crate::write_gate::WriteGate::open(ready)
            .begin_write()
            .expect("a fresh gate admits a write")
    }

    /// A ledger row standing in for one `ledger.begin(...)` call.
    fn unresolved_attempt() -> WriteAttempt {
        WriteAttempt {
            attempt_id: "11111111-1111-1111-1111-111111111111".to_string(),
            intent_id: "22222222-2222-2222-2222-222222222222".to_string(),
            attempt_no: 1,
            outcome: None,
            http_status: None,
            reconciled: false,
            age_secs: 0,
        }
    }

    /// The intent row `submitted` was recorded under.
    ///
    /// The digest is taken from the submission itself, which is what the
    /// caller that recorded the intent would have done — the send then has
    /// something real to check the bytes against.
    fn intent_for(submitted: &PrecommitSubmission) -> WriteIntent {
        WriteIntent {
            intent_id: "22222222-2222-2222-2222-222222222222".to_string(),
            network: pool_domain::Network::Testnet,
            workflow_id: "33333333-3333-3333-3333-333333333333".to_string(),
            write_kind: WriteKind::Precommit,
            generation: 1,
            benchmark_id: None,
            payload_digest: precommit_digest(submitted),
            payload_artifact_id: None,
            state: pool_workflow::IntentState::Prepared,
        }
    }

    fn policy() -> WritePolicy {
        WritePolicy::from_config_json(include_str!("../../../config/tig_integration.json"))
            .expect("shipped config parses")
    }

    fn track(num_bundles: u64, fuel_budget: u64) -> TrackSettings {
        TrackSettings {
            num_bundles,
            fuel_budget,
            hyperparameters: BTreeMap::from([
                ("noise".to_string(), json!(0.15)),
                ("restart_period".to_string(), json!(250)),
            ]),
        }
    }

    /// A submission the fixture world accepts: challenge c001's active tracks
    /// are t001 and t002, and §6.1 requires every one of them.
    async fn submission(base: &str) -> PrecommitSubmission {
        let block: serde_json::Value = reqwest::get(format!("{base}/get-block?include_data=true"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let block_id = block["block"]["id"].as_str().unwrap().to_string();
        PrecommitSubmission {
            player_id: POOL_PLAYER.to_string(),
            block_id,
            challenge_id: "c001".to_string(),
            algorithm_id: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings: BTreeMap::from([
                ("t001".to_string(), track(4, 1_500_000)),
                ("t002".to_string(), track(4, 1_500_000)),
            ]),
        }
    }

    async fn inject(base: &str, mode: &str) {
        let response = reqwest::Client::new()
            .post(format!("{base}/_fake/inject"))
            .json(&json!({ "target": "submit-precommit", "mode": mode }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "injection accepted");
    }

    async fn writes_received(base: &str) -> u64 {
        let state: Value = reqwest::get(format!("{base}/_fake/state"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        state["writes_received"]["submit-precommit"]
            .as_u64()
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn a_dropped_response_is_ambiguous_and_the_write_is_never_repeated() {
        // Criterion E3, as it is written: fake-tig applies the write and then
        // drops the response, and a server-side counter asserts exactly one
        // write reached it.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("ambiguous");
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let submitted = submission(&base).await;

        inject(&base, "ambiguous").await;
        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();

        assert_eq!(result.outcome, AttemptOutcome::Ambiguous);
        assert_eq!(result.benchmark_id, None, "no id came back to learn from");
        assert_eq!(
            writes_received(&base).await,
            1,
            "exactly one write reached TIG"
        );

        // The write WAS applied — which is the whole difficulty. Sending
        // again would create a second precommit and pay a second fee.
        let benchmarks: Value = reqwest::get(format!(
            "{base}/get-benchmarks?block_id={}&player_id={POOL_PLAYER}",
            submitted.block_id
        ))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let precommits = benchmarks["precommits"].as_array().unwrap();

        // §10's next action: reconcile against confirmed state rather than
        // resend. It finds the pool's own write without another request.
        let found = reconcile_precommit(precommits, &submitted).unwrap();
        assert!(
            matches!(
                found,
                Reconciliation::Confirmed { .. } | Reconciliation::PendingConfirmation
            ),
            "reconciliation must find the write that was applied, got {found:?}"
        );
        assert_eq!(
            writes_received(&base).await,
            1,
            "reconciliation is a read; it must not send another write"
        );
    }

    #[tokio::test]
    async fn an_accepted_write_returns_the_benchmark_id() {
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("accepted");
        let submitted = submission(&base).await;
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();

        assert_eq!(result.outcome, AttemptOutcome::Accepted);
        assert!(result.benchmark_id.is_some());
        assert_eq!(result.http_status, Some(200));
        assert_eq!(writes_received(&base).await, 1);
    }

    #[tokio::test]
    async fn a_server_error_is_ambiguous_rather_than_rejected() {
        // §11 retries reads on 5xx; a write cannot, because the request may
        // have been applied before the failure. Classifying it as rejected
        // would licence a resend.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("fivehundred");
        let submitted = submission(&base).await;
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        inject(&base, "error").await;

        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, AttemptOutcome::Ambiguous);
        assert_eq!(result.http_status, Some(500));
    }

    #[tokio::test]
    async fn a_rate_limit_is_ambiguous_too() {
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("ratelimited");
        let submitted = submission(&base).await;
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        inject(&base, "rate_limit").await;

        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, AttemptOutcome::Ambiguous);
        assert_eq!(result.http_status, Some(429));
    }

    #[tokio::test]
    async fn a_deterministic_refusal_is_rejected_not_ambiguous() {
        // TIG answered and decided no. Nothing was applied, so this is not
        // the reconcile-don't-resend case.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("rejected");
        let submitted = submission(&base).await;
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        inject(&base, "reject").await;

        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, AttemptOutcome::Rejected);
        assert_eq!(result.http_status, Some(400));
    }

    #[tokio::test]
    async fn the_submitted_track_id_is_empty_because_tig_selects_it() {
        // §6.1. fake-tig refuses a non-empty one, so this is checked by the
        // server as well as by the shape below.
        let base = fake_tig().await;
        let submitted = submission(&base).await;
        let body = precommit_body(&submitted);

        assert_eq!(body["settings"]["track_id"], json!(""));
        let tracks = body["track_settings"].as_object().unwrap();
        assert_eq!(
            tracks.keys().collect::<Vec<_>>(),
            vec!["t001", "t002"],
            "every active track is submitted, not the one the pool prefers"
        );
        assert_eq!(tracks["t001"]["num_bundles"], json!(4));
    }

    #[tokio::test]
    async fn a_hyperparameter_keeps_the_type_the_pool_chose() {
        // §4 requires lossless numeric handling on this body, and §6.6 copies
        // the source benchmark's hyperparameters. Rendering a number as a
        // string here would submit a re-typed method — and the pool would
        // then be answering for work it did not describe.
        let base = fake_tig().await;
        let body = precommit_body(&submission(&base).await);
        let hyperparameters = &body["track_settings"]["t001"]["hyperparameters"];

        assert_eq!(hyperparameters["noise"], json!(0.15));
        assert_eq!(hyperparameters["restart_period"], json!(250));
        assert!(
            hyperparameters["restart_period"].is_number(),
            "a numeric hyperparameter must leave as a number"
        );
    }

    #[tokio::test]
    async fn a_submission_that_is_not_the_recorded_payload_is_never_sent() {
        // §7.3 records the payload digest before the send and requires an
        // explicit new generation for a changed payload. If the send did not
        // check it, the digest would describe one write and the socket carry
        // another — and §10 would later reconcile TIG's record against a
        // payload that was never transmitted.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("mismatched");
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let recorded = submission(&base).await;
        let mut changed = recorded.clone();
        changed
            .track_settings
            .get_mut("t001")
            .expect("the fixture offers t001")
            .num_bundles += 1;

        let error = tx
            .send(
                &permit(),
                &key,
                &intent_for(&recorded),
                &unresolved_attempt(),
                &changed,
            )
            .await
            .expect_err("a payload the intent does not describe must not be sent");

        assert!(matches!(
            error,
            TransmitError::PayloadNotTheRecordedOne { .. }
        ));
        assert_eq!(
            writes_received(&base).await,
            0,
            "the refusal happens before anything reaches TIG"
        );
    }

    #[tokio::test]
    async fn an_attempt_filed_under_another_intent_is_never_sent() {
        // The attempt row is what §10 reconciles from, and it is read
        // against its own intent's payload. Sending under a mismatched pair
        // files the evidence against the wrong write.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("otherintent");
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let submitted = submission(&base).await;
        let mut foreign = unresolved_attempt();
        foreign.intent_id = "44444444-4444-4444-4444-444444444444".to_string();

        let error = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &foreign,
                &submitted,
            )
            .await
            .expect_err("the attempt must belong to the intent being sent");

        assert!(matches!(error, TransmitError::AttemptNotForIntent { .. }));
        assert_eq!(writes_received(&base).await, 0);
    }

    #[tokio::test]
    async fn a_precommit_is_never_sent_against_a_benchmark_intent() {
        // The intent's kind decides which endpoint its digest and its
        // attempts belong to. A precommit body under a benchmark intent is
        // an unrecorded write whichever way it is later read.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("wrongkind");
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let submitted = submission(&base).await;
        let mut intent = intent_for(&submitted);
        intent.write_kind = WriteKind::Benchmark;
        intent.benchmark_id = Some("bench_a".to_string());

        let error = tx
            .send(&permit(), &key, &intent, &unresolved_attempt(), &submitted)
            .await
            .expect_err("this path sends precommits");

        assert!(matches!(error, TransmitError::NotAPrecommitIntent { .. }));
        assert_eq!(writes_received(&base).await, 0);
    }

    #[test]
    fn the_digest_covers_the_bytes_that_are_sent() {
        // The digest is only binding if it is taken over the body itself: a
        // digest of some other rendering would let the two drift apart while
        // still comparing equal.
        let mut a = PrecommitSubmission {
            player_id: POOL_PLAYER.to_string(),
            block_id: "block_100020".to_string(),
            challenge_id: "c001".to_string(),
            algorithm_id: "a011".to_string(),
            compute_type: "aws_c7g".to_string(),
            track_settings: BTreeMap::from([("t001".to_string(), track(4, 1_500_000))]),
        };
        let expected: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&precommit_body(&a)).unwrap()).into();
        assert_eq!(precommit_digest(&a), expected);

        // Every field the pool chose moves the digest, including a
        // hyperparameter's type.
        let mut b = a.clone();
        b.track_settings
            .get_mut("t001")
            .unwrap()
            .hyperparameters
            .insert("restart_period".to_string(), json!("250"));
        assert_ne!(
            precommit_digest(&a),
            precommit_digest(&b),
            "a re-typed hyperparameter is a different payload"
        );

        a.algorithm_id = "a012".to_string();
        assert_ne!(precommit_digest(&a), expected);
    }

    #[tokio::test]
    async fn an_answered_attempt_cannot_be_sent_against() {
        // One attempt row covers one request. §7.3 records a response once,
        // so reusing a resolved row would either overwrite that record or
        // leave the new request with no row — both lose the evidence §10
        // reconciles from.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("resolved");
        let submitted = submission(&base).await;
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let mut resolved = unresolved_attempt();
        resolved.outcome = Some(AttemptOutcome::Accepted);

        let err = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &resolved,
                &submitted,
            )
            .await
            .expect_err("a resolved attempt is refused");
        assert!(matches!(err, TransmitError::AttemptAlreadyAnswered { .. }));
        assert_eq!(
            writes_received(&base).await,
            0,
            "nothing was sent, so nothing reached TIG"
        );
    }

    #[tokio::test]
    async fn an_ambiguous_attempt_is_never_sent_against_again() {
        // The bug this guard exists for. AMBIGUOUS counts as *unresolved*
        // for §10's lane — the lane stays closed until reconciliation
        // settles it — so checking `is_unresolved()` would let a second
        // precommit go out on the very row that says the first one's
        // outcome is unknown. §11 forbids a replacement precommit while the
        // previous outcome is ambiguous, and the ledger could not record the
        // second response anyway.
        let base = fake_tig().await;
        let (key, _path) = synthetic_key("ambiguousrow");
        let submitted = submission(&base).await;
        let tx = PrecommitTransmitter::new(&base, &policy()).unwrap();
        let mut ambiguous = unresolved_attempt();
        ambiguous.outcome = Some(AttemptOutcome::Ambiguous);
        assert!(ambiguous.is_unresolved(), "still unresolved for the lane");

        let err = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &ambiguous,
                &submitted,
            )
            .await
            .expect_err("an ambiguous attempt is not a row to send against");
        assert!(matches!(err, TransmitError::AttemptAlreadyAnswered { .. }));
        assert_eq!(writes_received(&base).await, 0, "nothing reached TIG");
    }

    #[tokio::test]
    async fn a_success_without_a_benchmark_id_is_ambiguous() {
        // A 2xx is TIG accepting the request for processing, so the
        // precommit may exist and the fee may be paid. Only the id is
        // missing, and the id is what a later reconciliation recovers.
        let app = axum::Router::new().route(
            "/submit-precommit",
            axum::routing::post(|| async { axum::Json(json!({ "accepted": true })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (key, _path) = synthetic_key("noid");
        let tx = PrecommitTransmitter::new(format!("http://{addr}"), &policy()).unwrap();
        let submitted = PrecommitSubmission {
            player_id: POOL_PLAYER.to_string(),
            block_id: "block_1".to_string(),
            challenge_id: "c001".to_string(),
            algorithm_id: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings: BTreeMap::from([("t001".to_string(), track(4, 1_500_000))]),
        };
        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();

        assert_eq!(result.outcome, AttemptOutcome::Ambiguous);
        assert_eq!(result.http_status, Some(200));
        assert_eq!(result.benchmark_id, None);
    }

    #[tokio::test]
    async fn a_transport_detail_never_carries_the_endpoint_or_a_body() {
        // The detail is written to the attempt ledger, which is bounded and
        // must not carry response bytes. Nothing here is reached by pointing
        // at a closed port except the classification.
        let (key, _path) = synthetic_key("closed");
        let tx = PrecommitTransmitter::new("http://127.0.0.1:1", &policy()).unwrap();
        let submitted = PrecommitSubmission {
            player_id: POOL_PLAYER.to_string(),
            block_id: "block_1".to_string(),
            challenge_id: "c001".to_string(),
            algorithm_id: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings: BTreeMap::from([("t001".to_string(), track(4, 1_500_000))]),
        };
        let result = tx
            .send(
                &permit(),
                &key,
                &intent_for(&submitted),
                &unresolved_attempt(),
                &submitted,
            )
            .await
            .unwrap();

        assert_eq!(result.outcome, AttemptOutcome::Ambiguous);
        assert!(
            !result.detail.contains("127.0.0.1"),
            "got: {}",
            result.detail
        );
        assert!(result.detail.len() <= 60, "got: {}", result.detail);
    }
}
