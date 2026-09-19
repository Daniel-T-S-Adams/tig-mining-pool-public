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

/// Record one offer, as the Pool API does: `RECEIVED`, undecided.
async fn record(
    conn: &mut PgConnection,
    worker: &str,
    slot: &str,
    spec: &[u8; 32],
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO pool.capacity_offer
             (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
              state, offer_sha256)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2::uuid, 1, $3,
                 'RECEIVED', $4)
         RETURNING offer_id::text",
    )
    .bind(worker)
    .bind(slot)
    .bind(spec.as_slice())
    .bind(SHA.as_slice())
    .fetch_one(&mut *conn)
    .await
}

/// Dispose of one offer, as the Controller does.
async fn dispose(
    controller: &mut PgConnection,
    offer: &str,
    state: &str,
) -> Result<(), sqlx::Error> {
    let lease = matches!(state, "QUEUED" | "READY_CHECK" | "PENDING");
    let queued = matches!(state, "QUEUED");
    let checking = matches!(state, "READY_CHECK");
    exec(
        controller,
        format!(
            "UPDATE pool.capacity_offer
                SET state = '{state}',
                    lease_expires_at = CASE WHEN {lease}
                        THEN now() + interval '90 seconds' ELSE lease_expires_at END,
                    queue_accepted_at = CASE WHEN {queued} AND queue_accepted_at IS NULL
                        THEN now() ELSE queue_accepted_at END,
                    ready_check_id = CASE WHEN {checking}
                        THEN gen_random_uuid() ELSE ready_check_id END,
                    ready_check_expires_at = CASE WHEN {checking}
                        THEN now() + interval '60 seconds' ELSE ready_check_expires_at END
              WHERE offer_id = '{offer}'::uuid"
        ),
    )
    .await
}

/// Record an offer and walk it to `state` through the states before it.
async fn offer_in(
    api: &mut PgConnection,
    controller: &mut PgConnection,
    worker: &str,
    slot: &str,
    spec: &[u8; 32],
    state: &str,
) -> String {
    let id = record(api, worker, slot, spec).await.unwrap();
    let path: &[&str] = match state {
        "RECEIVED" => &[],
        "QUEUED" | "PENDING" | "NO_ACTION" | "REJECTED" => &[state],
        "READY_CHECK" => &["QUEUED", "READY_CHECK"],
        "ADMITTED" => &["PENDING", "ADMITTED"],
        other => panic!("no path to {other}"),
    };
    for step in path {
        dispose(controller, &id, step)
            .await
            .unwrap_or_else(|e| panic!("disposing to {step}: {e}"));
    }
    id
}

#[tokio::test]
async fn a_slot_holds_one_live_offer() {
    // §6 step 1: "Atomically reject another open offer or assignment for the
    // slot." §16 invariant 1 is what it protects — one open assignment occupies
    // exactly one slot, and two live offers are two claims on one.
    let Some((db, mut api)) = migrated("offer_one_live", "pool_api").await else {
        return;
    };
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;

    let first = offer_in(&mut api, &mut controller, &w, &slot, &spec, "PENDING").await;

    // `RECEIVED` is in the live set too: an offer the pool has not yet disposed
    // of is still a claim on the slot, and leaving it out would let a member
    // record ten and have the controller pick.
    let second = record(&mut api, &w, &slot, &spec).await;
    assert!(second.is_err(), "a slot holds one live offer");

    // A refusal is not a live offer: §6 step 4 returns it "without queuing", so
    // it holds nothing and must not block the next attempt.
    dispose(&mut controller, &first, "CANCELLED").await.unwrap();
    let refused = record(&mut api, &w, &slot, &spec).await.unwrap();
    dispose(&mut controller, &refused, "REJECTED")
        .await
        .unwrap();
    record(&mut api, &w, &slot, &spec)
        .await
        .expect("a refusal occupies nothing");
}

#[tokio::test]
async fn an_offer_is_admitted_only_from_a_slot_qualified_now() {
    // §16 invariant 2 at the point it bites, and on the disposition rather than
    // the arrival: §6 applies these checks during admission, and the API's job
    // before that is to record what the member sent — including an offer the
    // pool is about to refuse, which is how the member gets told `REJECTED`
    // and how the retry of that request returns the same answer.
    let Some((db, mut api)) = migrated("offer_qualified", "pool_api").await else {
        return;
    };
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;

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

    // Recorded, because a refusal has to be recordable.
    let unqualified = record(&mut api, &w, &bare, &[0x02; 32]).await.unwrap();
    dispose(&mut controller, &unqualified, "REJECTED")
        .await
        .expect("an unqualified slot's refusal is recorded, not an error");

    let promoted = record(&mut api, &w, &bare, &[0x02; 32]).await;
    // The live index holds one per slot; the refused one is not live, so this
    // records, and the disposition is what refuses it.
    let promoted = promoted.unwrap();
    let refused = dispose(&mut controller, &promoted, "PENDING")
        .await
        .expect_err("an unqualified slot cannot be reserved");
    assert!(
        format!("{refused}").contains("no qualification at generation"),
        "expected the qualification check, got: {refused}"
    );

    // §6: the offer "repeats the current compute facts and qualification-spec
    // digest so unexpected drift fails closed".
    let drifting = record(&mut api, &w, &slot, &[0xee; 32]).await.unwrap();
    let drifted = dispose(&mut controller, &drifting, "PENDING")
        .await
        .expect_err("a digest that did not qualify is drift");
    assert!(
        format!("{drifted}").contains("not what qualified"),
        "expected the digest check, got: {drifted}"
    );
    dispose(&mut controller, &drifting, "REJECTED")
        .await
        .unwrap();

    // And the case the first version of this migration missed: a slot
    // reconfigured to a newer generation, offering under the pass its previous
    // configuration earned.
    sqlx::query(
        "INSERT INTO pool.slot_generation
             (network, slot_id, generation, slot_registration_id, registration_sha256,
              prior_slot_generation, compute_kind, compute_type, cpu_arch,
              cpu_vendor, logical_cores, agent_version, runtime_bundle_version,
              available_image_manifest_digests, spec_digest)
         VALUES ('testnet', $1::uuid, 2, gen_random_uuid(), $2, 1,
                 'CPU', 'aws_t4g', 'arm64', 'arm', 8, '0.1.0', '0.1.0',
                 '[]'::jsonb, $3)",
    )
    .bind(&slot)
    .bind(SHA.as_slice())
    .bind([0x33_u8; 32].as_slice())
    .execute(&mut api)
    .await
    .unwrap();
    sqlx::query("UPDATE pool.slot SET generation = 2 WHERE slot_id = $1::uuid")
        .bind(&slot)
        .execute(&mut api)
        .await
        .unwrap();

    let stale = record(&mut api, &w, &slot, &spec).await.unwrap();
    let stale_error = dispose(&mut controller, &stale, "PENDING")
        .await
        .expect_err("a reconfigured slot does not offer under its old pass");
    assert!(
        format!("{stale_error}").contains("is at generation"),
        "expected the current-generation check, got: {stale_error}"
    );
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
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;
    let id = offer_in(&mut api, &mut controller, &w, &slot, &spec, "QUEUED").await;

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

    dispose(&mut controller, &id, "READY_CHECK")
        .await
        .expect("a queued offer is promoted to a ready check");

    // A restarted controller re-issues, and the fixture
    // `ready_check_replay_stale_id_ignored` says so: a second id is issued for
    // the same offer and the pre-restart echo is ignored. What a re-issue must
    // be is a new *window*, so the id and the deadline move together.
    let stale_window = exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer SET ready_check_id = gen_random_uuid()
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await
    .expect_err("a re-issued check cannot inherit the old deadline");
    assert!(
        format!("{stale_window}").contains("later deadline"),
        "expected the ready-check trigger, got: {stale_window}"
    );

    exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer
                SET ready_check_id = gen_random_uuid(),
                    ready_check_expires_at = ready_check_expires_at + interval '60 seconds'
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await
    .expect("a restarted controller re-issues with a fresh window");
}

#[tokio::test]
async fn an_offer_does_not_go_backwards_and_closes_rather_than_sealing_its_slot() {
    // `ADMITTED` means a precommit intent exists. Going back to `PENDING` would
    // be an offer whose write is in flight claiming to be waiting for one —
    // which is how a second precommit gets created for one reservation. And
    // `ADMITTED` is not the end: §6 says "after durable acceptance the slot may
    // offer again", so the offer closes and releases its place.
    let Some((db, mut api)) = migrated("offer_ladder", "pool_api").await else {
        return;
    };
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;
    let id = offer_in(&mut api, &mut controller, &w, &slot, &spec, "PENDING").await;

    let backwards = dispose(&mut controller, &id, "QUEUED")
        .await
        .expect_err("a reserved offer does not rejoin the queue");
    assert!(
        format!("{backwards}").contains("does not go from"),
        "expected the ladder trigger, got: {backwards}"
    );

    dispose(&mut controller, &id, "ADMITTED")
        .await
        .expect("a reserved offer is admitted when its precommit intent exists");

    // §6 makes the member's commitment irrevocable once the precommit is
    // recorded, so neither side drops it here.
    for state in ["PENDING", "CANCELLED", "EXPIRED"] {
        let after = dispose(&mut controller, &id, state)
            .await
            .expect_err("an admitted offer is not dropped");
        assert!(
            format!("{after}").contains("does not go from"),
            "expected the ladder refusal for {state}, got: {after}"
        );
    }

    // An admitted offer still occupies its slot — until the work is over.
    let while_admitted = record(&mut api, &w, &slot, &spec).await;
    assert!(
        while_admitted.is_err(),
        "an admitted offer holds its slot (§16 invariant 1)"
    );

    dispose(&mut controller, &id, "CLOSED")
        .await
        .expect("the work ends and the offer closes");
    record(&mut api, &w, &slot, &spec)
        .await
        .expect("§6: after durable acceptance the slot may offer again");

    let after_closed = dispose(&mut controller, &id, "PENDING")
        .await
        .expect_err("CLOSED is terminal");
    assert!(
        format!("{after_closed}").contains("is terminal"),
        "expected the terminal refusal, got: {after_closed}"
    );
}

#[tokio::test]
async fn a_lease_exists_exactly_where_it_means_something() {
    // §6: a pending offer is heartbeated against an explicit `lease_expires_at`
    // and "if the lease expires before a precommit is submitted, the offer is
    // cancelled without trust effect". A refused offer holds no lease, because
    // it holds nothing at all — and a recorded one holds none yet, because
    // nothing has been granted.
    let Some((db, mut api)) = migrated("offer_lease", "pool_api").await else {
        return;
    };
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;
    let id = record(&mut api, &w, &slot, &spec).await.unwrap();

    let leaseless_pending = exec(
        &mut controller,
        format!("UPDATE pool.capacity_offer SET state = 'PENDING' WHERE offer_id = '{id}'::uuid"),
    )
    .await;
    assert!(
        leaseless_pending.is_err(),
        "a reserved offer is held by a lease"
    );

    let leased_refusal = exec(
        &mut controller,
        format!(
            "UPDATE pool.capacity_offer
                SET state = 'REJECTED', lease_expires_at = now() + interval '90 seconds'
              WHERE offer_id = '{id}'::uuid"
        ),
    )
    .await;
    assert!(
        leased_refusal.is_err(),
        "a refusal holds nothing, including a lease"
    );
}

#[tokio::test]
async fn the_api_records_and_the_controller_disposes() {
    // `architecture.md` §5.1 step 1: the Pool API "records its idempotent
    // command. It does not decide whether the member may receive work." That is
    // a statement about the INSERT, which no column grant can express — so the
    // value is constrained too.
    let Some((db, mut api)) = migrated("offer_grants", "pool_api").await else {
        return;
    };
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let (w, slot, spec) = qualified_slot(&mut api, "cpu-0", 0x01).await;

    // An offer that arrives already decided is refused whoever inserts it.
    for state in ["PENDING", "QUEUED", "ADMITTED", "REJECTED"] {
        let decided = exec(
            &mut api,
            format!(
                "INSERT INTO pool.capacity_offer
                     (network, worker_id, offer_id, slot_id, slot_generation,
                      spec_digest, state, offer_sha256)
                 VALUES ('testnet', '{w}'::uuid, gen_random_uuid(), '{slot}'::uuid, 1,
                         decode(repeat('01', 32), 'hex'), '{state}',
                         decode(repeat('7e', 32), 'hex'))"
            ),
        )
        .await
        .expect_err("an offer is recorded undecided");
        assert!(
            format!("{decided}").contains("recorded as RECEIVED")
                || format!("{decided}").contains("permission denied"),
            "expected the arrival guard for {state}, got: {decided}"
        );
    }

    // And the decision columns are not the API's, at insert or afterwards.
    let id = record(&mut api, &w, &slot, &spec).await.unwrap();
    for column in [
        "lease_expires_at = now() + interval '1 hour'",
        "queue_accepted_at = now() - interval '1 day'",
        "ready_check_id = gen_random_uuid()",
        "state = 'PENDING'",
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

    // The arrival guard holds against the table owner too, which is the case a
    // column grant cannot cover: an offer that appears already holding a lease,
    // a queue position or a ready check is a decision with no record of who
    // made it.
    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    for column in [
        ("lease_expires_at", "now() + interval '90 seconds'"),
        ("queue_accepted_at", "now()"),
        ("ready_check_id", "gen_random_uuid()"),
        ("terminal_reason", "'POOL_LIMIT'"),
    ] {
        let (name, value) = column;
        let granted = exec(
            &mut owner,
            format!(
                "INSERT INTO pool.capacity_offer
                     (network, worker_id, offer_id, slot_id, slot_generation,
                      spec_digest, state, offer_sha256, {name})
                 VALUES ('testnet', '{w}'::uuid, gen_random_uuid(), '{slot}'::uuid, 1,
                         decode(repeat('01', 32), 'hex'), 'RECEIVED',
                         decode(repeat('7e', 32), 'hex'), {value})"
            ),
        )
        .await
        .expect_err("a lease, a queue position and a ready check are the pool's to grant");
        assert!(
            format!("{granted}").contains("pool's to grant"),
            "expected the arrival guard for {name}, got: {granted}"
        );
    }

    // And an offer names a slot its own worker owns. Two independent
    // references — one to the worker, one to the slot — would each be satisfied
    // by a row offering somebody else's.
    // The *other* worker's slot, which has no live offer of its own — so the
    // one-live-offer index cannot be what refuses this, and the ownership
    // reference has to be.
    let (_other_worker, other_slot, _) = qualified_slot(&mut api, "cpu-9", 0x09).await;
    let strangers_slot = exec(
        &mut api,
        format!(
            "INSERT INTO pool.capacity_offer
                 (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
                  state, offer_sha256)
             VALUES ('testnet', '{w}'::uuid, gen_random_uuid(), '{other_slot}'::uuid, 1,
                     decode(repeat('09', 32), 'hex'), 'RECEIVED',
                     decode(repeat('7e', 32), 'hex'))"
        ),
    )
    .await;
    assert!(
        strangers_slot.is_err(),
        "a worker offers its own slot, not another's"
    );

    let invented = exec(
        &mut controller,
        format!(
            "INSERT INTO pool.capacity_offer
                 (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
                  state, offer_sha256)
             VALUES ('testnet', '{w}'::uuid, gen_random_uuid(), '{slot}'::uuid, 1,
                     decode(repeat('01', 32), 'hex'), 'RECEIVED',
                     decode(repeat('7e', 32), 'hex'))"
        ),
    )
    .await;
    assert!(
        invented.is_err(),
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
