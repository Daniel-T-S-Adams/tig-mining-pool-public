//! Throwaway provisioned databases for tests.
//!
//! Four crates now need a real PostgreSQL to test against — the migration
//! job, the snapshot store, write intents, and the write-attempt ledger —
//! and the harness had been copied into each. This is the fourth use, which
//! is where copying it again stops being defensible.
//!
//! **Test-only.** It is a dev-dependency of the crates that use it and a
//! dependency of none, so nothing here reaches a shipped binary. It creates
//! and drops databases and executes the real provisioning SQL, which is not
//! behaviour any service should be able to reach.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`. Without it [`TempDb::create`] and
//! [`TempDb::migrated`] return `None` so a developer with no database still
//! gets a meaningful `make check`; CI sets the URL and
//! `POOL_REQUIRE_DB_TESTS=1`, which turns a skip into a failure.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};

/// The repository's single ordered migration directory.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// The provisioning SQL real environments run, compiled in so tests cannot
/// drift from it. A grant that changed in that file has to change here too.
pub const PROVISION_ROLES_SQL: &str = include_str!("../../../scripts/provision-db-roles.sql");

/// Serialises the cluster-wide role creation these tests share.
///
/// Roles are cluster-wide while tests run in parallel, so two of them
/// altering the same role race in `pg_authid` ("tuple concurrently
/// updated"). A session advisory lock brackets the provisioning window and
/// is released when the connection closes, so a panicking test cannot wedge
/// the others.
const PROVISION_LOCK: i64 = 0x7069_6f6f_6c5f_726f;

/// Seed the workflow rows an intent needs to exist.
///
/// `pool.tig_write_intent` carries a foreign key to `pool.workflow`, because
/// `mining_system.md` §2 requires the benchmark → owner mapping to exist from
/// creation and §8 charges faults to it — an intent whose workflow was never
/// created is a write the pool could make and then be unable to attribute.
/// Production creates all three rows in one admission transaction; a test that
/// exercises intents alone seeds the owner mapping with this.
///
/// The interval opens at height 1: `mining_system.md` §6.1 counts a benchmark
/// as unverified from the intent's creation, and these tests do not care where.
pub async fn seed_workflows(pool: &PgPool, network: &str, workflow_ids: &[&str]) {
    seed(
        pool,
        network,
        workflow_ids,
        "POOL_BOOTSTRAP",
        "pool-bootstrap",
    )
    .await;
}

/// The same, member-owned.
///
/// `mining_system.md` §10 invariant 1's bootstrap carve-out is testnet-only and
/// the schema enforces it, so a mainnet fixture cannot use the placeholder.
pub async fn seed_workflows_owned_by_member(pool: &PgPool, network: &str, workflow_ids: &[&str]) {
    seed(pool, network, workflow_ids, "MEMBER", "member_fixture").await;
}

/// Satisfy `architecture.md` §13 invariant 4 for these (workflow, benchmark)
/// pairs.
///
/// `migrations/0011` refuses a benchmark write intent without a durable
/// package acceptance, which is the point — but a test about the intent *key*
/// rules or the attempt lane is not a test about acceptance, and making each
/// of them build the precondition by hand would bury its subject. The bytes
/// are a fixture; the row's existence is what the invariant is about.
pub async fn seed_acceptances(pool: &PgPool, network: &str, pairs: &[(&str, &str)]) {
    for (workflow_id, benchmark_id) in pairs {
        sqlx::query(
            "INSERT INTO pool.package_acceptance
                 (network, workflow_id, benchmark_id, package_sha256)
             VALUES ($1, $2, $3, decode(repeat('7a', 32), 'hex'))
             ON CONFLICT (network, workflow_id, benchmark_id) DO NOTHING",
        )
        .bind(network)
        .bind(workflow_id)
        .bind(benchmark_id)
        .execute(pool)
        .await
        .expect("seeding a package acceptance");
    }
}

/// The same for invariant 5: a canonical payload a proof intent may cite.
///
/// Returns nothing — the caller already knows the artifact id, because it has
/// to put the same one on the intent. `payload_digest` must match the intent's
/// too, so it is a parameter rather than a fixture constant: 0011 requires the
/// two to agree, and a helper that quietly chose its own would make every
/// proof-intent test pass for the wrong reason.
pub async fn seed_canonical_payload(
    pool: &PgPool,
    network: &str,
    artifact_id: &str,
    workflow_id: &str,
    benchmark_id: &str,
    payload_digest: [u8; 32],
) {
    sqlx::query(
        "INSERT INTO pool.canonical_payload
             (network, artifact_id, workflow_id, benchmark_id,
              sample_digest, payload_digest)
         VALUES ($1, $2, $3, $4, decode(repeat('5c', 32), 'hex'), $5)
         ON CONFLICT (network, artifact_id) DO NOTHING",
    )
    .bind(network)
    .bind(artifact_id)
    .bind(workflow_id)
    .bind(benchmark_id)
    .bind(payload_digest.as_slice())
    .execute(pool)
    .await
    .expect("seeding a canonical payload");
}

async fn seed(pool: &PgPool, network: &str, ids: &[&str], kind: &str, owner: &str) {
    for id in ids {
        sqlx::query(
            "INSERT INTO pool.workflow
                 (network, workflow_id, owner_kind, owner_id, unverified_from_block)
             VALUES ($1, $2, $3, $4, 1)
             ON CONFLICT (network, workflow_id) DO NOTHING",
        )
        .bind(network)
        .bind(id)
        .bind(kind)
        .bind(owner)
        .execute(pool)
        .await
        .expect("seeding a workflow");
    }
}

/// Run dynamically built SQL.
///
/// sqlx 0.9 requires `'static` query strings, so generated statements go
/// through `AssertSqlSafe`. Every caller builds its SQL from test-local
/// constants and role names, never from input.
pub async fn exec(conn: &mut PgConnection, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// The superuser password, read from the untracked dev secret at use time.
///
/// `POOL_TEST_SUPERUSER_URL` deliberately carries no password: putting one
/// in the environment is one of the exposures `architecture.md` §9 names,
/// and it would then reach every child process of the test runner. Absent
/// (as under CI trust auth) means no password is needed.
pub fn superuser_password_from_secrets() -> Option<String> {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../secrets/db-superuser-password"),
    )
    .ok()
    .map(|p| p.trim_end_matches(['\n', '\r']).to_string())
    .filter(|p| !p.is_empty())
}

/// The configured superuser URL, or `None` when database tests should skip.
///
/// Panics instead when `POOL_REQUIRE_DB_TESTS=1`: an environment that has
/// opted into database tests must not silently run none.
pub fn superuser_url() -> Option<String> {
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

/// PostgreSQL `insufficient_privilege`.
///
/// Assert privilege boundaries with this rather than `is_err()`. A statement
/// that fails while being planned — a malformed literal, a misspelt column —
/// never reaches the ACL check, so an `is_err()` assertion goes on passing
/// after the boundary it claims to test has been removed entirely. That is
/// not hypothetical: a bad `bytea` literal did exactly this in the snapshot
/// store's grant test.
pub fn is_insufficient_privilege(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("42501")
}

/// A throwaway database, dropped when the guard falls out of scope.
pub struct TempDb {
    name: String,
    superuser_url: String,
}

impl TempDb {
    /// A provisioned but unmigrated database, for tests that run the
    /// migrations themselves.
    pub async fn create(label: &str) -> Option<Self> {
        let superuser_url = superuser_url()?;
        // Database names go into unquoted identifiers, so fold anything a
        // caller's label might contain (a hyphen, say) down to `_`.
        let label: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let name = format!("pool_test_{}_{label}", std::process::id());

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

        // The REAL provisioning artifact, not a copy of it. If a future edit
        // granted a service role DDL in that file, the grant assertions in
        // the migration tests catch it; an inline reimplementation here
        // would stay green while every provisioned environment drifted. Its
        // role creation is cluster-wide and additive, and its `public`
        // schema grants are per-database, which is why it runs on the
        // throwaway database rather than on `postgres`.
        exec(&mut conn, PROVISION_ROLES_SQL.to_string())
            .await
            .unwrap();

        // Only the per-database grants the artifact deliberately leaves to
        // the environment.
        exec(
            &mut conn,
            format!(
                "GRANT CREATE, CONNECT ON DATABASE {} TO pool_migration;
                 GRANT CONNECT ON DATABASE {} TO pool_controller, pool_gateway, pool_api, \
                 pool_artifact_worker, pool_readonly;",
                db.name, db.name
            ),
        )
        .await
        .unwrap();

        // Released only after role creation has finished, since that is the
        // part that races.
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(PROVISION_LOCK)
            .execute(&mut admin)
            .await
            .unwrap();

        Some(db)
    }

    /// A provisioned database with every migration applied.
    pub async fn migrated(label: &str) -> Option<Self> {
        let db = Self::create(label).await?;
        let mut conn = PgConnection::connect_with(&db.as_superuser())
            .await
            .unwrap();
        MIGRATOR.run(&mut conn).await.unwrap();
        Some(db)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn as_superuser(&self) -> PgConnectOptions {
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

    /// Connect as the superuser but adopt `role` for the session.
    ///
    /// Privilege checks then apply as they would to that role logging in
    /// directly, with no password involved. Tests must never set a role
    /// password: roles are cluster-wide, so an `ALTER ROLE ... PASSWORD`
    /// would reach out of the throwaway database and overwrite the
    /// credentials `scripts/dev-db.sh` provisioned into `secrets/`, breaking
    /// the developer's cluster — which is exactly what an earlier version of
    /// this harness did.
    pub fn as_role(&self, role: &str) -> PgConnectOptions {
        self.as_superuser().options([("role", role)])
    }

    /// A pool connected as `role`.
    pub async fn pool_as(&self, role: &str) -> PgPool {
        PgPoolOptions::new()
            .max_connections(16)
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
