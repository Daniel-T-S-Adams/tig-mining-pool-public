//! Slice-2 criterion B8 at the schema level: what makes a wallet login one
//! use (`migrations/0026`, `accounting.md` §12.2).
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb};
use sqlx::{AssertSqlSafe, Connection, PgConnection};

const ALICE: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const BOB: &str = "0x2b5ad5c4795c026514f8317c7a215e218dccd6cf";
const NONCE: &str = "3f1a9c0e5b2d47a8bc6f91e0d3247a5b";

async fn migrated(name: &str, role: &str) -> Option<(TempDb, PgConnection)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
    Some((db, conn))
}

async fn consume(
    conn: &mut PgConnection,
    address: &str,
    nonce: &str,
    valid_for: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO pool.wallet_login_nonce
             (network, wallet_address, nonce, purpose, signature_expires_at)
         VALUES ('testnet', $1, $2, 'WORKER_ENROLLMENT', now() + interval '{valid_for}')"
    )))
    .bind(address)
    .bind(nonce)
    .execute(conn)
    .await
}

#[tokio::test]
async fn a_nonce_is_spent_once_for_the_address_that_signed_it() {
    let Some((_db, mut api)) = migrated("nonce_once", "pool_api").await else {
        return;
    };

    consume(&mut api, ALICE, NONCE, "10 minutes")
        .await
        .expect("the first login");

    let replay = consume(&mut api, ALICE, NONCE, "10 minutes").await;
    assert!(replay.is_err(), "the same signature must not spend twice");

    // And not for another purpose either. `purpose` is recorded, not part of
    // the key: a nonce is one use, not one use per authority. Adding it to
    // the key would let the same value be spent twice by a member who signed
    // two different texts with it.
    let other_purpose = sqlx::query(
        "INSERT INTO pool.wallet_login_nonce
             (network, wallet_address, nonce, purpose, signature_expires_at)
         VALUES ('testnet', $1, $2, 'WORKER_RECOVERY', now() + interval '10 minutes')",
    )
    .bind(ALICE)
    .bind(NONCE)
    .execute(&mut api)
    .await;
    assert!(
        other_purpose.is_err(),
        "a nonce is one use, not one use per purpose"
    );
}

#[tokio::test]
async fn one_member_cannot_spend_another_members_nonce() {
    // The key is `(network, address, nonce)` and not `(network, nonce)`.
    // Anyone can sign any nonce with their own key, so a global key would let
    // one party consume a value and deny it to another — a denial of service
    // on a member who had done nothing.
    let Some((_db, mut api)) = migrated("nonce_scope", "pool_api").await else {
        return;
    };

    consume(&mut api, ALICE, NONCE, "10 minutes")
        .await
        .expect("alice");
    consume(&mut api, BOB, NONCE, "10 minutes")
        .await
        .expect("bob's own use of the same value is his own");
}

#[tokio::test]
async fn a_spent_nonce_cannot_be_unspent() {
    // Editing the row is how a spent signature becomes unspent. Attempted as
    // `pool_migration`, which owns the table: `pool_api` has no UPDATE grant,
    // so run as the API the statement is refused before a trigger is
    // consulted and the test would pass with the trigger deleted.
    let Some((db, mut api)) = migrated("nonce_immutable", "pool_api").await else {
        return;
    };
    consume(&mut api, ALICE, NONCE, "10 minutes").await.unwrap();

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    for statement in [
        "UPDATE pool.wallet_login_nonce SET nonce = 'deadbeefdeadbeefdeadbeefdeadbeef'",
        "UPDATE pool.wallet_login_nonce SET signature_expires_at = now() - interval '1 hour'",
        "TRUNCATE pool.wallet_login_nonce",
    ] {
        let mut tx = owner.begin().await.unwrap();
        let changed = sqlx::query(AssertSqlSafe(statement.to_owned()))
            .execute(&mut *tx)
            .await;
        assert!(changed.is_err(), "{statement} must be refused");
        tx.rollback().await.unwrap();
    }

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.wallet_login_nonce")
        .fetch_one(&mut api)
        .await
        .unwrap();
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn a_nonce_is_not_forgotten_while_its_signature_could_still_be_used() {
    // Forgetting early is the same as never recording: the signature becomes
    // replayable, which is the one thing this table exists to stop.
    let Some((_db, mut api)) = migrated("nonce_retention", "pool_api").await else {
        return;
    };
    consume(&mut api, ALICE, NONCE, "10 minutes").await.unwrap();

    let early = sqlx::query("DELETE FROM pool.wallet_login_nonce")
        .execute(&mut api)
        .await;
    assert!(early.is_err(), "a live signature's nonce must stay");

    // A DELETE matching no rows reports success, so count rather than trust
    // the result.
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.wallet_login_nonce")
        .fetch_one(&mut api)
        .await
        .unwrap();
    assert_eq!(remaining, 1);

    // Past its expiry, it is prunable — otherwise the table would only grow.
    consume(&mut api, BOB, NONCE, "-1 minute").await.unwrap();
    let pruned = sqlx::query("DELETE FROM pool.wallet_login_nonce WHERE wallet_address = $1")
        .bind(BOB)
        .execute(&mut api)
        .await
        .expect("a spent signature past its expiry is prunable");
    assert_eq!(pruned.rows_affected(), 1);
}

#[tokio::test]
async fn an_address_or_nonce_the_pool_would_not_have_written_is_refused() {
    let Some((_db, mut api)) = migrated("nonce_shapes", "pool_api").await else {
        return;
    };

    // The address comes from signature recovery, which produces lowercase
    // hex; anything else means it came from somewhere else.
    for bad in [
        "0x7E5F4552091A69125D5DFCB7B8C2659029395BDF",
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf",
        "0x7e5f",
        "",
    ] {
        let mut tx = api.begin().await.unwrap();
        let written = consume(&mut tx, bad, NONCE, "10 minutes").await;
        assert!(written.is_err(), "address {bad:?} must be refused");
        tx.rollback().await.unwrap();
    }

    // Under 128 bits, or not hex at all.
    for bad in ["", "abc", &"a".repeat(31), "NOT-HEX", &"f".repeat(129)] {
        let mut tx = api.begin().await.unwrap();
        let written = consume(&mut tx, ALICE, bad, "10 minutes").await;
        assert!(written.is_err(), "nonce {bad:?} must be refused");
        tx.rollback().await.unwrap();
    }

    // Both ends of what is accepted, so the rejections are bounds.
    for good in [&"a".repeat(32), &"f".repeat(128)] {
        consume(&mut api, ALICE, good, "10 minutes")
            .await
            .unwrap_or_else(|e| panic!("nonce of {} must load: {e}", good.len()));
    }
}

#[tokio::test]
async fn a_purpose_outside_the_two_a_login_authorises_is_refused() {
    // Each purpose is a distinct authority: a signature obtained to enrol a
    // worker must not recover one.
    let Some((_db, mut api)) = migrated("nonce_purpose", "pool_api").await else {
        return;
    };

    for bad in ["", "LOGIN", "worker_enrollment", "ACCOUNT_RECOVERY"] {
        let mut tx = api.begin().await.unwrap();
        let written = sqlx::query(
            "INSERT INTO pool.wallet_login_nonce
                 (network, wallet_address, nonce, purpose, signature_expires_at)
             VALUES ('testnet', $1, $2, $3, now() + interval '10 minutes')",
        )
        .bind(ALICE)
        .bind(NONCE)
        .bind(bad)
        .execute(&mut *tx)
        .await;
        assert!(written.is_err(), "purpose {bad:?} must be refused");
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn no_other_component_knows_which_logins_were_spent() {
    // `security.md` §4.1 keeps the login in the Pool API; no other component
    // verifies a member signature.
    let Some((db, mut api)) = migrated("nonce_roles", "pool_api").await else {
        return;
    };
    consume(&mut api, ALICE, NONCE, "10 minutes").await.unwrap();

    for role in ["pool_controller", "pool_gateway", "pool_artifact_worker"] {
        let mut other = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
        let read = sqlx::query("SELECT 1 FROM pool.wallet_login_nonce")
            .execute(&mut other)
            .await;
        assert!(read.is_err(), "{role} must not read spent logins");
        let written = consume(&mut other, BOB, NONCE, "10 minutes").await;
        assert!(written.is_err(), "{role} must not spend one");
    }

    let mut readonly = PgConnection::connect_with(&db.as_role("pool_readonly"))
        .await
        .unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.wallet_login_nonce")
        .fetch_one(&mut readonly)
        .await
        .expect("the read-only role reads it");
    assert_eq!(seen, 1);
    assert!(
        consume(&mut readonly, BOB, NONCE, "10 minutes")
            .await
            .is_err(),
        "and spends none"
    );
}
