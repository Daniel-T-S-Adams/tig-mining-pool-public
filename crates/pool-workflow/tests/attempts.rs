//! Slice-1 criteria E1, E2 and E6: the gateway's write-attempt ledger,
//! proven against a real PostgreSQL 18.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::Network;
use pool_test_support::{TempDb, is_insufficient_privilege};
use pool_workflow::attempt::AttemptError;
use pool_workflow::{
    AttemptOutcome, IntentState, NewIntent, PostgresAttemptLedger, PostgresIntentRepository,
    TigWriteIntentRepository, WriteAttemptLedger, WriteKind,
};
use sqlx::Row;

fn intent(workflow: &str, kind: WriteKind, benchmark: Option<&str>) -> NewIntent {
    NewIntent {
        network: Network::Testnet,
        workflow_id: workflow.to_string(),
        write_kind: kind,
        generation: 1,
        benchmark_id: benchmark.map(str::to_string),
        payload_digest: [0xab; 32],
        // §13 invariant 5: a proof write names the canonical payload it sends.
        // `seed_preconditions` records the matching row.
        payload_artifact_id: (kind == WriteKind::Proof)
            .then(|| format!("artifact/{workflow}/proof")),
    }
}

/// Record whatever `migrations/0011` requires behind this intent.
///
/// These tests are about the attempt lane, not about acceptance. The
/// preconditions still have to hold — 0011 makes them structural for every
/// writer — so they are established here rather than asserted here.
async fn seed_preconditions(pool: &sqlx::PgPool, new: &NewIntent) {
    let Some(benchmark_id) = new.benchmark_id.as_deref() else {
        return;
    };
    match new.write_kind {
        WriteKind::Benchmark => {
            pool_test_support::seed_acceptances(
                pool,
                "testnet",
                &[(new.workflow_id.as_str(), benchmark_id)],
            )
            .await;
        }
        WriteKind::Proof => {
            pool_test_support::seed_canonical_payload(
                pool,
                "testnet",
                new.payload_artifact_id
                    .as_deref()
                    .expect("a proof names one"),
                &new.workflow_id,
                benchmark_id,
                new.payload_digest,
            )
            .await;
        }
        WriteKind::Precommit => {}
    }
}

#[tokio::test]
async fn an_attempt_is_recorded_before_the_response_and_separately_from_it() {
    // E1 / §7.3: "records every attempt before sending, then records the
    // response separately". The row has to exist while the outcome is still
    // unknown — that is the whole point, because §10 can only reconcile a
    // write it knows was attempted.
    let Some(db) = TempDb::migrated("e1").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);

    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();

    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();
    assert_eq!(attempt.attempt_no, 1);
    assert!(
        attempt.is_unresolved(),
        "an attempt starts unresolved: the request has not been answered"
    );
    assert!(
        ledger.attempts_for(&recorded.intent_id).await.unwrap()[0].is_unresolved(),
        "and it is durable in that state"
    );

    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Accepted,
            Some(200),
            None,
        )
        .await
        .unwrap();

    let after = ledger.attempts_for(&recorded.intent_id).await.unwrap();
    assert_eq!(after[0].outcome, Some(AttemptOutcome::Accepted));
    assert_eq!(after[0].http_status, Some(200));
}

#[tokio::test]
async fn a_response_is_recorded_once() {
    // §7.3 makes the ledger the evidence §10 reconciles from; an outcome
    // that could be rewritten would let a later attempt restate an earlier
    // one's result.
    let Some(db) = TempDb::migrated("once").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();

    ledger
        .resolve(&attempt.attempt_id, AttemptOutcome::Ambiguous, None, None)
        .await
        .unwrap();
    match ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Accepted,
            Some(200),
            None,
        )
        .await
    {
        Err(AttemptError::AlreadyResolved { .. }) => {}
        other => panic!("expected AlreadyResolved, got {other:?}"),
    }

    let stored = ledger.attempts_for(&recorded.intent_id).await.unwrap();
    assert_eq!(stored[0].outcome, Some(AttemptOutcome::Ambiguous));
}

#[tokio::test]
async fn an_ambiguous_outcome_moves_the_intent_in_the_same_transaction() {
    // §7.3: an ambiguous outcome leaves the intent OUTCOME_UNKNOWN, and §10
    // reconciles rather than resending. Recorded together, because a crash
    // between them would leave a write whose outcome nobody knows looking
    // like one that was never attempted.
    let Some(db) = TempDb::migrated("ambiguous").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let controller = db.pool_as("pool_controller").await;
    let intents = PostgresIntentRepository::new(controller.clone());
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();

    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Ambiguous,
            None,
            Some("timeout"),
        )
        .await
        .unwrap();

    let state = intents
        .find(Network::Testnet, "w1", WriteKind::Precommit, 1)
        .await
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(state, IntentState::OutcomeUnknown);
}

#[tokio::test]
async fn an_accepted_response_does_not_advance_the_intent() {
    // tig_integration.md §7: "a recorded HTTP 200 never advances a
    // workflow". Confirmation comes from reads, so a successful transport
    // result leaves the intent exactly where it was.
    let Some(db) = TempDb::migrated("accepted").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let controller = db.pool_as("pool_controller").await;
    let intents = PostgresIntentRepository::new(controller.clone());
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();

    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Accepted,
            Some(200),
            None,
        )
        .await
        .unwrap();

    let state = intents
        .find(Network::Testnet, "w1", WriteKind::Precommit, 1)
        .await
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(
        state,
        IntentState::Prepared,
        "a transport success is not evidence; §7 advances from confirmed reads"
    );
}

#[tokio::test]
async fn only_one_precommit_may_be_unresolved_in_the_lane() {
    // E2 / §10: a lost precommit response cannot be matched by id, so a
    // second one in flight makes the reconciliation search ambiguous by
    // construction.
    let Some(db) = TempDb::migrated("lane").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);

    let first = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let second = intents
        .create(intent("w2", WriteKind::Precommit, None))
        .await
        .unwrap();

    let a = ledger.begin(&first.intent_id).await.unwrap();
    match ledger.begin(&second.intent_id).await {
        Err(AttemptError::PrecommitLaneOccupied { network }) => assert_eq!(network, "testnet"),
        other => panic!("expected PrecommitLaneOccupied, got {other:?}"),
    }

    // Resolving the first frees the lane.
    ledger
        .resolve(&a.attempt_id, AttemptOutcome::Rejected, Some(400), None)
        .await
        .unwrap();
    ledger
        .begin(&second.intent_id)
        .await
        .expect("the lane is free once the first attempt resolved");
}

#[tokio::test]
async fn an_ambiguous_precommit_keeps_the_lane_closed_until_reconciliation() {
    // §10 defines the single unresolved lane for exactly this case — "a lost
    // precommit HTTP response is harder because the client may not know the
    // generated ID" — and §11 forbids a replacement precommit while the
    // previous outcome is ambiguous. Recording the ambiguity must NOT reopen
    // the lane; only reconciliation settling it does.
    //
    // An earlier version of this test named that guarantee and never
    // asserted it: it checked the lane while the attempt was still
    // unresolved, then stopped at the intent state, while the behaviour
    // after AMBIGUOUS was the opposite of what the name claimed.
    let Some(db) = TempDb::migrated("ambiguouslane").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let first = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let second = intents
        .create(intent("w2", WriteKind::Precommit, None))
        .await
        .unwrap();
    let a = ledger.begin(&first.intent_id).await.unwrap();

    ledger
        .resolve(
            &a.attempt_id,
            AttemptOutcome::Ambiguous,
            None,
            Some("timeout"),
        )
        .await
        .unwrap();

    // The ambiguity is recorded — and the lane is still closed.
    assert_eq!(
        intents
            .find(Network::Testnet, "w1", WriteKind::Precommit, 1)
            .await
            .unwrap()
            .unwrap()
            .state,
        IntentState::OutcomeUnknown
    );
    match ledger.begin(&second.intent_id).await {
        Err(AttemptError::PrecommitLaneOccupied { .. }) => {}
        other => panic!("an ambiguous precommit must keep the lane closed, got {other:?}"),
    }

    // Only reconciliation reopens it.
    ledger
        .reconcile(&a.attempt_id, AttemptOutcome::Accepted)
        .await
        .unwrap();
    ledger
        .begin(&second.intent_id)
        .await
        .expect("reconciliation settled the ambiguity, so the lane reopens");
}

#[tokio::test]
async fn reconciliation_settles_an_ambiguity_and_cannot_restate_it() {
    let Some(db) = TempDb::migrated("reconcile").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();

    // Nothing to settle yet.
    match ledger
        .reconcile(&attempt.attempt_id, AttemptOutcome::Accepted)
        .await
    {
        Err(AttemptError::NotAmbiguous { .. }) => {}
        other => panic!("expected NotAmbiguous, got {other:?}"),
    }

    ledger
        .resolve(&attempt.attempt_id, AttemptOutcome::Ambiguous, None, None)
        .await
        .unwrap();

    // Reconciliation must produce a finding, not repeat the ambiguity.
    match ledger
        .reconcile(&attempt.attempt_id, AttemptOutcome::Ambiguous)
        .await
    {
        Err(AttemptError::NotSettled { .. }) => {}
        other => panic!("expected NotSettled, got {other:?}"),
    }

    ledger
        .reconcile(&attempt.attempt_id, AttemptOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(
        ledger.attempts_for(&recorded.intent_id).await.unwrap()[0].outcome,
        Some(AttemptOutcome::Rejected)
    );

    // And a settled outcome is final.
    match ledger
        .reconcile(&attempt.attempt_id, AttemptOutcome::Accepted)
        .await
    {
        Err(AttemptError::NotAmbiguous { .. }) => {}
        other => panic!("a definitive outcome is recorded once, got {other:?}"),
    }
}

#[tokio::test]
async fn an_ambiguity_cannot_be_unwound_by_raw_sql() {
    // The repository refuses to restate an ambiguity, but the schema is what
    // has to hold: an ambiguous attempt returned to unresolved — or restated
    // with different bytes — would leave the lane closed with nothing left
    // able to settle it, and §10's reconciliation evidence rewritten.
    // Asserted around the repository, because the repository's own check is
    // not the guarantee.
    let Some(db) = TempDb::migrated("unwind").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let gateway = db.pool_as("pool_gateway").await;
    let ledger = PostgresAttemptLedger::new(gateway.clone());
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();
    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Ambiguous,
            None,
            Some("timeout"),
        )
        .await
        .unwrap();

    let back_to_unresolved = sqlx::query(
        "UPDATE pool.tig_write_attempt SET outcome = NULL, resolved_at = NULL
          WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .execute(&gateway)
    .await;
    assert!(
        back_to_unresolved.is_err(),
        "an ambiguity cannot be returned to unresolved"
    );

    let restated = sqlx::query(
        "UPDATE pool.tig_write_attempt SET detail = 'something else'
          WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .execute(&gateway)
    .await;
    assert!(
        restated.is_err(),
        "an ambiguity cannot be restated with different bytes"
    );

    // Settling it IS allowed — that is the transition that reopens the lane.
    // Settling IS allowed — that is the transition that reopens the lane —
    // but it may not rewrite the evidence of the lost response.
    let rewrites_evidence = sqlx::query(
        "UPDATE pool.tig_write_attempt SET outcome = 'ACCEPTED', resolved_at = now()
          WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .execute(&gateway)
    .await;
    assert!(
        rewrites_evidence.is_err(),
        "reconciliation settles the outcome; it cannot restate when the response arrived"
    );

    sqlx::query(
        "UPDATE pool.tig_write_attempt SET outcome = 'ACCEPTED', reconciled_at = now()
          WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .execute(&gateway)
    .await
    .expect("reconciliation settles an ambiguity");
}

#[tokio::test]
async fn an_ambiguous_attempt_reports_its_lane_as_occupied() {
    // The predicate the send path asks. It has to agree with the partial
    // indexes: telling a caller the lane is free while an ambiguity stands
    // is the replacement precommit §11 forbids — the index would still
    // refuse the write, so the damage is to the decision, not the data.
    let Some(db) = TempDb::migrated("predicate").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();
    assert!(attempt.is_unresolved());
    assert!(!attempt.has_response());

    ledger
        .resolve(&attempt.attempt_id, AttemptOutcome::Ambiguous, None, None)
        .await
        .unwrap();
    let after = &ledger.attempts_for(&recorded.intent_id).await.unwrap()[0];
    assert!(
        after.is_unresolved(),
        "an ambiguous attempt still occupies its lane"
    );
    assert!(after.has_response(), "a response was recorded, ambiguously");
    assert!(!after.reconciled);

    ledger
        .reconcile(&attempt.attempt_id, AttemptOutcome::Accepted)
        .await
        .unwrap();
    let settled = &ledger.attempts_for(&recorded.intent_id).await.unwrap()[0];
    assert!(!settled.is_unresolved(), "settled, so the lane is free");
    assert!(settled.reconciled);
}

#[tokio::test]
async fn reconciliation_preserves_when_the_outcome_became_ambiguous() {
    // §10.3 pages on an outcome that stays ambiguous for more than two
    // target blocks, and resolved_at is the fact that page reads. Settling
    // must not overwrite it.
    let Some(db) = TempDb::migrated("preserve").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let gateway = db.pool_as("pool_gateway").await;
    let ledger = PostgresAttemptLedger::new(gateway.clone());
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = ledger.begin(&recorded.intent_id).await.unwrap();
    ledger
        .resolve(
            &attempt.attempt_id,
            AttemptOutcome::Ambiguous,
            Some(504),
            Some("gateway timeout"),
        )
        .await
        .unwrap();

    // As text, so the assertion needs no date type — and compares the
    // stored value exactly.
    let before: Option<String> = sqlx::query_scalar(
        "SELECT resolved_at::text FROM pool.tig_write_attempt WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .fetch_one(&gateway)
    .await
    .unwrap();

    ledger
        .reconcile(&attempt.attempt_id, AttemptOutcome::Rejected)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT resolved_at::text AS resolved_at, http_status, detail,
                reconciled_at IS NOT NULL AS reconciled
           FROM pool.tig_write_attempt WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .fetch_one(&gateway)
    .await
    .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("resolved_at"),
        before,
        "when it became ambiguous is preserved"
    );
    assert_eq!(row.get::<Option<i32>, _>("http_status"), Some(504));
    assert_eq!(
        row.get::<Option<String>, _>("detail").as_deref(),
        Some("gateway timeout")
    );
    assert!(row.get::<bool, _>("reconciled"));
}

#[tokio::test]
async fn an_attempt_cannot_be_born_already_resolved() {
    // A row inserted carrying an outcome never enters either partial index,
    // so it would sail past the serialized precommit lane and the
    // per-benchmark write lock the schema claims to enforce by construction.
    let Some(db) = TempDb::migrated("bornunresolved").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let gateway = db.pool_as("pool_gateway").await;
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();

    let born_resolved = sqlx::query(
        "INSERT INTO pool.tig_write_attempt (intent_id, attempt_no, outcome, resolved_at)
         VALUES ($1::uuid, 1, 'ACCEPTED', now())",
    )
    .bind(&recorded.intent_id)
    .execute(&gateway)
    .await;
    assert!(
        born_resolved.is_err(),
        "§7.3 records the attempt before the response; it cannot arrive already resolved"
    );

    // And the lane is genuinely still enforceable afterwards.
    PostgresAttemptLedger::new(gateway.clone())
        .begin(&recorded.intent_id)
        .await
        .expect("a normal attempt still starts");
    let second = intents
        .create(intent("w2", WriteKind::Precommit, None))
        .await
        .unwrap();
    assert!(
        PostgresAttemptLedger::new(gateway)
            .begin(&second.intent_id)
            .await
            .is_err(),
        "the lane holds"
    );
}

#[tokio::test]
async fn the_detail_column_is_bounded() {
    // The only process writing this row holds the TIG API key, so an
    // unbounded free-text column is the shape §9 forbids and §3 excludes.
    let Some(db) = TempDb::migrated("detail").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let gateway = db.pool_as("pool_gateway").await;
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    let attempt = PostgresAttemptLedger::new(gateway.clone())
        .begin(&recorded.intent_id)
        .await
        .unwrap();

    let huge = "x".repeat(201);
    let refused = sqlx::query(
        "UPDATE pool.tig_write_attempt SET outcome = 'REJECTED', detail = $2, resolved_at = now()
          WHERE attempt_id = $1::uuid",
    )
    .bind(&attempt.attempt_id)
    .bind(&huge)
    .execute(&gateway)
    .await;
    assert!(
        refused.is_err(),
        "detail carries a classification, never a response body"
    );
}

#[tokio::test]
async fn two_writes_for_one_benchmark_cannot_be_in_flight() {
    // E6 / §11. Benchmark and proof writes reconcile by benchmark_id, so two
    // in flight for the same benchmark cannot be told apart afterwards.
    let Some(db) = TempDb::migrated("benchmark").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let ledger = PostgresAttemptLedger::new(db.pool_as("pool_gateway").await);

    let controller = db.pool_as("pool_controller").await;
    for new in [
        intent("w1", WriteKind::Benchmark, Some("bench-a")),
        intent("w2", WriteKind::Proof, Some("bench-a")),
        intent("w3", WriteKind::Benchmark, Some("bench-b")),
    ] {
        seed_preconditions(&controller, &new).await;
    }

    let bench = intents
        .create(intent("w1", WriteKind::Benchmark, Some("bench-a")))
        .await
        .unwrap();
    // A proof for the SAME benchmark: a different kind, still the same
    // benchmark, so still indistinguishable on reconciliation.
    let proof = intents
        .create(intent("w2", WriteKind::Proof, Some("bench-a")))
        .await
        .unwrap();
    let other = intents
        .create(intent("w3", WriteKind::Benchmark, Some("bench-b")))
        .await
        .unwrap();

    let a = ledger.begin(&bench.intent_id).await.unwrap();
    match ledger.begin(&proof.intent_id).await {
        Err(AttemptError::BenchmarkWriteInFlight { benchmark_id }) => {
            assert_eq!(benchmark_id, "bench-a")
        }
        other => panic!("expected BenchmarkWriteInFlight, got {other:?}"),
    }

    // A different benchmark is unaffected: the rule is per benchmark, not a
    // global write lock.
    ledger
        .begin(&other.intent_id)
        .await
        .expect("another benchmark may be written concurrently");

    ledger
        .resolve(&a.attempt_id, AttemptOutcome::Accepted, Some(200), None)
        .await
        .unwrap();
    ledger
        .begin(&proof.intent_id)
        .await
        .expect("resolved, so the benchmark is free");
}

#[tokio::test]
async fn the_lane_columns_come_from_the_intent_not_the_caller() {
    // The partial indexes key on network, write_kind and benchmark_id, so a
    // wrong value would silently disable the lane it is meant to enforce.
    // The trigger takes them from the intent; a caller cannot supply them.
    let Some(db) = TempDb::migrated("denormalised").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let intents = PostgresIntentRepository::new(db.pool_as("pool_controller").await);
    let gateway = db.pool_as("pool_gateway").await;
    let new = intent("w1", WriteKind::Benchmark, Some("bench-a"));
    seed_preconditions(&db.pool_as("pool_controller").await, &new).await;
    let recorded = intents.create(new).await.unwrap();

    // Try to insert an attempt claiming a different benchmark entirely.
    sqlx::query(
        "INSERT INTO pool.tig_write_attempt (intent_id, attempt_no, network, write_kind, benchmark_id)
         VALUES ($1::uuid, 1, 'mainnet', 'precommit', 'somewhere-else')",
    )
    .bind(&recorded.intent_id)
    .execute(&gateway)
    .await
    .expect("the insert succeeds; the trigger overwrites what was claimed");

    let row = sqlx::query(
        "SELECT network, write_kind, benchmark_id FROM pool.tig_write_attempt
          WHERE intent_id = $1::uuid",
    )
    .bind(&recorded.intent_id)
    .fetch_one(&gateway)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("network"), "testnet");
    assert_eq!(row.get::<String, _>("write_kind"), "benchmark");
    assert_eq!(
        row.get::<Option<String>, _>("benchmark_id").as_deref(),
        Some("bench-a")
    );
}

#[tokio::test]
async fn the_controller_reads_the_ledger_and_never_writes_it() {
    // architecture.md §3 gives the ledger to the gateway. The controller
    // reconciles from it (§10 step 7) and writes none of it, so the
    // component that chooses work cannot fabricate evidence that a write was
    // attempted.
    let Some(db) = TempDb::migrated("ledgergrants").await else {
        return;
    };
    // The intents below need their owner mapping to exist: §6.1 opens the
    // unverified interval when the intent is created, and the foreign key
    // from `tig_write_intent` makes that ordering structural.
    pool_test_support::seed_workflows(
        &db.pool_as("pool_controller").await,
        "testnet",
        &["w1", "w2", "w3", "author", "rearm", "terminal"],
    )
    .await;
    let controller = db.pool_as("pool_controller").await;
    let intents = PostgresIntentRepository::new(controller.clone());
    let recorded = intents
        .create(intent("w1", WriteKind::Precommit, None))
        .await
        .unwrap();
    PostgresAttemptLedger::new(db.pool_as("pool_gateway").await)
        .begin(&recorded.intent_id)
        .await
        .unwrap();

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_attempt")
        .fetch_one(&controller)
        .await
        .unwrap();
    assert_eq!(rows, 1, "the controller reads the ledger");

    let insert = sqlx::query(
        "INSERT INTO pool.tig_write_attempt (intent_id, attempt_no)
         VALUES ($1::uuid, 99)",
    )
    .bind(&recorded.intent_id)
    .execute(&controller)
    .await
    .expect_err("the controller must not write the ledger");
    assert!(
        is_insufficient_privilege(&insert),
        "must be refused for lack of privilege, got: {insert}"
    );
}
