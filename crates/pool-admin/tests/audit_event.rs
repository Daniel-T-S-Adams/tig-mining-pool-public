//! `migrations/0024` at the schema level: what an audit row records, what it
//! refuses, and who may write one.
//!
//! `security.md` §9 makes audit events "append-only facts" and says "ordinary
//! application roles cannot update or delete audit rows". Two of these tests
//! are about that sentence; the rest are about the fields being answerable by
//! query rather than merely present.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb, exec};
use sqlx::{AssertSqlSafe, Connection, PgConnection};

async fn migrated(name: &str, role: &str) -> Option<(TempDb, PgConnection)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
    Some((db, conn))
}

/// The replay rejection §3.2 asks to be audited, as the API will write it.
async fn replay_rejected(conn: &mut PgConnection) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO pool.audit_event
             (deployment, network, actor_type, actor_id, action, outcome,
              resource_type, resource_id, request_id, reason, evidence)
         VALUES ('local-dev', 'testnet', 'WORKER',
                 '11111111-1111-4111-8111-111111111111',
                 'REQUEST_REPLAY_REJECTED', 'REJECTED',
                 'WORKER_CREDENTIAL', '22222222-2222-4222-8222-222222222222',
                 '33333333-3333-4333-8333-333333333333',
                 'REQUEST_ID_REUSED_WITH_DIFFERENT_BYTES',
                 $1::jsonb)
         RETURNING audit_event_id::text",
    )
    .bind(r#"{"recorded_sha256":"aa00","presented_sha256":"bb11"}"#)
    .fetch_one(conn)
    .await
}

#[tokio::test]
async fn the_api_appends_an_event_and_can_read_it_back() {
    let Some((_db, mut api)) = migrated("audit_write", "pool_api").await else {
        return;
    };

    let id = replay_rejected(&mut api).await.expect("the API audits");

    let (action, outcome, actor): (String, String, String) = sqlx::query_as(
        "SELECT action, outcome, actor_type FROM pool.audit_event
          WHERE audit_event_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut api)
    .await
    .unwrap();
    assert_eq!(action, "REQUEST_REPLAY_REJECTED");
    assert_eq!(outcome, "REJECTED");
    assert_eq!(actor, "WORKER");
}

#[tokio::test]
async fn an_event_cannot_be_edited_or_removed_by_anyone() {
    // Attempted as `pool_migration`, which owns the table and holds every
    // privilege on it. Run as `pool_api` this would prove nothing: that role
    // has no UPDATE or DELETE grant, so the statement is refused before a
    // trigger is consulted, and the test would pass with the trigger deleted.
    // The privilege half is `the_api_can_only_append`.
    let Some((db, mut api)) = migrated("audit_immutable", "pool_api").await else {
        return;
    };
    let id = replay_rejected(&mut api).await.unwrap();

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();

    for statement in [
        "UPDATE pool.audit_event SET outcome = 'ALLOWED'",
        "UPDATE pool.audit_event SET evidence = '{}'::jsonb",
        "DELETE FROM pool.audit_event",
        // TRUNCATE fires no row triggers, so `0024`'s `FOR EACH ROW` guard
        // did not see it and the owner could empty an append-only table in
        // one statement. `0025` adds the statement-level trigger that does.
        // Written without the `WHERE` the others take, hence the marker.
        "TRUNCATE pool.audit_event -- no-where",
    ] {
        // One transaction per case: a failed statement aborts its
        // transaction, so a shared one would report failure for every case
        // after the first regardless of what the rule does.
        let mut tx = owner.begin().await.unwrap();
        let changed = if let Some(bare) = statement.strip_suffix(" -- no-where") {
            sqlx::query(AssertSqlSafe(bare.to_owned()))
                .execute(&mut *tx)
                .await
        } else {
            sqlx::query(AssertSqlSafe(format!(
                "{statement} WHERE audit_event_id = $1::uuid"
            )))
            .bind(&id)
            .execute(&mut *tx)
            .await
        };
        assert!(changed.is_err(), "{statement} must be refused");
        tx.rollback().await.unwrap();
    }

    let (outcome, remaining): (String, i64) = sqlx::query_as(
        "SELECT outcome, (SELECT count(*) FROM pool.audit_event)
           FROM pool.audit_event WHERE audit_event_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut api)
    .await
    .unwrap();
    assert_eq!(outcome, "REJECTED", "the row is as it was written");
    assert_eq!(remaining, 1, "and it is still there");
}

#[tokio::test]
async fn the_api_can_only_append() {
    // The privilege half of §9's "ordinary application roles cannot update or
    // delete audit rows", checked with the trigger gone so that removing the
    // trigger would not silently leave the table editable.
    let Some((db, mut api)) = migrated("audit_no_edit_grant", "pool_api").await else {
        return;
    };
    let id = replay_rejected(&mut api).await.unwrap();

    let mut migration = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    exec(
        &mut migration,
        "DROP TRIGGER audit_event_append_only ON pool.audit_event".to_owned(),
    )
    .await
    .expect("the migration role owns the trigger");

    for statement in [
        "UPDATE pool.audit_event SET outcome = 'ALLOWED'",
        "DELETE FROM pool.audit_event",
    ] {
        let mut tx = api.begin().await.unwrap();
        let changed = sqlx::query(AssertSqlSafe(format!(
            "{statement} WHERE audit_event_id = $1::uuid"
        )))
        .bind(&id)
        .execute(&mut *tx)
        .await;
        assert!(
            changed.is_err(),
            "{statement} must be refused by the missing privilege"
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn an_unknown_actor_carries_no_identity_and_a_known_one_must() {
    // `UNKNOWN` is a real answer — §9 audits authentication failures, and a
    // caller who did not authenticate has no identity the pool may assert.
    // Pairing it with an `actor_id` would read as an identification that was
    // never made; omitting the id for a named actor loses the only thing the
    // row was recording.
    let Some((_db, mut api)) = migrated("audit_actor", "pool_api").await else {
        return;
    };

    let write = |actor_type: &'static str, actor_id: Option<&'static str>| {
        sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome)
             VALUES ('local-dev', 'testnet', $1, $2, 'REQUEST_REJECTED', 'REJECTED')",
        )
        .bind(actor_type)
        .bind(actor_id)
    };

    for (actor_type, actor_id) in [("UNKNOWN", Some("someone")), ("WORKER", None)] {
        let mut tx = api.begin().await.unwrap();
        let written = write(actor_type, actor_id).execute(&mut *tx).await;
        assert!(
            written.is_err(),
            "{actor_type} with actor_id {actor_id:?} must be refused"
        );
        tx.rollback().await.unwrap();
    }

    for (actor_type, actor_id) in [("UNKNOWN", None), ("WORKER", Some("w-1"))] {
        write(actor_type, actor_id)
            .execute(&mut api)
            .await
            .unwrap_or_else(|e| panic!("{actor_type}/{actor_id:?} must be accepted: {e}"));
    }
}

#[tokio::test]
async fn an_actor_outside_the_known_set_is_refused() {
    // Closed, like `outcome` and unlike `action`: "who acted" has a fixed
    // answer set in this system, and a spelling outside it — `worker`, or a
    // component name — would make a query for one party's actions silently
    // miss rows rather than fail.
    let Some((_db, mut api)) = migrated("audit_actor_set", "pool_api").await else {
        return;
    };

    for actor_type in ["worker", "SYSTEM", "pool_api", "", "ANONYMOUS"] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome)
             VALUES ('local-dev', 'testnet', $1, 'someone', 'SOMETHING_HAPPENED', 'APPLIED')",
        )
        .bind(actor_type)
        .execute(&mut *tx)
        .await;
        assert!(
            written.is_err(),
            "actor_type {actor_type:?} must be refused"
        );
        tx.rollback().await.unwrap();
    }

    for actor_type in ["MEMBER", "WORKER", "POOL", "OPERATOR", "TIG"] {
        sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome)
             VALUES ('local-dev', 'testnet', $1, 'someone', 'SOMETHING_HAPPENED', 'APPLIED')",
        )
        .bind(actor_type)
        .execute(&mut api)
        .await
        .unwrap_or_else(|e| panic!("{actor_type} must be accepted: {e}"));
    }
}

#[tokio::test]
async fn an_outcome_outside_the_known_set_is_refused() {
    // Unlike `action`, this is closed: every audit read starts from "what
    // happened", and a value outside the set makes that unanswerable by query.
    let Some((_db, mut api)) = migrated("audit_outcome", "pool_api").await else {
        return;
    };

    for outcome in ["rejected", "DENIED", "", "OK"] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome)
             VALUES ('local-dev', 'testnet', 'POOL', 'pool', 'SOMETHING_HAPPENED', $1)",
        )
        .bind(outcome)
        .execute(&mut *tx)
        .await;
        assert!(written.is_err(), "outcome {outcome:?} must be refused");
        tx.rollback().await.unwrap();
    }

    for outcome in ["ALLOWED", "REJECTED", "APPLIED", "FAILED"] {
        sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome)
             VALUES ('local-dev', 'testnet', 'POOL', 'pool', 'SOMETHING_HAPPENED', $1)",
        )
        .bind(outcome)
        .execute(&mut api)
        .await
        .unwrap_or_else(|e| panic!("{outcome} must be accepted: {e}"));
    }
}

#[tokio::test]
async fn an_action_or_reason_that_is_not_a_token_is_refused() {
    // A free-text action would make the table a log rather than something a
    // query can group by, which is the whole difference between §9's audit
    // fact and the tracing line beside it.
    let Some((_db, mut api)) = migrated("audit_tokens", "pool_api").await else {
        return;
    };

    for action in ["lower_case", "Mixed_Case", "A", "AB", "WITH SPACE", ""] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome)
             VALUES ('local-dev', 'testnet', 'POOL', 'pool', $1, 'APPLIED')",
        )
        .bind(action)
        .execute(&mut *tx)
        .await;
        assert!(written.is_err(), "action {action:?} must be refused");
        tx.rollback().await.unwrap();
    }

    let mut tx = api.begin().await.unwrap();
    let written = sqlx::query(
        "INSERT INTO pool.audit_event
             (deployment, network, actor_type, actor_id, action, outcome, reason)
         VALUES ('local-dev', 'testnet', 'POOL', 'pool', 'SOMETHING_HAPPENED',
                 'APPLIED', 'because it seemed right')",
    )
    .execute(&mut *tx)
    .await;
    assert!(written.is_err(), "a sentence is not a typed reason");
    tx.rollback().await.unwrap();

    // And the shapes that are accepted, so the rejections above are a rule
    // rather than a check that refuses everything.
    sqlx::query(
        "INSERT INTO pool.audit_event
             (deployment, network, actor_type, actor_id, action, outcome, reason)
         VALUES ('local-dev', 'testnet', 'POOL', 'pool', 'ABC', 'APPLIED', 'XYZ')",
    )
    .execute(&mut api)
    .await
    .expect("a three-character token is a token");
}

#[tokio::test]
async fn evidence_is_a_compact_object_and_not_a_place_to_put_a_payload() {
    // §9: "raw secrets and solutions are never audit evidence". The way a blob
    // reaches an audit row is someone attaching, just this once, a value that
    // did not fit anywhere else — so the size cap has to fail at the INSERT
    // rather than be a convention.
    let Some((_db, mut api)) = migrated("audit_evidence", "pool_api").await else {
        return;
    };

    let write = |evidence: String| {
        sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome, evidence)
             VALUES ('local-dev', 'testnet', 'POOL', 'pool', 'SOMETHING_HAPPENED',
                     'APPLIED', $1::jsonb)",
        )
        .bind(evidence)
    };

    // Not an object: an array or a bare string has no field names, so nothing
    // reading the row can say what the value was evidence *of*.
    for shape in ["[]", "\"a string\"", "42", "null"] {
        let mut tx = api.begin().await.unwrap();
        let written = write(shape.to_owned()).execute(&mut *tx).await;
        assert!(written.is_err(), "evidence {shape} must be refused");
        tx.rollback().await.unwrap();
    }

    let solution = "a".repeat(4096);
    let mut tx = api.begin().await.unwrap();
    let written = write(format!(r#"{{"just_this_once":"{solution}"}}"#))
        .execute(&mut *tx)
        .await;
    assert!(written.is_err(), "a payload must not fit");
    tx.rollback().await.unwrap();

    write(r#"{"package_sha256":"aa00","chunk_count":7}"#.to_owned())
        .execute(&mut api)
        .await
        .expect("hashes and counts are what evidence is");
}

#[tokio::test]
async fn a_resource_is_named_whole_or_not_at_all() {
    // Half a reference is worse than none: a reader cannot tell whether the
    // row is about a resource whose type was lost, or about nothing.
    let Some((_db, mut api)) = migrated("audit_resource", "pool_api").await else {
        return;
    };

    for (resource_type, resource_id) in [(Some("WORKER"), None), (None, Some("w-1"))] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome,
                  resource_type, resource_id)
             VALUES ('local-dev', 'testnet', 'POOL', 'pool', 'SOMETHING_HAPPENED',
                     'APPLIED', $1, $2)",
        )
        .bind(resource_type)
        .bind(resource_id)
        .execute(&mut *tx)
        .await;
        assert!(
            written.is_err(),
            "{resource_type:?}/{resource_id:?} must be refused"
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn no_other_component_writes_an_audit_row_yet() {
    // Only `pool_api` audits so far. The controller, the gateway and the
    // artifact worker each gain INSERT with their own first audited action —
    // the same incremental rule (`architecture.md` §7.1) that put this table
    // in this migration rather than an earlier one.
    let Some((db, mut api)) = migrated("audit_roles", "pool_api").await else {
        return;
    };
    replay_rejected(&mut api).await.unwrap();

    for role in ["pool_controller", "pool_gateway", "pool_artifact_worker"] {
        let mut other = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
        let written = replay_rejected(&mut other).await;
        assert!(written.is_err(), "{role} does not audit yet");
    }

    // The read-only role reads: `security.md` §8 makes access to audit logs
    // read-only and role restricted, not absent.
    let mut readonly = PgConnection::connect_with(&db.as_role("pool_readonly"))
        .await
        .unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.audit_event")
        .fetch_one(&mut readonly)
        .await
        .expect("the read-only role reads audit rows");
    assert_eq!(seen, 1);

    let written = replay_rejected(&mut readonly).await;
    assert!(written.is_err(), "and writes none");
}
