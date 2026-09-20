//! Slice-2 criteria G1–G7 and I1–I7 at the schema level: upload sessions, the
//! quarantine chunk ledger, artifacts, and the receipt.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_test_support::{MIGRATOR, TempDb, exec};
use sqlx::{Connection, PgConnection};

const SHA: [u8; 32] = [0x7e; 32];
const CHUNK: i64 = 1_048_576;

async fn migrated(name: &str, role: &str) -> Option<(TempDb, PgConnection)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();
    Some((db, conn))
}

/// A published assignment, with everything behind it. Returns its id.
async fn assignment(owner: &mut PgConnection, tag: u8) -> String {
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

    sqlx::query_scalar(
        "INSERT INTO pool.assignment
             (network, assignment_id, member_id, worker_id, slot_id, slot_generation,
              offer_id, workflow_id, benchmark_id, confirmed_track_id, compute_type,
              num_nonces, assignment_digest, identity,
              workflow_expiry_block, proof_reserve_blocks, package_due_before_block,
              ack_by)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, $2::uuid, $3::uuid, 1,
                 $4::uuid, $5, $6, 'n_nodes=100', 'aws_t4g', 512, $7, '{}'::jsonb,
                 220, 10, 210, now() + interval '60 seconds')
         RETURNING assignment_id::text",
    )
    .bind(&member)
    .bind(&worker)
    .bind(&slot)
    .bind(&offer)
    .bind(&workflow)
    .bind(&benchmark)
    .bind([tag; 32].as_slice())
    .fetch_one(&mut *owner)
    .await
    .unwrap()
}

/// Open an upload session for `assignment`, declaring one chunk's worth.
async fn open_session(conn: &mut PgConnection, assignment: &str) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO pool.upload_session
             (network, upload_id, assignment_id, package_id, declaration_sha256,
              media_type, compressed_size, uncompressed_size, manifest_sha256,
              package_sha256, chunk_size, expires_at)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, gen_random_uuid(), $2,
                 'application/vnd.tig-pool.proof-material-v1.tar+zstd',
                 $3, $3 * 2, $2, $2, $4, now() + interval '1 hour')
         RETURNING upload_id::text",
    )
    .bind(assignment)
    .bind(SHA.as_slice())
    .bind(CHUNK)
    .bind(CHUNK as i32)
    .fetch_one(&mut *conn)
    .await
}

/// Commit one chunk and advance the offset, as the API does after the object is
/// durable.
async fn commit_chunk(
    api: &mut PgConnection,
    upload: &str,
    offset: i64,
    length: i64,
) -> Result<(), sqlx::Error> {
    exec(
        api,
        format!(
            "INSERT INTO pool.upload_chunk
                 (network, upload_id, chunk_offset, chunk_length, chunk_sha256, object_key)
             VALUES ('testnet', '{upload}'::uuid, {offset}, {length},
                     decode(repeat('7e', 32), 'hex'),
                     'quarantine/testnet/{upload}/{offset}')"
        ),
    )
    .await?;
    exec(
        api,
        format!(
            "UPDATE pool.upload_session SET committed_offset = {}
              WHERE upload_id = '{upload}'::uuid",
            offset + length
        ),
    )
    .await
}

#[tokio::test]
async fn the_committed_offset_is_the_ledgers_contiguous_prefix() {
    // `architecture.md` §8.2: "The Pool API never advances the returned offset
    // before the object is durable." The offset is the member's resume point,
    // so a column the caller may set to anything is a resume point that can
    // skip bytes nobody stored.
    let Some((db, mut owner)) = migrated("upload_offset", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x01).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let upload = open_session(&mut api, &a).await.unwrap();

    let ahead = exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET committed_offset = {CHUNK}
              WHERE upload_id = '{upload}'::uuid"
        ),
    )
    .await
    .expect_err("an offset with no committed bytes behind it is not a resume point");
    assert!(
        format!("{ahead}").contains("contiguous prefix"),
        "expected the ledger check, got: {ahead}"
    );

    commit_chunk(&mut api, &upload, 0, CHUNK / 2)
        .await
        .expect("the first range commits and the offset follows it");

    // §11: a chunk at a higher offset leaves a hole, and the offset cannot
    // cross one.
    exec(
        &mut api,
        format!(
            "INSERT INTO pool.upload_chunk
                 (network, upload_id, chunk_offset, chunk_length, chunk_sha256, object_key)
             VALUES ('testnet', '{upload}'::uuid, {}, {}, decode(repeat('7e', 32), 'hex'),
                     'quarantine/testnet/{upload}/{}')",
            CHUNK / 2 + 4096,
            CHUNK / 4,
            CHUNK / 2 + 4096
        ),
    )
    .await
    .unwrap();

    let over_a_hole = exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET committed_offset = {}
              WHERE upload_id = '{upload}'::uuid",
            CHUNK / 2 + 4096 + CHUNK / 4
        ),
    )
    .await;
    assert!(
        over_a_hole.is_err(),
        "the offset does not cross a gap in the ledger"
    );
}

#[tokio::test]
async fn a_committed_chunk_is_not_rewritten() {
    // §11: an identical retry is idempotent and a different range at the same
    // offset is a terminal conflict. Neither rewrites what is there, because
    // the bytes in the store do not change when a row does.
    let Some((db, mut owner)) = migrated("upload_chunks", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x02).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let upload = open_session(&mut api, &a).await.unwrap();
    commit_chunk(&mut api, &upload, 0, CHUNK / 2).await.unwrap();

    let again = commit_chunk(&mut api, &upload, 0, CHUNK / 2).await;
    assert!(again.is_err(), "one offset, one committed range");

    let rewritten = exec(
        &mut owner,
        format!(
            "UPDATE pool.upload_chunk SET chunk_length = 1
              WHERE upload_id = '{upload}'::uuid AND chunk_offset = 0"
        ),
    )
    .await
    .expect_err("a committed range records bytes already durable");
    assert!(
        format!("{rewritten}").contains("not rewritten"),
        "expected the append-only trigger, got: {rewritten}"
    );

    // §11: "Bytes beyond the declared package size are never accepted."
    let beyond = exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET committed_offset = {}
              WHERE upload_id = '{upload}'::uuid",
            CHUNK * 4
        ),
    )
    .await;
    assert!(beyond.is_err(), "the offset stays inside the declaration");
}

#[tokio::test]
async fn finalization_needs_every_declared_byte() {
    // `architecture.md` §5.2 step 2: "Finalization verifies that the durable
    // chunk ledger covers the declaration exactly."
    let Some((db, mut owner)) = migrated("upload_finalize", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x03).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let upload = open_session(&mut api, &a).await.unwrap();
    commit_chunk(&mut api, &upload, 0, CHUNK / 2).await.unwrap();

    let early = exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET state = 'FINALIZED'
              WHERE upload_id = '{upload}'::uuid"
        ),
    )
    .await
    .expect_err("half a package is not a package");
    assert!(
        format!("{early}").contains("every declared byte"),
        "expected the finalization check, got: {early}"
    );

    commit_chunk(&mut api, &upload, CHUNK / 2, CHUNK / 2)
        .await
        .unwrap();
    exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET state = 'FINALIZED'
              WHERE upload_id = '{upload}'::uuid"
        ),
    )
    .await
    .expect("every declared byte is present");
}

#[tokio::test]
async fn an_upload_climbs_one_rung_at_a_time_and_one_reaches_acceptance() {
    // §12's three meanings are a ladder, and §16 invariant 8 lets only one
    // package generation reach the top: "rejected package generations remain
    // auditable but may be replaced with a new `package_id`".
    let Some((db, mut owner)) = migrated("upload_ladder", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x04).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let upload = open_session(&mut api, &a).await.unwrap();
    commit_chunk(&mut api, &upload, 0, CHUNK).await.unwrap();

    let set = |state: &str| {
        format!(
            "UPDATE pool.upload_session SET state = '{state}' WHERE upload_id = '{upload}'::uuid"
        )
    };

    let skipped = exec(&mut owner, set("STRUCTURALLY_ACCEPTED"))
        .await
        .expect_err("an upload does not skip its verification");
    assert!(
        format!("{skipped}").contains("does not go from"),
        "expected the ladder trigger, got: {skipped}"
    );

    for state in [
        "FINALIZED",
        "RECEIVED",
        "STRUCTURALLY_ACCEPTED",
        "DURABLY_ACCEPTED",
    ] {
        exec(&mut owner, set(state))
            .await
            .unwrap_or_else(|e| panic!("climbing to {state}: {e}"));
    }

    // A second generation for the same assignment may exist — and may not also
    // be accepted.
    let second = open_session(&mut api, &a).await.unwrap();
    commit_chunk(&mut api, &second, 0, CHUNK).await.unwrap();
    for state in ["FINALIZED", "RECEIVED", "STRUCTURALLY_ACCEPTED"] {
        exec(
            &mut owner,
            format!(
                "UPDATE pool.upload_session SET state = '{state}'
                  WHERE upload_id = '{second}'::uuid"
            ),
        )
        .await
        .unwrap();
    }
    let twice = exec(
        &mut owner,
        format!(
            "UPDATE pool.upload_session SET state = 'DURABLY_ACCEPTED'
              WHERE upload_id = '{second}'::uuid"
        ),
    )
    .await;
    assert!(
        twice.is_err(),
        "a package retry cannot create a second accepted artifact (§16 invariant 8)"
    );
}

#[tokio::test]
async fn a_receipt_follows_durable_acceptance_and_never_moves() {
    // §16 invariant 9: `RECEIVED` and `STRUCTURALLY_ACCEPTED` release nothing —
    // and the receipt is what releases. Invariant 10: it is immutable, and
    // recoverable means readable again, not reissuable.
    let Some((db, mut owner)) = migrated("upload_receipt", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x05).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let upload = open_session(&mut api, &a).await.unwrap();
    commit_chunk(&mut api, &upload, 0, CHUNK).await.unwrap();
    for state in ["FINALIZED", "RECEIVED", "STRUCTURALLY_ACCEPTED"] {
        exec(
            &mut owner,
            format!(
                "UPDATE pool.upload_session SET state = '{state}'
                  WHERE upload_id = '{upload}'::uuid"
            ),
        )
        .await
        .unwrap();
    }

    let artifact: String = sqlx::query_scalar(
        "INSERT INTO pool.artifact
             (network, artifact_id, assignment_id, benchmark_id, kind, backend,
              container, object_key, media_type, format_version, sha256,
              compressed_size, uncompressed_size, manifest_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'bench-05', 'PACKAGE',
                 'FILESYSTEM', 'accepted', 'accepted/testnet/bench-05/pkg.tar.zst',
                 'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1', $2,
                 $3, $3, $2)
         RETURNING artifact_id::text",
    )
    .bind(&a)
    .bind(SHA.as_slice())
    .bind(CHUNK)
    .fetch_one(&mut owner)
    .await
    .unwrap();

    let receipt = |upload_id: &str| {
        format!(
            "INSERT INTO pool.acceptance_receipt
                 (network, assignment_id, receipt_id, artifact_id, upload_id, package_sha256)
             VALUES ('testnet', '{a}'::uuid, gen_random_uuid(), '{artifact}'::uuid,
                     '{upload_id}'::uuid, decode(repeat('7e', 32), 'hex'))"
        )
    };

    let early = exec(&mut owner, receipt(&upload))
        .await
        .expect_err("structural acceptance releases nothing");
    assert!(
        format!("{early}").contains("follows durable acceptance"),
        "expected the receipt check, got: {early}"
    );

    // Publication, then durable acceptance, then the receipt.
    exec(
        &mut owner,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'ACCEPTED', accepted_at = now()
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .unwrap();
    exec(
        &mut owner,
        format!(
            "UPDATE pool.upload_session SET state = 'DURABLY_ACCEPTED'
              WHERE upload_id = '{upload}'::uuid"
        ),
    )
    .await
    .unwrap();
    exec(&mut owner, receipt(&upload))
        .await
        .expect("durable acceptance is what a receipt follows");

    let moved = exec(
        &mut owner,
        format!(
            "UPDATE pool.acceptance_receipt SET package_sha256 = decode(repeat('00', 32), 'hex')
              WHERE assignment_id = '{a}'::uuid"
        ),
    )
    .await
    .expect_err("a receipt is immutable");
    assert!(
        format!("{moved}").contains("immutable"),
        "expected the receipt trigger, got: {moved}"
    );
}

#[tokio::test]
async fn an_artifact_is_fixed_where_it_was_published() {
    // §8.2 publishes to a deterministic immutable key and verifies size and
    // hash before committing; proof construction later recomputes that SHA-256
    // before using the package. An editable digest makes that a comparison
    // against whatever was written last.
    let Some((_db, mut owner)) = migrated("upload_artifact", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x06).await;
    let artifact: String = sqlx::query_scalar(
        "INSERT INTO pool.artifact
             (network, artifact_id, assignment_id, benchmark_id, kind, backend,
              container, object_key, media_type, format_version, sha256,
              compressed_size, uncompressed_size, manifest_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'bench-06', 'PACKAGE',
                 'FILESYSTEM', 'accepted', 'accepted/testnet/bench-06/pkg.tar.zst',
                 'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1', $2,
                 $3, $3, $2)
         RETURNING artifact_id::text",
    )
    .bind(&a)
    .bind(SHA.as_slice())
    .bind(CHUNK)
    .fetch_one(&mut owner)
    .await
    .unwrap();

    for column in [
        "sha256 = decode(repeat('00', 32), 'hex')",
        "object_key = 'accepted/testnet/bench-06/elsewhere.tar.zst'",
        "compressed_size = 1",
    ] {
        let edited = exec(
            &mut owner,
            format!("UPDATE pool.artifact SET {column} WHERE artifact_id = '{artifact}'::uuid"),
        )
        .await
        .expect_err("an artifact's identity, location and digest are fixed");
        assert!(
            format!("{edited}").contains("fixed at publication"),
            "expected the artifact trigger for {column}, got: {edited}"
        );
    }

    // §8.4's lifecycle: the controller decides deletable, the worker deletes,
    // and nothing walks back to publishing.
    exec(
        &mut owner,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'ACCEPTED', accepted_at = now()
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .unwrap();
    let backwards = exec(
        &mut owner,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'PUBLISHING', accepted_at = NULL
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await;
    assert!(
        backwards.is_err(),
        "an artifact does not return to publishing"
    );

    // A derived payload is not a package, and may not claim a package's fields.
    let mislabelled = exec(
        &mut owner,
        format!(
            "INSERT INTO pool.artifact
                 (network, artifact_id, assignment_id, benchmark_id, kind, backend,
                  container, object_key, media_type, format_version, sha256,
                  compressed_size, uncompressed_size, manifest_sha256)
             VALUES ('testnet', gen_random_uuid(), '{a}'::uuid, 'bench-06',
                     'COMMITMENT_PAYLOAD', 'FILESYSTEM', 'derived',
                     'derived/testnet/bench-06/commitment.json.br',
                     'application/json', 'v1', decode(repeat('11', 32), 'hex'),
                     100, 200, decode(repeat('11', 32), 'hex'))"
        ),
    )
    .await;
    assert!(
        mislabelled.is_err(),
        "a derived payload has no uncompressed size or manifest digest"
    );
}

#[tokio::test]
async fn a_session_arrives_open_and_empty() {
    // Every other rule about a session is `BEFORE UPDATE`, so the row's first
    // state is where they can all be skipped: a session created already
    // `DURABLY_ACCEPTED`, or with a committed offset covering the declaration,
    // never runs any of them.
    let Some((db, mut owner)) = migrated("upload_arrival", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x08).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();

    let arrive = |state: &str, offset: i64| {
        format!(
            "INSERT INTO pool.upload_session
                 (network, upload_id, assignment_id, package_id, declaration_sha256,
                  media_type, compressed_size, uncompressed_size, manifest_sha256,
                  package_sha256, chunk_size, committed_offset, state, expires_at)
             VALUES ('testnet', gen_random_uuid(), '{a}'::uuid, gen_random_uuid(),
                     decode(repeat('7e', 32), 'hex'),
                     'application/vnd.tig-pool.proof-material-v1.tar+zstd',
                     {CHUNK}, {}, decode(repeat('7e', 32), 'hex'),
                     decode(repeat('7e', 32), 'hex'), {CHUNK}, {offset}, '{state}',
                     now() + interval '1 hour')",
            CHUNK * 2
        )
    };

    let accepted = exec(&mut api, arrive("DURABLY_ACCEPTED", 0))
        .await
        .expect_err("a session is created OPEN");
    assert!(
        format!("{accepted}").contains("created OPEN"),
        "expected the arrival guard, got: {accepted}"
    );

    let prefilled = exec(&mut api, arrive("OPEN", CHUNK))
        .await
        .expect_err("a new session has no committed bytes");
    assert!(
        format!("{prefilled}").contains("no committed bytes"),
        "expected the arrival guard, got: {prefilled}"
    );

    // The guard holds against the table owner too, which is the case a grant
    // cannot cover.
    let by_owner = exec(&mut owner, arrive("DURABLY_ACCEPTED", CHUNK)).await;
    assert!(by_owner.is_err(), "the arrival guard is not a grant");

    exec(&mut api, arrive("OPEN", 0))
        .await
        .expect("an empty session is what a member opens");
}

#[tokio::test]
async fn retention_eligibility_is_the_controllers_and_deletion_the_workers() {
    // §8.4: "The controller alone decides that confirmed TIG state and pending
    // work make an artifact deletable. The Artifact Worker alone performs
    // physical deletion." Both write `lifecycle_state`, so a column grant
    // cannot tell those two decisions apart — and a worker that could mark its
    // own output deletable could then delete the one authoritative accepted
    // package before the controller had checked anything.
    let Some((db, mut owner)) = migrated("upload_retention", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x09).await;
    let artifact: String = sqlx::query_scalar(
        "INSERT INTO pool.artifact
             (network, artifact_id, assignment_id, benchmark_id, kind, backend,
              container, object_key, media_type, format_version, sha256,
              compressed_size, uncompressed_size, manifest_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'bench-09', 'PACKAGE',
                 'FILESYSTEM', 'accepted', 'accepted/testnet/bench-09/pkg.tar.zst',
                 'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1', $2,
                 $3, $3, $2)
         RETURNING artifact_id::text",
    )
    .bind(&a)
    .bind(SHA.as_slice())
    .bind(CHUNK)
    .fetch_one(&mut owner)
    .await
    .unwrap();

    let mut worker = PgConnection::connect_with(&db.as_role("pool_artifact_worker"))
        .await
        .unwrap();
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    // The controller cannot report a publication. Two guards stand in its way
    // and the privilege is the first: it holds no grant on `accepted_at`, so
    // this is refused before the owner check is reached. The owner check
    // covers the case the grant does not — a role that later gets the column.
    let accepted_by_controller = exec(
        &mut controller,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'ACCEPTED', accepted_at = now()
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .expect_err("publication is the worker's report, not the controller's claim");
    assert!(
        format!("{accepted_by_controller}").contains("permission denied"),
        "expected a privilege refusal, got: {accepted_by_controller}"
    );

    exec(
        &mut worker,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'ACCEPTED', accepted_at = now()
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .expect("the worker publishes and says so");

    let deletable_by_worker = exec(
        &mut worker,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'DELETABLE'
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .expect_err("retention eligibility is the controller's");
    assert!(
        format!("{deletable_by_worker}").contains("retention eligibility"),
        "expected the retention owner check, got: {deletable_by_worker}"
    );

    exec(
        &mut controller,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'DELETABLE', deletable_at = now()
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .expect("the controller decides an artifact may go");

    exec(
        &mut worker,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'DELETED', deleted_at = now()
              WHERE artifact_id = '{artifact}'::uuid"
        ),
    )
    .await
    .expect("and the worker is the one that deletes it");
}

#[tokio::test]
async fn a_receipt_names_its_own_assignments_upload_and_package() {
    // A receipt releases the slot and the member's retention obligation (§9,
    // §16 invariant 10). Checking only that *some* upload is durably accepted
    // would release one member's obligation on the strength of another's
    // package — and §16 invariant 11 fails the moment the first member deletes
    // its only copy.
    let Some((db, mut owner)) = migrated("upload_receipt_chain", "pool_migration").await else {
        return;
    };
    let mine = assignment(&mut owner, 0x0a).await;
    let theirs = assignment(&mut owner, 0x0b).await;

    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let mut worker = PgConnection::connect_with(&db.as_role("pool_artifact_worker"))
        .await
        .unwrap();
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    // Their package, carried all the way to durable acceptance.
    let their_upload = open_session(&mut api, &theirs).await.unwrap();
    commit_chunk(&mut api, &their_upload, 0, CHUNK)
        .await
        .unwrap();
    exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET state = 'FINALIZED'
              WHERE upload_id = '{their_upload}'::uuid"
        ),
    )
    .await
    .unwrap();
    for state in ["RECEIVED", "STRUCTURALLY_ACCEPTED"] {
        exec(
            &mut worker,
            format!(
                "UPDATE pool.upload_session SET state = '{state}'
                  WHERE upload_id = '{their_upload}'::uuid"
            ),
        )
        .await
        .unwrap();
    }
    let their_artifact: String = sqlx::query_scalar(
        "INSERT INTO pool.artifact
             (network, artifact_id, assignment_id, benchmark_id, kind, backend,
              container, object_key, media_type, format_version, sha256,
              compressed_size, uncompressed_size, manifest_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'bench-0b', 'PACKAGE',
                 'FILESYSTEM', 'accepted', 'accepted/testnet/bench-0b/pkg.tar.zst',
                 'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1', $2,
                 $3, $3, $2)
         RETURNING artifact_id::text",
    )
    .bind(&theirs)
    .bind(SHA.as_slice())
    .bind(CHUNK)
    .fetch_one(&mut worker)
    .await
    .unwrap();
    exec(
        &mut worker,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'ACCEPTED', accepted_at = now()
              WHERE artifact_id = '{their_artifact}'::uuid"
        ),
    )
    .await
    .unwrap();
    exec(
        &mut controller,
        format!(
            "UPDATE pool.upload_session SET state = 'DURABLY_ACCEPTED'
              WHERE upload_id = '{their_upload}'::uuid"
        ),
    )
    .await
    .unwrap();

    // The same again for my assignment, so each half can be borrowed on its own
    // — with only one side wrong at a time, the other side's check cannot be
    // what refuses it.
    let my_upload = open_session(&mut api, &mine).await.unwrap();
    commit_chunk(&mut api, &my_upload, 0, CHUNK).await.unwrap();
    exec(
        &mut api,
        format!(
            "UPDATE pool.upload_session SET state = 'FINALIZED'
              WHERE upload_id = '{my_upload}'::uuid"
        ),
    )
    .await
    .unwrap();
    for state in ["RECEIVED", "STRUCTURALLY_ACCEPTED"] {
        exec(
            &mut worker,
            format!(
                "UPDATE pool.upload_session SET state = '{state}'
                  WHERE upload_id = '{my_upload}'::uuid"
            ),
        )
        .await
        .unwrap();
    }
    let my_artifact: String = sqlx::query_scalar(
        "INSERT INTO pool.artifact
             (network, artifact_id, assignment_id, benchmark_id, kind, backend,
              container, object_key, media_type, format_version, sha256,
              compressed_size, uncompressed_size, manifest_sha256)
         VALUES ('testnet', gen_random_uuid(), $1::uuid, 'bench-0a', 'PACKAGE',
                 'FILESYSTEM', 'accepted', 'accepted/testnet/bench-0a/pkg.tar.zst',
                 'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1', $2,
                 $3, $3, $2)
         RETURNING artifact_id::text",
    )
    .bind(&mine)
    .bind(SHA.as_slice())
    .bind(CHUNK)
    .fetch_one(&mut worker)
    .await
    .unwrap();
    exec(
        &mut worker,
        format!(
            "UPDATE pool.artifact SET lifecycle_state = 'ACCEPTED', accepted_at = now()
              WHERE artifact_id = '{my_artifact}'::uuid"
        ),
    )
    .await
    .unwrap();
    exec(
        &mut controller,
        format!(
            "UPDATE pool.upload_session SET state = 'DURABLY_ACCEPTED'
              WHERE upload_id = '{my_upload}'::uuid"
        ),
    )
    .await
    .unwrap();

    let receipt = |artifact: &str, upload: &str| {
        format!(
            "INSERT INTO pool.acceptance_receipt
                 (network, assignment_id, receipt_id, artifact_id, upload_id, package_sha256)
             VALUES ('testnet', '{mine}'::uuid, gen_random_uuid(), '{artifact}'::uuid,
                     '{upload}'::uuid, decode(repeat('7e', 32), 'hex'))"
        )
    };

    let borrowed_upload = exec(&mut controller, receipt(&my_artifact, &their_upload))
        .await
        .expect_err("a receipt names its own assignment's upload");
    assert!(
        format!("{borrowed_upload}").contains("the upload belongs to"),
        "expected the upload's assignment check, got: {borrowed_upload}"
    );

    let borrowed_package = exec(&mut controller, receipt(&their_artifact, &my_upload))
        .await
        .expect_err("a receipt names its own assignment's package");
    assert!(
        format!("{borrowed_package}").contains("the artifact belongs to"),
        "expected the artifact's assignment check, got: {borrowed_package}"
    );

    exec(&mut controller, receipt(&my_artifact, &my_upload))
        .await
        .expect("its own upload and its own package");
}

#[tokio::test]
async fn an_artifact_arrives_unpublished() {
    // The same hole as a session arriving finished, on the sibling table: every
    // lifecycle and ownership rule is `BEFORE UPDATE`, and the worker holds
    // `INSERT`. A row created already `DELETABLE` — with `accepted_at` set,
    // which the CHECKs permit — skips the controller's retention decision
    // entirely, and the worker can then delete it under its own grant.
    let Some((db, mut owner)) = migrated("upload_artifact_arrival", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x0c).await;
    let mut worker = PgConnection::connect_with(&db.as_role("pool_artifact_worker"))
        .await
        .unwrap();

    let arrive = |state: &str, stamps: &str| {
        format!(
            "INSERT INTO pool.artifact
                 (network, artifact_id, assignment_id, benchmark_id, kind, backend,
                  container, object_key, media_type, format_version, sha256,
                  compressed_size, uncompressed_size, manifest_sha256,
                  lifecycle_state{stamps_cols})
             VALUES ('testnet', gen_random_uuid(), '{a}'::uuid, 'bench-0c', 'PACKAGE',
                     'FILESYSTEM', 'accepted',
                     'accepted/testnet/bench-0c/' || gen_random_uuid()::text,
                     'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1',
                     decode(repeat('7e', 32), 'hex'), {CHUNK}, {CHUNK},
                     decode(repeat('7e', 32), 'hex'), '{state}'{stamps_vals})",
            stamps_cols = if stamps.is_empty() {
                ""
            } else {
                ", accepted_at"
            },
            stamps_vals = if stamps.is_empty() { "" } else { ", now()" }
        )
    };

    let born_deletable = exec(&mut worker, arrive("DELETABLE", "accepted"))
        .await
        .expect_err("an artifact is created before it is published");
    assert!(
        format!("{born_deletable}").contains("before it is published"),
        "expected the arrival guard, got: {born_deletable}"
    );

    let born_accepted = exec(&mut worker, arrive("ACCEPTED", "accepted"))
        .await
        .expect_err("publication is a transition, not a starting point");
    assert!(
        format!("{born_accepted}").contains("before it is published"),
        "expected the arrival guard, got: {born_accepted}"
    );

    // The guard holds against the owner too — the role a later slice grants
    // INSERT to is the one this is for.
    let by_owner = exec(&mut owner, arrive("DELETED", "accepted")).await;
    assert!(by_owner.is_err(), "the arrival guard is not a grant");

    // And a dated arrival in a *permitted* state, so the timestamps are
    // refused by this guard rather than by the state check beside it.
    // `deletable_at` is the one no CHECK ties to a lifecycle state, which makes
    // it the only case that reaches here.
    let born_dated = exec(
        &mut worker,
        format!(
            "INSERT INTO pool.artifact
                 (network, artifact_id, assignment_id, benchmark_id, kind, backend,
                  container, object_key, media_type, format_version, sha256,
                  compressed_size, uncompressed_size, manifest_sha256,
                  lifecycle_state, deletable_at)
             VALUES ('testnet', gen_random_uuid(), '{a}'::uuid, 'bench-0c', 'PACKAGE',
                     'FILESYSTEM', 'accepted',
                     'accepted/testnet/bench-0c/' || gen_random_uuid()::text,
                     'application/vnd.tig-pool.proof-material-v1.tar+zstd', 'v1',
                     decode(repeat('7e', 32), 'hex'), {CHUNK}, {CHUNK},
                     decode(repeat('7e', 32), 'hex'), 'PUBLISHING', now())"
        ),
    )
    .await
    .expect_err("a new artifact has no retention decision behind it");
    assert!(
        format!("{born_dated}").contains("no acceptance, retention or deletion"),
        "expected the arrival guard, got: {born_dated}"
    );

    exec(&mut worker, arrive("PUBLISHING", ""))
        .await
        .expect("a row starts where publication starts");
}

#[tokio::test]
async fn each_role_writes_only_its_own_step() {
    // `architecture.md` §6: the API owns the session and the ledger, the worker
    // verifies and publishes, and the controller alone records durable
    // acceptance and its receipt.
    let Some((db, mut owner)) = migrated("upload_grants", "pool_migration").await else {
        return;
    };
    let a = assignment(&mut owner, 0x07).await;
    let mut api = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let upload = open_session(&mut api, &a).await.unwrap();
    commit_chunk(&mut api, &upload, 0, CHUNK).await.unwrap();

    let mut worker = PgConnection::connect_with(&db.as_role("pool_artifact_worker"))
        .await
        .unwrap();
    let mut controller = PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();

    // The worker does not invent committed bytes.
    let forged = exec(
        &mut worker,
        format!(
            "INSERT INTO pool.upload_chunk
                 (network, upload_id, chunk_offset, chunk_length, chunk_sha256, object_key)
             VALUES ('testnet', '{upload}'::uuid, {CHUNK}, 1, decode(repeat('7e', 32), 'hex'),
                     'quarantine/testnet/{upload}/{CHUNK}')"
        ),
    )
    .await;
    assert!(forged.is_err(), "the API alone commits quarantine ranges");

    // The API does not publish artifacts.
    let published = exec(
        &mut api,
        format!(
            "INSERT INTO pool.artifact
                 (network, artifact_id, assignment_id, benchmark_id, kind, backend,
                  container, object_key, media_type, format_version, sha256,
                  compressed_size, uncompressed_size, manifest_sha256)
             VALUES ('testnet', gen_random_uuid(), '{a}'::uuid, 'bench-07', 'PACKAGE',
                     'FILESYSTEM', 'accepted', 'accepted/testnet/bench-07/pkg.tar.zst',
                     'application/octet-stream', 'v1', decode(repeat('7e', 32), 'hex'),
                     {CHUNK}, {CHUNK}, decode(repeat('7e', 32), 'hex'))"
        ),
    )
    .await;
    assert!(published.is_err(), "the API publishes no artifact");

    // Neither the API nor the worker records durable acceptance — and the
    // session is walked to the rung before it first, so what refuses them is
    // the grant and not the ladder.
    for state in ["FINALIZED", "RECEIVED", "STRUCTURALLY_ACCEPTED"] {
        exec(
            &mut owner,
            format!(
                "UPDATE pool.upload_session SET state = '{state}'
                  WHERE upload_id = '{upload}'::uuid"
            ),
        )
        .await
        .unwrap();
    }

    for (role, conn) in [("api", &mut api), ("worker", &mut worker)] {
        let accepted = exec(
            conn,
            format!(
                "UPDATE pool.upload_session SET state = 'DURABLY_ACCEPTED'
                  WHERE upload_id = '{upload}'::uuid"
            ),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{accepted}").contains("to record, not"),
            "{role} must not record durable acceptance, got: {accepted}"
        );
    }

    // The positive half: each rung's owner may take it, and the neighbour may
    // not. Without this the trigger could refuse everyone and the test above
    // would still pass.
    let mut fresh = PgConnection::connect_with(&db.as_role("pool_api"))
        .await
        .unwrap();
    let second = open_session(&mut fresh, &a).await.unwrap_err();
    assert!(
        format!("{second}").contains("upload_one_live_session_per_assignment")
            || format!("{second}").contains("duplicate key"),
        "one live session per assignment, got: {second}"
    );

    let controller_accepts = exec(
        &mut controller,
        format!(
            "UPDATE pool.upload_session SET state = 'DURABLY_ACCEPTED'
              WHERE upload_id = '{upload}'::uuid"
        ),
    )
    .await;
    assert!(
        controller_accepts.is_ok(),
        "durable acceptance is the controller's: {controller_accepts:?}"
    );

    let receipted = exec(
        &mut api,
        format!(
            "INSERT INTO pool.acceptance_receipt
                 (network, assignment_id, receipt_id, artifact_id, upload_id, package_sha256)
             VALUES ('testnet', '{a}'::uuid, gen_random_uuid(), gen_random_uuid(),
                     '{upload}'::uuid, decode(repeat('7e', 32), 'hex'))"
        ),
    )
    .await;
    assert!(receipted.is_err(), "the API issues no receipt");

    // And the gateway sees none of it.
    for table in [
        "pool.upload_session",
        "pool.upload_chunk",
        "pool.artifact",
        "pool.acceptance_receipt",
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
