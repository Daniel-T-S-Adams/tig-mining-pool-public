//! Slice-2 criteria C1–C8 at the schema level: what `migrations/0019` lets a
//! slot be, and what it refuses.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb, exec};
use sqlx::{AssertSqlSafe, Connection, PgConnection};

const ALICE: &str = "0x1111111111111111111111111111111111111111";
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

/// One enrolled worker for a fresh member.
async fn worker(conn: &mut PgConnection) -> String {
    let member: String = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1) RETURNING member_id::text",
    )
    .bind(ALICE)
    .fetch_one(&mut *conn)
    .await
    .unwrap();

    sqlx::query_scalar(
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
    .unwrap()
}

/// A registered CPU slot at generation 1, with its generation row. Returns the
/// slot id; `digest` seeds the generation's spec digest.
///
/// One transaction, because that is what §2 requires of a registration and
/// what the deferred `slot_is_at_a_registered_generation` reference assumes:
/// the slot names a generation that the same commit creates.
async fn registered(conn: &mut PgConnection, worker_id: &str, key: &str, digest: u8) -> String {
    let mut tx = sqlx::Connection::begin(conn).await.unwrap();

    let slot: String = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2, 1)
         RETURNING slot_id::text",
    )
    .bind(worker_id)
    .bind(key)
    .fetch_one(&mut *tx)
    .await
    .unwrap();

    generation(&mut *tx, &slot, 1, None, digest).await.unwrap();
    tx.commit().await.unwrap();
    slot
}

/// One `pool.slot_generation` row for a CPU slot.
async fn generation<'c, E>(
    conn: E,
    slot: &str,
    generation: i64,
    prior: Option<i64>,
    digest: u8,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO pool.slot_generation
             (network, slot_id, generation, slot_registration_id, registration_sha256,
              prior_slot_generation, compute_kind, compute_type, cpu_arch,
              cpu_vendor, logical_cores, agent_version, runtime_bundle_version,
              available_image_manifest_digests, spec_digest)
         VALUES ('testnet', $1::uuid, $2, gen_random_uuid(), $3, $4,
                 'CPU', 'aws_t4g', 'arm64', 'arm', 4, '0.1.0', '0.1.0',
                 '[\"sha256:aa\"]'::jsonb, $5)",
    )
    .bind(slot)
    .bind(generation)
    .bind(SHA.as_slice())
    .bind(prior)
    .bind([digest; 32].as_slice())
    .execute(conn)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn a_slot_key_locates_one_slot_per_worker() {
    // §2: `client_slot_key` is "unique within a worker", because
    // `(worker_id, client_slot_key)` is what a reconfiguration uses to find
    // the slot it is reconfiguring.
    let Some((_db, mut api)) = migrated("slot_key", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    registered(&mut api, &w, "cpu-0", 0x01).await;

    // A complete second registration — slot *and* generation, in one
    // transaction as §2 requires — so the only thing that can refuse it is the
    // uniqueness of the key. A bare slot row is refused by the deferred
    // generation reference instead, and the test would then pass whether or
    // not the key was unique.
    let mut tx = sqlx::Connection::begin(&mut api).await.unwrap();
    let second: Result<String, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'cpu-0', 1)
         RETURNING slot_id::text",
    )
    .bind(&w)
    .fetch_one(&mut *tx)
    .await;

    match second {
        Err(e) => assert!(
            format!("{e}").contains("slot_key_is_unique_per_worker"),
            "expected the key index to refuse it, got: {e}"
        ),
        Ok(slot) => {
            generation(&mut *tx, &slot, 1, None, 0x02).await.unwrap();
            let committed = tx.commit().await;
            assert!(
                committed.is_err(),
                "one key is one slot for a worker, and this one committed"
            );
        }
    }
}

#[tokio::test]
async fn a_reconfiguration_names_the_current_generation_and_increments_it() {
    // §2: a new registration "names the current generation as
    // `prior_slot_generation`, atomically increments the generation, and
    // requires requalification. A stale prior generation conflicts."
    let Some((_db, mut api)) = migrated("slot_reconfigure", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let skipped = generation(&mut api, &slot, 3, Some(1), 0x03).await;
    assert!(skipped.is_err(), "a reconfiguration increments by one");

    generation(&mut api, &slot, 2, Some(1), 0x04)
        .await
        .expect("naming the current generation is what a reconfiguration does");

    sqlx::query("UPDATE pool.slot SET generation = 2 WHERE slot_id = $1::uuid")
        .bind(&slot)
        .execute(&mut api)
        .await
        .expect("the slot moves to the generation it just registered");

    // The stale case, in the only form that isolates it: the slot is at 2, the
    // registration names 1, and `prior < generation` still holds — so nothing
    // but the staleness itself can refuse it. Naming a prior *above* the
    // generation, which an earlier version of this test did, is refused by a
    // CHECK before the trigger is reached.
    let stale = generation(&mut api, &slot, 3, Some(1), 0x05)
        .await
        .expect_err("a stale prior generation conflicts");
    assert!(
        format!("{stale}").contains("stale prior generation"),
        "expected the prior-generation check, got: {stale}"
    );
}

#[tokio::test]
async fn two_generations_cannot_claim_one_spec_digest() {
    // §6 binds the digest to `[worker_id, slot_id, slot_generation, compute,
    // runtime_inventory]`, so two generations sharing one is a contradiction:
    // either the digest did not cover the generation, or identical facts were
    // registered twice — which §2 calls a no-op returning the existing
    // generation, not a new one.
    let Some((_db, mut api)) = migrated("slot_digest_unique", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let same_digest = generation(&mut api, &slot, 2, Some(1), 0x01).await;
    assert!(
        same_digest.is_err(),
        "a new generation describes something new"
    );
}

#[tokio::test]
async fn a_registration_retry_cannot_mint_a_second_generation() {
    // §16 invariant 3: "Registration, qualification, and capacity-offer retries
    // cannot create an extra slot generation, qualification result, precommit,
    // or reservation."
    let Some((_db, mut api)) = migrated("slot_retry", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let registration: String = sqlx::query_scalar(
        "SELECT slot_registration_id::text FROM pool.slot_generation
          WHERE slot_id = $1::uuid",
    )
    .bind(&slot)
    .fetch_one(&mut api)
    .await
    .unwrap();

    let twice = sqlx::query(
        "INSERT INTO pool.slot_generation
             (network, slot_id, generation, slot_registration_id, registration_sha256,
              prior_slot_generation, compute_kind, compute_type, cpu_arch,
              cpu_vendor, logical_cores, agent_version, runtime_bundle_version,
              available_image_manifest_digests, spec_digest)
         VALUES ('testnet', $1::uuid, 2, $2::uuid, $3, 1,
                 'CPU', 'aws_t4g', 'arm64', 'arm', 4, '0.1.0', '0.1.0',
                 '[]'::jsonb, $4)",
    )
    .bind(&slot)
    .bind(&registration)
    .bind(SHA.as_slice())
    .bind([0x09_u8; 32].as_slice())
    .execute(&mut api)
    .await;
    assert!(twice.is_err(), "one registration id is one generation");
}

#[tokio::test]
async fn reconfiguration_is_refused_while_the_slot_is_working() {
    // §2: "Reconfiguration is forbidden while an offer, assignment, or upload
    // is open." §9's ladder is where that is already recorded: anything but
    // AVAILABLE is a slot committed to work.
    let Some((db, mut api)) = migrated("slot_busy", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    sqlx::query("UPDATE pool.slot SET state = 'RESERVED' WHERE slot_id = $1::uuid")
        .bind(&slot)
        .execute(&mut controller)
        .await
        .unwrap();

    let busy = generation(&mut api as &mut PgConnection, &slot, 2, Some(1), 0x05).await;
    assert!(busy.is_err(), "a reserved slot is not reconfigured");
}

#[tokio::test]
async fn a_generation_is_a_record_and_not_a_draft() {
    // §2 has a reconfiguration add a generation rather than edit one, because
    // the qualification that referenced it and any assignment issued under it
    // described what it said at the time.
    let Some((db, mut api)) = migrated("slot_generation_append", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    let edited = exec(
        &mut owner,
        format!(
            "UPDATE pool.slot_generation SET logical_cores = 64 WHERE slot_id = '{slot}'::uuid"
        ),
    )
    .await
    .expect_err("a generation is not edited");
    assert!(
        format!("{edited}").contains("records one registration"),
        "expected the append-only trigger, got: {edited}"
    );
}

#[tokio::test]
async fn compute_facts_must_describe_the_kind_they_claim() {
    // §6: a CPU slot is a fixed group of logical cores; a v0 GPU slot is one
    // NVIDIA device with its driver versions. A row describing neither, or
    // both, is a report the pool cannot check against the compute matrix.
    let Some((_db, mut api)) = migrated("slot_facts", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;

    let cpu_shaped_gpu = register_generation(
        &mut api,
        &w,
        "gpu-0",
        "'GPU', 'nvidia_a10g', 'amd64', 'intel', 8, NULL, NULL, NULL, NULL",
        0x21,
    )
    .await;
    assert!(
        cpu_shaped_gpu.is_err(),
        "a GPU slot reports a device and its drivers, not cores"
    );

    // The case that isolates the GPU rule from the CPU one: a GPU generation
    // carrying *neither* set of facts. The CPU constraint is satisfied — no CPU
    // facts on a non-CPU row — so only the GPU constraint can refuse it.
    let factless_gpu = register_generation(
        &mut api,
        &w,
        "gpu-1",
        "'GPU', 'nvidia_a10g', 'amd64', NULL, NULL, NULL, NULL, NULL, NULL",
        0x23,
    )
    .await;
    assert!(
        factless_gpu.is_err(),
        "a GPU slot that names no device is not a report"
    );

    // And the mirror, which isolates the CPU rule: a CPU generation with no
    // cores.
    let factless_cpu = register_generation(
        &mut api,
        &w,
        "cpu-1",
        "'CPU', 'aws_t4g', 'arm64', NULL, NULL, NULL, NULL, NULL, NULL",
        0x24,
    )
    .await;
    assert!(factless_cpu.is_err(), "a CPU slot reports its cores");

    register_generation(
        &mut api,
        &w,
        "gpu-2",
        "'GPU', 'nvidia_a10g', 'amd64', NULL, NULL, 'GPU-0000', '550.54', '12.4', '1.15'",
        0x22,
    )
    .await
    .expect("a GPU slot that reports its device registers");
}

/// One complete registration — slot and generation, in one transaction — with
/// the compute facts supplied as a literal tuple.
///
/// Its own transaction per call, and rolled back on failure: a failed statement
/// aborts the transaction it was in, so a second attempt inside one fails
/// whatever the schema says, and every case after the first would pass for
/// free.
async fn register_generation(
    api: &mut PgConnection,
    worker_id: &str,
    key: &str,
    facts: &str,
    digest: u8,
) -> Result<(), sqlx::Error> {
    let mut tx = sqlx::Connection::begin(api).await.unwrap();
    let slot: String = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2, 1)
         RETURNING slot_id::text",
    )
    .bind(worker_id)
    .bind(key)
    .fetch_one(&mut *tx)
    .await
    .unwrap();

    let sql = format!(
        "INSERT INTO pool.slot_generation
             (network, slot_id, generation, slot_registration_id, registration_sha256,
              compute_kind, compute_type, cpu_arch, cpu_vendor, logical_cores,
              gpu_device, nvidia_driver_version, cuda_driver_version,
              container_toolkit_version,
              agent_version, runtime_bundle_version,
              available_image_manifest_digests, spec_digest)
         VALUES ('testnet', $1::uuid, 1, gen_random_uuid(), $2,
                 {facts}, '0.1.0', '0.1.0', '[]'::jsonb, $3)"
    );
    let outcome = sqlx::query(AssertSqlSafe(sql))
        .bind(&slot)
        .bind(SHA.as_slice())
        .bind([digest; 32].as_slice())
        .execute(&mut *tx)
        .await
        .map(|_| ());

    if outcome.is_ok() {
        tx.commit().await.unwrap();
    } else {
        let _ = tx.rollback().await;
    }
    outcome
}

#[tokio::test]
async fn a_qualification_belongs_to_one_generation_and_its_digest() {
    // §6 binds the task to one exact `qualification_spec_digest`, and a new
    // generation invalidates every older qualification — which here is not a
    // rule anyone applies but a consequence of what the row points at.
    let Some((_db, mut api)) = migrated("slot_qualification_bind", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let wrong_digest = sqlx::query(
        "INSERT INTO pool.slot_qualification
             (network, qualification_id, slot_id, generation, spec_digest,
              fixture_id, task_expires_at)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 1, $2, 'fx-1',
                 now() + interval '15 minutes')",
    )
    .bind(&slot)
    .bind([0xff_u8; 32].as_slice())
    .execute(&mut api)
    .await;
    assert!(
        wrong_digest.is_err(),
        "a task carries its generation's spec digest"
    );

    sqlx::query(
        "INSERT INTO pool.slot_qualification
             (network, qualification_id, slot_id, generation, spec_digest,
              fixture_id, task_expires_at)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 1, $2, 'fx-1',
                 now() + interval '15 minutes')",
    )
    .bind(&slot)
    .bind([0x01_u8; 32].as_slice())
    .execute(&mut api)
    .await
    .expect("the generation's own digest is what a task is issued for");

    let missing_generation = sqlx::query(
        "INSERT INTO pool.slot_qualification
             (network, qualification_id, slot_id, generation, spec_digest,
              fixture_id, task_expires_at)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 9, $2, 'fx-1',
                 now() + interval '15 minutes')",
    )
    .bind(&slot)
    .bind([0x01_u8; 32].as_slice())
    .execute(&mut api)
    .await;
    assert!(
        missing_generation.is_err(),
        "a qualification qualifies a generation that exists"
    );
}

#[tokio::test]
async fn a_qualification_decides_once_and_records_what_decided_it() {
    // §6: success or mismatch, once, from a submitted result. A QUALIFIED row
    // with no result is a pass nobody can re-derive; a decided row that can be
    // flipped is standing that moves without a decision.
    let Some((db, mut api)) = migrated("slot_qualification_decide", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let qualification: String = sqlx::query_scalar(
        "INSERT INTO pool.slot_qualification
             (network, qualification_id, slot_id, generation, spec_digest,
              fixture_id, task_expires_at)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 1, $2, 'fx-1',
                 now() + interval '15 minutes')
         RETURNING qualification_id::text",
    )
    .bind(&slot)
    .bind([0x01_u8; 32].as_slice())
    .fetch_one(&mut api)
    .await
    .unwrap();

    let resultless = sqlx::query(
        "UPDATE pool.slot_qualification SET state = 'QUALIFIED', decided_at = now()
          WHERE qualification_id = $1::uuid",
    )
    .bind(&qualification)
    .execute(&mut api)
    .await;
    assert!(
        resultless.is_err(),
        "a pass names the result that produced it"
    );

    sqlx::query(
        "UPDATE pool.slot_qualification
            SET state = 'QUALIFIED', decided_at = now(),
                qualification_result_id = gen_random_uuid(), result_sha256 = $2
          WHERE qualification_id = $1::uuid",
    )
    .bind(&qualification)
    .bind(SHA.as_slice())
    .execute(&mut api)
    .await
    .expect("a result decides the task");

    let again = sqlx::query(
        "UPDATE pool.slot_qualification SET state = 'FAILED', failure_reason = 'OUTPUT_MISMATCH'
          WHERE qualification_id = $1::uuid",
    )
    .bind(&qualification)
    .execute(&mut api)
    .await
    .expect_err("a decided qualification is not re-decided");
    assert!(
        format!("{again}").contains("decides once"),
        "expected the decide-once trigger, got: {again}"
    );

    let mut owner = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    let retimed = exec(
        &mut owner,
        format!(
            "UPDATE pool.slot_qualification SET task_expires_at = now() + interval '1 day'
              WHERE qualification_id = '{qualification}'::uuid"
        ),
    )
    .await
    .expect_err("the fifteen minutes are the task's, not the updater's");
    assert!(
        format!("{retimed}").contains("fixed when it is issued"),
        "expected the task trigger, got: {retimed}"
    );
}

#[tokio::test]
async fn one_generation_holds_one_pass() {
    // Two QUALIFIED rows for one generation look harmless until a failed
    // requalification leaves the older pass standing beside it.
    let Some((_db, mut api)) = migrated("slot_one_pass", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    for expect_ok in [true, false] {
        let id: String = sqlx::query_scalar(
            "INSERT INTO pool.slot_qualification
                 (network, qualification_id, slot_id, generation, spec_digest,
                  fixture_id, task_expires_at)
             VALUES ('testnet', gen_random_uuid(), $1::uuid, 1, $2, 'fx-1',
                     now() + interval '15 minutes')
             RETURNING qualification_id::text",
        )
        .bind(&slot)
        .bind([0x01_u8; 32].as_slice())
        .fetch_one(&mut api)
        .await
        .unwrap();

        let passed = sqlx::query(
            "UPDATE pool.slot_qualification
                SET state = 'QUALIFIED', decided_at = now(),
                    qualification_result_id = gen_random_uuid(), result_sha256 = $2
              WHERE qualification_id = $1::uuid",
        )
        .bind(&id)
        .bind(SHA.as_slice())
        .execute(&mut api)
        .await;
        assert_eq!(passed.is_ok(), expect_ok, "one pass per generation");
    }
}

#[tokio::test]
async fn a_slot_climbs_the_ladder_one_rung_at_a_time() {
    // §9's two paths, and §12's release. A slot that reaches UPLOADING without
    // COMPUTING is a package for work nobody did.
    let Some((db, mut api)) = migrated("slot_ladder", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    let set = |state: &str| {
        format!("UPDATE pool.slot SET state = '{state}' WHERE slot_id = '{slot}'::uuid")
    };

    let skipped = exec(&mut controller, set("UPLOADING"))
        .await
        .expect_err("a slot does not jump to uploading");
    assert!(
        format!("{skipped}").contains("does not go from"),
        "expected the ladder trigger, got: {skipped}"
    );

    for state in ["QUEUED", "RESERVED"] {
        exec(&mut controller, set(state))
            .await
            .unwrap_or_else(|e| panic!("a slot climbs to {state}: {e}"));
    }

    // A skip from partway up, not only from the bottom: a reserved slot that
    // reaches UPLOADING skipped both computing and packaging, and a rule that
    // only refuses the first rung would let it.
    let mid_skip = exec(&mut controller, set("UPLOADING"))
        .await
        .expect_err("a reserved slot has not computed anything yet");
    assert!(
        format!("{mid_skip}").contains("does not go from"),
        "expected the ladder trigger, got: {mid_skip}"
    );
    let backwards = exec(&mut controller, set("QUEUED"))
        .await
        .expect_err("and it does not descend into the queue again");
    assert!(
        format!("{backwards}").contains("does not go from"),
        "expected the ladder trigger, got: {backwards}"
    );

    for state in ["COMPUTING", "PACKAGING", "UPLOADING"] {
        exec(&mut controller, set(state))
            .await
            .unwrap_or_else(|e| panic!("a slot climbs to {state}: {e}"));
    }

    // §12: the receipt is what releases it, and a release is legal from
    // anywhere — a cancellation or an expiry frees the slot too.
    exec(&mut controller, set("AVAILABLE"))
        .await
        .expect("a slot is released to AVAILABLE");
    exec(&mut controller, set("RESERVED"))
        .await
        .expect("and the direct reservation is §9's other path");
    exec(&mut controller, set("AVAILABLE")).await.unwrap();
}

#[tokio::test]
async fn the_api_registers_and_the_controller_reserves() {
    // `architecture.md` §6 splits these deliberately: the API records what a
    // member said, the controller decides what the pool does about it. A member
    // who could write the slot's state could reserve their own slot.
    let Some((db, mut api)) = migrated("slot_grants", "pool_api").await else {
        return;
    };
    let w = worker(&mut api).await;
    let slot = registered(&mut api, &w, "cpu-0", 0x01).await;

    let reserved = exec(
        &mut api,
        format!("UPDATE pool.slot SET state = 'RESERVED' WHERE slot_id = '{slot}'::uuid"),
    )
    .await
    .expect_err("the API does not reserve slots");
    assert!(
        format!("{reserved}").contains("permission denied"),
        "expected a privilege refusal, got: {reserved}"
    );

    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let registered_by_controller = exec(
        &mut controller,
        format!(
            "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
             VALUES ('testnet', gen_random_uuid(), '{w}'::uuid, 'cpu-1', 1)"
        ),
    )
    .await;
    assert!(
        registered_by_controller.is_err(),
        "the controller does not register slots"
    );

    for privilege in ["INSERT", "UPDATE", "DELETE"] {
        let sql = format!("SELECT has_table_privilege('pool_readonly', $1, '{privilege}')");
        for table in [
            "pool.slot",
            "pool.slot_generation",
            "pool.slot_qualification",
        ] {
            let writable: bool = sqlx::query_scalar(AssertSqlSafe(sql.clone()))
                .bind(table)
                .fetch_one(&mut controller)
                .await
                .unwrap();
            assert!(!writable, "monitoring never {privilege}s {table}");
        }
    }

    for table in [
        "pool.slot",
        "pool.slot_generation",
        "pool.slot_qualification",
    ] {
        let visible: bool =
            sqlx::query_scalar("SELECT has_table_privilege('pool_gateway', $1, 'SELECT')")
                .bind(table)
                .fetch_one(&mut controller)
                .await
                .unwrap();
        assert!(!visible, "the gateway must not read {table}");
    }
}
