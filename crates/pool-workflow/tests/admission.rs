//! Slice-1 criteria D2, D2a, D2c, D2d and D2e: the precommit admission
//! transaction, proven against a real PostgreSQL 18.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these tests skip, so a
//! developer without a database still gets a meaningful `make check`. CI sets
//! the URL and `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use pool_domain::{Network, challenge_tie_seed, draw_rank};
use pool_test_support::TempDb;
use pool_workflow::{
    AdmissionError, AnchorSnapshot, IntentState, NewDecision, RecordedDraw, RecordedTie, WriteKind,
    admit_precommit,
};
use serde_json::json;
use sqlx::{Connection, Row};

const ANCHOR: &str = "block_100080";
const ANCHOR_DIGEST: [u8; 32] = [0xcd; 32];

/// Persist a usable snapshot: D2e means the anchor must be the newest one the
/// database actually holds, so every admission test needs its anchor to exist.
async fn persist_snapshot(pool: &sqlx::PgPool, block_id: &str, digest: [u8; 32], height: i64) {
    sqlx::query(
        "INSERT INTO pool.block_snapshot
             (network, block_id, content_digest, height, reads_complete, active_cache_ready)
         VALUES ('testnet', $1, $2, $3, true, true)",
    )
    .bind(block_id)
    .bind(digest.as_slice())
    .bind(height)
    .execute(pool)
    .await
    .unwrap();
}

/// The usual case: one usable snapshot, and it is the decision's anchor.
async fn with_anchor(db: &TempDb) -> sqlx::PgPool {
    let pool = db.pool_as("pool_controller").await;
    persist_snapshot(&pool, ANCHOR, ANCHOR_DIGEST, 100_080).await;
    pool
}

/// A decision derived the way the controller derives one: the ranks come from
/// the shipped §6.3 derivation over the anchor block, not from constants.
fn decision(workflow: &str, generation: i32) -> NewDecision {
    decision_with_tie(workflow, generation, None)
}

/// The same decision, anchored to a different block.
fn decision_at(
    workflow: &str,
    generation: i32,
    block_id: &str,
    digest: [u8; 32],
    height: i64,
) -> NewDecision {
    NewDecision {
        anchor: AnchorSnapshot {
            block_id: block_id.to_string(),
            content_digest: digest,
            height,
        },
        ..decision(workflow, generation)
    }
}

fn decision_with_tie(workflow: &str, generation: i32, tie: Option<RecordedTie>) -> NewDecision {
    let seed = challenge_tie_seed(Network::Testnet, ANCHOR);
    let mut draw_ranks = serde_json::Map::new();
    for challenge in ["c001", "c002", "c003"] {
        let rank = draw_rank(&seed, challenge);
        draw_ranks.insert(challenge.to_string(), json!(hex(rank.as_bytes())));
    }

    NewDecision {
        network: Network::Testnet,
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
            tie,
        },
        selected_challenge: "c001".to_string(),
        selected_algorithm: "a011".to_string(),
        // Numbers stay numbers: §6.6 copies the source benchmark's
        // hyperparameters and §4 requires lossless numeric handling.
        track_settings: json!({
            "t001": {
                "num_bundles": 4,
                "fuel_budget": 1_500_000u64,
                "hyperparameters": { "noise": 0.15, "restart_period": 250 }
            }
        }),
        reserve_inputs: json!({
            "penalty_amount": "10000000000000000000",
            "num_bundles_by_track": { "t001": 4 },
            "fee_by_track": { "t001": "2000000000000000" },
            "failure_charge_policy_version": "fixture-v1"
        }),
        precommit_reserve: "40002000000000000000".to_string(),
        config_digest: [0xef; 32],
        payload_digest: [generation as u8; 32],
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn a_decision_and_its_intent_commit_together() {
    // D2: there is no moment when one exists without the other. A decision
    // nothing will send, or a write nobody can justify, are both unreachable
    // because they are one transaction.
    let Some(db) = TempDb::migrated("admit_pair").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    let admitted = admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();
    assert_eq!(admitted.pool_unverified, 0);
    assert_eq!(admitted.unverified_limit, 4);
    assert_eq!(admitted.intent.write_kind, WriteKind::Precommit);
    assert_eq!(admitted.intent.state, IntentState::Prepared);
    assert_eq!(admitted.intent.benchmark_id, None);

    let row = sqlx::query(
        "SELECT d.decision_id::text AS decision_id, d.anchor_block_id, d.anchor_height,
                d.tie_domain, d.tie_draw_ranks, d.tie_candidates, d.tie_winner,
                d.selected_challenge, d.pool_unverified, d.unverified_limit,
                d.precommit_reserve::text AS reserve, d.track_settings, d.reserve_inputs,
                i.intent_id::text AS intent_id
         FROM pool.precommit_decision d
         JOIN pool.tig_write_intent i
           ON i.network = d.network AND i.workflow_id = d.workflow_id
          AND i.write_kind = 'precommit' AND i.generation = d.generation",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(
        row.get::<String, _>("decision_id"),
        admitted.decision_id,
        "the decision and the intent are joined by the same generation"
    );
    assert_eq!(row.get::<String, _>("intent_id"), admitted.intent.intent_id);
    assert_eq!(row.get::<String, _>("anchor_block_id"), ANCHOR);
    assert_eq!(row.get::<i64, _>("anchor_height"), 100_080);
    assert_eq!(
        row.get::<String, _>("tie_domain"),
        pool_domain::CHALLENGE_TIE_DOMAIN
    );
    assert_eq!(row.get::<String, _>("reserve"), "40002000000000000000");

    // D2d: the rank map covers every eligible challenge, not only a tied
    // subset, and the ranks are the derivation's own.
    let ranks: serde_json::Value = row.get("tie_draw_ranks");
    let seed = challenge_tie_seed(Network::Testnet, ANCHOR);
    for challenge in ["c001", "c002", "c003"] {
        assert_eq!(
            ranks[challenge],
            json!(hex(draw_rank(&seed, challenge).as_bytes())),
            "{challenge}'s recorded rank must be the derived one"
        );
    }
    assert!(
        row.get::<Option<serde_json::Value>, _>("tie_candidates")
            .is_none()
    );
    assert!(row.get::<Option<String>, _>("tie_winner").is_none());

    // D2c: the reservation inputs are stored and no accounting batch exists.
    let inputs: serde_json::Value = row.get("reserve_inputs");
    assert_eq!(inputs["penalty_amount"], json!("10000000000000000000"));
    assert_eq!(inputs["failure_charge_policy_version"], json!("fixture-v1"));

    // The pool's own settings keep their types.
    let settings: serde_json::Value = row.get("track_settings");
    assert_eq!(
        settings["t001"]["hyperparameters"]["restart_period"],
        json!(250)
    );
    assert!(settings["t001"]["hyperparameters"]["restart_period"].is_number());
}

#[tokio::test]
async fn a_tie_records_its_candidates_and_winner() {
    // D2d: a tie is audit evidence, so the tied set and the winner are stored
    // together. The database refuses half of it.
    let Some(db) = TempDb::migrated("admit_tie").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    let tie = RecordedTie {
        candidates: vec!["c001".to_string(), "c003".to_string()],
        winner: "c001".to_string(),
    };
    admit_precommit(&pool, &decision_with_tie("w1", 1, Some(tie)), 4)
        .await
        .unwrap();

    let row = sqlx::query("SELECT tie_candidates, tie_winner FROM pool.precommit_decision")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<serde_json::Value, _>("tie_candidates"),
        json!(["c001", "c003"])
    );
    assert_eq!(row.get::<String, _>("tie_winner"), "c001");
}

#[tokio::test]
async fn the_next_precommit_at_the_limit_is_refused() {
    // D2a, and `mining_system.md` §10 invariant 23: the pool never creates a
    // precommit at or above internal_pool_unverified_limit. Driven to the
    // limit, the next admission produces neither a decision nor an intent.
    let Some(db) = TempDb::migrated("admit_limit").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    for n in 1..=3 {
        let admitted = admit_precommit(&pool, &decision(&format!("w{n}"), 1), 3)
            .await
            .unwrap_or_else(|e| panic!("admission {n} must succeed: {e}"));
        assert_eq!(admitted.pool_unverified, i64::from(n) - 1);
    }

    let error = admit_precommit(&pool, &decision("w4", 1), 3)
        .await
        .expect_err("the fourth is at the limit");
    assert!(matches!(
        error,
        AdmissionError::AtUnverifiedLimit {
            pool_unverified: 3,
            unverified_limit: 3
        }
    ));

    assert_eq!(count(&pool, "pool.precommit_decision").await, 3);
    assert_eq!(count(&pool, "pool.tig_write_intent").await, 3);
}

#[tokio::test]
async fn a_terminal_workflow_stops_occupying_the_limit() {
    // `mining_system.md` §6.1: a benchmark is unverified "from creation of its
    // pool precommit intent until TIG records it as verified or it reaches a
    // terminal stopped, expired, or failed state". The recount reads that
    // interval, so capacity is released exactly when the interval closes —
    // not when an intent happens to be rejected, and not never.
    let Some(db) = TempDb::migrated("admit_terminal_releases").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    admit_precommit(&pool, &decision("w1", 1), 2).await.unwrap();
    admit_precommit(&pool, &decision("w2", 1), 2).await.unwrap();
    assert!(
        matches!(
            admit_precommit(&pool, &decision("w3", 1), 2).await,
            Err(AdmissionError::AtUnverifiedLimit { .. })
        ),
        "both workflows are unverified, so the limit is reached"
    );

    // w1 reaches a terminal state; its interval closes and the slot frees.
    let w1 = pool_workflow::workflow::find(&pool, pool_domain::Network::Testnet, "w1")
        .await
        .unwrap()
        .unwrap();
    pool_workflow::workflow::expire(
        &pool,
        pool_domain::Network::Testnet,
        "w1",
        w1.revision,
        "deadline passed",
        // After the anchor height the interval opened at; an interval cannot
        // close before it opened.
        100_200,
    )
    .await
    .unwrap();

    let admitted = admit_precommit(&pool, &decision("w3", 1), 2).await.unwrap();
    assert_eq!(
        admitted.pool_unverified, 1,
        "only w2's interval is still open"
    );

    // And a workflow that is merely *confirmed* still occupies one: a
    // confirmed precommit is an unverified benchmark until TIG verifies it.
    assert!(matches!(
        admit_precommit(&pool, &decision("w4", 1), 2).await,
        Err(AdmissionError::AtUnverifiedLimit {
            pool_unverified: 2,
            ..
        })
    ));
}

#[tokio::test]
async fn concurrent_admissions_cannot_both_take_the_last_slot() {
    // The lease is what makes the recount and the insert one decision. Without
    // it both tasks count `limit - 1` and both admit, because each is counting
    // rows the other has not written yet.
    let Some(db) = TempDb::migrated("admit_race").await else {
        return;
    };
    let pool = Arc::new(with_anchor(&db).await);
    admit_precommit(&pool, &decision("w1", 1), 2).await.unwrap();

    let mut tasks = Vec::new();
    for n in 2..=9 {
        let pool = Arc::clone(&pool);
        tasks.push(tokio::spawn(async move {
            admit_precommit(&pool, &decision(&format!("w{n}"), 1), 2).await
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            admitted += 1;
        }
    }

    assert_eq!(admitted, 1, "exactly one racer takes the last slot");
    assert_eq!(count(&pool, "pool.precommit_decision").await, 2);
    assert_eq!(count(&pool, "pool.tig_write_intent").await, 2);
}

#[tokio::test]
async fn a_second_decision_for_one_generation_is_refused() {
    // §7.3's rule about payloads, said about the reasoning: a changed decision
    // is a new generation. Returning the existing row would claim this call
    // decided it, when the recorded count, limit and draw belong to the first.
    let Some(db) = TempDb::migrated("admit_regen").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();

    let error = admit_precommit(&pool, &decision("w1", 1), 4)
        .await
        .expect_err("generation 1 is already decided");
    assert!(matches!(error, AdmissionError::AlreadyDecided { .. }));

    // A new generation is the way through, and it does not create a second
    // workflow for the limit's purposes.
    let admitted = admit_precommit(&pool, &decision("w1", 2), 4).await.unwrap();
    assert_eq!(admitted.intent.generation, 2);
    assert_eq!(count(&pool, "pool.precommit_decision").await, 2);
}

#[tokio::test]
async fn a_recorded_decision_cannot_be_edited() {
    // The decision's trace of what it read is immutable, like the snapshot it
    // came from. Correcting one means a new generation.
    //
    // Two layers, tested separately because they fail differently. The
    // controller has no UPDATE grant at all, so it never reaches the trigger;
    // the trigger is what holds if a later slice grants UPDATE for some
    // legitimate column, which is exactly when a withheld grant stops being
    // the protection.
    let Some(db) = TempDb::migrated("admit_immutable").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();

    let denied = sqlx::query("UPDATE pool.precommit_decision SET selected_challenge = 'c003'")
        .execute(&pool)
        .await
        .expect_err("the controller has no UPDATE grant");
    assert!(
        pool_test_support::is_insufficient_privilege(&denied),
        "unexpected error: {denied}"
    );

    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();
    let refused = sqlx::query("UPDATE pool.precommit_decision SET selected_challenge = 'c003'")
        .execute(&mut owner)
        .await
        .expect_err("a recorded decision is immutable even to the owner");
    assert!(
        refused.to_string().contains("immutable"),
        "unexpected error: {refused}"
    );
}

#[tokio::test]
async fn the_gateway_can_read_a_decision_and_cannot_write_one() {
    // architecture.md invariant 2: the gateway "cannot decide or manufacture"
    // a write. Enforced by grants, tested under the real gateway role.
    let Some(db) = TempDb::migrated("admit_grants").await else {
        return;
    };
    let controller = with_anchor(&db).await;
    admit_precommit(&controller, &decision("w1", 1), 4)
        .await
        .unwrap();

    let gateway = db.pool_as("pool_gateway").await;
    assert_eq!(count(&gateway, "pool.precommit_decision").await, 1);

    let error = sqlx::query(
        "INSERT INTO pool.precommit_decision
             (network, workflow_id, generation, anchor_block_id, anchor_digest,
              anchor_height, tie_domain, tie_draw_ranks, selected_challenge,
              selected_algorithm, track_settings, pool_unverified, unverified_limit,
              reserve_inputs, precommit_reserve, config_digest)
         VALUES ('testnet', 'forged', 1, 'block_1', decode(repeat('cd', 32), 'hex'),
                 1, 'd', '{\"c001\": \"00\"}'::jsonb, 'c001', 'a011', '{}'::jsonb,
                 0, 4, '{}'::jsonb, 0, decode(repeat('ef', 32), 'hex'))",
    )
    .execute(&gateway)
    .await
    .expect_err("the gateway must not be able to decide");
    assert!(
        error.to_string().contains("permission denied"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn admission_blocks_on_the_serialized_lease() {
    // The race test above can pass by luck if the two admissions happen not to
    // overlap. This asserts the lease itself: while another transaction holds
    // it, an admission cannot proceed, and it proceeds the moment it is
    // released.
    let Some(db) = TempDb::migrated("admit_lease").await else {
        return;
    };
    let pool = Arc::new(with_anchor(&db).await);

    let mut holder = sqlx::PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(pool_workflow::PRECOMMIT_ADMISSION_LOCK)
        .execute(&mut holder)
        .await
        .unwrap();

    let admitting = {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move { admit_precommit(&pool, &decision("w1", 1), 4).await })
    };

    // Long enough that a lock-free admission would have finished: the same
    // call takes milliseconds when the lease is free.
    let blocked = tokio::time::timeout(std::time::Duration::from_secs(2), &mut { admitting }).await;
    let admitting = match blocked {
        Err(_elapsed) => {
            // Still waiting, which is the point. Release and let it finish.
            sqlx::query("COMMIT").execute(&mut holder).await.unwrap();
            None
        }
        Ok(finished) => Some(finished),
    };
    assert!(
        admitting.is_none(),
        "the admission completed while another transaction held the lease"
    );
}

#[tokio::test]
async fn another_network_does_not_occupy_this_one_s_limit() {
    // The gate is per network. Counting across networks would let testnet
    // work refuse mainnet work and the reverse; the table allows both even
    // though this build's configuration refuses mainnet.
    let Some(db) = TempDb::migrated("admit_network").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    for n in 1..=3 {
        // The workflow row too: an intent cannot exist without its owner
        // mapping, which is the foreign key F6 relies on.
        // A member-owned row, because §10 invariant 1's bootstrap carve-out is
        // testnet-only and the schema enforces that. What this test is about
        // is the network scoping of the count.
        sqlx::query(
            "INSERT INTO pool.workflow
                 (network, workflow_id, owner_kind, owner_id, unverified_from_block)
             VALUES ('mainnet', $1, 'MEMBER', 'member_1', 1)",
        )
        .bind(format!("m{n}"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pool.tig_write_intent
                 (network, workflow_id, write_kind, generation, payload_digest)
             VALUES ('mainnet', $1, 'precommit', 1, decode(repeat('ab', 32), 'hex'))",
        )
        .bind(format!("m{n}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    let admitted = admit_precommit(&pool, &decision("w1", 1), 1).await.unwrap();
    assert_eq!(
        admitted.pool_unverified, 0,
        "three mainnet precommits occupy nothing on testnet"
    );
}

#[tokio::test]
async fn the_database_refuses_a_decision_that_breaks_its_own_record() {
    // The Rust gate reaches these first, which is exactly why they are worth
    // asserting directly: a future admission path that forgot a check is
    // refused by PostgreSQL rather than by the code that happened to remember.
    //
    // Each case names the constraint that must refuse it. Accepting *any*
    // constraint would let one of them mask the rest — which is what happened
    // once already, when the anchor foreign key silently began refusing every
    // row in this loop before its own constraint was reached.
    let Some(db) = TempDb::migrated("admit_constraints").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    let _ = &pool;
    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();

    for (constraint, values) in [
        // D2a: at the limit is not under it.
        (
            "precommit_decision_under_limit",
            r#"'c001', 'a011', 3, 3, '{"c001": "00"}'::jsonb, NULL, NULL"#,
        ),
        // A tie recorded half-written is not audit evidence.
        (
            "precommit_decision_tie_recorded_whole",
            r#"'c001', 'a011', 0, 4, '{"c001": "00"}'::jsonb, NULL, 'c001'"#,
        ),
        (
            "precommit_decision_tie_recorded_whole",
            r#"'c001', 'a011', 0, 4, '{"c001": "00"}'::jsonb, '["c001"]'::jsonb, NULL"#,
        ),
        // The tie is how the selection was made; they cannot disagree.
        (
            "precommit_decision_tie_winner_is_the_selection",
            r#"'c001', 'a011', 0, 4, '{"c001": "00"}'::jsonb, '["c001"]'::jsonb, 'c003'"#,
        ),
        // A selection with no rank could not be re-derived.
        (
            "precommit_decision_selection_has_a_rank",
            r#"'c009', 'a011', 0, 4, '{"c001": "00"}'::jsonb, NULL, NULL"#,
        ),
        // A winner outside its own tied set is not a draw result.
        (
            "precommit_decision_winner_is_a_candidate",
            r#"'c001', 'a011', 0, 4, '{"c001": "00", "c003": "01"}'::jsonb, '["c003"]'::jsonb, 'c001'"#,
        ),
        // A candidate with no rank cannot be compared against the winner, so
        // the recorded draw could not be checked.
        (
            "precommit_decision_candidates_are_ranked",
            r#"'c001', 'a011', 0, 4, '{"c001": "00"}'::jsonb, '["c001", "c003"]'::jsonb, 'c001'"#,
        ),
        // NaN sorts above every numeric, so it satisfies `>= 0`. A fractional
        // reserve is deliberately absent: this column has scale 0, so
        // PostgreSQL rounds it before any CHECK runs and the database cannot
        // refuse it. `a_non_canonical_reserve_never_reaches_the_numeric_cast`
        // is where that is caught.
        (
            "precommit_decision_reserve_is_a_number",
            r#"'c001', 'a011', 0, 4, '{"c001": "00"}'::jsonb, NULL, NULL"#,
        ),
    ] {
        // The anchor is the real, persisted one, so the foreign key cannot be
        // what refuses these rows.
        let reserve = if constraint == "precommit_decision_reserve_is_a_number" {
            "'NaN'::numeric"
        } else {
            "0"
        };
        let sql = format!(
            "INSERT INTO pool.precommit_decision
                 (network, workflow_id, generation, anchor_block_id, anchor_digest,
                  anchor_height, tie_domain, selected_challenge, selected_algorithm,
                  pool_unverified, unverified_limit, tie_draw_ranks, tie_candidates,
                  tie_winner, track_settings, reserve_inputs, precommit_reserve,
                  config_digest)
             VALUES ('testnet', 'w1', 1, '{ANCHOR}', decode(repeat('cd', 32), 'hex'),
                     1, 'd', {values}, '{{}}'::jsonb, '{{}}'::jsonb, {reserve},
                     decode(repeat('ef', 32), 'hex'))"
        );
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut owner)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(constraint),
            "expected {constraint} to refuse this row, got: {error}"
        );
    }
}

#[tokio::test]
async fn a_pool_with_no_usable_snapshot_cannot_decide() {
    // §9 step 7: no decision is derived from a snapshot that was never
    // completed. Before the first usable one exists there is nothing to decide
    // from, and that is a distinct answer from a stale anchor.
    let Some(db) = TempDb::migrated("admit_nosnapshot").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let error = admit_precommit(&pool, &decision("w1", 1), 4)
        .await
        .expect_err("nothing to decide from");
    assert!(
        matches!(error, AdmissionError::NoUsableSnapshot { .. }),
        "unexpected error: {error}"
    );
    assert_eq!(count(&pool, "pool.precommit_decision").await, 0);
}

#[tokio::test]
async fn an_incomplete_snapshot_is_not_an_anchor() {
    // §9 step 7 and criterion C5: a partial assembly cannot reach a decision.
    // It is not merely "not newest" — it is not usable at all, so a pool whose
    // only newer snapshot is incomplete still decides from the complete one.
    let Some(db) = TempDb::migrated("admit_partial").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    sqlx::query(
        "INSERT INTO pool.block_snapshot
             (network, block_id, content_digest, height, reads_complete, active_cache_ready)
         VALUES ('testnet', 'block_100082', decode(repeat('ab', 32), 'hex'), 100082, true, false)",
    )
    .execute(&pool)
    .await
    .unwrap();

    admit_precommit(&pool, &decision("w1", 1), 4)
        .await
        .expect("the newest *usable* snapshot is still the anchor");
}

#[tokio::test]
async fn a_decision_cannot_name_a_snapshot_that_was_never_persisted() {
    // The half the database enforces by itself, so a future admission path
    // that skipped the check above still cannot invent an anchor.
    let Some(db) = TempDb::migrated("admit_ghost").await else {
        return;
    };
    // A usable snapshot exists; the forged row names a different block anyway.
    with_anchor(&db).await;
    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();

    let error = sqlx::query(
        r#"INSERT INTO pool.precommit_decision
             (network, workflow_id, generation, anchor_block_id, anchor_digest,
              anchor_height, tie_domain, selected_challenge, selected_algorithm,
              pool_unverified, unverified_limit, tie_draw_ranks, track_settings,
              reserve_inputs, precommit_reserve, config_digest)
         VALUES ('testnet', 'w1', 1, 'block_never', decode(repeat('11', 32), 'hex'),
                 1, 'd', 'c001', 'a011', 0, 4, '{"c001": "00"}'::jsonb,
                 '{}'::jsonb, '{}'::jsonb, 0, decode(repeat('ef', 32), 'hex'))"#,
    )
    .execute(&mut owner)
    .await
    .expect_err("no such snapshot");
    assert!(
        error.to_string().contains("anchor_is_persisted"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn an_already_decided_generation_says_so_even_at_the_limit() {
    // Order matters. If the capacity gate ran first, a retry of the very
    // admission that consumed the last slot would come back as
    // AtUnverifiedLimit — telling the caller it was refused for capacity when
    // in fact its write already exists.
    let Some(db) = TempDb::migrated("admit_retry_at_limit").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    admit_precommit(&pool, &decision("w1", 1), 1).await.unwrap();

    let error = admit_precommit(&pool, &decision("w1", 1), 1)
        .await
        .expect_err("this generation is already decided");
    assert!(
        matches!(error, AdmissionError::AlreadyDecided { .. }),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_non_canonical_reserve_never_reaches_the_numeric_cast() {
    // `accounting.md` §3: amounts cross this boundary as canonical unsigned
    // base-10. PostgreSQL would reshape each of these rather than refuse it —
    // rounding a fraction to scale 0, expanding an exponent, and sorting NaN
    // above every number so that it satisfies the column's `>= 0` check.
    let Some(db) = TempDb::migrated("admit_reserve").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    for bad in ["1.5", "1e3", "NaN", "-1", "", " 1", "007", "+1"] {
        let mut d = decision("w1", 1);
        d.precommit_reserve = bad.to_string();
        let error = match admit_precommit(&pool, &d, 4).await {
            Err(error) => error,
            Ok(_) => panic!("{bad:?} must be refused"),
        };
        assert!(
            matches!(error, AdmissionError::ReserveNotCanonical { .. }),
            "{bad:?} gave the wrong error: {error}"
        );
    }
    assert_eq!(count(&pool, "pool.precommit_decision").await, 0);
}

#[tokio::test]
async fn admission_creates_the_workflow_with_its_owner_and_open_interval() {
    // F6 and `mining_system.md` §6.1. The workflow row is created in the same
    // transaction as the decision and the intent, with its permanent owner and
    // with §6.1's unverified interval already open at the decision's anchor
    // height — "from creation of its pool precommit intent", not from its
    // confirmation.
    let Some(db) = TempDb::migrated("admit_workflow").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();

    let row = sqlx::query(
        "SELECT state, owner_kind, owner_id, unverified_from_block, unverified_to_block
         FROM pool.workflow WHERE network = 'testnet' AND workflow_id = 'w1'",
    )
    .fetch_one(&pool)
    .await
    .expect("the admission created the workflow row");

    assert_eq!(row.get::<String, _>("state"), "DECIDED");
    assert_eq!(row.get::<String, _>("owner_kind"), "POOL_BOOTSTRAP");
    assert_eq!(row.get::<String, _>("owner_id"), "pool-bootstrap");
    assert_eq!(
        row.get::<i64, _>("unverified_from_block"),
        100_080,
        "the interval opens at the decision's anchor height, which is where \
         the precommit intent was created"
    );
    assert_eq!(row.get::<Option<i64>, _>("unverified_to_block"), None);
}

#[tokio::test]
async fn an_intent_cannot_exist_without_its_owner_mapping() {
    // F6, made structural. §2 requires the benchmark → owner mapping to be
    // permanent and §8 charges faults to it, so an intent whose workflow was
    // never created is a write the pool could make and then be unable to
    // attribute. The foreign key is what makes the admission's ordering
    // impossible to skip rather than merely conventional.
    let Some(db) = TempDb::migrated("admit_orphan_intent").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    let error = sqlx::query(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest)
         VALUES ('testnet', 'never_decided', 'precommit', 1,
                 decode(repeat('ab', 32), 'hex'))",
    )
    .execute(&pool)
    .await
    .expect_err("no workflow owns that id");
    assert!(
        error
            .to_string()
            .contains("tig_write_intent_has_a_workflow"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_workflow_cannot_exist_without_an_open_interval() {
    // §6.1's interval opens at creation, so there is no such thing as a
    // workflow that is not yet unverified. A nullable column would let one
    // exist and §7.6's concurrency counting would skip it.
    let Some(db) = TempDb::migrated("admit_interval_required").await else {
        return;
    };
    let pool = with_anchor(&db).await;

    let error = sqlx::query(
        "INSERT INTO pool.workflow (network, workflow_id, owner_kind, owner_id)
         VALUES ('testnet', 'w9', 'POOL_BOOTSTRAP', 'pool-bootstrap')",
    )
    .execute(&pool)
    .await
    .expect_err("a workflow is unverified from the moment it exists");
    assert!(
        error.to_string().contains("unverified_from_block"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_settled_workflow_cannot_take_a_new_generation() {
    // §7.3 allows a new generation for a changed payload, but only while the
    // workflow can still receive the write. Once it is terminal its §6.1
    // interval is closed and `confirm_precommit` would refuse the
    // confirmation — so admitting one would have the pool make a TIG write it
    // could never record.
    let Some(db) = TempDb::migrated("admit_settled").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();

    let w = pool_workflow::workflow::find(&pool, pool_domain::Network::Testnet, "w1")
        .await
        .unwrap()
        .unwrap();
    pool_workflow::workflow::expire(
        &pool,
        pool_domain::Network::Testnet,
        "w1",
        w.revision,
        "deadline passed",
        100_200,
    )
    .await
    .unwrap();

    let error = admit_precommit(&pool, &decision("w1", 2), 4)
        .await
        .expect_err("the workflow is settled");
    assert!(
        matches!(error, AdmissionError::WorkflowSettled { ref state, .. } if state == "EXPIRED"),
        "unexpected error: {error}"
    );
    assert_eq!(count(&pool, "pool.precommit_decision").await, 1);
    assert_eq!(count(&pool, "pool.tig_write_intent").await, 1);
}

#[tokio::test]
async fn a_concurrent_confirmation_cannot_be_overtaken_by_a_new_generation() {
    // The row is read FOR UPDATE. Without that lock this read races a
    // concurrent `workflow::transition`: under READ COMMITTED the admission
    // could see DECIDED while the other transaction was committing the move to
    // PRECOMMIT_CONFIRMED, and admit a second generation for a workflow whose
    // first had already confirmed at TIG.
    //
    // The interleave is forced rather than hoped for: a separate connection
    // holds the row locked while the admission runs, so the admission must
    // wait and then read the committed state.
    let Some(db) = TempDb::migrated("admit_race_confirm").await else {
        return;
    };
    let pool = std::sync::Arc::new(with_anchor(&db).await);
    admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();

    let mut holder = sqlx::PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
    sqlx::query("SELECT 1 FROM pool.workflow WHERE workflow_id = 'w1' FOR UPDATE")
        .execute(&mut holder)
        .await
        .unwrap();

    let admitting = {
        let pool = std::sync::Arc::clone(&pool);
        tokio::spawn(async move { admit_precommit(&pool, &decision("w1", 2), 4).await })
    };

    // While the row is held, the admission cannot have read it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !admitting.is_finished(),
        "the admission read the row unlocked"
    );

    // Commit the confirmation under the lock, then release.
    sqlx::query(
        "UPDATE pool.workflow
         SET state = 'PRECOMMIT_CONFIRMED', revision = revision + 1,
             benchmark_id = 'bench_a', confirmed_track_id = 't002',
             confirmed_settings = '{}'::jsonb, precommit_confirmed_block = 100,
             block_started = 98
         WHERE workflow_id = 'w1'",
    )
    .execute(&mut holder)
    .await
    .unwrap();
    sqlx::query("COMMIT").execute(&mut holder).await.unwrap();

    let error = admitting
        .await
        .unwrap()
        .expect_err("the workflow confirmed while this admission waited");
    assert!(
        matches!(error, AdmissionError::WorkflowSettled { ref state, .. }
                 if state == "PRECOMMIT_CONFIRMED"),
        "unexpected error: {error}"
    );
    assert_eq!(
        count(&pool, "pool.tig_write_intent").await,
        1,
        "no second generation was admitted"
    );
}

async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    let sql = match table {
        "pool.precommit_decision" => "SELECT count(*) AS n FROM pool.precommit_decision",
        "pool.tig_write_intent" => "SELECT count(*) AS n FROM pool.tig_write_intent",
        other => panic!("unknown table {other}"),
    };
    sqlx::query(sql).fetch_one(pool).await.unwrap().get("n")
}

#[tokio::test]
async fn a_second_generation_is_aged_from_its_own_anchor() {
    // §8 measures a workflow with no `block_started` from its decision's
    // anchor, and "its decision" means the latest one. `admit_precommit`
    // issues a new generation against a fresh anchor for a workflow still at
    // DECIDED and deliberately leaves `unverified_from_block` alone —
    // §6.1's interval measures capacity from when the workflow started
    // consuming it, and a retry does not restart that.
    //
    // So the two diverge from generation 2 onward. Ageing from
    // `unverified_from_block` would expire a precommit that had only just been
    // sent, against a `settings.block_id` TIG will still accept: the pool's
    // clock pre-empting a live confirmed benchmark, which §7 gives TIG alone,
    // and closing §6.1's interval on a workflow that is still consuming
    // capacity — under-counting `pool_unverified` in the direction
    // `mining_system.md` §10 invariants 22-23 forbid.
    let Some(db) = TempDb::migrated("admit_regen_age").await else {
        return;
    };
    let pool = with_anchor(&db).await;
    let guardrails = pool_workflow::Guardrails::from_config_json(include_str!(
        "../../../config/tig_integration.json"
    ))
    .unwrap();
    let window = i64::from(guardrails.workflow_expiry_age_blocks);

    admit_precommit(&pool, &decision("w1", 1), 4).await.unwrap();
    let first = pool_workflow::workflow::find(&pool, Network::Testnet, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.unverified_from_block, 100_080);

    // A later block, and a second generation decided against it. D2e requires
    // the newest anchor the database holds, so this is also the only anchor
    // generation 2 could have used.
    const SECOND: &str = "block_100180";
    const SECOND_DIGEST: [u8; 32] = [0xab; 32];
    persist_snapshot(&pool, SECOND, SECOND_DIGEST, 100_180).await;
    admit_precommit(
        &pool,
        &decision_at("w1", 2, SECOND, SECOND_DIGEST, 100_180),
        4,
    )
    .await
    .unwrap();

    let after = pool_workflow::workflow::find(&pool, Network::Testnet, "w1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.unverified_from_block, 100_080,
        "the capacity interval still starts where the workflow did"
    );

    // Due by the first anchor, not by the second. The sweep must read the
    // second.
    assert!(
        pool_workflow::workflow::expire_if_due(
            &pool,
            &guardrails,
            Network::Testnet,
            "w1",
            100_080 + window,
        )
        .await
        .unwrap()
        .is_none(),
        "generation 2 was decided 100 blocks later and is not yet due"
    );

    let expired = pool_workflow::workflow::expire_if_due(
        &pool,
        &guardrails,
        Network::Testnet,
        "w1",
        100_180 + window,
    )
    .await
    .unwrap()
    .expect("a whole window past the latest decision's anchor");
    assert_eq!(
        expired.state,
        pool_workflow::WorkflowState::Expired,
        "the deadline still exists; it is measured from the right block"
    );
}
