//! DECIDED -> PRECOMMIT_CONFIRMED, from a real server's confirmed read.
//!
//! The one transition the restart pass cannot make, exercised the way the
//! product will make it: a decision is admitted, its precommit is sent to a
//! live fake-tig, TIG confirms it on the next block, and the controller binds
//! the confirmed entry to the workflow by searching on the exact settings it
//! submitted.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use fake_tig::{Config, DEFAULT_API_KEY, build_world, router};
use http_body_util::BodyExt;
use pool_controller::bind::{Bound, bind_confirmed_precommits};
use pool_domain::{Network, challenge_tie_seed, draw_rank};
use pool_test_support::TempDb;
use pool_workflow::{
    AnchorSnapshot, AttemptOutcome, DecisionPayloadInputs, IntentState, NewDecision,
    PostgresAttemptLedger, PostgresIntentRepository, PrecommitSubmission, RecordedDraw,
    TigWriteIntentRepository, WorkflowState, WriteAttemptLedger, WriteKind, admit_precommit,
    precommit_body, precommit_digest, workflow,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";
const ANCHOR: &str = "block_100080";
const ANCHOR_DIGEST: [u8; 32] = [0xcd; 32];
const NET: Network = Network::Testnet;

fn app() -> Router {
    let dir = format!("{}/../../fixtures/tig/v1", env!("CARGO_MANIFEST_DIR"));
    router(build_world(Config::new(dir)).expect("fixture loads"))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> Value {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        b = b.header("x-api-key", k);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        status.is_success(),
        "{method} {uri} -> {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn track_settings() -> Value {
    json!({
        "t001": { "num_bundles": 2, "fuel_budget": 1_000_000u64, "hyperparameters": null },
        "t002": { "num_bundles": 1, "fuel_budget": 1_000_000u64, "hyperparameters": null }
    })
}

fn submission() -> PrecommitSubmission {
    PrecommitSubmission::from_decision(
        PLAYER,
        &DecisionPayloadInputs {
            anchor_block_id: ANCHOR.to_string(),
            selected_challenge: "c001".to_string(),
            selected_algorithm: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings: track_settings(),
        },
    )
    .unwrap()
}

fn decision(workflow: &str, generation: i32) -> NewDecision {
    let seed = challenge_tie_seed(NET, ANCHOR);
    let mut draw_ranks = serde_json::Map::new();
    for c in ["c001", "c002", "c003"] {
        let rank = draw_rank(&seed, c);
        draw_ranks.insert(
            c.to_string(),
            json!(
                rank.as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
        );
    }
    NewDecision {
        network: NET,
        workflow_id: workflow.to_string(),
        generation,
        anchor: AnchorSnapshot {
            block_id: ANCHOR.to_string(),
            content_digest: ANCHOR_DIGEST,
            height: 100_080,
        },
        draw: RecordedDraw {
            domain: pool_domain::CHALLENGE_TIE_DOMAIN.to_string(),
            draw_ranks,
            tie: None,
        },
        selected_challenge: "c001".to_string(),
        selected_algorithm: "a011".to_string(),
        compute_type: "aws_t4g".to_string(),
        track_settings: track_settings(),
        reserve_inputs: json!({}),
        precommit_reserve: "0".to_string(),
        config_digest: [0xef; 32],
        payload_digest: precommit_digest(&submission()),
    }
}

async fn persist_anchor(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO pool.block_snapshot
             (network, block_id, content_digest, height, reads_complete, active_cache_ready)
         VALUES ('testnet', $1, $2, 100080, true, true)",
    )
    .bind(ANCHOR)
    .bind(ANCHOR_DIGEST.as_slice())
    .execute(pool)
    .await
    .unwrap();
}

/// What the gateway does: record the attempt, post the exact body, record
/// the response. Done here by hand because the controller crate cannot
/// depend on the gateway, and because doing it by hand is what proves the
/// controller binds from the *read* and not from anything the send returned.
async fn send_precommit(app: &Router, gateway: &sqlx::PgPool, intent_id: &str) -> Value {
    let ledger = PostgresAttemptLedger::new(gateway.clone());
    let attempt = ledger.begin(intent_id).await.unwrap();
    let resp = call(
        app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body(&submission())),
    )
    .await;
    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Accepted,
            Some(200),
            Some("accepted"),
        )
        .await
        .unwrap();
    resp
}

async fn precommits(app: &Router) -> Vec<Value> {
    let block = call(app, "GET", "/get-block?include_data=true", None, None).await;
    let block_id = block["block"]["id"].as_str().unwrap().to_owned();
    let body = call(
        app,
        "GET",
        &format!("/get-benchmarks?block_id={block_id}&player_id={PLAYER}"),
        None,
        None,
    )
    .await;
    body["precommits"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn a_sent_precommit_is_bound_from_the_confirmed_read_and_its_intent_settles() {
    let Some(db) = TempDb::migrated("bind_confirmed").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    persist_anchor(&controller).await;
    let app = app();

    let admitted = admit_precommit(&controller, &decision("w1", 1), 4)
        .await
        .unwrap();
    assert_eq!(admitted.intent.state, IntentState::Prepared);

    // Nothing sent: nothing to search for, and the pass says nothing.
    let before = bind_confirmed_precommits(&controller, NET, PLAYER, &precommits(&app).await)
        .await
        .unwrap();
    assert!(before.outcomes.is_empty(), "{before:?}");

    // Sent and accepted, but not yet confirmed: the entry is listed with a
    // null block_confirmed, and §7 says that is not evidence.
    let resp = send_precommit(&app, &gateway, &admitted.intent.intent_id).await;
    let response_id = resp["benchmark_id"].as_str().unwrap().to_owned();
    let pending = bind_confirmed_precommits(&controller, NET, PLAYER, &precommits(&app).await)
        .await
        .unwrap();
    assert_eq!(
        pending.outcomes,
        vec![Bound::Pending {
            workflow_id: "w1".to_string()
        }]
    );
    let w = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        w.state,
        WorkflowState::Decided,
        "an accepted response is not a confirmation"
    );

    // TIG confirms on the next block. Now the read is the evidence.
    call(&app, "POST", "/_fake/advance-block", None, None).await;
    let bound = bind_confirmed_precommits(&controller, NET, PLAYER, &precommits(&app).await)
        .await
        .unwrap();
    assert_eq!(
        bound.outcomes,
        vec![Bound::Confirmed {
            workflow_id: "w1".to_string(),
            benchmark_id: response_id.clone()
        }],
        "found by its settings, and it is the id the lost response would have carried"
    );

    let w = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w.state, WorkflowState::PrecommitConfirmed);
    assert_eq!(w.benchmark_id.as_deref(), Some(response_id.as_str()));
    assert!(
        w.block_started.is_some(),
        "§8's deadlines are ages from block_started"
    );
    // §6.2 builds the commitment to exactly this length, and it is a
    // *detail* — it is not in `confirmed_settings`, so binding has to carry
    // it across explicitly or the length check has nothing to read.
    let published = precommits(&app)
        .await
        .into_iter()
        .find(|p| p["benchmark_id"] == json!(response_id))
        .expect("the entry that was just bound");
    assert_eq!(
        w.confirmed_num_nonces,
        published["details"]["num_nonces"].as_i64(),
        "the confirmed nonce count must reach the workflow"
    );
    assert!(w.confirmed_num_nonces.is_some(), "and TIG published one");
    assert!(
        w.confirmed_settings
            .as_ref()
            .and_then(|s| s.get("num_nonces"))
            .is_none(),
        "settings and details are disjoint; num_nonces is a detail"
    );
    // TIG chose the track; the confirmed settings replace the proposed ones.
    assert!(matches!(
        w.confirmed_track_id.as_deref(),
        Some("t001") | Some("t002")
    ));

    let intent = PostgresIntentRepository::new(controller.clone())
        .find(NET, "w1", WriteKind::Precommit, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        intent.state,
        IntentState::Confirmed,
        "settled from the same read"
    );

    // Idempotent: a second pass finds nothing DECIDED and changes nothing.
    let again = bind_confirmed_precommits(&controller, NET, PLAYER, &precommits(&app).await)
        .await
        .unwrap();
    assert!(again.outcomes.is_empty(), "{again:?}");
}

#[tokio::test]
async fn a_workflow_that_never_sent_is_not_searched_for() {
    // Searching for a workflow that submitted nothing invites the tuple search
    // to bind someone else's confirmed precommit to it — the same settings
    // could be submitted by two decisions — which is a second owner for one
    // benchmark against §10 invariant 1.
    let Some(db) = TempDb::migrated("bind_unsent").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    persist_anchor(&controller).await;
    let app = app();

    // w1 sends and confirms; w2 decided the same thing and sent nothing.
    let a = admit_precommit(&controller, &decision("w1", 1), 4)
        .await
        .unwrap();
    admit_precommit(&controller, &decision("w2", 1), 4)
        .await
        .unwrap();
    send_precommit(&app, &gateway, &a.intent.intent_id).await;
    call(&app, "POST", "/_fake/advance-block", None, None).await;

    let report = bind_confirmed_precommits(&controller, NET, PLAYER, &precommits(&app).await)
        .await
        .unwrap();
    assert_eq!(
        report.outcomes.len(),
        1,
        "only the workflow that sent is looked at: {report:?}"
    );
    assert!(
        matches!(&report.outcomes[0], Bound::Confirmed { workflow_id, .. } if workflow_id == "w1")
    );

    let w2 = workflow::find(&controller, NET, "w2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w2.state, WorkflowState::Decided);
    assert_eq!(w2.benchmark_id, None, "w1's benchmark did not become w2's");
}

#[tokio::test]
async fn two_sent_workflows_with_identical_settings_stop_rather_than_guess() {
    // §10: "stops for operator resolution if more than one candidate matches."
    // Two workflows that both sent the same settings each match both
    // confirmed entries, and neither can tell which is its own. Guessing
    // attributes a benchmark — and its fees and rewards — to a workflow that
    // may not own it.
    let Some(db) = TempDb::migrated("bind_ambiguous").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    persist_anchor(&controller).await;
    let app = app();

    let a = admit_precommit(&controller, &decision("w1", 1), 4)
        .await
        .unwrap();
    let b = admit_precommit(&controller, &decision("w2", 1), 4)
        .await
        .unwrap();
    send_precommit(&app, &gateway, &a.intent.intent_id).await;
    // The lane admits one unresolved precommit at a time; a's is resolved.
    send_precommit(&app, &gateway, &b.intent.intent_id).await;
    call(&app, "POST", "/_fake/advance-block", None, None).await;

    let report = bind_confirmed_precommits(&controller, NET, PLAYER, &precommits(&app).await)
        .await
        .unwrap();
    assert_eq!(report.outcomes.len(), 2);
    for o in &report.outcomes {
        assert!(matches!(o, Bound::StopForOperator { .. }), "{o:?}");
    }
    for id in ["w1", "w2"] {
        let w = workflow::find(&controller, NET, id).await.unwrap().unwrap();
        assert_eq!(w.state, WorkflowState::Decided, "{id} was not guessed");
    }
    let _ = BTreeMap::<String, String>::new();
}
