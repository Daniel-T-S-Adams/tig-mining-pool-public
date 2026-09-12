//! Slice-1 criterion C2, proven against a real PostgreSQL 18.
//!
//! `docs/tig_integration.md` §9 step 7: the accepted snapshot and its
//! completeness status are persisted atomically, before any decision derived
//! from them.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these tests skip, so a
//! developer without a database still gets a meaningful `make check`. CI
//! sets the URL and `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure —
//! the same contract as `pool-admin/tests/migrate.rs`.
//!
//! The throwaway-database harness is `pool-test-support`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pool_domain::Network;
use pool_snapshot::store::{BlockSnapshotStore, NotUsable, StoreError, content_digest};
use pool_snapshot::{PostgresSnapshotStore, Snapshot};
use pool_test_support::{TempDb, is_insufficient_privilege};
use sqlx::Row;

fn snapshot(block_id: &str, height: u64, complete: bool) -> Snapshot {
    let mut reads = BTreeMap::new();
    reads.insert(
        "get-opow".to_string(),
        serde_json::json!({ "qualifiers": 3 }),
    );
    let mut tracks = BTreeMap::new();
    tracks.insert("c001".to_string(), serde_json::json!({ "bundles": 1 }));
    Snapshot {
        block_id: block_id.to_string(),
        height,
        block: serde_json::json!({ "id": block_id, "details": { "round": 834 } }),
        reads,
        tracks,
        reads_complete: complete,
        active_cache_ready: complete,
    }
}

#[tokio::test]
async fn an_accepted_snapshot_is_persisted_with_its_completeness_status() {
    let Some(db) = TempDb::migrated("accepted").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);
    let snap = snapshot("block-a", 100, true);
    let expected_digest = content_digest(&snap).unwrap();

    let persisted = store.persist(Network::Testnet, snap).await.unwrap();

    // The status is in the same row as the content it describes: there is no
    // window in which one is visible without the other.
    let record = store
        .load_usable_record(Network::Testnet, "block-a")
        .await
        .unwrap()
        .expect("the record was written");
    assert_eq!(record.height, 100);
    assert!(record.reads_complete);
    assert!(record.active_cache_ready);
    assert_eq!(record.content_digest, expected_digest);
    assert_eq!(&record, persisted.record());

    // And only now does a decision input exist.
    assert_eq!(
        persisted.for_decision().unwrap().block_id,
        "block-a".to_string()
    );
}

#[tokio::test]
async fn an_incomplete_snapshot_is_recorded_but_yields_no_decision() {
    // C5: the orchestrator does no work while the snapshot is incomplete.
    // The row is still written — the status is a fact worth keeping, and an
    // operator needs to see that the block was reached at all.
    let Some(db) = TempDb::migrated("incomplete").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);

    let persisted = store
        .persist(Network::Testnet, snapshot("block-b", 101, false))
        .await
        .unwrap();

    // Recorded — an operator needs to see the block was reached at all —
    // but not offered to a decision.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pool.block_snapshot WHERE block_id = 'block-b' AND NOT reads_complete",
    )
    .fetch_one(&db.pool_as("pool_controller").await)
    .await
    .unwrap();
    assert_eq!(rows, 1, "the partial assembly is still recorded");
    assert!(
        store
            .load_usable_record(Network::Testnet, "block-b")
            .await
            .unwrap()
            .is_none(),
        "a partial assembly must never be offered as a decision input"
    );
    assert_eq!(persisted.for_decision(), Err(NotUsable::ReadsIncomplete));
    // Nor to reconciliation: a read that was not made is not an absence.
    assert_eq!(
        persisted.for_reconciliation(),
        Err(NotUsable::ReadsIncomplete)
    );
}

#[tokio::test]
async fn complete_reads_serve_reconciliation_before_the_active_cache_is_ready() {
    // §10 reconciliation consumes §7's confirmed reads and divides by
    // nothing, so the active-benchmark cache that gates a *decision* has no
    // bearing on it. Until the cache exists at all, this is the only way a
    // confirmation the pool can already see reaches a workflow.
    let Some(db) = TempDb::migrated("reconciliation_gate").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);
    let mut snap = snapshot("block-r", 102, true);
    snap.active_cache_ready = false;

    let persisted = store.persist(Network::Testnet, snap).await.unwrap();

    assert_eq!(
        persisted.for_decision(),
        Err(NotUsable::ActiveCacheUnavailable)
    );
    assert_eq!(
        persisted.for_reconciliation().map(|s| s.height),
        Ok(102),
        "reconciliation needs complete reads and nothing more"
    );
}

#[tokio::test]
async fn the_last_local_height_counts_only_completely_read_blocks() {
    // §10 measures a gap from "the last local height", and the gap exists to
    // record per-block data the pool cannot recover. A height whose anchored
    // reads never all succeeded holds no more of that data than one never
    // polled — and none of it can be fetched later, since the public API
    // serves no historical snapshot — so counting it here would leave the
    // loss unrecorded and §10.3's alert silent.
    let Some(db) = TempDb::migrated("last_height").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let store = PostgresSnapshotStore::new(controller.clone());

    assert_eq!(
        store.last_local_height(Network::Testnet).await.unwrap(),
        None
    );

    store
        .persist(Network::Testnet, snapshot("block-h1", 200, true))
        .await
        .unwrap();
    store
        .persist(Network::Testnet, snapshot("block-h2", 201, false))
        .await
        .unwrap();
    store
        .persist(Network::Mainnet, snapshot("block-h3", 900, true))
        .await
        .unwrap();

    assert_eq!(
        store.last_local_height(Network::Testnet).await.unwrap(),
        Some(200),
        "the incompletely read block at 201 holds no per-block data"
    );
    // Its record is still there — an operator needs to see the block was
    // reached at all. It is the *height* that does not count.
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pool.block_snapshot WHERE height = 201")
            .fetch_one(&controller)
            .await
            .unwrap();
    assert_eq!(rows, 1);

    // Reading it completely later — the next poll of the same block — makes
    // it count.
    store
        .persist(Network::Testnet, snapshot("block-h2", 201, true))
        .await
        .unwrap();
    assert_eq!(
        store.last_local_height(Network::Testnet).await.unwrap(),
        Some(201)
    );

    assert_eq!(
        store.last_local_height(Network::Mainnet).await.unwrap(),
        Some(900),
        "heights are per network"
    );
}

#[tokio::test]
async fn re_persisting_the_identical_snapshot_is_idempotent() {
    // The crash-retry path: a process that died between the commit and its
    // own acknowledgement re-assembles the same immutable block data and
    // must not be told its own write is a conflict.
    let Some(db) = TempDb::migrated("idempotent").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);

    let first = store
        .persist(Network::Testnet, snapshot("block-c", 102, true))
        .await
        .unwrap();
    let second = store
        .persist(Network::Testnet, snapshot("block-c", 102, true))
        .await
        .expect("re-persisting identical content succeeds");

    assert_eq!(first.record(), second.record());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.block_snapshot")
        .fetch_one(&db.pool_as("pool_controller").await)
        .await
        .unwrap();
    assert_eq!(rows, 1, "the retry must not add a second row");
}

#[tokio::test]
async fn a_different_snapshot_for_the_same_block_is_refused() {
    // §9 makes an accepted snapshot immutable and forbids substituting a
    // field into one. Silently keeping the first row would leave the
    // accepted record describing content the caller never assembled, and
    // silently overwriting it would be the substitution §9 rules out; both
    // are invisible to the caller, so the divergence is raised instead.
    let Some(db) = TempDb::migrated("divergent").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);

    store
        .persist(Network::Testnet, snapshot("block-d", 103, true))
        .await
        .unwrap();

    let mut different = snapshot("block-d", 103, true);
    different.reads.insert(
        "get-opow".to_string(),
        serde_json::json!({ "qualifiers": 4 }),
    );

    match store.persist(Network::Testnet, different).await {
        Err(StoreError::Divergent { network, block_id }) => {
            assert_eq!(network, Network::Testnet);
            assert_eq!(block_id, "block-d");
        }
        other => panic!("expected Divergent, got {other:?}"),
    }
}

#[tokio::test]
async fn the_same_block_id_on_another_network_is_a_separate_snapshot() {
    // The key is (network, block_id): testnet and mainnet block ids are
    // independent sequences, so one must never be read as the other's.
    let Some(db) = TempDb::migrated("networks").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);

    store
        .persist(Network::Testnet, snapshot("block-e", 104, true))
        .await
        .unwrap();
    store
        .persist(Network::Mainnet, snapshot("block-e", 900, true))
        .await
        .expect("a different network is not a conflict");

    let testnet = store
        .load_usable_record(Network::Testnet, "block-e")
        .await
        .unwrap()
        .expect("testnet record");
    let mainnet = store
        .load_usable_record(Network::Mainnet, "block-e")
        .await
        .unwrap()
        .expect("mainnet record");
    assert_eq!(testnet.height, 104);
    assert_eq!(mainnet.height, 900);
}

#[tokio::test]
async fn a_block_that_was_never_accepted_has_no_record() {
    let Some(db) = TempDb::migrated("absent").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);
    assert!(
        store
            .load_usable_record(Network::Testnet, "block-never")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_controller_cannot_rewrite_or_delete_an_accepted_snapshot() {
    // The immutability of §9 is a privilege, not a convention: the migration
    // grants SELECT and INSERT and withholds UPDATE and DELETE, so a future
    // component that decided to "correct" an accepted snapshot in place is
    // refused by PostgreSQL rather than by a code review.
    let Some(db) = TempDb::migrated("immutable").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    PostgresSnapshotStore::new(pool.clone())
        .persist(Network::Testnet, snapshot("block-f", 105, true))
        .await
        .unwrap();

    let update = sqlx::query("UPDATE pool.block_snapshot SET reads_complete = false")
        .execute(&pool)
        .await
        .expect_err("pool_controller must not hold UPDATE");
    assert!(
        is_insufficient_privilege(&update),
        "UPDATE must be refused for lack of privilege, got: {update}"
    );

    let delete = sqlx::query("DELETE FROM pool.block_snapshot")
        .execute(&pool)
        .await
        .expect_err("pool_controller must not hold DELETE");
    assert!(
        is_insufficient_privilege(&delete),
        "DELETE must be refused for lack of privilege, got: {delete}"
    );

    // The row is still exactly as accepted.
    let complete: bool =
        sqlx::query("SELECT reads_complete FROM pool.block_snapshot WHERE block_id = 'block-f'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("reads_complete");
    assert!(complete);
}

#[tokio::test]
async fn monitoring_can_read_snapshot_status_but_not_write_it() {
    // architecture.md §10.2 reads snapshot age and completeness; §9 scopes
    // that credential to reading.
    let Some(db) = TempDb::migrated("readonly").await else {
        return;
    };
    PostgresSnapshotStore::new(db.pool_as("pool_controller").await)
        .persist(Network::Testnet, snapshot("block-g", 106, true))
        .await
        .unwrap();

    let readonly = db.pool_as("pool_readonly").await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.block_snapshot")
        .fetch_one(&readonly)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // `decode(repeat('00', 32), 'hex')`, not a `'\\x..'` string literal: a
    // malformed bytea literal fails while the statement is planned, before
    // the ACL check runs, and the assertion below would then hold even with
    // INSERT granted.
    let insert = sqlx::query(
        "INSERT INTO pool.block_snapshot
             (network, block_id, content_digest, height, reads_complete, active_cache_ready)
         VALUES ('testnet', 'block-h', decode(repeat('00', 32), 'hex'), 1, true, true)",
    )
    .execute(&readonly)
    .await
    .expect_err("pool_readonly must not hold INSERT");
    assert!(
        is_insufficient_privilege(&insert),
        "INSERT must be refused for lack of privilege, got: {insert}"
    );
}

#[tokio::test]
async fn a_partial_assembly_is_superseded_by_a_later_complete_one() {
    // §9 step 6 and §5.2 make re-assembly at the same block the normal
    // recovery path, and `active_cache_ready` is false for every snapshot
    // until step 4 lands — so treating a recorded partial assembly as
    // immutable would refuse every later complete assembly of that block as
    // Divergent, and the block could never gain a usable record at all.
    let Some(db) = TempDb::migrated("supersede").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);

    store
        .persist(Network::Testnet, snapshot("block-i", 107, false))
        .await
        .unwrap();
    assert!(
        store
            .load_usable_record(Network::Testnet, "block-i")
            .await
            .unwrap()
            .is_none()
    );

    let complete = store
        .persist(Network::Testnet, snapshot("block-i", 107, true))
        .await
        .expect("a complete assembly supersedes the partial one");

    let usable = store
        .load_usable_record(Network::Testnet, "block-i")
        .await
        .unwrap()
        .expect("the block now has a usable record");
    assert_eq!(&usable, complete.record());
    assert!(complete.for_decision().is_ok());

    // The partial assembly was superseded, not edited: §9 forbids
    // substituting into a recorded snapshot, so both rows remain.
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pool.block_snapshot WHERE block_id = 'block-i'")
            .fetch_one(&db.pool_as("pool_controller").await)
            .await
            .unwrap();
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn several_partial_assemblies_of_one_block_may_coexist() {
    // None of them can reach a decision, so there is nothing to arbitrate
    // between; refusing the second would again strand the block.
    let Some(db) = TempDb::migrated("partials").await else {
        return;
    };
    let store = PostgresSnapshotStore::new(db.pool_as("pool_controller").await);

    store
        .persist(Network::Testnet, snapshot("block-j", 108, false))
        .await
        .unwrap();
    let mut other = snapshot("block-j", 108, false);
    other.reads.insert(
        "get-opow".to_string(),
        serde_json::json!({ "qualifiers": 9 }),
    );
    store
        .persist(Network::Testnet, other)
        .await
        .expect("a second partial assembly is not a divergence");

    assert!(
        store
            .load_usable_record(Network::Testnet, "block-j")
            .await
            .unwrap()
            .is_none()
    );
}
