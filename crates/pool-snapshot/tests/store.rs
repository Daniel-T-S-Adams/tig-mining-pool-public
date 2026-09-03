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
//! The throwaway-database harness below is a trimmed copy of that file's.
//! Two copies is not yet a shared test-support crate; the third user should
//! extract it rather than copy it again.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pool_domain::Network;
use pool_snapshot::store::{BlockSnapshotStore, NotUsable, StoreError, content_digest};
use pool_snapshot::{PostgresSnapshotStore, Snapshot};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool, Row};

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

const PROVISION_ROLES_SQL: &str = include_str!("../../../scripts/provision-db-roles.sql");

/// Serialises the cluster-wide role creation these tests share; see
/// `pool-admin/tests/migrate.rs` for why concurrent provisioning races.
const PROVISION_LOCK: i64 = 0x7069_6f6f_6c5f_726f;

async fn exec(conn: &mut PgConnection, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// Read at use time from the untracked dev secret; never from the
/// environment (`architecture.md` §9).
fn superuser_password_from_secrets() -> Option<String> {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../secrets/db-superuser-password"),
    )
    .ok()
    .map(|p| p.trim_end_matches(['\n', '\r']).to_string())
    .filter(|p| !p.is_empty())
}

fn superuser_url() -> Option<String> {
    match std::env::var("POOL_TEST_SUPERUSER_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            if std::env::var("POOL_REQUIRE_DB_TESTS").as_deref() == Ok("1") {
                panic!(
                    "POOL_TEST_SUPERUSER_URL is unset but POOL_REQUIRE_DB_TESTS=1: database tests \
                     must not be skipped here"
                );
            }
            eprintln!("skipping: POOL_TEST_SUPERUSER_URL unset (run ./scripts/dev-db.sh)");
            None
        }
    }
}

struct TempDb {
    name: String,
    superuser_url: String,
}

impl TempDb {
    /// A throwaway database with the real migrations applied, or `None` when
    /// no database is configured.
    async fn migrated(label: &str) -> Option<Self> {
        let superuser_url = superuser_url()?;
        let label: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let name = format!("pool_snap_{}_{label}", std::process::id());

        let mut admin_opts = superuser_url.parse::<PgConnectOptions>().unwrap();
        if let Some(password) = superuser_password_from_secrets() {
            admin_opts = admin_opts.password(&password);
        }
        let mut admin = PgConnection::connect_with(&admin_opts).await.unwrap();

        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(PROVISION_LOCK)
            .execute(&mut admin)
            .await
            .unwrap();

        exec(&mut admin, format!("DROP DATABASE IF EXISTS {name};"))
            .await
            .unwrap();
        exec(&mut admin, format!("CREATE DATABASE {name};"))
            .await
            .unwrap();

        let db = Self {
            name,
            superuser_url,
        };
        let mut conn = PgConnection::connect_with(&db.as_superuser())
            .await
            .unwrap();

        // The real provisioning artifact, not a copy: a grant this file
        // reimplemented would stay green while every provisioned
        // environment drifted.
        exec(&mut conn, PROVISION_ROLES_SQL.to_string())
            .await
            .unwrap();
        exec(
            &mut conn,
            format!(
                "GRANT CONNECT ON DATABASE {} TO pool_controller, pool_readonly;",
                db.name
            ),
        )
        .await
        .unwrap();

        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(PROVISION_LOCK)
            .execute(&mut admin)
            .await
            .unwrap();

        MIGRATOR.run(&mut conn).await.unwrap();
        Some(db)
    }

    fn as_superuser(&self) -> PgConnectOptions {
        let mut opts = self
            .superuser_url
            .parse::<PgConnectOptions>()
            .unwrap()
            .database(&self.name);
        if let Some(password) = superuser_password_from_secrets() {
            opts = opts.password(&password);
        }
        opts
    }

    /// Superuser connection that adopts `role` for the session, so privilege
    /// checks apply as they would to that role — with no password involved
    /// and no cluster-wide credential touched.
    fn as_role(&self, role: &str) -> PgConnectOptions {
        self.as_superuser().options([("role", role)])
    }

    async fn pool_as(&self, role: &str) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect_with(self.as_role(role))
            .await
            .unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let url = self.superuser_url.clone();
        let name = self.name.clone();
        // Best effort: a leaked test database is noise, not a failure.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut opts = match url.parse::<PgConnectOptions>() {
                    Ok(o) => o,
                    Err(_) => return,
                };
                if let Some(password) = superuser_password_from_secrets() {
                    opts = opts.password(&password);
                }
                if let Ok(mut conn) = PgConnection::connect_with(&opts).await {
                    let _ = exec(
                        &mut conn,
                        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE);"),
                    )
                    .await;
                }
            });
        })
        .join();
    }
}

/// PostgreSQL `insufficient_privilege`.
///
/// Asserted by code, not by `is_err()`. A statement that fails while being
/// planned — a malformed literal, a misspelt column — never reaches the ACL
/// check, so an `is_err()` assertion would go on passing after the privilege
/// boundary it claims to test had been removed entirely.
fn is_insufficient_privilege(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("42501")
}

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
