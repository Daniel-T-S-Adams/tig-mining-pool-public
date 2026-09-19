//! Slice-2 criteria B1–B9, D13 and L2, at the schema level: what
//! `migrations/0018` lets each role do, and what the rows refuse on their own.
//!
//! These assert the *table's* behaviour, before any service exists to rely on
//! it. Where slice 1 put a grant test beside the code that owns the rows, the
//! code here is still ahead of the schema — `pool-api` arrives in the next PR —
//! so the tests live with the migration tooling that produced the tables.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb};
use sqlx::{Connection, PgConnection};

const ALICE: &str = "0x1111111111111111111111111111111111111111";
const BOB: &str = "0x2222222222222222222222222222222222222222";
const KEY_A: [u8; 32] = [0xa1; 32];
const KEY_B: [u8; 32] = [0xb2; 32];
const SHA: [u8; 32] = [0x7e; 32];

/// A migrated database plus a connection for `role`.
async fn migrated(name: &str, role: &str) -> Option<(TempDb, PgConnection)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
    Some((db, conn))
}

/// The account, its worker, and that worker's credential, as the Pool API
/// creates them. Returns `(member_id, worker_id, credential_id)`.
async fn enrolled(
    conn: &mut PgConnection,
    wallet: &str,
    key: &[u8; 32],
) -> (String, String, String) {
    let member: String = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1) RETURNING member_id::text",
    )
    .bind(wallet)
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

    let credential: String = sqlx::query_scalar(
        "INSERT INTO pool.worker_credential (network, credential_id, worker_id, public_key)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2)
         RETURNING credential_id::text",
    )
    .bind(&worker)
    .bind(key.as_slice())
    .fetch_one(&mut *conn)
    .await
    .unwrap();

    (member, worker, credential)
}

#[tokio::test]
async fn the_api_creates_an_account_and_cannot_change_one() {
    // `architecture.md` §6 gives the Pool API the account system, and gives the
    // controller the application of an operator command. The wallet is the
    // identity (ADR 0011), so the only field an account has to change is its
    // standing — and that is the controller's.
    let Some((_db, mut api)) = migrated("member_api_grants", "pool_api").await else {
        return;
    };
    let (member, _, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let suspend =
        sqlx::query("UPDATE pool.member SET state = 'SUSPENDED' WHERE member_id = $1::uuid")
            .bind(&member)
            .execute(&mut api)
            .await;
    assert!(
        suspend.is_err(),
        "the API may create an account and may not change one"
    );
    let delete = sqlx::query("DELETE FROM pool.member WHERE member_id = $1::uuid")
        .bind(&member)
        .execute(&mut api)
        .await;
    assert!(delete.is_err(), "nor delete one");
}

#[tokio::test]
async fn only_the_controller_suspends_an_account_and_it_creates_none() {
    // The split is the point: one operation, one owner (`architecture.md` §13
    // invariant 6). A controller that could create accounts would be a second
    // enrollment path with none of §3.1's ticket rules.
    let Some((db, mut api)) = migrated("member_suspend", "pool_api").await else {
        return;
    };
    let (member, _, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    sqlx::query("UPDATE pool.member SET state = 'SUSPENDED' WHERE member_id = $1::uuid")
        .bind(&member)
        .execute(&mut controller)
        .await
        .expect("the controller applies the suspension an operator ordered");

    let created = sqlx::query(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1)",
    )
    .bind(BOB)
    .execute(&mut controller)
    .await;
    assert!(created.is_err(), "the controller does not enrol members");
}

#[tokio::test]
async fn one_wallet_is_one_account_per_network() {
    // ADR 0011: the address *is* the identity. Two rows for one address would
    // be two accounts for one member's money.
    let Some((_db, mut api)) = migrated("member_wallet_unique", "pool_api").await else {
        return;
    };
    enrolled(&mut api, ALICE, &KEY_A).await;

    let again = sqlx::query(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1)",
    )
    .bind(ALICE)
    .execute(&mut api)
    .await;
    assert!(again.is_err(), "one wallet is one testnet account");

    sqlx::query(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('mainnet', gen_random_uuid(), $1)",
    )
    .bind(ALICE)
    .execute(&mut api)
    .await
    .expect("the same wallet on another network is another account");
}

#[tokio::test]
async fn a_mixed_case_address_is_refused_rather_than_normalised() {
    // Folding case on write would let two spellings of one account exist until
    // the fold changed; refusing keeps the uniqueness index honest.
    let Some((_db, mut api)) = migrated("member_wallet_case", "pool_api").await else {
        return;
    };
    for bad in [
        "0x1111111111111111111111111111111111111AAA",
        "1111111111111111111111111111111111111111",
        "0x111111111111111111111111111111111111111",
    ] {
        let r = sqlx::query(
            "INSERT INTO pool.member (network, member_id, wallet_address)
             VALUES ('testnet', gen_random_uuid(), $1)",
        )
        .bind(bad)
        .execute(&mut api)
        .await;
        assert!(r.is_err(), "{bad} is not a lowercase Base address");
    }
}

#[tokio::test]
async fn the_multiplier_is_append_only_and_bounded() {
    // ADR 0010 and `accounting.md` §11.4: versioned policy, read at admission
    // and fixed into the reservation. A row that could be edited afterwards
    // would make the history disagree with the reservations taken under it.
    let Some((db, mut api)) = migrated("member_multiplier", "pool_api").await else {
        return;
    };
    let (member, _, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO pool.member_collateral_multiplier
             (network, member_id, version, bps, reason)
         VALUES ('testnet', $1::uuid, 1, 5000, 'early member discount')",
    )
    .bind(&member)
    .execute(&mut controller)
    .await
    .expect("the pool sets a member's multiplier");

    // Two different refusals, and both matter. The controller has no UPDATE
    // grant, so it is stopped at the privilege. The *owner* has every
    // privilege and is stopped by the trigger — which is the guard that
    // survives someone granting UPDATE later, so it is tested as itself
    // rather than through a grant that happens to shadow it.
    let by_grant = sqlx::query(
        "UPDATE pool.member_collateral_multiplier SET bps = 10000 WHERE member_id = $1::uuid",
    )
    .bind(&member)
    .execute(&mut controller)
    .await
    .expect_err("the controller has no UPDATE on this table");
    assert!(
        format!("{by_grant}").contains("permission denied"),
        "expected a privilege refusal, got: {by_grant}"
    );

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    for statement in [
        "UPDATE pool.member_collateral_multiplier SET bps = 10000 WHERE member_id = $1::uuid",
        "DELETE FROM pool.member_collateral_multiplier WHERE member_id = $1::uuid",
    ] {
        let by_trigger = sqlx::query(statement)
            .bind(&member)
            .execute(&mut owner)
            .await
            .expect_err("append-only holds against the table owner too");
        assert!(
            format!("{by_trigger}").contains("append-only"),
            "expected the append-only trigger, got: {by_trigger}"
        );
    }

    for bad in [0_i32, 10_001, -1] {
        let r = sqlx::query(
            "INSERT INTO pool.member_collateral_multiplier
                 (network, member_id, version, bps, reason)
             VALUES ('testnet', $1::uuid, 2, $2, 'out of range')",
        )
        .bind(&member)
        .bind(bad)
        .execute(&mut controller)
        .await;
        assert!(r.is_err(), "{bad} bps is outside 1..=10000");
    }

    let blank = sqlx::query(
        "INSERT INTO pool.member_collateral_multiplier
             (network, member_id, version, bps, reason)
         VALUES ('testnet', $1::uuid, 2, 5000, '   ')",
    )
    .bind(&member)
    .execute(&mut controller)
    .await;
    assert!(
        blank.is_err(),
        "a discount without a reason reports nothing"
    );
}

#[tokio::test]
async fn a_worker_and_a_credential_cannot_exist_without_what_owns_them() {
    // The chain `credential -> worker -> member` is what `security.md` §4.2
    // resolves authorization through. A row with a dangling parent is a
    // credential nobody can attribute.
    let Some((_db, mut api)) = migrated("member_fk", "pool_api").await else {
        return;
    };

    let orphan_worker = sqlx::query(
        "INSERT INTO pool.worker
             (network, worker_id, member_id, protocol_version,
              enrollment_request_id, enrollment_request_sha256)
         VALUES ('testnet', gen_random_uuid(), gen_random_uuid(), '0.1.0',
                 gen_random_uuid(), $1)",
    )
    .bind(SHA.as_slice())
    .execute(&mut api)
    .await;
    assert!(orphan_worker.is_err(), "a worker belongs to a member");

    let orphan_credential = sqlx::query(
        "INSERT INTO pool.worker_credential (network, credential_id, worker_id, public_key)
         VALUES ('testnet', gen_random_uuid(), gen_random_uuid(), $1)",
    )
    .bind(KEY_A.as_slice())
    .execute(&mut api)
    .await;
    assert!(
        orphan_credential.is_err(),
        "a credential belongs to a worker"
    );
}

#[tokio::test]
async fn one_enrollment_request_creates_one_worker() {
    // §3.1: a retried enrollment returns the same worker. Without this index a
    // retry racing itself creates two workers from one ticket, while every
    // statement involved still succeeds.
    let Some((_db, mut api)) = migrated("member_enroll_idem", "pool_api").await else {
        return;
    };
    let (member, _, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let request: String = sqlx::query_scalar(
        "SELECT enrollment_request_id::text FROM pool.worker WHERE member_id = $1::uuid",
    )
    .bind(&member)
    .fetch_one(&mut api)
    .await
    .unwrap();

    let twice = sqlx::query(
        "INSERT INTO pool.worker
             (network, worker_id, member_id, protocol_version,
              enrollment_request_id, enrollment_request_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, '0.1.0', $2::uuid, $3)",
    )
    .bind(&member)
    .bind(&request)
    .bind(SHA.as_slice())
    .execute(&mut api)
    .await;
    assert!(twice.is_err(), "one enrollment request is one worker");
}

#[tokio::test]
async fn a_public_key_authorizes_exactly_one_worker() {
    // `security.md` §4.2 authenticates a credential and reads its stored
    // worker. Two workers sharing a key makes that read ambiguous at the one
    // point where ambiguity is authorization.
    let Some((_db, mut api)) = migrated("member_key_unique", "pool_api").await else {
        return;
    };
    enrolled(&mut api, ALICE, &KEY_A).await;
    let (_, other_worker, _) = enrolled(&mut api, BOB, &KEY_B).await;

    let shared = sqlx::query(
        "INSERT INTO pool.worker_credential (network, credential_id, worker_id, public_key)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2)",
    )
    .bind(&other_worker)
    .bind(KEY_A.as_slice())
    .execute(&mut api)
    .await;
    assert!(shared.is_err(), "a key belongs to one worker");
}

#[tokio::test]
async fn a_rotation_cannot_mint_two_credentials() {
    // §3.3: "Repeating the same `rotation_id` and new public key returns the
    // same result." A second credential for one rotation is a key the member
    // never asked for and cannot revoke by rotating again.
    let Some((_db, mut api)) = migrated("member_rotation", "pool_api").await else {
        return;
    };
    let (_, worker, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let rotation: String = sqlx::query_scalar("SELECT gen_random_uuid()::text")
        .fetch_one(&mut api)
        .await
        .unwrap();

    for (key, expect_ok) in [(KEY_B, true), ([0xc3; 32], false)] {
        let r = sqlx::query(
            "INSERT INTO pool.worker_credential
                 (network, credential_id, worker_id, public_key, rotation_id)
             VALUES ('testnet', gen_random_uuid(), $1::uuid, $2, $3::uuid)",
        )
        .bind(&worker)
        .bind(key.as_slice())
        .bind(&rotation)
        .execute(&mut api)
        .await;
        assert_eq!(
            r.is_ok(),
            expect_ok,
            "one rotation id produces one credential"
        );
    }
}

#[tokio::test]
async fn a_revoked_worker_records_when_and_why() {
    // §3.3 makes the two revocation reasons behave differently — an accidental
    // one may be undone by worker recovery, a security one may not — so a
    // revocation without a reason is a decision nobody can apply later.
    let Some((_db, mut api)) = migrated("member_revocation", "pool_api").await else {
        return;
    };
    let (_, worker, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let silent = sqlx::query("UPDATE pool.worker SET state = 'REVOKED' WHERE worker_id = $1::uuid")
        .bind(&worker)
        .execute(&mut api)
        .await;
    assert!(silent.is_err(), "a revocation says when and why");

    let unknown = sqlx::query(
        "UPDATE pool.worker
            SET state = 'REVOKED', revoked_at = now(), revocation_reason = 'BECAUSE'
          WHERE worker_id = $1::uuid",
    )
    .bind(&worker)
    .execute(&mut api)
    .await;
    assert!(unknown.is_err(), "the reason is a bounded code");

    sqlx::query(
        "UPDATE pool.worker
            SET state = 'REVOKED', revoked_at = now(), revocation_reason = 'SECURITY'
          WHERE worker_id = $1::uuid",
    )
    .bind(&worker)
    .execute(&mut api)
    .await
    .expect("a recorded revocation is accepted");
}

#[tokio::test]
async fn a_ticket_is_bound_to_its_purpose_and_used_once() {
    // §3.1 and §3.3: one purpose, one worker for recovery, one use. A ticket
    // accepted for the other purpose would let an enrollment attach a key to an
    // existing worker — the attack the two signing domains separate.
    let Some((_db, mut api)) = migrated("member_ticket", "pool_api").await else {
        return;
    };
    let (member, worker, _) = enrolled(&mut api, ALICE, &KEY_A).await;

    let recovery_without_worker = sqlx::query(
        "INSERT INTO pool.enrollment_ticket
             (network, ticket_hmac, member_id, purpose, expires_at)
         VALUES ('testnet', $1, $2::uuid, 'WORKER_RECOVERY', now() + interval '15 minutes')",
    )
    .bind([0x01_u8; 32].as_slice())
    .bind(&member)
    .execute(&mut api)
    .await;
    assert!(
        recovery_without_worker.is_err(),
        "a recovery ticket names the worker it recovers"
    );

    let enrollment_with_worker = sqlx::query(
        "INSERT INTO pool.enrollment_ticket
             (network, ticket_hmac, member_id, purpose, worker_id, expires_at)
         VALUES ('testnet', $1, $2::uuid, 'WORKER_ENROLLMENT', $3::uuid,
                 now() + interval '15 minutes')",
    )
    .bind([0x02_u8; 32].as_slice())
    .bind(&member)
    .bind(&worker)
    .execute(&mut api)
    .await;
    assert!(
        enrollment_with_worker.is_err(),
        "an enrollment ticket has no worker yet"
    );

    sqlx::query(
        "INSERT INTO pool.enrollment_ticket
             (network, ticket_hmac, member_id, purpose, expires_at)
         VALUES ('testnet', $1, $2::uuid, 'WORKER_ENROLLMENT', now() + interval '15 minutes')",
    )
    .bind([0x03_u8; 32].as_slice())
    .bind(&member)
    .execute(&mut api)
    .await
    .unwrap();

    let half_consumed =
        sqlx::query("UPDATE pool.enrollment_ticket SET consumed_at = now() WHERE ticket_hmac = $1")
            .bind([0x03_u8; 32].as_slice())
            .execute(&mut api)
            .await;
    assert!(
        half_consumed.is_err(),
        "consumption records who consumed it, or it is not recorded at all"
    );

    let consumed = sqlx::query(
        "UPDATE pool.enrollment_ticket
            SET consumed_at = now(), consumed_by_worker_id = $2::uuid
          WHERE ticket_hmac = $1 AND consumed_at IS NULL",
    )
    .bind([0x03_u8; 32].as_slice())
    .bind(&worker)
    .execute(&mut api)
    .await
    .unwrap();
    assert_eq!(consumed.rows_affected(), 1);

    let again = sqlx::query(
        "UPDATE pool.enrollment_ticket
            SET consumed_at = now(), consumed_by_worker_id = $2::uuid
          WHERE ticket_hmac = $1 AND consumed_at IS NULL",
    )
    .bind([0x03_u8; 32].as_slice())
    .bind(&worker)
    .execute(&mut api)
    .await
    .unwrap();
    assert_eq!(again.rows_affected(), 0, "a ticket is used once");
}

#[tokio::test]
async fn monitoring_sees_standing_and_not_tickets_or_keys() {
    // `migrations/0001` refuses a blanket default so each table decides this.
    // The decision here: an operator asks whether a worker is revoked, never
    // what its key is or which member is mid-enrollment.
    let Some((db, mut api)) = migrated("member_readonly", "pool_api").await else {
        return;
    };
    enrolled(&mut api, ALICE, &KEY_A).await;

    let mut readonly = PgConnection::connect_with(&db.as_role("pool_readonly"))
        .await
        .unwrap();

    for (table, visible) in [
        ("pool.member", true),
        ("pool.member_collateral_multiplier", true),
        ("pool.worker", true),
        ("pool.worker_credential", false),
        ("pool.enrollment_ticket", false),
    ] {
        let granted: bool =
            sqlx::query_scalar("SELECT has_table_privilege('pool_readonly', $1, 'SELECT')")
                .bind(table)
                .fetch_one(&mut readonly)
                .await
                .unwrap();
        assert_eq!(granted, visible, "monitoring visibility of {table}");

        let writable: bool =
            sqlx::query_scalar("SELECT has_table_privilege('pool_readonly', $1, 'INSERT')")
                .bind(table)
                .fetch_one(&mut readonly)
                .await
                .unwrap();
        assert!(!writable, "monitoring never writes {table}");
    }
}

#[tokio::test]
async fn the_gateway_touches_nothing_member_facing() {
    // `architecture.md` §2.2: the credential-bearing process has no reason to
    // read a member. No grant is the decision, not an oversight.
    let Some((db, _api)) = migrated("member_gateway", "pool_api").await else {
        return;
    };
    let mut gateway = PgConnection::connect_with(&db.as_role("pool_gateway"))
        .await
        .unwrap();

    for table in [
        "pool.member",
        "pool.member_collateral_multiplier",
        "pool.worker",
        "pool.worker_credential",
        "pool.enrollment_ticket",
    ] {
        let visible: bool =
            sqlx::query_scalar("SELECT has_table_privilege('pool_gateway', $1, 'SELECT')")
                .bind(table)
                .fetch_one(&mut gateway)
                .await
                .unwrap();
        assert!(!visible, "the gateway must not read {table}");
    }
}
