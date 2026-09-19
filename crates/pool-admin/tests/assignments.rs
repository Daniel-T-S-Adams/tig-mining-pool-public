//! Slice-2 criteria E1–E6 and F1–F6 at the schema level: what
//! `migrations/0021` lets an assignment be, and what a member event may say.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb, exec};
use sqlx::{Connection, PgConnection};

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

/// Everything an assignment needs to exist: a member, a worker, a qualified
/// slot, an admitted offer, and a workflow whose precommit TIG confirmed.
struct Ready {
    member: String,
    worker: String,
    slot: String,
    offer: String,
    workflow: String,
    benchmark: String,
}

async fn ready(owner: &mut PgConnection, tag: u8) -> Ready {
    let wallet = format!("0x{}", format!("{tag:02x}").repeat(20));
    let member: String = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ('testnet', gen_random_uuid(), $1) RETURNING member_id::text",
    )
    .bind(&wallet)
    .fetch_one(&mut *owner)
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
    .fetch_one(&mut *owner)
    .await
    .unwrap();

    let spec = [tag; 32];
    let mut tx = sqlx::Connection::begin(&mut *owner).await.unwrap();
    let slot: String = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'cpu-0', 1)
         RETURNING slot_id::text",
    )
    .bind(&worker)
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
    .fetch_one(&mut *owner)
    .await
    .unwrap();
    exec(
        owner,
        format!(
            "UPDATE pool.slot_qualification
                SET state = 'QUALIFIED', decided_at = now(),
                    qualification_result_id = gen_random_uuid(),
                    result_sha256 = decode(repeat('7e', 32), 'hex')
              WHERE qualification_id = '{qualification}'::uuid"
        ),
    )
    .await
    .unwrap();

    let offer: String = sqlx::query_scalar(
        "INSERT INTO pool.capacity_offer
             (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
              state, offer_sha256)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2::uuid, 1, $3, 'RECEIVED', $4)
         RETURNING offer_id::text",
    )
    .bind(&worker)
    .bind(&slot)
    .bind(spec.as_slice())
    .bind(SHA.as_slice())
    .fetch_one(&mut *owner)
    .await
    .unwrap();
    for state in ["PENDING", "ADMITTED"] {
        let lease = if state == "PENDING" {
            ", lease_expires_at = now() + interval '90 seconds'"
        } else {
            ""
        };
        exec(
            owner,
            format!(
                "UPDATE pool.capacity_offer SET state = '{state}'{lease}
                  WHERE offer_id = '{offer}'::uuid"
            ),
        )
        .await
        .unwrap();
    }

    // A workflow whose precommit TIG confirmed — slice 1's row, in the state
    // §6 step 8 requires before an assignment may be published.
    let workflow = format!("wf-{tag:02x}");
    let benchmark = format!("bench-{tag:02x}");
    exec(
        owner,
        format!(
            "INSERT INTO pool.workflow
                 (network, workflow_id, state, owner_kind, owner_id,
                  unverified_from_block, benchmark_id, block_started,
                  confirmed_track_id, confirmed_settings, confirmed_num_nonces,
                  precommit_confirmed_block)
             VALUES ('testnet', '{workflow}', 'PRECOMMIT_CONFIRMED', 'MEMBER', '{member}',
                     100, '{benchmark}', 100, 'n_nodes=100', '{{}}'::jsonb, 512, 101)"
        ),
    )
    .await
    .unwrap();

    Ready {
        member,
        worker,
        slot,
        offer,
        workflow,
        benchmark,
    }
}

/// Publish one assignment for `r`, as the controller does.
async fn publish(
    controller: &mut PgConnection,
    r: &Ready,
    digest: u8,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO pool.assignment
             (network, assignment_id, member_id, worker_id, slot_id, slot_generation,
              offer_id, workflow_id, benchmark_id, confirmed_track_id, compute_type,
              num_nonces, assignment_digest, identity,
              workflow_expiry_block, proof_reserve_blocks, package_due_before_block,
              ack_by)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2::uuid, $3::uuid, 1,
                 $4::uuid, $5, $6, 'n_nodes=100', 'aws_t4g',
                 512, $7, '{}'::jsonb, 220, 10, 210, now() + interval '60 seconds')
         RETURNING assignment_id::text",
    )
    .bind(&r.member)
    .bind(&r.worker)
    .bind(&r.slot)
    .bind(&r.offer)
    .bind(&r.workflow)
    .bind(&r.benchmark)
    .bind([digest; 32].as_slice())
    .fetch_one(&mut *controller)
    .await
}

#[tokio::test]
async fn an_assignment_is_published_only_from_a_confirmed_precommit() {
    // §6 step 8 and §16 invariant 4. The workflow's state is where the
    // confirmation lives, and a `DECIDED` workflow has a precommit that may
    // never land.
    let Some((db, mut owner)) = migrated("assign_confirmed", "pool_migration").await else {
        return;
    };
    let mut r = ready(&mut owner, 0x01).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    // A second workflow for the same member, still `DECIDED` — slice 1 makes a
    // workflow's benchmark id immutable once set, so an unconfirmed workflow
    // has to be one that was never confirmed rather than one walked backwards.
    exec(
        &mut owner,
        format!(
            "INSERT INTO pool.workflow
                 (network, workflow_id, state, owner_kind, owner_id,
                  unverified_from_block, block_started)
             VALUES ('testnet', 'wf-unconfirmed', 'DECIDED', 'MEMBER', '{}', 100, 100)",
            r.member
        ),
    )
    .await
    .unwrap();

    let confirmed_workflow = r.workflow.clone();
    r.workflow = "wf-unconfirmed".to_string();
    let unconfirmed = publish(&mut controller, &r, 0x11)
        .await
        .expect_err("a decided workflow has no confirmed precommit");
    assert!(
        format!("{unconfirmed}").contains("from a confirmed precommit"),
        "expected the confirmation check, got: {unconfirmed}"
    );

    r.workflow = confirmed_workflow;
    publish(&mut controller, &r, 0x11)
        .await
        .expect("a confirmed precommit is what an assignment is published from");
}

#[tokio::test]
async fn an_assignment_names_the_benchmark_its_workflow_holds() {
    // §16 invariant 4: an assignment "contains only confirmed TIG precommit
    // facts". A benchmark id the workflow does not hold is a fact from
    // somewhere else.
    let Some((db, mut owner)) = migrated("assign_benchmark", "pool_migration").await else {
        return;
    };
    let mut r = ready(&mut owner, 0x02).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    let real = r.benchmark.clone();
    r.benchmark = "bench-somebody-elses".to_string();
    let wrong = publish(&mut controller, &r, 0x22)
        .await
        .expect_err("the assignment carries the workflow's benchmark");
    assert!(
        format!("{wrong}").contains("confirmed facts"),
        "expected the confirmed-facts check, got: {wrong}"
    );

    r.benchmark = real;
    publish(&mut controller, &r, 0x22).await.unwrap();
}

#[tokio::test]
async fn an_assignment_carries_every_confirmed_fact_and_its_owner() {
    // §16 invariant 4 is about *every* confirmed fact, not only the benchmark
    // id: TIG selects the track and derives the nonce count, so publishing the
    // pool's proposal for either publishes something TIG never confirmed. And
    // `mining_system.md` §10 invariant 1 gives the benchmark exactly one member
    // owner — the one the workflow already names.
    let Some((db, mut owner)) = migrated("assign_facts", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x0c).await;
    let other = ready(&mut owner, 0x0d).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    let sql_for = |column: &str, value: &str| {
        format!(
            "INSERT INTO pool.assignment
                 (network, assignment_id, member_id, worker_id, slot_id, slot_generation,
                  offer_id, workflow_id, benchmark_id, confirmed_track_id, compute_type,
                  num_nonces, assignment_digest, identity,
                  workflow_expiry_block, proof_reserve_blocks, package_due_before_block,
                  ack_by)
             VALUES ('testnet', gen_random_uuid(), '{member}'::uuid, '{worker}'::uuid,
                     '{slot}'::uuid, 1, '{offer}'::uuid, '{workflow}', '{benchmark}',
                     {track}, {compute}, {nonces},
                     decode(repeat('cc', 32), 'hex'), '{{}}'::jsonb,
                     220, 10, 210, now() + interval '60 seconds')",
            member = if column == "member_id" {
                value.to_string()
            } else {
                r.member.clone()
            },
            worker = r.worker,
            slot = r.slot,
            offer = r.offer,
            workflow = r.workflow,
            benchmark = r.benchmark,
            track = if column == "track" {
                format!("'{value}'")
            } else {
                "'n_nodes=100'".to_string()
            },
            compute = if column == "compute" {
                format!("'{value}'")
            } else {
                "'aws_t4g'".to_string()
            },
            nonces = if column == "nonces" {
                value.to_string()
            } else {
                "512".to_string()
            },
        )
    };

    let wrong_track = exec(&mut controller, sql_for("track", "n_nodes=999"))
        .await
        .expect_err("a track TIG did not select is not a confirmed fact");
    assert!(
        format!("{wrong_track}").contains("confirmed facts"),
        "expected the confirmed-facts check, got: {wrong_track}"
    );

    let wrong_nonces = exec(&mut controller, sql_for("nonces", "4096"))
        .await
        .expect_err("a nonce count TIG did not derive is not a confirmed fact");
    assert!(
        format!("{wrong_nonces}").contains("confirmed facts"),
        "expected the confirmed-facts check, got: {wrong_nonces}"
    );

    let wrong_owner = exec(&mut controller, sql_for("member_id", &other.member))
        .await
        .expect_err("the workflow's owner is the benchmark's owner");
    assert!(
        format!("{wrong_owner}").contains("owned by"),
        "expected the ownership check, got: {wrong_owner}"
    );

    let wrong_compute = exec(&mut controller, sql_for("compute", "nvidia_a10g"))
        .await
        .expect_err("§7: the confirmed compute type equals the slot's");
    assert!(
        format!("{wrong_compute}").contains("the slot generation is"),
        "expected the compute check, got: {wrong_compute}"
    );

    exec(&mut controller, sql_for("none", ""))
        .await
        .expect("the confirmed facts, unchanged, publish");
}

#[tokio::test]
async fn an_assignment_occupies_the_slot_its_offer_reserved() {
    // §16 invariant 1: "one open assignment occupies exactly one slot" — the
    // one its offer reserved. Naming a *different slot of the same worker*
    // leaves the reserved one occupied by nothing and the named one occupied
    // twice, and the ownership references say nothing about it because both
    // slots belong to the same worker.
    let Some((db, mut owner)) = migrated("assign_slot", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x0e).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    // A second slot for the same worker, registered at generation 1.
    let mut tx = sqlx::Connection::begin(&mut owner).await.unwrap();
    let sibling: String = sqlx::query_scalar(
        "INSERT INTO pool.slot (network, slot_id, worker_id, client_slot_key, generation)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'cpu-1', 1)
         RETURNING slot_id::text",
    )
    .bind(&r.worker)
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
    .bind(&sibling)
    .bind(SHA.as_slice())
    .bind([0xef_u8; 32].as_slice())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mismatched = exec(
        &mut controller,
        format!(
            "INSERT INTO pool.assignment
                 (network, assignment_id, member_id, worker_id, slot_id, slot_generation,
                  offer_id, workflow_id, benchmark_id, confirmed_track_id, compute_type,
                  num_nonces, assignment_digest, identity,
                  workflow_expiry_block, proof_reserve_blocks, package_due_before_block,
                  ack_by)
             VALUES ('testnet', gen_random_uuid(), '{member}'::uuid, '{worker}'::uuid,
                     '{sibling}'::uuid, 1, '{offer}'::uuid, '{workflow}', '{benchmark}',
                     'n_nodes=100', 'aws_t4g', 512,
                     decode(repeat('ee', 32), 'hex'), '{{}}'::jsonb,
                     220, 10, 210, now() + interval '60 seconds')",
            member = r.member,
            worker = r.worker,
            offer = r.offer,
            workflow = r.workflow,
            benchmark = r.benchmark
        ),
    )
    .await
    .expect_err("an assignment occupies the slot its offer reserved");
    assert!(
        format!("{mismatched}").contains("the offer reserved"),
        "expected the offer-slot check, got: {mismatched}"
    );
}

#[tokio::test]
async fn an_assignment_follows_an_admitted_offer() {
    // §6's disposition ends at `ADMITTED`, which means a precommit intent
    // exists. An assignment published from an offer still `PENDING` would be
    // work handed out before the pool committed to it.
    let Some((db, mut owner)) = migrated("assign_offer", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x03).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    exec(
        &mut owner,
        format!(
            "UPDATE pool.capacity_offer SET state = 'CLOSED' WHERE offer_id = '{}'::uuid",
            r.offer
        ),
    )
    .await
    .unwrap();

    let closed = publish(&mut controller, &r, 0x33)
        .await
        .expect_err("a closed offer is not an admission");
    assert!(
        format!("{closed}").contains("follows an admitted offer"),
        "expected the offer check, got: {closed}"
    );
}

#[tokio::test]
async fn one_workflow_publishes_one_assignment() {
    // §16 invariant 1. Three indexes stand behind it here — one per benchmark,
    // one per workflow, one per offer — and they overlap deliberately: an
    // assignment's benchmark must equal its workflow's, so a second assignment
    // for one workflow is also a second for one benchmark. What a test can
    // drive is the outcome rather than which index produced it, and the outcome
    // is that asking twice yields one assignment.
    let Some((db, mut owner)) = migrated("assign_unique", "pool_migration").await else {
        return;
    };
    let first = ready(&mut owner, 0x04).await;
    let second = ready(&mut owner, 0x05).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    publish(&mut controller, &first, 0x44).await.unwrap();

    let twice = publish(&mut controller, &first, 0x45).await;
    assert!(
        twice.is_err(),
        "one workflow, one benchmark and one offer publish one assignment"
    );

    // And a digest is one piece of work: the agent recomputes it, so two
    // assignments carrying one would be work a member cannot tell apart.
    let same_digest = publish(&mut controller, &second, 0x44).await;
    assert!(
        same_digest.is_err(),
        "two assignments cannot share an identity digest"
    );

    publish(&mut controller, &second, 0x66)
        .await
        .expect("a different workflow publishes its own assignment");
}

#[tokio::test]
async fn the_package_deadline_reserves_the_pools_blocks() {
    // §7: `package_due_before_block = workflow_expiry_block -
    // proof_reserve_blocks`, and §16 invariant 6 makes those blocks the pool's
    // time for commitment, sampling, proof and confirmation. A deadline that
    // did not leave them would be promising the member time the pool needs.
    let Some((db, mut owner)) = migrated("assign_deadline", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x07).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    let stretched = exec(
        &mut controller,
        format!(
            "INSERT INTO pool.assignment
                 (network, assignment_id, member_id, worker_id, slot_id, slot_generation,
                  offer_id, workflow_id, benchmark_id, confirmed_track_id, compute_type,
                  num_nonces, assignment_digest, identity,
                  workflow_expiry_block, proof_reserve_blocks, package_due_before_block,
                  ack_by)
             VALUES ('testnet', gen_random_uuid(), '{}'::uuid, '{}'::uuid, '{}'::uuid, 1,
                     '{}'::uuid, '{}', '{}', 'n_nodes=100', 'aws_t4g',
                     512, decode(repeat('77', 32), 'hex'), '{{}}'::jsonb,
                     220, 10, 219, now() + interval '60 seconds')",
            r.member, r.worker, r.slot, r.offer, r.workflow, r.benchmark
        ),
    )
    .await;
    assert!(
        stretched.is_err(),
        "a package deadline that eats the reserve is not publishable"
    );
}

#[tokio::test]
async fn one_slot_holds_one_open_assignment_and_the_state_is_the_wire_contract() {
    // §16 invariant 1's occupancy half. `migrations/0020` guards the offer
    // side; this is the assignment side, and without it two open assignments
    // could hold one slot — the offer index would not notice, because an offer
    // closes when the work ends rather than when the assignment does.
    //
    // Also the spelling: `common.schema.json`'s `AssignmentState` says
    // `ASSIGNMENT_AVAILABLE`. `AVAILABLE` is the *slot*'s word, and a state on
    // the wire that no agent can decode is not a state.
    let Some((db, mut owner)) = migrated("assign_occupancy", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x10).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let id = publish(&mut controller, &r, 0xa0).await.unwrap();

    let state: String =
        sqlx::query_scalar("SELECT state FROM pool.assignment WHERE assignment_id = $1::uuid")
            .bind(&id)
            .fetch_one(&mut controller)
            .await
            .unwrap();
    assert_eq!(
        state, "ASSIGNMENT_AVAILABLE",
        "the member-visible state is the one the schema pins"
    );

    // A second workflow and offer for the same slot: the slot is still busy,
    // so a second open assignment for it is two claims on one slot.
    let second = second_admission(&mut owner, &r, 0x10).await;
    let twice = exec(
        &mut controller,
        format!(
            "INSERT INTO pool.assignment
                 (network, assignment_id, member_id, worker_id, slot_id, slot_generation,
                  offer_id, workflow_id, benchmark_id, confirmed_track_id, compute_type,
                  num_nonces, assignment_digest, identity,
                  workflow_expiry_block, proof_reserve_blocks, package_due_before_block,
                  ack_by)
             VALUES ('testnet', gen_random_uuid(), '{member}'::uuid, '{worker}'::uuid,
                     '{slot}'::uuid, 1, '{offer}'::uuid, '{workflow}', '{benchmark}',
                     'n_nodes=100', 'aws_t4g', 512,
                     decode(repeat('a1', 32), 'hex'), '{{}}'::jsonb,
                     220, 10, 210, now() + interval '60 seconds')",
            member = r.member,
            worker = r.worker,
            slot = r.slot,
            offer = second.offer,
            workflow = second.workflow,
            benchmark = second.benchmark
        ),
    )
    .await;
    assert!(
        twice.is_err(),
        "one slot holds one open assignment (§16 invariant 1)"
    );
}

/// A second admitted offer and confirmed workflow for the *same* slot, so a
/// second assignment can be attempted against it.
async fn second_admission(owner: &mut PgConnection, r: &Ready, tag: u8) -> Ready {
    // The first offer closes, which is what `migrations/0020` lets an admitted
    // one do — and is exactly the gap: an offer closes when the work it led to
    // ends, so a slot can take a new offer while the previous assignment is
    // still open. The offer index will not notice; the assignment index must.
    exec(
        owner,
        format!(
            "UPDATE pool.capacity_offer SET state = 'CLOSED' WHERE offer_id = '{}'::uuid",
            r.offer
        ),
    )
    .await
    .unwrap();

    let offer: String = sqlx::query_scalar(
        "INSERT INTO pool.capacity_offer
             (network, worker_id, offer_id, slot_id, slot_generation, spec_digest,
              state, offer_sha256)
         VALUES ('testnet', $1::uuid, gen_random_uuid(), $2::uuid, 1, $3, 'RECEIVED', $4)
         RETURNING offer_id::text",
    )
    .bind(&r.worker)
    .bind(&r.slot)
    .bind([tag; 32].as_slice())
    .bind(SHA.as_slice())
    .fetch_one(&mut *owner)
    .await
    .unwrap();
    for state in ["PENDING", "ADMITTED"] {
        let lease = if state == "PENDING" {
            ", lease_expires_at = now() + interval '90 seconds'"
        } else {
            ""
        };
        exec(
            owner,
            format!(
                "UPDATE pool.capacity_offer SET state = '{state}'{lease}
                  WHERE offer_id = '{offer}'::uuid"
            ),
        )
        .await
        .unwrap();
    }

    let workflow = format!("wf-{tag:02x}-second");
    let benchmark = format!("bench-{tag:02x}-second");
    exec(
        owner,
        format!(
            "INSERT INTO pool.workflow
                 (network, workflow_id, state, owner_kind, owner_id,
                  unverified_from_block, benchmark_id, block_started,
                  confirmed_track_id, confirmed_settings, confirmed_num_nonces,
                  precommit_confirmed_block)
             VALUES ('testnet', '{workflow}', 'PRECOMMIT_CONFIRMED', 'MEMBER', '{}',
                     100, '{benchmark}', 100, 'n_nodes=100', '{{}}'::jsonb, 512, 101)",
            r.member
        ),
    )
    .await
    .unwrap();

    Ready {
        member: r.member.clone(),
        worker: r.worker.clone(),
        slot: r.slot.clone(),
        offer,
        workflow,
        benchmark,
    }
}

#[tokio::test]
async fn an_assignment_climbs_its_ladder_and_stops_at_the_receipt() {
    // §9's ladder, and §12's receipt: `PACKAGE_DURABLY_ACCEPTED` releases the
    // slot and the retention obligation, and the pool-side TIG states after it
    // are the workflow's, not the assignment's.
    let Some((db, mut owner)) = migrated("assign_ladder", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x08).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let id = publish(&mut controller, &r, 0x88).await.unwrap();

    let set = |state: &str| {
        format!("UPDATE pool.assignment SET state = '{state}' WHERE assignment_id = '{id}'::uuid")
    };

    let skipped = exec(&mut controller, set("UPLOADING"))
        .await
        .expect_err("an assignment does not skip to uploading");
    assert!(
        format!("{skipped}").contains("does not go from"),
        "expected the ladder trigger, got: {skipped}"
    );

    for state in [
        "ACKNOWLEDGED",
        "COMPUTING",
        "PACKAGING",
        "UPLOADING",
        "PACKAGE_RECEIVED",
        "PACKAGE_STRUCTURALLY_ACCEPTED",
        "PACKAGE_DURABLY_ACCEPTED",
    ] {
        exec(&mut controller, set(state))
            .await
            .unwrap_or_else(|e| panic!("climbing to {state}: {e}"));
    }

    let after = exec(&mut controller, set("FAILED"))
        .await
        .expect_err("the receipt is the end of the assignment");
    assert!(
        format!("{after}").contains("is terminal"),
        "expected the terminal refusal, got: {after}"
    );
}

#[tokio::test]
async fn a_terminal_assignment_says_who_and_why_and_a_live_one_does_not() {
    // §15 requires every terminal outcome to record one of MEMBER/POOL/TIG/
    // UNRESOLVED with a machine reason. ADR 0013 detached that from money —
    // it is reporting — but an outcome nobody can report is worse than one
    // nobody charges for.
    let Some((db, mut owner)) = migrated("assign_attribution", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x09).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let id = publish(&mut controller, &r, 0x99).await.unwrap();

    let silent = exec(
        &mut controller,
        format!("UPDATE pool.assignment SET state = 'FAILED' WHERE assignment_id = '{id}'::uuid"),
    )
    .await;
    assert!(silent.is_err(), "a terminal outcome says who and why");

    let early = exec(
        &mut controller,
        format!(
            "UPDATE pool.assignment SET attribution = 'MEMBER', terminal_reason = 'guessing'
              WHERE assignment_id = '{id}'::uuid"
        ),
    )
    .await;
    assert!(
        early.is_err(),
        "a live assignment is not classified in advance"
    );

    exec(
        &mut controller,
        format!(
            "UPDATE pool.assignment
                SET state = 'FAILED', attribution = 'POOL', terminal_reason = 'artifact lost'
              WHERE assignment_id = '{id}'::uuid"
        ),
    )
    .await
    .expect("a recorded outcome is accepted");
}

#[tokio::test]
async fn events_are_facts_in_one_sequence() {
    // §8: a strictly increasing `event_seq` from 1, a unique `event_id`, and a
    // duplicate id returning the original acceptance — so nothing rewrites one.
    let Some((db, mut owner)) = migrated("assign_events", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x0a).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let id = publish(&mut controller, &r, 0xaa).await.unwrap();

    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();

    let record = |seq: i64, kind: &str| {
        format!(
            "INSERT INTO pool.assignment_event
                 (network, assignment_id, event_id, event_seq, event_type, payload, event_sha256)
             VALUES ('testnet', '{id}'::uuid, gen_random_uuid(), {seq}, '{kind}',
                     '{{}}'::jsonb, decode(repeat('7e', 32), 'hex'))"
        )
    };

    exec(&mut api, record(1, "PROGRESS")).await.unwrap();
    let duplicate_seq = exec(&mut api, record(1, "PROGRESS")).await;
    assert!(duplicate_seq.is_err(), "one sequence number, one event");

    exec(&mut api, record(2, "COMPUTE_COMPLETED"))
        .await
        .unwrap();

    let unknown = exec(&mut api, record(3, "INVENTED")).await;
    assert!(unknown.is_err(), "the event types are a closed set");

    let edited = exec(
        &mut owner,
        format!(
            "UPDATE pool.assignment_event SET event_type = 'TERMINAL_ERROR'
              WHERE assignment_id = '{id}'::uuid AND event_seq = 1"
        ),
    )
    .await
    .expect_err("an event is a fact the member reported");
    assert!(
        format!("{edited}").contains("not edited"),
        "expected the append-only trigger, got: {edited}"
    );
}

#[tokio::test]
async fn the_controller_publishes_and_the_api_reports() {
    // `architecture.md` §6: "Create a confirmed assignment | Controller" and
    // "Record a member offer, event, or heartbeat command | Pool API". A member
    // who could publish an assignment could hand themselves work.
    let Some((db, mut owner)) = migrated("assign_grants", "pool_migration").await else {
        return;
    };
    let r = ready(&mut owner, 0x0b).await;
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let id = publish(&mut controller, &r, 0xbb).await.unwrap();

    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();

    let published = publish(&mut api, &r, 0xcc).await;
    assert!(published.is_err(), "the API publishes no assignment");

    let advanced = exec(
        &mut api,
        format!(
            "UPDATE pool.assignment SET state = 'ACKNOWLEDGED' WHERE assignment_id = '{id}'::uuid"
        ),
    )
    .await;
    assert!(
        advanced.is_err(),
        "the API records events; the controller applies them"
    );

    for privilege in ["INSERT", "UPDATE", "DELETE"] {
        for table in ["pool.assignment", "pool.assignment_event"] {
            let sql = format!("SELECT has_table_privilege('pool_readonly', $1, '{privilege}')");
            let writable: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(table)
                .fetch_one(&mut controller)
                .await
                .unwrap();
            assert!(!writable, "monitoring never {privilege}s {table}");
        }
    }
}
