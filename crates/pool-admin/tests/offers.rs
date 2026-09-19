//! Slice-2 criteria D1–D13 at the schema level: what `migrations/0020` lets a
//! capacity offer be.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb, exec};
use sqlx::{AssertSqlSafe, Connection, PgConnection};

/// Two lowercase hex characters for one byte, for building a distinct
/// wallet address per test fixture.
fn hex_byte(b: u8) -> String {
    format!("{b:02x}")
}
const SHA: [u8; 32] = [0x7e; 32];

async fn migrated(name: &str, role: &str) -> Option<(TempDb, PgConnection)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
    Some((db, conn))
}

/// A member, a worker, a registered slot at generation 1, and a QUALIFIED
/// qualification for it. Returns `(worker_id, slot_id, spec_digest)`.
async fn qualified_slot(
    conn: &mut PgConnection,
    key: &str,
    digest: u8,
) -> (String, String, [u8; 32]) {
    // A distinct wallet per call: the API may create accounts and may not
    // update one, so an upsert here would be refused by exactly the grant
    // `migrations/0018` exists to make.
    let wallet = format!("0x{}", hex_byte(digest).repeat(20));
    let member: String = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1) RETURNING member_id::text",
    )
    .bind(&wallet)
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

    let spec = [digest; 32];
    let mut tx = sqlx::Connection::begin(&mut *conn).await.unwrap();
    let slot: String = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2, 1)
         RETURNING slot_id::text",
    )
    .bind(&worker)
    .bind(key)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pool.slot_generation
             (network, slot_id, generation, slot_registration_id, registration_sha256,
              compute_kind, compute_type, cpu_arch, cpu_vendor, logical_cores,
              agent_version, runtime_bundle_version,
              available_image_manifest_digests, spec_digest)
         VALUES ('testnet', $1::uuid, 1, gen_random_uuid(), $2,
                 'CPU', 'aws_t4g', 'arm64', 'arm', 4, '0.1.0', '0.1.0',
                 '[]'::jsonb, $3)",
    )
    .bind(&slot)
    .bind(SHA.as_slice())
    .bind(spec.as_slice())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let qualification: String = sqlx::query_scalar(
        "INSERT INTO pool.slot_qualification
             (network, qualification_id, slot_id, generation, spec_digest,
              fixture_id, task_expires_at)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 1, $2, 'fx-1',
                 now() + interval '15 minutes')
         RETURNING qualification_id::text",
    )
    .bind(&slot)
    .bind(spec.as_slice())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE pool.slot_qualification
            SET state = 'QUALIFIED', decided_at = now(),
                qualification_result_id = gen_random_uuid(), result_sha256 = $2
          WHERE qualification_id = $1::uuid",
    )
    .bind(&qualification)
    .bind(SHA.as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();

    (worker, slot, spec)
}

/// Record one offer in `state`.
async fn offer(
    conn: &mut PgConnection,
    worker: &str,
    slot: &str,
    spec: &[u8; 32],
    state: &str,
) -> Result<String, sqlx::Error> {
    let queued = matches!(state, "QUEUED" | "READY_CHECK");
    let leased = matches!(state, "QUEUED" | "READY_CHECK" | "PENDING");
    sqlx::query_scalar(
        "INSERT INTO pool.capacity_offer
             (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
              state, offer_sha256, queue_accepted_at, lease_expires_at,
              ready_check_id, ready_check_expires_at)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2::uuid, 1, $3,
                 $4, $5,
                 CASE WHEN $6 THEN now() ELSE NULL END,
                 CASE WHEN $7 THEN now() + interval '90 seconds' ELSE NULL END,
                 CASE WHEN $4 = 'READY_CHECK' THEN gen_random_uuid() ELSE NULL END,
                 CASE WHEN $4 = 'READY_CHECK' THEN now() + interval '60 seconds' ELSE NULL END)
         RETURNING offer_id::text",
    )
    .bind(worker)
    .bind(slot)
    .bind(spec.as_slice())
    .bind(state)
    .bind(SHA.as_slice())
    .bind(queued)
    .bind(leased)
    .fetch_one(&mut *conn)
    .await
}

#[tokio::test]
async fn a_slot_holds_one_live_offer() {
    // §6 step 1: "Atomically reject another open offer or assignment for the
    // slot." §16 invariant 1 is what it protects — one open assignment
    // occupies exactly one slot, and two live offers are two claims on one.
    let Some((_db, mut api)) = migrated("offer_one_live", "pool_api").await else {
        return;
    };
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;

    offer(&mut api, &w, &slot, &spec, "PENDING").await.unwrap();

    for state in ["PENDING", "QUEUED", "READY_CHECK"] {
        let second = offer(&mut api, &w, &slot, &spec, state).await;
        assert!(
            second.is_err(),
            "a slot holds one live offer, not a {state}"
        );
    }

    // A refusal is not a live offer: §6 step 4 returns it "without queuing", so
    // it holds nothing and must not block the next attempt.
    offer(&mut api, &w, &slot, &spec, "REJECTED")
        .await
        .expect("a refusal occupies nothing");
}

#[tokio::test]
async fn an_offer_needs_a_qualification_for_the_generation_it_names() {
    // §16 invariant 2, at the point it bites. The offer carries a generation
    // and a digest; without a QUALIFIED row for that exact pair there is
    // nothing to offer from.
    let Some((db, mut api)) = migrated("offer_qualified", "pool_api").await else {
        return;
    };
    let (w, slot, _spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;

    // A second slot, registered but never qualified.
    let mut tx = sqlx::Connection::begin(&mut api).await.unwrap();
    let bare: String = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'cpu-1', 1)
         RETURNING slot_id::text",
    )
    .bind(&w)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pool.slot_generation
             (network, slot_id, generation, slot_registration_id, registration_sha256,
              compute_kind, compute_type, cpu_arch, cpu_vendor, logical_cores,
              agent_version, runtime_bundle_version,
              available_image_manifest_digests, spec_digest)
         VALUES ('testnet', $1::uuid, 1, gen_random_uuid(), $2,
                 'CPU', 'aws_t4g', 'arm64', 'arm', 4, '0.1.0', '0.1.0',
                 '[]'::jsonb, $3)",
    )
    .bind(&bare)
    .bind(SHA.as_slice())
    .bind([0x02_u8; 32].as_slice())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let unqualified = offer(&mut api, &w, &bare, &[0x02; 32], "PENDING")
        .await
        .expect_err("an unqualified slot cannot offer");
    assert!(
        format!("{unqualified}").contains("no qualification at generation"),
        "expected the qualification check, got: {unqualified}"
    );

    // §6: the offer "repeats the current compute facts and qualification-spec
    // digest so unexpected drift fails closed". A digest that is not what
    // qualified is exactly that drift.
    let drifted = offer(&mut api, &w, &slot, &[0xee; 32], "PENDING")
        .await
        .expect_err("a digest that did not qualify is drift");
    assert!(
        format!("{drifted}").contains("not what qualified"),
        "expected the digest check, got: {drifted}"
    );

    // And a refusal is recordable whatever the slot's standing — §6 step 4's
    // `REJECTED` is how `UNQUALIFIED` gets told to the member at all.
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let _ = &mut controller;
    offer(&mut api, &w, &bare, &[0x02; 32], "REJECTED")
        .await
        .expect("an unqualified slot's refusal is recordable");
}

#[tokio::test]
async fn a_queued_offer_keeps_its_place_while_its_lease_renews() {
    // §6 step 5 returns `QUEUED` "with a renewable lease", and §6's FIFO key
    // `(queue_accepted_at, offer_id)` is the place that lease must not move.
    // What §16 invariant 15 denies a queued offer is TIG capacity and
    // collateral — not a lease.
    let Some((db, mut api)) = migrated("offer_queue", "pool_api").await else {
        return;
    };
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;
    let id = offer(&mut api, &w, &slot, &spec, "QUEUED").await.unwrap();

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    // A lease renews without touching the place in the line.
    exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer SET lease_expires_at = now() + interval '90 seconds'
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await
    .expect("a queued offer's lease is renewable");

    let moved = exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer SET queue_accepted_at = now() + interval '1 hour'
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await
    .expect_err("a queued offer keeps its place in the line");
    assert!(
        format!("{moved}").contains("place in the line"),
        "expected the FIFO trigger, got: {moved}"
    );

    // Promotion: §6 issues a fresh `ready_check_id`, and only the worker's echo
    // permits the admission that follows.
    exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer
                SET state = 'READY_CHECK', ready_check_id = gen_random_uuid(),
                    ready_check_expires_at = now() + interval '60 seconds',
                    lease_expires_at = now() + interval '90 seconds'
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await
    .expect("a queued offer is promoted to a ready check");

    let reissued = exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer SET ready_check_id = gen_random_uuid()
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await
    .expect_err("a ready check is issued once");
    assert!(
        format!("{reissued}").contains("issued once"),
        "expected the ready-check trigger, got: {reissued}"
    );
}

#[tokio::test]
async fn an_offer_does_not_go_backwards_and_admitted_is_terminal() {
    // `ADMITTED` means a precommit intent exists for this offer. Going back to
    // `PENDING` from there would be an offer whose write is in flight claiming
    // to be waiting for one — which is how a second precommit gets created for
    // one reservation.
    let Some((db, mut api)) = migrated("offer_ladder", "pool_api").await else {
        return;
    };
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;
    let id = offer(&mut api, &w, &slot, &spec, "PENDING").await.unwrap();

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let set = |state: &str| {
        format!("UPDATE pool.capacity_offer SET state = '{state}' WHERE offer_id = '{id}'::uuid")
    };

    let backwards = exec(&mut controller, set("QUEUED"))
        .await
        .expect_err("a reserved offer does not rejoin the queue");
    assert!(
        format!("{backwards}").contains("does not go from"),
        "expected the ladder trigger, got: {backwards}"
    );

    exec(&mut controller, set("ADMITTED"))
        .await
        .expect("a reserved offer is admitted when its precommit intent exists");

    for state in ["PENDING", "CANCELLED", "EXPIRED"] {
        let after = exec(&mut controller, set(state))
            .await
            .expect_err("ADMITTED is terminal for the offer");
        assert!(
            format!("{after}").contains("is terminal"),
            "expected the terminal refusal for {state}, got: {after}"
        );
    }
}

#[tokio::test]
async fn a_lease_exists_exactly_where_it_means_something() {
    // §6: a pending offer is heartbeated against an explicit `lease_expires_at`
    // and "if the lease expires before a precommit is submitted, the offer is
    // cancelled without trust effect". A refused offer holds no lease, because
    // it holds nothing at all.
    let Some((_db, mut api)) = migrated("offer_lease", "pool_api").await else {
        return;
    };
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;

    let leaseless_pending = sqlx::query(
        "INSERT INTO pool.capacity_offer
             (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
              state, offer_sha256)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2::uuid, 1, $3,
                 'PENDING', $4)",
    )
    .bind(&w)
    .bind(&slot)
    .bind(spec.as_slice())
    .bind(SHA.as_slice())
    .execute(&mut api)
    .await;
    assert!(
        leaseless_pending.is_err(),
        "a reserved offer is held by a lease"
    );

    let leased_refusal = sqlx::query(
        "INSERT INTO pool.capacity_offer
             (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
              state, offer_sha256, lease_expires_at)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2::uuid, 1, $3,
                 'REJECTED', $4, now() + interval '90 seconds')",
    )
    .bind(&w)
    .bind(&slot)
    .bind(spec.as_slice())
    .bind(SHA.as_slice())
    .execute(&mut api)
    .await;
    assert!(
        leased_refusal.is_err(),
        "a refusal holds nothing, including a lease"
    );
}

#[tokio::test]
async fn the_api_records_and_the_controller_disposes() {
    // `architecture.md` §6: "Record a member offer ... | Pool API" and
    // "Admit/reject/queue an offer and reserve its slot | Controller". The API
    // cannot write the queue position, the lease, or the ready check — a member
    // who could would be admitting themselves.
    let Some((db, mut api)) = migrated("offer_grants", "pool_api").await else {
        return;
    };
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;
    let id = offer(&mut api, &w, &slot, &spec, "QUEUED").await.unwrap();

    for column in [
        "lease_expires_at = now() + interval '1 hour'",
        "queue_accepted_at = now() - interval '1 day'",
        "ready_check_id = gen_random_uuid()",
    ] {
        let denied = exec(
            &mut api,
            format!("UPDATE pool.capacity_offer SET {column} WHERE offer_id = '{id}'::uuid"),
        )
        .await
        .expect_err("the API records offers, it does not dispose of them");
        assert!(
            format!("{denied}").contains("permission denied"),
            "expected a privilege refusal for {column}, got: {denied}"
        );
    }

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let recorded = exec(
        &mut controller,
        format!(
            "INSERT INTO pool.capacity_offer
                 (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
                  state, offer_sha256)
             VALUES ('testnet', '{w}'::uuid, gen_random_uuid(), '{slot}'::uuid, 1,
                     decode(repeat('01', 32), 'hex'), 'REJECTED',
                     decode(repeat('7e', 32), 'hex'))"
        ),
    )
    .await;
    assert!(
        recorded.is_err(),
        "the controller does not invent offers a member did not make"
    );

    for privilege in ["INSERT", "UPDATE", "DELETE"] {
        let sql = format!(
            "SELECT has_table_privilege('pool_readonly', 'pool.capacity_offer', '{privilege}')"
        );
        let writable: bool = sqlx::query_scalar(AssertSqlSafe(sql))
            .fetch_one(&mut controller)
            .await
            .unwrap();
        assert!(!writable, "monitoring never {privilege}s an offer");
    }
    let gateway_sees: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('pool_gateway', 'pool.capacity_offer', 'SELECT')",
    )
    .fetch_one(&mut controller)
    .await
    .unwrap();
    assert!(!gateway_sees, "the gateway reads no member-facing row");
}
