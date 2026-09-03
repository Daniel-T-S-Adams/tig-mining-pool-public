//! Slice-1 criteria J1–J3: migration policy, proven against a real
//! PostgreSQL 18.
//!
//! Each test works in its own throwaway database, so ordering between tests
//! cannot hide a failure and a crashed run leaves nothing behind that a
//! later run depends on.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`. Without it these tests skip, because
//! a developer without a local database should still be able to run
//! `make check`. CI sets the URL, and `POOL_REQUIRE_DB_TESTS=1` turns a skip
//! into a failure so the coverage cannot vanish silently there.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::borrow::Cow;

use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection, Row, SqlSafeStr};

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// The provisioning SQL real environments run, compiled in so the tests
/// cannot drift from it. Passwords live in a separate file precisely so
/// this one is plain SQL and can be executed here.
const PROVISION_ROLES_SQL: &str = include_str!("../../../scripts/provision-db-roles.sql");

// These tests never set or use a role password. Roles are cluster-wide, so
// an `ALTER ROLE ... PASSWORD` here would reach out of the throwaway
// database and overwrite the credentials `scripts/dev-db.sh` provisioned
// into `secrets/`, breaking the developer's cluster — which is exactly what
// an earlier version of this file did. Instead the tests connect as the
// superuser and adopt the role with the `role` startup option, which
// exercises the same privilege checks without touching any credential.

/// Run dynamically built SQL. sqlx 0.9 requires `'static` query strings, so
/// generated statements go through `AssertSqlSafe`. Every caller here builds
/// its SQL from test-local constants and role names, never from input.
async fn exec(conn: &mut PgConnection, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// The superuser password, read from the untracked dev secret at use time.
///
/// `POOL_TEST_SUPERUSER_URL` deliberately carries no password: putting one in
/// the environment is one of the exposures `architecture.md` §9 names
/// ("ordinary environment dumps"), and it would then also reach every child
/// process of the test runner. Absent (as under CI trust auth) means no
/// password is needed.
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

/// A throwaway database, dropped when the guard falls out of scope.
struct TempDb {
    name: String,
    superuser_url: String,
}

impl TempDb {
    async fn create(label: &str) -> Option<Self> {
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

        // Roles are cluster-wide while these tests run in parallel, so two
        // of them altering the same role races in pg_authid ("tuple
        // concurrently updated"). A session advisory lock serialises the
        // provisioning window; it is released when this connection closes,
        // so a panicking test cannot wedge the others.
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(0x7069_6f6f_6c5f_726fi64)
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

        // Execute the REAL provisioning artifact against this database, not
        // a copy of it. If a future edit granted a service role DDL in that
        // file, the grant assertions below catch it; an inline
        // reimplementation here would stay green while every provisioned
        // environment drifted. Its role creation is cluster-wide and
        // additive — an existing role keeps its provisioned password — and
        // its `public` schema grants are per-database, which is why this
        // runs on the throwaway database rather than on `postgres`.
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
            .bind(0x7069_6f6f_6c5f_726fi64)
            .execute(&mut admin)
            .await
            .unwrap();

        Some(db)
    }

    /// Connect to the throwaway database as the superuser, but adopt `role`
    /// for the session. Privilege checks then apply as they would to that
    /// role logging in directly, with no password involved.
    fn as_role(&self, role: &str) -> PgConnectOptions {
        self.as_superuser().options([("role", role)])
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

#[tokio::test]
async fn migrations_apply_to_an_empty_database() {
    // J2, first half: every migration applies from nothing.
    let Some(db) = TempDb::create("empty").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();

    MIGRATOR.run(&pool).await.expect("migrations must apply");

    let rows =
        sqlx::query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows.len(),
        MIGRATOR.iter().count(),
        "every migration must be recorded"
    );
    for row in &rows {
        assert!(row.get::<bool, _>("success"));
        let checksum: Vec<u8> = row.get("checksum");
        assert!(!checksum.is_empty(), "each migration records a checksum");
    }
}

#[tokio::test]
async fn each_migration_applies_on_top_of_its_predecessors() {
    // J2, second half: not just "all at once from empty", but each
    // migration against a copy of the schema that precedes it. With one
    // migration this is the same run; the harness generalises so the
    // guarantee holds as later slices add files.
    let Some(db) = TempDb::create("stepwise").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();

    let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    assert!(!versions.is_empty(), "there must be migrations to test");

    for (index, version) in versions.iter().enumerate() {
        MIGRATOR
            .run_to(*version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migration {version} failed on the preceding schema: {e}"));

        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            applied as usize,
            index + 1,
            "after running to {version}, exactly {} migrations should be applied",
            index + 1
        );
    }
}

#[tokio::test]
async fn migrations_are_ordered_and_unique() {
    // Forward-only in practice needs strictly increasing versions; two
    // files sharing a version would apply in an undefined order.
    let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    let mut sorted = versions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        versions, sorted,
        "migration versions must be unique and ascending"
    );
}

#[tokio::test]
async fn rerunning_migrations_is_a_no_op() {
    // J1: `pool-admin migrate` is safe to run again, which is what makes it
    // usable as a deploy step.
    let Some(db) = TempDb::create("rerun").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();

    MIGRATOR.run(&pool).await.unwrap();
    let first: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();

    MIGRATOR.run(&pool).await.expect("second run must succeed");
    let second: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();

    assert_eq!(first, second, "a second run must change nothing");
}

#[tokio::test]
async fn an_edited_applied_migration_is_refused() {
    // J1: forward-only. Editing a migration that has already run must fail
    // loudly rather than silently diverge from the deployed schema.
    let Some(db) = TempDb::create("checksum").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();

    MIGRATOR.run(&pool).await.unwrap();

    // Every migration, with only the first one's SQL changed. Passing just
    // the tampered migration would leave every later version applied but
    // absent from the list, and sqlx reports that first — a real error, but
    // "missing", not "modified", so the assertion below would be answered by
    // the wrong failure and stop testing forward-only the moment a second
    // migration existed.
    let tampered = Migrator::with_migrations(
        MIGRATOR
            .iter()
            .enumerate()
            .map(|(i, m)| {
                Migration::new(
                    m.version,
                    Cow::Owned(m.description.to_string()),
                    MigrationType::Simple,
                    if i == 0 {
                        // Same version, different SQL: a different checksum.
                        AssertSqlSafe("SELECT 1;".to_string()).into_sql_str()
                    } else {
                        m.sql.clone()
                    },
                    false,
                )
            })
            .collect::<Vec<_>>(),
    );

    let err = tampered
        .run(&pool)
        .await
        .expect_err("a changed checksum must be refused");
    let message = format!("{err}").to_lowercase();
    assert!(
        message.contains("modified") || message.contains("checksum"),
        "error should say the applied migration was modified, got: {err}"
    );
}

#[tokio::test]
async fn service_roles_cannot_create_objects_in_the_pool_schema() {
    // J3 / D4 shape: the boundary in `architecture.md` §6 is enforced by
    // grants, not by convention. A service role may resolve names in the
    // schema and may not create in it; DDL belongs to the migration
    // identity alone.
    let Some(db) = TempDb::create("grants").await else {
        return;
    };
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();

    for role in [
        "pool_controller",
        "pool_gateway",
        "pool_api",
        "pool_artifact_worker",
        "pool_readonly",
    ] {
        let mut conn = PgConnection::connect_with(&db.as_role(role)).await.unwrap();

        let usage: bool = sqlx::query_scalar("SELECT has_schema_privilege($1, 'pool', 'USAGE')")
            .bind(role)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert!(usage, "{role} must have USAGE on schema pool");

        let create: bool = sqlx::query_scalar("SELECT has_schema_privilege($1, 'pool', 'CREATE')")
            .bind(role)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert!(!create, "{role} must NOT have CREATE on schema pool");

        // And the privilege check is not theoretical: the DDL really fails.
        let attempt = exec(
            &mut conn,
            "CREATE TABLE pool.should_not_exist (id int);".to_string(),
        )
        .await;
        assert!(
            attempt.is_err(),
            "{role} must not be able to create a table in schema pool"
        );
    }
}

#[tokio::test]
async fn configured_statement_timeout_reaches_a_real_session() {
    // `statement_timeout_ms` is documented as applied to every session and
    // is rejected at zero on that basis, so the claim needs a real
    // connection behind it. The string assertion in pool-config's tests
    // proves only that the URL contains the parameter; this proves the
    // server honoured it. Without it, a change to how sqlx parses `options`
    // would silently make the setting inert again.
    let Some(db) = TempDb::create("timeout").await else {
        return;
    };

    // Drive the connection through Config::database_url() itself, which is
    // the code path the binaries use.
    let superuser = db.as_superuser();
    // Read (never set) the provisioned migration password, exactly as
    // migration_role_config does; under trust auth any value is accepted.
    let password = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../secrets/db-migration-password"),
    )
    .ok()
    .map(|p| p.trim_end_matches(['\n', '\r']).to_string())
    .filter(|p| !p.trim().is_empty())
    .unwrap_or_else(|| "trust-auth-no-password-required".to_string());

    let scratch = SecretScratch::new("timeout");
    let password_file = scratch.write_secret("db-password", &password);

    let config_toml = format!(
        r#"
network = "testnet"

[database]
host = "{host}"
port = {port}
name = "{name}"
user = "{user}"
password_file = "{password_file}"
statement_timeout_ms = 7777

[telemetry]
format = "json"
level = "info"
deployment = "test"
"#,
        host = superuser.get_host(),
        port = superuser.get_port(),
        name = db.name,
        user = "pool_migration",
        password_file = password_file.display(),
    );
    let config_path = scratch.dir.join("config.toml");
    std::fs::write(&config_path, config_toml).unwrap();

    // `PoolAdminMigrate` requires `pool_migration` — the DDL-bearing
    // credential `architecture.md` §9 restricts to the one-shot migration
    // job — so this case must name the role it actually connects as.
    let config = pool_config::Config::load(&config_path, pool_config::Binary::PoolAdminMigrate)
        .expect("test config must load");

    let mut conn = PgConnection::connect(&config.database_url().unwrap())
        .await
        .expect("connect via Config::database_url()");

    let observed: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut conn)
        .await
        .unwrap();

    assert_eq!(
        observed, "7777ms",
        "the configured statement_timeout must be applied to the session"
    );
}

/// Extract the password from `POOL_TEST_SUPERUSER_URL`, or `None` when the
/// URL has none (trust auth).
///
/// Parsed by stripping the scheme first. Splitting the whole URL on `:`
/// from the right matches the `://` separator on a passwordless URL and
/// yields `"//postgres"` — a bogus credential that trust auth silently
/// accepts, so the test would pass while exercising nothing.
fn superuser_password() -> Option<String> {
    superuser_password_from_secrets()
}

/// A scratch directory for files that hold a credential: owner-only
/// permissions, and removed on panic as well as on success.
struct SecretScratch {
    dir: std::path::PathBuf,
}

impl SecretScratch {
    fn new(label: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pool-admin-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir }
    }

    fn write_secret(&self, name: &str, value: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join(name);
        std::fs::write(&path, value.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}

impl Drop for SecretScratch {
    fn drop(&mut self) {
        // Runs on panic too, so a failed assertion never leaves a copy of a
        // credential behind in a world-readable temp directory.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn a_dry_run_applies_nothing_and_leaves_the_real_run_working() {
    // The flag exists so an operator can see the plan before mutating the
    // schema they own (`architecture.md` §6). Untested, it could report a
    // plan and apply it anyway.
    let Some(db) = TempDb::create("dryrun").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();

    // A dry run must not create the ledger table, let alone fill it.
    let before: Option<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = '_sqlx_migrations'",
    )
    .fetch_one(&pool)
    .await
    .ok();
    assert_eq!(before, Some(0), "the ledger must not exist yet");

    let scratch = SecretScratch::new("dryrun-cfg");
    let Some(config_path) = migration_role_config(&db, &scratch) else {
        // Same shape as superuser_url(): an environment that has opted into
        // required database coverage is not allowed to skip silently. A
        // second, unguarded skip path would quietly undo the guarantee the
        // workflow, Makefile and README all advertise.
        assert_ne!(
            std::env::var("POOL_REQUIRE_DB_TESTS").as_deref(),
            Ok("1"),
            "no pool_migration credential available, but POOL_REQUIRE_DB_TESTS=1: this test \
             must not be skipped here"
        );
        eprintln!("skipping dry-run assertions: no pool_migration credential available");
        return;
    };
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pool-admin"))
        .arg("--config")
        .arg(&config_path)
        .arg("migrate")
        .arg("--dry-run")
        .output()
        .expect("binary runs");
    assert!(
        output.status.success(),
        "dry run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = '_sqlx_migrations'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, 0, "a dry run must apply nothing");

    // And the real run still works afterwards.
    MIGRATOR
        .run(&pool)
        .await
        .expect("migrations must still apply after a dry run");
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied as usize, MIGRATOR.iter().count());
}

/// A config naming `pool_migration` against the throwaway database, for the
/// tests that must drive the real binary rather than the `Migrator`.
///
/// The migration role's password is *read* from the provisioned dev secret,
/// never set: setting it would reach out of the throwaway database and
/// overwrite the developer's cluster credential. Under trust auth (CI) any
/// value is accepted, so a placeholder is enough. Returns `None` when
/// neither is available, so the caller skips rather than asserting against a
/// credential that cannot work.
fn migration_role_config(db: &TempDb, scratch: &SecretScratch) -> Option<std::path::PathBuf> {
    let superuser = db.as_superuser();
    let password = match std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../secrets/db-migration-password"),
    ) {
        Ok(p) if !p.trim().is_empty() => p.trim_end_matches(['\n', '\r']).to_string(),
        // Trust auth accepts anything; scram would reject it, and the
        // caller skips instead of reporting a confusing auth failure.
        _ if superuser_password().is_none() => "trust-auth-no-password".to_string(),
        _ => return None,
    };
    let password_file = scratch.write_secret("db-password", &password);
    let path = scratch.dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            r#"
network = "testnet"

[database]
host = "{host}"
port = {port}
name = "{name}"
user = "{user}"
password_file = "{password_file}"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "test"
"#,
            host = superuser.get_host(),
            port = superuser.get_port(),
            name = db.name,
            user = "pool_migration",
            password_file = password_file.display(),
        ),
    )
    .unwrap();
    Some(path)
}

#[tokio::test]
async fn a_dry_run_refuses_an_edited_applied_migration() {
    // A dry run is the operator's pre-deploy check, so it has to refuse
    // everything the real run would refuse. Comparing versions alone would
    // report a clean plan for a repository whose applied migration file had
    // been edited, and the forward-only violation would only surface at
    // deploy time (`architecture.md` §7.1).
    let Some(db) = TempDb::create("dryrun-checksum").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    // Rewrite the recorded checksum, which is indistinguishable from having
    // edited the migration file after it was applied.
    let first = MIGRATOR.iter().next().unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
        .bind(vec![0u8; 32])
        .bind(first.version)
        .execute(&pool)
        .await
        .unwrap();

    let scratch = SecretScratch::new("dryrun-checksum-cfg");
    let Some(config_path) = migration_role_config(&db, &scratch) else {
        assert_ne!(
            std::env::var("POOL_REQUIRE_DB_TESTS").as_deref(),
            Ok("1"),
            "no pool_migration credential available, but POOL_REQUIRE_DB_TESTS=1"
        );
        return;
    };

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pool-admin"))
        .arg("--config")
        .arg(&config_path)
        .arg("migrate")
        .arg("--dry-run")
        .output()
        .expect("binary runs");

    assert!(
        !output.status.success(),
        "a dry run must fail on a modified applied migration"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("has been modified"),
        "the failure must name the cause, got: {combined}"
    );
}

#[tokio::test]
async fn the_monitoring_role_gets_no_blanket_visibility_and_never_writes() {
    // Monitoring visibility must be granted per table by the migration that
    // creates it, not inherited from a default privilege. A blanket default
    // would be fail-open for the credential `architecture.md` §9 scopes:
    // every future table readable without anyone deciding so.
    let Some(db) = TempDb::create("readonly").await else {
        return;
    };
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();

    // A table owned by the migrator, with no explicit grant to monitoring.
    exec(
        &mut PgConnection::connect_with(&db.as_role("pool_migration"))
            .await
            .unwrap(),
        "CREATE TABLE IF NOT EXISTS pool.readonly_probe (id int);".to_string(),
    )
    .await
    .unwrap();

    let mut conn = PgConnection::connect_with(&db.as_role("pool_readonly"))
        .await
        .unwrap();

    let inherited: bool =
        sqlx::query_scalar("SELECT has_table_privilege('pool_readonly', $1, 'SELECT')")
            .bind("pool.readonly_probe")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert!(
        !inherited,
        "a new table must not be readable by monitoring without an explicit grant"
    );

    // Whatever it is granted, it is never a write.
    for write in ["INSERT", "UPDATE", "DELETE", "TRUNCATE"] {
        let granted: bool =
            sqlx::query_scalar("SELECT has_table_privilege('pool_readonly', $1, $2)")
                .bind("pool.readonly_probe")
                .bind(write)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert!(!granted, "pool_readonly must not hold {write}");
    }

    let writable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.role_table_grants \
         WHERE grantee = 'pool_readonly' AND table_schema = 'pool' \
           AND privilege_type IN ('INSERT','UPDATE','DELETE','TRUNCATE')",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        writable, 0,
        "pool_readonly holds write grants on {writable} table(s) in schema pool"
    );
}

#[tokio::test]
async fn a_dry_run_refuses_a_partially_applied_migration() {
    // sqlx marks a migration dirty (`success = false`) when it dies partway,
    // and `run` then refuses with "migration N is partially applied". A
    // pre-deploy check that reports a clean plan for a database the real run
    // cannot migrate is worse than none, so the dry run must refuse it too.
    let Some(db) = TempDb::create("dirty").await else {
        return;
    };
    let pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let first = MIGRATOR.iter().next().unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = $1")
        .bind(first.version)
        .execute(&pool)
        .await
        .unwrap();

    let scratch = SecretScratch::new("dirty-cfg");
    let Some(config_path) = migration_role_config(&db, &scratch) else {
        assert_ne!(
            std::env::var("POOL_REQUIRE_DB_TESTS").as_deref(),
            Ok("1"),
            "no pool_migration credential available, but POOL_REQUIRE_DB_TESTS=1"
        );
        return;
    };

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pool-admin"))
        .arg("--config")
        .arg(&config_path)
        .arg("migrate")
        .arg("--dry-run")
        .output()
        .expect("binary runs");

    assert!(
        !output.status.success(),
        "a dry run must refuse a partially applied migration"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("partially applied"),
        "the failure must name the cause, got: {combined}"
    );
}

#[tokio::test]
async fn concurrent_migrations_do_not_both_claim_the_work() {
    // `migrate.complete` is the auditable result of an owned mutation
    // (`architecture.md` §6, §13 invariant 6). Two invocations racing — a
    // retried deploy step, or two runners — must not both report having
    // applied the schema: exactly one did.
    //
    // Opportunistic by nature: it only exercises the bug when the two
    // processes actually interleave, which measured at roughly 1 run in 5
    // against the unfixed code. It never fails spuriously, but it is not the
    // guard — `migrate_serialises_on_its_operation_lock` below tests the
    // mechanism deterministically.
    let Some(db) = TempDb::create("concurrent").await else {
        return;
    };
    let scratch = SecretScratch::new("concurrent-cfg");
    let Some(config_path) = migration_role_config(&db, &scratch) else {
        assert_ne!(
            std::env::var("POOL_REQUIRE_DB_TESTS").as_deref(),
            Ok("1"),
            "no pool_migration credential available, but POOL_REQUIRE_DB_TESTS=1"
        );
        return;
    };

    // Started as close together as possible, against a database with
    // nothing applied.
    let spawn = || {
        let path = config_path.clone();
        std::thread::spawn(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_pool-admin"))
                .arg("--config")
                .arg(&path)
                .arg("migrate")
                .output()
                .expect("binary runs")
        })
    };
    let a = spawn();
    let b = spawn();
    let (a, b) = (a.join().unwrap(), b.join().unwrap());

    for (label, out) in [("a", &a), ("b", &b)] {
        assert!(
            out.status.success(),
            "invocation {label} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Sum the applied_now each process reported.
    let applied_now = |out: &std::process::Output| -> i64 {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        combined
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v.get("event").and_then(|e| e.as_str()) == Some("migrate.complete"))
            .and_then(|v| v.get("applied_now").and_then(|n| n.as_i64()))
            .unwrap_or_else(|| panic!("no migrate.complete event in output: {combined}"))
    };

    let total_claimed = applied_now(&a) + applied_now(&b);
    let actually_applied = MIGRATOR.iter().count() as i64;
    assert_eq!(
        total_claimed, actually_applied,
        "the two invocations together claimed {total_claimed} migrations but only \
         {actually_applied} exist; one of them reported the other's work"
    );
}

/// Must match `MIGRATE_LOCK_KEY` in `crates/pool-admin/src/main.rs`.
///
/// Drift is caught rather than silent: with a different key the migrate
/// process would not block below and the test fails.
const MIGRATE_LOCK_KEY: i64 = 0x706f_6f6c_6d69_6772;

#[tokio::test]
async fn migrate_serialises_on_its_operation_lock() {
    // The deterministic half of the concurrency guarantee. Rather than hope
    // two processes interleave, hold the operation lock and prove a migrate
    // invocation waits for it — which is what makes the before/after
    // snapshots bracket only this process's work.
    let Some(db) = TempDb::create("lockwait").await else {
        return;
    };
    let scratch = SecretScratch::new("lockwait-cfg");
    let Some(config_path) = migration_role_config(&db, &scratch) else {
        assert_ne!(
            std::env::var("POOL_REQUIRE_DB_TESTS").as_deref(),
            Ok("1"),
            "no pool_migration credential available, but POOL_REQUIRE_DB_TESTS=1"
        );
        return;
    };

    // Hold the lock on a connection the test owns.
    let mut holder = PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut holder)
        .await
        .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pool-admin"))
        .arg("--config")
        .arg(&config_path)
        .arg("migrate")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("binary starts");

    // It must still be waiting: the lock is held.
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(
        child.try_wait().unwrap().is_none(),
        "migrate did not wait for the operation lock"
    );

    // Release, and it should complete.
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut holder)
        .await
        .unwrap();

    let out = child.wait_with_output().expect("migrate finishes");
    assert!(
        out.status.success(),
        "migrate failed after the lock was released: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
