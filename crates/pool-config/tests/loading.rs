//! Slice-1 criteria A1–A3: fail-closed configuration loading.
//!
//! Each rejection case is exercised in isolation, so a change that loosens
//! one guard fails one named test rather than quietly widening what the
//! binaries accept.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pool_config::{Binary, Config, ConfigError};

/// A scratch directory with a populated password file, so cases that are
/// *not* about the password file all pass that check.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pool-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("db-password"), b"local-dev-only").unwrap();
        Self { dir }
    }

    fn password_file(&self) -> String {
        self.dir.join("db-password").display().to_string()
    }

    fn write(&self, toml: &str) -> PathBuf {
        let path = self.dir.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A configuration that loads cleanly, which each case then breaks in
/// exactly one way.
fn valid_toml(password_file: &str) -> String {
    format!(
        r#"
network = "testnet"

[database]
host = "127.0.0.1"
port = 5433
name = "pool_dev"
user = "pool_migration"
password_file = "{password_file}"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "test"
"#
    )
}

fn assert_invalid(err: ConfigError, expected_fragment: &str) {
    match err {
        ConfigError::Invalid { reason, .. } => assert!(
            reason.contains(expected_fragment),
            "reason {reason:?} does not mention {expected_fragment:?}"
        ),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn valid_config_loads() {
    let scratch = Scratch::new("valid");
    let path = scratch.write(&valid_toml(&scratch.password_file()));
    let config = Config::load(&path, Binary::PoolAdminMigrate).expect("valid config must load");
    assert_eq!(config.database.port, 5433);
    assert_eq!(config.telemetry.deployment, "test");
}

#[test]
fn unknown_field_is_rejected() {
    // A2/A1: a typo or a removed setting must stop startup, not be ignored.
    let scratch = Scratch::new("unknown");
    let toml = format!(
        "{}\nsurprise = true\n",
        valid_toml(&scratch.password_file())
    );
    let path = scratch.write(&toml);
    match Config::load(&path, Binary::PoolAdminMigrate) {
        Err(ConfigError::Parse { .. }) => {}
        other => panic!("expected Parse error, got {other:?}"),
    }
}

#[test]
fn unknown_field_in_nested_table_is_rejected() {
    let scratch = Scratch::new("unknown-nested");
    let toml =
        valid_toml(&scratch.password_file()).replace("[telemetry]", "[telemetry]\nsampling = 0.5");
    let path = scratch.write(&toml);
    match Config::load(&path, Binary::PoolAdminMigrate) {
        Err(ConfigError::Parse { .. }) => {}
        other => panic!("expected Parse error, got {other:?}"),
    }
}

#[test]
fn missing_network_is_rejected() {
    // `architecture.md` §9: network has no production default, so its
    // absence must be a parse failure rather than a silent fallback.
    let scratch = Scratch::new("no-network");
    let toml = valid_toml(&scratch.password_file()).replace("network = \"testnet\"\n", "");
    let path = scratch.write(&toml);
    match Config::load(&path, Binary::PoolAdminMigrate) {
        Err(ConfigError::Parse { .. }) => {}
        other => panic!("expected Parse error, got {other:?}"),
    }
}

#[test]
fn mainnet_is_rejected_in_this_build() {
    // `tig_integration.md` §2.1: failure to reach testnet must never
    // redirect to mainnet. Editing a config file cannot do it either.
    let scratch = Scratch::new("mainnet");
    let toml = valid_toml(&scratch.password_file())
        .replace("network = \"testnet\"", "network = \"mainnet\"");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("mainnet must be refused");
    assert_invalid(err, "must be exactly \"testnet\"");
}

#[test]
fn unrecognised_network_is_rejected() {
    let scratch = Scratch::new("bad-network");
    let toml = valid_toml(&scratch.password_file())
        .replace("network = \"testnet\"", "network = \"devnet\"");
    let path = scratch.write(&toml);
    match Config::load(&path, Binary::PoolAdminMigrate) {
        Err(ConfigError::Parse { .. }) => {}
        other => panic!("expected Parse error, got {other:?}"),
    }
}

#[test]
fn binary_must_match_its_database_role() {
    // A controller config pointed at the migration role would silently hand
    // it DDL (`architecture.md` §6, §7.1).
    let scratch = Scratch::new("role");
    let path = scratch.write(&valid_toml(&scratch.password_file()));
    let err = Config::load(&path, Binary::PoolController)
        .expect_err("controller must refuse the migration role");
    assert_invalid(err, "pool_controller");
}

#[test]
fn each_binary_accepts_only_its_own_role() {
    let scratch = Scratch::new("role-matrix");
    for (binary, role) in [
        (Binary::PoolAdminMigrate, "pool_migration"),
        (Binary::PoolController, "pool_controller"),
        (Binary::TigGateway, "pool_gateway"),
    ] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", &format!("user = \"{role}\""));
        // The controller's own required section. It has no default, so a
        // controller config without it does not load at all.
        if matches!(binary, Binary::PoolController) {
            toml.push_str(ORCHESTRATION);
        }
        let path = scratch.write(&toml);
        Config::load(&path, binary)
            .unwrap_or_else(|e| panic!("{} with role {role} must load: {e}", binary.as_str()));
    }
}

/// `mining_system.md` §11 makes `internal_pool_unverified_limit` versioned
/// policy with no settled value, so this is a test fixture and never a
/// production constant.
const ORCHESTRATION: &str = "\n[orchestration]\ninternal_pool_unverified_limit = 8\n";

#[test]
fn a_controller_without_an_unverified_limit_does_not_load() {
    // No default: a compiled fallback would be a policy value reached exactly
    // when the operator forgot to set one.
    let scratch = Scratch::new("no-limit");
    let toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolController)
        .expect_err("the controller has no default limit");
    assert_invalid(err, "internal_pool_unverified_limit");
}

#[test]
fn a_zero_unverified_limit_is_rejected() {
    let scratch = Scratch::new("zero-limit");
    let mut toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
    toml.push_str("\n[orchestration]\ninternal_pool_unverified_limit = 0\n");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolController).expect_err("0 admits nothing");
    assert_invalid(err, "at least 1");
}

#[test]
fn another_binary_may_not_carry_the_orchestration_policy() {
    // A gateway config naming an orchestration limit reads as though the
    // gateway enforced it, which `architecture.md` §3 says it does not.
    let scratch = Scratch::new("gateway-orchestration");
    let mut toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
    toml.push_str(ORCHESTRATION);
    let path = scratch.write(&toml);
    let err =
        Config::load(&path, Binary::TigGateway).expect_err("the gateway enforces no such limit");
    assert_invalid(err, "belongs to pool-controller");
}

#[test]
fn missing_password_file_is_rejected() {
    let scratch = Scratch::new("no-password");
    let toml = valid_toml("/nonexistent/db-password");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("missing password file");
    assert_invalid(err, "not readable");
}

#[test]
fn empty_password_file_is_rejected() {
    // An empty file is the shape a half-provisioned environment leaves
    // behind; catching it at startup beats failing on the first query.
    let scratch = Scratch::new("empty-password");
    std::fs::write(scratch.dir.join("db-password"), b"").unwrap();
    let path = scratch.write(&valid_toml(&scratch.password_file()));
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("empty password file");
    assert_invalid(err, "is empty");
}

#[test]
fn whitespace_only_password_file_is_rejected() {
    // A newline-only file is non-empty on disk but produces an empty
    // password, so a byte-length check would pass it and the failure would
    // surface as a confusing authentication error at first connect.
    let scratch = Scratch::new("whitespace-password");
    std::fs::write(scratch.dir.join("db-password"), b"\n  \n").unwrap();
    let path = scratch.write(&valid_toml(&scratch.password_file()));
    let err =
        Config::load(&path, Binary::PoolAdminMigrate).expect_err("whitespace-only password file");
    assert_invalid(err, "empty or whitespace only");
}

#[test]
fn zero_statement_timeout_is_rejected() {
    let scratch = Scratch::new("timeout");
    let toml = valid_toml(&scratch.password_file())
        .replace("statement_timeout_ms = 30000", "statement_timeout_ms = 0");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("zero timeout");
    assert_invalid(err, "statement_timeout_ms");
}

#[test]
fn zero_port_is_rejected() {
    let scratch = Scratch::new("port");
    let toml = valid_toml(&scratch.password_file()).replace("port = 5433", "port = 0");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("zero port");
    assert_invalid(err, "database.port");
}

#[test]
fn empty_deployment_is_rejected() {
    let scratch = Scratch::new("deployment");
    let toml = valid_toml(&scratch.password_file())
        .replace("deployment = \"test\"", "deployment = \"  \"");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("blank deployment");
    assert_invalid(err, "telemetry.deployment");
}

#[test]
fn missing_file_is_reported_as_a_read_error() {
    let err = Config::load(
        Path::new("/nonexistent/config.toml"),
        Binary::PoolAdminMigrate,
    )
    .expect_err("missing config file");
    match err {
        ConfigError::Read { .. } => {}
        other => panic!("expected Read, got {other:?}"),
    }
}

#[test]
fn decision_digest_is_stable_and_excludes_non_decision_fields() {
    // A3: the digest is stored with each decision, so it must not churn
    // when a log level or a database host changes — only when something
    // that could change what the pool decides changes.
    let scratch = Scratch::new("digest");
    let base = Config::load(
        scratch.write(&valid_toml(&scratch.password_file())),
        Binary::PoolAdminMigrate,
    )
    .unwrap();

    let retuned = valid_toml(&scratch.password_file())
        .replace("level = \"info\"", "level = \"debug\"")
        .replace("host = \"127.0.0.1\"", "host = \"db.internal\"")
        .replace("port = 5433", "port = 6000");
    let other = Config::load(scratch.write(&retuned), Binary::PoolAdminMigrate).unwrap();

    assert_eq!(
        base.decision_digest(),
        other.decision_digest(),
        "telemetry and connection details must not enter the decision digest"
    );
    assert_eq!(base.decision_digest_hex().len(), 64);
    // Recomputing must give the same answer; the digest is a pure function.
    assert_eq!(base.decision_digest(), base.decision_digest());
}

#[test]
fn database_url_is_built_from_the_password_file() {
    let scratch = Scratch::new("url");
    let config = Config::load(
        scratch.write(&valid_toml(&scratch.password_file())),
        Binary::PoolAdminMigrate,
    )
    .unwrap();
    let url = config.database_url().unwrap();
    assert!(url.starts_with("postgres://pool_migration:local-dev-only@127.0.0.1:5433/pool_dev"));
    // The declared statement timeout is actually applied to the session
    // rather than merely parsed and validated.
    assert!(
        url.contains("statement_timeout%3D30000"),
        "statement_timeout must reach the connection, got: {url}"
    );
}

#[test]
fn database_url_escapes_values_that_could_break_out() {
    // A password containing URL syntax must be treated as data. Unescaped,
    // `@` and `/` would re-point the client at a different host or database.
    let scratch = Scratch::new("url-escape");
    std::fs::write(scratch.dir.join("db-password"), b"p@ss:w/rd?#%").unwrap();
    let config = Config::load(
        scratch.write(&valid_toml(&scratch.password_file())),
        Binary::PoolAdminMigrate,
    )
    .unwrap();
    let url = config.database_url().unwrap();

    assert!(
        url.contains("p%40ss%3Aw%2Frd%3F%23%25"),
        "password must be percent-encoded, got: {url}"
    );
    // Exactly one `@` separates credentials from host.
    assert_eq!(
        url.matches('@').count(),
        1,
        "an escaped password must not introduce a second @: {url}"
    );
    assert!(
        url.contains("@127.0.0.1:5433/pool_dev"),
        "host and database must survive escaping intact: {url}"
    );
}

#[test]
fn config_debug_output_carries_no_password() {
    // The struct holds a *path*, never the secret, so even an accidental
    // `{:?}` of the whole config cannot leak it (`architecture.md` §2.2).
    let scratch = Scratch::new("debug");
    let config = Config::load(
        scratch.write(&valid_toml(&scratch.password_file())),
        Binary::PoolAdminMigrate,
    )
    .unwrap();
    let rendered = format!("{config:?}");
    assert!(
        !rendered.contains("local-dev-only"),
        "config Debug output must not contain the password"
    );
}

#[test]
fn the_connect_timeout_defaults_and_cannot_be_zero() {
    // A config that omits it still gets a bound — the point is that *some*
    // explicit value reaches the pool, because sqlx's own default is thirty
    // seconds of invisible retrying.
    let scratch = Scratch::new("connect-timeout");
    let path = scratch.write(&valid_toml(&scratch.password_file()));
    let config = Config::load(&path, Binary::PoolAdminMigrate).expect("loads");
    assert!(
        config.database.connect_timeout_ms > 0 && config.database.connect_timeout_ms < 30_000,
        "the default must be a real bound, tighter than the library's: {}",
        config.database.connect_timeout_ms
    );

    // Zero cannot succeed, so it would fail closed whatever the database was
    // doing — a configuration that looks like a timeout and behaves like an
    // outage.
    let toml = valid_toml(&scratch.password_file())
        .replace("statement_timeout_ms = 30000", "connect_timeout_ms = 0");
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolAdminMigrate).expect_err("zero is not a timeout");
    assert_invalid(err, "connect_timeout_ms must not be 0");
}
