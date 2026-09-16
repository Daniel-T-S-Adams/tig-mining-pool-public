//! One block in, one block reconciled — the controller's pass, end to end
//! against a live fake-tig.
//!
//! `ingest` and `reconcile_block` are exercised the way the binary will run
//! them: a real snapshot assembled over HTTP, persisted, then read back as
//! evidence for the workflows the database holds. The precommit is sent by
//! hand, as `bind.rs` does, because the controller cannot depend on the
//! gateway.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use fake_tig::{Config, DEFAULT_API_KEY, SharedWorld, build_world, router};
use http_body_util::BodyExt;
use pool_controller::bind::Bound;
use pool_controller::ingest::{IngestError, Ingested, Ingestor};
use pool_controller::reconciler::{Outcome, reconcile_block};
use pool_domain::{Network, challenge_tie_seed, draw_rank};
use pool_snapshot::active_cache::BenchmarkDataSource;
use pool_snapshot::store::{
    BlockSnapshotStore, NotUsable, PersistedSnapshot, SnapshotRecord, StoreError,
};
use pool_snapshot::{
    AnchoredRead, PostgresActiveBenchmarkStore, PostgresSnapshotStore, Snapshot, SnapshotError,
    SnapshotSource, TigSnapshotSource,
};
use pool_test_support::TempDb;
use pool_workflow::restart::{self, ConfirmedWindow};
use pool_workflow::{
    AnchorSnapshot, AttemptOutcome, DecisionPayloadInputs, Guardrails, IntentState, NewDecision,
    PostgresAttemptLedger, PostgresIntentRepository, PrecommitSubmission, RecordedDraw,
    TigWriteIntentRepository, WorkflowState, WriteAttemptLedger, WriteKind, admit_precommit,
    open_block_gaps, precommit_body, precommit_digest, workflow,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tig_client::{ReadLimits, TigReadClient};
use tower::ServiceExt;

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";
const NET: Network = Network::Testnet;

/// Loose enough that nothing expires unless a test drives the chain there.
fn guardrails() -> Guardrails {
    Guardrails {
        max_assignment_age_blocks: 2,
        package_due_age_blocks: 3,
        workflow_expiry_age_blocks: 5,
        proof_reserve_blocks: 2,
    }
}

struct FakeTig {
    app: Router,
    world: SharedWorld,
    /// One source for the test's life, as the binary keeps one for the
    /// process's: §9 caches each read by its key for the life of a block, so
    /// two assemblies of one block read the same `get-benchmarks` and
    /// persist the same content. A fresh source per assembly would re-read
    /// a latest-state endpoint and could produce a second, different
    /// usable snapshot for the block, which the store refuses as divergent.
    source: TigSnapshotSource,
}

async fn fake_tig() -> FakeTig {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tig/v1");
    let world = build_world(Config::new(fixtures)).expect("fixture world loads");
    let app = router(world.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let policy = tig_client::testing::shipped_policy_for_test().unwrap();
    let limits = ReadLimits {
        max_backoff: Duration::from_millis(50),
        ..tig_client::testing::pool_ceiling_for_test(&policy)
    };
    let client =
        TigReadClient::new_unrestricted_for_test(format!("http://{addr}"), &policy, limits)
            .unwrap();
    FakeTig {
        app,
        world,
        source: TigSnapshotSource::new(client, PLAYER),
    }
}

impl FakeTig {
    fn source(&self) -> &TigSnapshotSource {
        &self.source
    }

    /// The chain's current block, as the server reports it.
    async fn tip(&self) -> (String, u64) {
        let block = call(&self.app, "GET", "/get-block?include_data=true", None, None).await;
        (
            block["block"]["id"].as_str().unwrap().to_owned(),
            block["block"]["details"]["height"].as_u64().unwrap(),
        )
    }

    fn advance(&self, blocks: u32) {
        let mut world = self.world.lock().unwrap();
        for _ in 0..blocks {
            world.advance_block();
        }
    }
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

fn submission(anchor: &str) -> PrecommitSubmission {
    PrecommitSubmission::from_decision(
        PLAYER,
        &DecisionPayloadInputs {
            anchor_block_id: anchor.to_string(),
            selected_challenge: "c001".to_string(),
            selected_algorithm: "a011".to_string(),
            compute_type: "aws_t4g".to_string(),
            track_settings: track_settings(),
        },
    )
    .unwrap()
}

fn decision(workflow: &str, anchor: &Anchor) -> NewDecision {
    let seed = challenge_tie_seed(NET, &anchor.block_id);
    let mut draw_ranks = serde_json::Map::new();
    for c in ["c001", "c002", "c003"] {
        let rank = draw_rank(&seed, c);
        draw_ranks.insert(c.to_string(), json!(rank.to_hex()));
    }
    NewDecision {
        network: NET,
        workflow_id: workflow.to_string(),
        generation: 1,
        anchor: AnchorSnapshot {
            block_id: anchor.block_id.clone(),
            content_digest: anchor.digest,
            height: anchor.height,
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
        trace_id: None,
        payload_digest: precommit_digest(&submission(&anchor.block_id)),
    }
}

/// A decision anchor at the chain's current block: the real thing, ingested.
///
/// Admission requires the newest usable persisted snapshot (D2e), and with
/// the fixture's empty active set the cache is complete on the first pass,
/// so the ingested snapshot is usable and anchors the decisions below.
async fn ingest_anchor(pool: &PgPool, tig: &FakeTig) -> Anchor {
    let ingested = ingest_live(pool, tig).await;
    assert!(
        ingested.snapshot.for_decision().is_ok(),
        "the fixture's active set is empty, so the first pass is usable"
    );
    let record = ingested.snapshot.record();
    Anchor {
        block_id: record.block_id.clone(),
        height: i64::try_from(record.height).unwrap(),
        digest: record.content_digest,
    }
}

struct Anchor {
    block_id: String,
    height: i64,
    digest: [u8; 32],
}

/// What the gateway does: record the attempt, post the exact body, record
/// the response.
async fn send_precommit(tig: &FakeTig, gateway: &PgPool, intent_id: &str, anchor: &str) -> String {
    let ledger = PostgresAttemptLedger::new(gateway.clone());
    let attempt = ledger.begin(intent_id).await.unwrap();
    let resp = call(
        &tig.app,
        "POST",
        "/submit-precommit",
        Some(DEFAULT_API_KEY),
        Some(precommit_body(&submission(anchor))),
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
    resp["benchmark_id"].as_str().unwrap().to_owned()
}

async fn ingest_with<S>(pool: &PgPool, source: &S) -> Result<Ingested, IngestError>
where
    S: SnapshotSource + BenchmarkDataSource,
{
    let store = PostgresSnapshotStore::new(pool.clone());
    let cache = PostgresActiveBenchmarkStore::new(pool.clone());
    Ingestor {
        pool,
        store: &store,
        cache: &cache,
        source,
        network: NET,
        assembly_attempts: 3,
        cache_budget: 10,
    }
    .ingest()
    .await
}

async fn ingest_live(pool: &PgPool, tig: &FakeTig) -> Ingested {
    ingest_with(pool, tig.source())
        .await
        .expect("ingests against fake-tig")
}

async fn state(pool: &PgPool, id: &str) -> WorkflowState {
    workflow::find(pool, NET, id).await.unwrap().unwrap().state
}

#[tokio::test]
async fn a_block_carrying_a_confirmation_advances_the_workflow_that_sent_it() {
    let Some(db) = TempDb::migrated("tick_confirms").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    let tig = fake_tig().await;
    let anchor = ingest_anchor(&controller, &tig).await;

    // w1 sends; w2 decided and sent nothing.
    let a = admit_precommit(&controller, &decision("w1", &anchor), 4)
        .await
        .unwrap();
    admit_precommit(&controller, &decision("w2", &anchor), 4)
        .await
        .unwrap();
    let benchmark_id = send_precommit(&tig, &gateway, &a.intent.intent_id, &anchor.block_id).await;

    // The same block again. §9 caches every read by its key for the life
    // of a block, so this assembly reads the `get-benchmarks` taken before
    // the send: the precommit is not listed yet, and the pass finds
    // nothing. (Fresh reads would list it unconfirmed, which §7 says is
    // not evidence either.) Nothing moves.
    let ingested = ingest_live(&controller, &tig).await;
    assert!(ingested.gaps_recorded.is_empty());
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &guardrails(), &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    assert_eq!(
        report.bind.outcomes,
        vec![Bound::NotFound {
            workflow_id: "w1".to_string()
        }]
    );
    assert_eq!(state(&controller, "w1").await, WorkflowState::Decided);
    assert!(!report.blocks_claiming(), "{:?}", report.needs_attention());

    // The next block confirms it. One pass binds the workflow, settles its
    // intent, and leaves the unsent one where it was.
    tig.advance(1);
    let ingested = ingest_live(&controller, &tig).await;
    assert!(
        ingested.gaps_recorded.is_empty(),
        "consecutive blocks are not a gap"
    );
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &guardrails(), &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    assert_eq!(report.height, anchor.height + 1);
    assert_eq!(
        report.bind.outcomes,
        vec![Bound::Confirmed {
            workflow_id: "w1".to_string(),
            benchmark_id: benchmark_id.clone()
        }]
    );
    assert!(report.expired.is_empty(), "{:?}", report.expired);
    assert!(!report.blocks_claiming(), "{:?}", report.needs_attention());

    let w1 = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w1.state, WorkflowState::PrecommitConfirmed);
    assert_eq!(w1.benchmark_id.as_deref(), Some(benchmark_id.as_str()));
    assert_eq!(state(&controller, "w2").await, WorkflowState::Decided);
    let intent = PostgresIntentRepository::new(controller.clone())
        .find(NET, "w1", WriteKind::Precommit, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.state, IntentState::Confirmed);

    // The bound workflow is now the restart pass's to carry, and with no
    // further evidence it stays put: a second pass changes nothing.
    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &guardrails(), &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    assert!(report.bind.outcomes.is_empty(), "{:?}", report.bind);
    assert!(report.restart.advanced.is_empty(), "{:?}", report.restart);
    assert_eq!(
        report.restart.unchanged,
        vec!["w1".to_string(), "w2".to_string()],
        "both are nonterminal, and neither has evidence to move on"
    );
    assert_eq!(
        state(&controller, "w1").await,
        WorkflowState::PrecommitConfirmed
    );
}

#[tokio::test]
async fn a_workflow_the_binding_stopped_on_is_not_expired_by_the_clock() {
    // Two workflows decide the same settings and both send. §10's tuple
    // search then matches both against one confirmed precommit and stops for
    // an operator. Their attempts are ACCEPTED, so the in-flight guard does
    // not withhold them — and once the anchor ages past §8's guardrail the
    // pool's clock would terminate workflows TIG has confirmed, under a
    // reason code meaning the work never began, while the operator is still
    // untangling them. The deadline is withheld instead.
    let Some(db) = TempDb::migrated("tick_stop_no_expiry").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    let tig = fake_tig().await;
    let anchor = ingest_anchor(&controller, &tig).await;
    let g = guardrails();

    for w in ["w1", "w2"] {
        let admitted = admit_precommit(&controller, &decision(w, &anchor), 4)
            .await
            .unwrap();
        send_precommit(&tig, &gateway, &admitted.intent.intent_id, &anchor.block_id).await;
    }
    // Both sends confirm as separate precommits with identical settings, so
    // the search finds two candidates for each workflow.
    tig.advance(g.workflow_expiry_age_blocks);

    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &g, &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    assert!(
        report
            .bind
            .outcomes
            .iter()
            .all(|o| matches!(o, Bound::StopForOperator { .. })),
        "{:?}",
        report.bind
    );
    assert!(report.expired.is_empty(), "{:?}", report.expired);
    assert_eq!(
        report.expiry_withheld,
        vec!["w1".to_string(), "w2".to_string()]
    );
    assert!(report.blocks_claiming());
    for w in ["w1", "w2"] {
        assert_eq!(state(&controller, w).await, WorkflowState::Decided);
    }
}

#[tokio::test]
async fn an_unsent_decision_expires_when_the_guardrail_passes_and_not_before() {
    let Some(db) = TempDb::migrated("tick_expires").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let anchor = ingest_anchor(&controller, &tig).await;
    admit_precommit(&controller, &decision("w1", &anchor), 4)
        .await
        .unwrap();
    let g = guardrails();

    // One short of the guardrail: still unfinished, still allowed.
    tig.advance(g.workflow_expiry_age_blocks - 1);
    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &g, &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    assert!(report.expired.is_empty(), "{:?}", report.expired);
    assert_eq!(state(&controller, "w1").await, WorkflowState::Decided);

    // At the guardrail: expired by the pass, with the reason on the row.
    tig.advance(1);
    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &g, &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    assert_eq!(report.expired, vec!["w1".to_string()]);
    let w1 = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w1.state, WorkflowState::Expired);
    assert!(w1.terminal_reason.is_some());
    assert!(!report.blocks_claiming(), "{:?}", report.needs_attention());
}

#[tokio::test]
async fn missed_blocks_are_recorded_as_a_gap_once() {
    let Some(db) = TempDb::migrated("tick_gap").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let first = i64::try_from(tig.tip().await.1).unwrap();

    // The first block ever observed is not preceded by a gap.
    let ingested = ingest_live(&controller, &tig).await;
    assert!(ingested.gaps_recorded.is_empty());
    assert!(open_block_gaps(&controller, NET).await.unwrap().is_empty());

    // Two blocks pass; the pool sees the second. The one between is lost.
    tig.advance(2);
    let ingested = ingest_live(&controller, &tig).await;
    assert_eq!(ingested.gaps_recorded, vec![first + 1]);
    assert_eq!(
        open_block_gaps(&controller, NET).await.unwrap(),
        vec![first + 1]
    );

    // Seeing the same block again re-records nothing.
    let ingested = ingest_live(&controller, &tig).await;
    assert!(
        ingested.gaps_recorded.is_empty(),
        "{:?}",
        ingested.gaps_recorded
    );

    // And the next consecutive block is not a gap.
    tig.advance(1);
    let ingested = ingest_live(&controller, &tig).await;
    assert!(ingested.gaps_recorded.is_empty());
    assert_eq!(
        open_block_gaps(&controller, NET).await.unwrap(),
        vec![first + 1],
        "the earlier gap stays open until an operator resolves it"
    );
}

/// A store that knows a last height and cannot persist.
struct DeadStore {
    last: Option<u64>,
}

impl BlockSnapshotStore for DeadStore {
    async fn persist(&self, _: Network, _: Snapshot) -> Result<PersistedSnapshot, StoreError> {
        Err(StoreError::Unavailable("the store is down".to_string()))
    }

    async fn load_usable_record(
        &self,
        _: Network,
        _: &str,
    ) -> Result<Option<SnapshotRecord>, StoreError> {
        Ok(None)
    }

    async fn last_local_height(&self, _: Network) -> Result<Option<u64>, StoreError> {
        Ok(self.last)
    }
}

#[tokio::test]
async fn the_gap_is_recorded_before_the_snapshot_that_revealed_it() {
    // "Last local height" is read from the persisted snapshots, so persist
    // first and crash, and the next run measures from the new height with
    // nothing recorded for the ones between. Gap first, and there is no
    // order in which the loss goes unrecorded. Staged with a store that
    // fails to persist: the gap must be on record anyway.
    let Some(db) = TempDb::migrated("tick_gap_order").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let tig = fake_tig().await;
    let observed = i64::try_from(tig.tip().await.1).unwrap();
    let store = DeadStore {
        last: Some(u64::try_from(observed - 3).unwrap()),
    };

    let cache = PostgresActiveBenchmarkStore::new(controller.clone());
    let err = Ingestor {
        pool: &controller,
        store: &store,
        cache: &cache,
        source: tig.source(),
        network: NET,
        assembly_attempts: 3,
        cache_budget: 10,
    }
    .ingest()
    .await
    .expect_err("the store refuses");
    assert!(matches!(err, IngestError::Store(_)), "{err}");
    assert_eq!(
        open_block_gaps(&controller, NET).await.unwrap(),
        vec![observed - 2, observed - 1],
        "the gap was recorded even though the snapshot was not"
    );
}

/// A source that cannot make one read.
struct Blindfolded<'a> {
    inner: &'a TigSnapshotSource,
    missing: AnchoredRead,
}

impl SnapshotSource for Blindfolded<'_> {
    async fn get_block(&self) -> Result<Value, SnapshotError> {
        self.inner.get_block().await
    }

    async fn get_anchored(
        &self,
        read: AnchoredRead,
        block_id: &str,
    ) -> Result<Value, SnapshotError> {
        if read == self.missing {
            return Err(SnapshotError::Unavailable {
                endpoint: read.endpoint().to_string(),
                reason: "staged outage".to_string(),
            });
        }
        self.inner.get_anchored(read, block_id).await
    }

    async fn get_tracks(&self, challenge_id: &str, block_id: &str) -> Result<Value, SnapshotError> {
        self.inner.get_tracks(challenge_id, block_id).await
    }
}

impl BenchmarkDataSource for Blindfolded<'_> {
    async fn get_benchmark_data(&self, benchmark_id: &str) -> Result<Value, SnapshotError> {
        self.inner.get_benchmark_data(benchmark_id).await
    }
}

#[tokio::test]
async fn a_snapshot_missing_a_read_is_persisted_but_reconciles_nothing() {
    // C5 for the reconciler: a read that was not made is not evidence of
    // absence. Staged at the block that would have confirmed w1 and expired
    // w2 — with get-benchmarks unavailable, the pass must do neither.
    let Some(db) = TempDb::migrated("tick_blind").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    let tig = fake_tig().await;
    let anchor = ingest_anchor(&controller, &tig).await;
    let a = admit_precommit(&controller, &decision("w1", &anchor), 4)
        .await
        .unwrap();
    send_precommit(&tig, &gateway, &a.intent.intent_id, &anchor.block_id).await;
    admit_precommit(&controller, &decision("w2", &anchor), 4)
        .await
        .unwrap();
    let g = guardrails();
    tig.advance(g.workflow_expiry_age_blocks);

    let blind = Blindfolded {
        inner: tig.source(),
        missing: AnchoredRead::Benchmarks,
    };
    let ingested = ingest_with(&controller, &blind)
        .await
        .expect("an incomplete snapshot is still accepted and persisted");
    assert!(!ingested.snapshot.record().reads_complete);

    let outcome = reconcile_block(&controller, NET, PLAYER, &g, &ingested.snapshot)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            Outcome::Blind {
                reason: NotUsable::ReadsIncomplete,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(state(&controller, "w1").await, WorkflowState::Decided);
    assert_eq!(state(&controller, "w2").await, WorkflowState::Decided);

    // The same block, fully read: now everything the block carries happens,
    // in the pass's order. w1 is bound first — TIG started its benchmark at
    // the anchor, so at this height it is exactly at the guardrail too — and
    // then expired, carrying the benchmark_id the binding gave it. Expiry
    // before binding would have left an expired row with no id, and §10's
    // tuple search would later find a confirmed precommit with no owner.
    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(report) =
        reconcile_block(&controller, NET, PLAYER, &g, &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    let [
        Bound::Confirmed {
            workflow_id,
            benchmark_id,
        },
    ] = report.bind.outcomes.as_slice()
    else {
        panic!("{:?}", report.bind);
    };
    assert_eq!(workflow_id, "w1");
    assert_eq!(report.expired, vec!["w1".to_string(), "w2".to_string()]);
    let w1 = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w1.state, WorkflowState::Expired);
    assert_eq!(w1.benchmark_id.as_deref(), Some(benchmark_id.as_str()));
    assert_eq!(state(&controller, "w2").await, WorkflowState::Expired);
}

// ---- What blocks claiming, with no database ---------------------------------

fn report_with(
    bind: Vec<Bound>,
    attention: Vec<pool_workflow::NeedsAttention>,
    expiry_failed: Vec<(String, String)>,
) -> pool_controller::reconciler::BlockReport {
    pool_controller::reconciler::BlockReport {
        block_id: "block_1".to_string(),
        height: 1,
        bind: pool_controller::bind::BindReport { outcomes: bind },
        restart: pool_workflow::RestartReport {
            advanced: Vec::new(),
            unchanged: Vec::new(),
            needs_attention: attention,
        },
        expired: Vec::new(),
        expiry_withheld: Vec::new(),
        expiry_failed,
    }
}

#[test]
fn a_precommit_the_pool_cannot_bind_blocks_claiming() {
    // Several confirmed candidates for one decision is the duplicate §10
    // exists to prevent; claiming more work on top of it compounds what an
    // operator has to untangle. A workflow the pass could not evaluate is
    // one whose state the pool does not have.
    for outcome in [
        Bound::StopForOperator {
            workflow_id: "w1".to_string(),
            reason: "2 candidates".to_string(),
        },
        Bound::Failed {
            workflow_id: "w1".to_string(),
            error: "db".to_string(),
        },
    ] {
        let report = report_with(vec![outcome.clone()], Vec::new(), Vec::new());
        assert!(report.blocks_claiming(), "{outcome:?}");
        assert_eq!(report.needs_attention().len(), 1, "{outcome:?}");
    }
}

#[test]
fn an_unconfirmed_or_absent_precommit_does_not_block_claiming() {
    // Each names a workflow whose state is known and merely unconfirmed.
    for outcome in [
        Bound::Pending {
            workflow_id: "w1".to_string(),
        },
        Bound::NotFound {
            workflow_id: "w1".to_string(),
        },
        Bound::Confirmed {
            workflow_id: "w1".to_string(),
            benchmark_id: "b1".to_string(),
        },
    ] {
        let report = report_with(vec![outcome.clone()], Vec::new(), Vec::new());
        assert!(!report.blocks_claiming(), "{outcome:?}");
        assert!(report.needs_attention().is_empty(), "{outcome:?}");
    }
}

#[test]
fn a_failed_expiry_sweep_blocks_claiming() {
    let report = report_with(
        Vec::new(),
        Vec::new(),
        vec![("w1".to_string(), "db".to_string())],
    );
    assert!(report.blocks_claiming());
    assert_eq!(report.needs_attention().len(), 1);
}

#[test]
fn the_restart_pass_keeps_its_own_say() {
    // The restart report decides which of its buckets block; this pass
    // relays that rather than re-deciding it, and lists every bucket for
    // the operator whether or not it blocks.
    let blocking = report_with(
        Vec::new(),
        vec![pool_workflow::NeedsAttention::Failed {
            workflow_id: "w1".to_string(),
            error: "db".to_string(),
        }],
        Vec::new(),
    );
    assert!(blocking.blocks_claiming());

    let informational = report_with(
        Vec::new(),
        vec![pool_workflow::NeedsAttention::OutsideWindow {
            workflow_id: "w1".to_string(),
            benchmark_id: "b1".to_string(),
        }],
        Vec::new(),
    );
    assert!(!informational.blocks_claiming());
    assert_eq!(informational.needs_attention().len(), 1);
}

#[tokio::test]
async fn a_crash_after_tig_changed_state_is_recovered_by_the_controller_monotonically() {
    // **G2's fourth crash point, controller half.** `architecture.md` §12:
    // "Controller dies after TIG changes state: reconciliation advances
    // monotonically from confirmed TIG evidence."
    //
    // The gateway's crash tests settle the *attempt* from the confirmed read,
    // which is what reopens §10's serialized lane. Advancing the *workflow*
    // from that same evidence is §6's reconciler, and until this test it was
    // the one half of G2 nothing asserted — the plan says so itself.
    //
    // The crash is staged, not simulated with a signal: what a controller
    // crash at this point leaves is a durable state — TIG holds a confirmed
    // precommit and the local workflow has not moved — and that state is
    // reached here by sending, letting TIG confirm, and then reconciling from
    // a *fresh* pass with nothing carried in memory, which is what a restarted
    // process has.
    let Some(db) = TempDb::migrated("tick_crash_point_four").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let gateway = db.pool_as("pool_gateway").await;
    let tig = fake_tig().await;
    let anchor = ingest_anchor(&controller, &tig).await;

    let admitted = admit_precommit(&controller, &decision("w1", &anchor), 4)
        .await
        .unwrap();
    let benchmark_id =
        send_precommit(&tig, &gateway, &admitted.intent.intent_id, &anchor.block_id).await;

    // TIG's state changes. The controller does nothing — this is the window
    // the crash sits in.
    tig.advance(1);
    assert_eq!(
        state(&controller, "w1").await,
        WorkflowState::Decided,
        "the premise: TIG has confirmed and the pool has not noticed"
    );

    // A fresh pass, as a restarted controller makes.
    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(first) =
        reconcile_block(&controller, NET, PLAYER, &guardrails(), &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };

    assert_eq!(
        state(&controller, "w1").await,
        WorkflowState::PrecommitConfirmed,
        "§12: reconciliation advances from confirmed TIG evidence"
    );
    assert!(
        !first.blocks_claiming(),
        "an ordinary recovery is not an operator condition: {:?}",
        first.needs_attention()
    );

    // Monotonic, in all three of its senses.
    //
    // It advanced **as far as the evidence supports and no further**: TIG has
    // confirmed the precommit and nothing else, so a workflow that had run on
    // to BENCHMARK_CONFIRMED would have invented a confirmation no read
    // carried.
    let w = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w.benchmark_id.as_deref(), Some(benchmark_id.as_str()));
    assert!(
        w.confirmed_track_id.is_some(),
        "F2: TIG's settings replace the proposed ones: {w:?}"
    );

    // It does **not advance twice** on the same evidence. A second pass over
    // an unchanged window is what every subsequent poll is, and a transition
    // that re-fired would move the workflow on a read it had already consumed.
    let revision_after_first = w.revision;
    let ingested = ingest_live(&controller, &tig).await;
    let Outcome::Reconciled(second) =
        reconcile_block(&controller, NET, PLAYER, &guardrails(), &ingested.snapshot)
            .await
            .unwrap()
    else {
        panic!("a complete snapshot must be reconciled");
    };
    let w = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        w.state,
        WorkflowState::PrecommitConfirmed,
        "a second pass over the same evidence changes nothing"
    );
    assert_eq!(
        w.revision, revision_after_first,
        "and writes nothing: a bumped revision is a transition that re-fired"
    );
    assert!(!second.blocks_claiming(), "{:?}", second.needs_attention());

    let intent: String =
        sqlx::query_scalar("SELECT state FROM pool.tig_write_intent WHERE intent_id = $1::uuid")
            .bind(&admitted.intent.intent_id)
            .fetch_one(&controller)
            .await
            .unwrap();
    assert_eq!(
        intent, "CONFIRMED",
        "the write that landed is recorded as landed"
    );

    // And it never goes **backwards** — which needs a window that has *dropped*
    // the evidence, not one that still carries it. §8's window is the latest
    // 120 blocks, so a confirmed precommit eventually falls out of it, and a
    // pass that read absence as un-confirmation would walk a recovered workflow
    // back to where the crash left it, on every poll thereafter.
    //
    // The earlier version of this test asserted only that a second pass over
    // the *same* window changed nothing, and the plan then claimed a coverage
    // the test did not deliver.
    let empty = ConfirmedWindow {
        at_block: i64::try_from(ingested.snapshot.record().height).unwrap() + 1,
        ..ConfirmedWindow::default()
    };
    let report = restart::reconcile_after_restart(&controller, NET, &empty)
        .await
        .unwrap();
    let w = workflow::find(&controller, NET, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        w.state,
        WorkflowState::PrecommitConfirmed,
        "a window that dropped the precommit is not evidence it never confirmed"
    );
    assert_eq!(
        w.revision, revision_after_first,
        "and nothing is written on the way to not moving"
    );
    assert!(
        !report.blocks_claiming(),
        "an aged-out benchmark is ordinary (§7), not an operator condition: {report:?}"
    );
    // The *report* must not claim a backwards move either. `advance_one`
    // returns where the workflow ended, and the caller classifies a returned
    // state that differs from the row as `advanced` — so a pass that answered
    // "absent, therefore PRECOMMIT_SUBMITTED" would tell §10's operator the
    // workflow had moved back, even with the row untouched.
    assert!(
        report.advanced.is_empty(),
        "nothing advanced from an empty window: {:?}",
        report.advanced
    );
    assert!(
        report.unchanged.iter().any(|id| id == "w1"),
        "and the workflow is reported at rest: {report:?}"
    );
}
