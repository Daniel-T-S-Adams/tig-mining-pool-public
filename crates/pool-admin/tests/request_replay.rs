//! Slice-2 criterion A4, at the schema level: what `migrations/0023` lets the
//! Pool API do with a recorded request attempt, and what the row refuses on
//! its own.
//!
//! `member_protocol.md` §3.2: the server "remembers each
//! `(credential_id, request_id)` for 24 hours. Reusing a request ID with
//! different signed bytes is rejected and audited." Almost every test here is
//! about the *remembering* — a record that can be edited, or dropped early, is
//! a rule that reports success on the case it exists to catch.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb, exec};
use sqlx::{AssertSqlSafe, Connection, PgConnection};

const ALICE: &str = "0x1111111111111111111111111111111111111111";
const KEY_A: [u8; 32] = [0xa1; 32];
const SHA: [u8; 32] = [0x7e; 32];

/// Two different signed byte strings, as their digests.
const SIGNED_A: &str = "aa00000000000000000000000000000000000000000000000000000000000001";
const SIGNED_B: &str = "bb00000000000000000000000000000000000000000000000000000000000002";

async fn migrated(name: &str, role: &str) -> Option<(TempDb, PgConnection)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
    Some((db, conn))
}

/// An account, a worker, and the credential whose attempts get recorded.
async fn credential(conn: &mut PgConnection) -> String {
    let member: String = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1) RETURNING member_id::text",
    )
    .bind(ALICE)
    .fetch_one(&mut *conn)
    .await
    .unwrap();

    let worker: String = sqlx::query_scalar(
        "INSERT INTO pool.worker
             (network, worker_id, member_id, protocol_version,
              enrollment_request_id, enrollment_request_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, '0.1.0', gen_random_uuid(), $2)
         RETURNING worker_id::text",
    )
    .bind(&member)
    .bind(SHA.as_slice())
    .fetch_one(&mut *conn)
    .await
    .unwrap();

    sqlx::query_scalar(
        "INSERT INTO pool.worker_credential (network, credential_id, worker_id, public_key)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2)
         RETURNING credential_id::text",
    )
    .bind(&worker)
    .bind(KEY_A.as_slice())
    .fetch_one(&mut *conn)
    .await
    .unwrap()
}

/// Record one attempt, seen `seen_ago` before now and remembered for
/// `remember` after that.
async fn record(
    conn: &mut PgConnection,
    credential_id: &str,
    request_id: &str,
    digest: &str,
    seen_ago: &str,
    remember: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    // The two intervals are test-supplied literals, never member input; the
    // three values a caller varies are bound.
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO pool.request_replay
             (network, credential_id, request_id, signed_sha256,
              first_seen_at, forget_after)
         VALUES ('testnet', $1::uuid, $2::uuid, $3,
                 now() - interval '{seen_ago}',
                 now() - interval '{seen_ago}' + interval '{remember}')"
    )))
    .bind(credential_id)
    .bind(request_id)
    .bind(digest)
    .execute(conn)
    .await
}

const NEW_REQUEST: &str = "11111111-1111-4111-8111-111111111111";

#[tokio::test]
async fn an_attempt_is_recorded_once_and_a_reuse_collides() {
    // The rule is uniqueness on `(credential_id, request_id)`, so it is the
    // primary key rather than an index beside one: a second attempt under the
    // same request ID is refused by the database, not by whichever caller
    // happened to read first.
    let Some((_db, mut api)) = migrated("replay_unique", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;

    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "0 seconds",
        "24 hours",
    )
    .await
    .expect("a first attempt is recorded");

    // Same ID, different signed bytes: §3.2's rejected replay.
    let replay = record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_B,
        "0 seconds",
        "24 hours",
    )
    .await;
    assert!(replay.is_err(), "a reused request ID must collide");

    // And the same ID with the *same* bytes collides too — the caller reads
    // the stored digest and decides, rather than writing again and hoping the
    // second write is a no-op.
    let retry = record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "0 seconds",
        "24 hours",
    )
    .await;
    assert!(retry.is_err(), "an identical retry is a read, not a write");
}

#[tokio::test]
async fn a_recorded_attempt_cannot_be_rewritten() {
    // The signed digest is the whole of the evidence. If it could be replaced,
    // a replay would be stored as a first sighting and §3.2's check would pass
    // on exactly the request it exists to refuse.
    //
    // Attempted as `pool_migration`, which owns the table and holds every
    // privilege on it. Run as `pool_api` this would prove nothing: that role
    // has no UPDATE grant, so the statement is refused before a trigger is
    // consulted, and the test would pass with the trigger deleted. The grant
    // half is `the_api_cannot_update_even_with_the_trigger_gone`.
    let Some((db, mut api)) = migrated("replay_immutable", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;
    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "0 seconds",
        "24 hours",
    )
    .await
    .unwrap();

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();

    for column in ["signed_sha256 = $2", "first_seen_at = now()"] {
        // One transaction per case: a failed statement aborts its
        // transaction, so a shared one would report failure for every case
        // after the first regardless of what the rule does.
        let mut tx = owner.begin().await.unwrap();
        let edit = sqlx::query(AssertSqlSafe(format!(
            "UPDATE pool.request_replay SET {column}
              WHERE credential_id = $1::uuid"
        )))
        .bind(&credential_id)
        .bind(SIGNED_B)
        .execute(&mut *tx)
        .await;
        assert!(edit.is_err(), "{column} must not be editable");
        tx.rollback().await.unwrap();
    }

    // The stored digest is still the one that was written.
    let stored: String = sqlx::query_scalar(
        "SELECT signed_sha256 FROM pool.request_replay WHERE credential_id = $1::uuid",
    )
    .bind(&credential_id)
    .fetch_one(&mut api)
    .await
    .unwrap();
    assert_eq!(stored, SIGNED_A);
}

#[tokio::test]
async fn a_record_is_not_forgotten_before_its_window_closes() {
    // Deleting early is the same failure as never writing: the next reuse of
    // that request ID is accepted as new. The database decides, rather than
    // the sweep that issues the DELETE — a sweep with a wrong `WHERE` clause
    // is precisely what this catches.
    let Some((_db, mut api)) = migrated("replay_retention", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;

    // Seen an hour ago, remembered for a day: twenty-three hours still to run.
    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "1 hour",
        "24 hours",
    )
    .await
    .unwrap();

    let early = sqlx::query("DELETE FROM pool.request_replay WHERE credential_id = $1::uuid")
        .bind(&credential_id)
        .execute(&mut api)
        .await;
    assert!(early.is_err(), "a live record must not be prunable");

    // A DELETE matching no rows reports success, so "the sweep ran and removed
    // nothing" and "the sweep was refused" look alike from the outside. Count
    // the rows.
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pool.request_replay WHERE credential_id = $1::uuid",
    )
    .bind(&credential_id)
    .fetch_one(&mut api)
    .await
    .unwrap();
    assert_eq!(remaining, 1, "the record is still there");
}

#[tokio::test]
async fn the_whole_window_cannot_be_forgotten_in_one_statement() {
    // TRUNCATE fires no row triggers, so the bounded-DELETE guard does not
    // see it. Losing the memory entire is worse than losing one record:
    // every request id in the forgotten window becomes reusable, which is the
    // replay §3.2 exists to refuse.
    //
    // Attempted as `pool_migration`, which owns the table — `pool_api` has no
    // TRUNCATE privilege, so run as the API this would prove nothing.
    let Some((db, mut api)) = migrated("replay_truncate", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;
    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "25 hours",
        "24 hours",
    )
    .await
    .unwrap();

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    let emptied = sqlx::query("TRUNCATE pool.request_replay")
        .execute(&mut owner)
        .await;
    assert!(emptied.is_err(), "the window must not be dropped whole");

    // Even though this very record is past due and prunable row by row.
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.request_replay")
        .fetch_one(&mut api)
        .await
        .unwrap();
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn a_record_past_its_window_is_prunable() {
    // The other half. Without this, the test above would pass with DELETE
    // refused unconditionally, and the table would grow for ever.
    let Some((_db, mut api)) = migrated("replay_prune", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;

    // Seen 25 hours ago and remembered for 24: an hour past due.
    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "25 hours",
        "24 hours",
    )
    .await
    .unwrap();

    let pruned = sqlx::query("DELETE FROM pool.request_replay WHERE credential_id = $1::uuid")
        .bind(&credential_id)
        .execute(&mut api)
        .await
        .expect("a due record is prunable");
    assert_eq!(pruned.rows_affected(), 1);
}

#[tokio::test]
async fn a_window_shorter_than_a_day_is_refused() {
    // §3.2's 24 hours is a floor, not a target: a deployment may remember
    // longer and may not remember for less, because the window is what makes
    // the rule mean anything.
    let Some((_db, mut api)) = migrated("replay_window", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;

    for short in ["0 seconds", "23 hours 59 minutes", "-1 hours"] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO pool.request_replay
                 (network, credential_id, request_id, signed_sha256,
                  first_seen_at, forget_after)
             VALUES ('testnet', $1::uuid, gen_random_uuid(), $2,
                     now(), now() + interval '{short}')"
        )))
        .bind(&credential_id)
        .bind(SIGNED_A)
        .execute(&mut *tx)
        .await;
        assert!(written.is_err(), "a {short} window must be refused");
        tx.rollback().await.unwrap();
    }

    // Exactly a day is accepted, so the rejections above are a bound rather
    // than a check that refuses everything.
    sqlx::query(
        "INSERT INTO pool.request_replay
             (network, credential_id, request_id, signed_sha256,
              first_seen_at, forget_after)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2,
                 now(), now() + interval '24 hours')",
    )
    .bind(&credential_id)
    .bind(SIGNED_A)
    .execute(&mut api)
    .await
    .expect("exactly a day is enough");
}

#[tokio::test]
async fn a_record_names_a_credential_that_exists() {
    // Without the foreign key a record could be written for a credential that
    // was never issued — a row whose reuse nothing would ever check, because
    // no request can be signed by a key the pool does not hold.
    let Some((_db, mut api)) = migrated("replay_fk", "pool_api").await else {
        return;
    };

    let orphan = record(
        &mut api,
        "22222222-2222-4222-8222-222222222222",
        NEW_REQUEST,
        SIGNED_A,
        "0 seconds",
        "24 hours",
    )
    .await;
    assert!(orphan.is_err(), "an unknown credential has no attempts");
}

#[tokio::test]
async fn a_digest_that_is_not_a_sha_256_is_refused() {
    // The comparison that decides replay is string equality on this column, so
    // a value in another shape would compare unequal to the same bytes written
    // differently and report a replay that did not happen.
    let Some((_db, mut api)) = migrated("replay_digest", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;

    for bad in [
        "",
        "AA00000000000000000000000000000000000000000000000000000000000001",
        "aa000000000000000000000000000000000000000000000000000000000000",
        "0xaa00000000000000000000000000000000000000000000000000000000001",
    ] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(
            "INSERT INTO pool.request_replay
                 (network, credential_id, request_id, signed_sha256,
                  first_seen_at, forget_after)
             VALUES ('testnet', $1::uuid, gen_random_uuid(), $2,
                     now(), now() + interval '24 hours')",
        )
        .bind(&credential_id)
        .bind(bad)
        .execute(&mut *tx)
        .await;
        assert!(written.is_err(), "digest {bad:?} must be refused");
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn no_other_role_reads_or_writes_an_attempt() {
    // `architecture.md` §3 gives request authentication to the API, and no
    // other component verifies a member signature. A controller able to write
    // here could record an attempt that never happened, or erase one that did.
    let Some((db, mut api)) = migrated("replay_roles", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;
    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "0 seconds",
        "24 hours",
    )
    .await
    .unwrap();

    for role in ["pool_controller", "pool_gateway", "pool_artifact_worker"] {
        let mut other = PgConnection::connect_with(&db.as_role(role)).await.unwrap();

        let read = sqlx::query("SELECT 1 FROM pool.request_replay")
            .execute(&mut other)
            .await;
        assert!(read.is_err(), "{role} must not read attempts");

        let written = record(
            &mut other,
            &credential_id,
            "33333333-3333-4333-8333-333333333333",
            SIGNED_B,
            "0 seconds",
            "24 hours",
        )
        .await;
        assert!(written.is_err(), "{role} must not record an attempt");
    }

    // The read-only role does read it: `architecture.md` §9 has an operator
    // able to see what the pool decided, and an unexplained rejection is one
    // of the things they are asked about.
    let mut readonly = PgConnection::connect_with(&db.as_role("pool_readonly"))
        .await
        .unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.request_replay")
        .fetch_one(&mut readonly)
        .await
        .expect("the read-only role reads it");
    assert_eq!(seen, 1);

    let written = record(
        &mut readonly,
        &credential_id,
        "44444444-4444-4444-8444-444444444444",
        SIGNED_B,
        "0 seconds",
        "24 hours",
    )
    .await;
    assert!(written.is_err(), "and writes nothing");
}

#[tokio::test]
async fn the_api_cannot_update_even_with_the_trigger_gone() {
    // Two defences, deliberately: the grant says what the role does and the
    // trigger says what nobody does. This checks the privilege half, so that
    // removing the trigger would not silently leave the table editable.
    let Some((db, mut api)) = migrated("replay_no_update_grant", "pool_api").await else {
        return;
    };
    let credential_id = credential(&mut api).await;
    record(
        &mut api,
        &credential_id,
        NEW_REQUEST,
        SIGNED_A,
        "0 seconds",
        "24 hours",
    )
    .await
    .unwrap();

    let mut migration = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    exec(
        &mut migration,
        "DROP TRIGGER request_replay_immutable ON pool.request_replay".to_owned(),
    )
    .await
    .expect("the migration role owns the trigger");

    let edit = sqlx::query(
        "UPDATE pool.request_replay SET signed_sha256 = $2 WHERE credential_id = $1::uuid",
    )
    .bind(&credential_id)
    .bind(SIGNED_B)
    .execute(&mut api)
    .await;
    assert!(
        edit.is_err(),
        "the missing UPDATE privilege refuses before the trigger would"
    );
}
