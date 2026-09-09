//! Binary-level behaviour that needs no database: fail-closed startup
//! (criterion A1) and the §10.1 log contract (criterion I1).
//!
//! These drive the real binary rather than the library, because the thing
//! under test is what an operator actually sees on stdout.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_pool-admin");

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("pool-admin-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    /// A config that parses and validates but points at a port nothing is
    /// listening on, so `migrate` gets as far as logging and then fails to
    /// connect. That is enough to observe the log contract without needing
    /// a database or any real credential.
    fn unreachable_config(&self) -> PathBuf {
        let password_file = self.dir.join("db-password");
        std::fs::write(&password_file, b"not-a-real-credential").unwrap();
        let path = self.dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
network = "testnet"

[database]
host = "127.0.0.1"
port = 1
name = "pool_dev"
user = "pool_migration"
password_file = "{}"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "cli-test"
"#,
                password_file.display()
            ),
        )
        .unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn every_log_line_carries_service_deployment_and_network() {
    // `architecture.md` §10.1 requires these on *every* line, not on a
    // startup banner. Attaching them to one event and calling it done is
    // exactly the failure this test exists to catch: it also covers lines
    // emitted by dependencies, which no hand-written log call controls.
    let scratch = Scratch::new("logfields");
    let output = Command::new(BIN)
        .arg("--config")
        .arg(scratch.unreachable_config())
        .arg("migrate")
        .output()
        .expect("binary runs");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut json_lines = 0;
    for line in combined.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // non-JSON noise (e.g. a panic message) is not a log line
        };
        json_lines += 1;

        // Look through the whole span stack, not just the innermost span.
        // A nested span would otherwise hide the root's fields, and the
        // formatter is configured with `with_span_list(true)` precisely so
        // they survive that.
        let mut scopes: Vec<&serde_json::Value> = Vec::new();
        if let Some(current) = value.get("span") {
            scopes.push(current);
        }
        if let Some(list) = value.get("spans").and_then(|s| s.as_array()) {
            scopes.extend(list.iter());
        }
        assert!(
            !scopes.is_empty(),
            "log line carries no span at all: {line}"
        );
        for field in ["service", "deployment", "network"] {
            assert!(
                scopes.iter().any(|s| s.get(field).is_some()),
                "log line is missing {field} anywhere in its span stack \
                 (architecture.md §10.1): {line}"
            );
        }
        assert!(
            value.get("timestamp").is_some() && value.get("level").is_some(),
            "log line is missing timestamp/severity: {line}"
        );
    }

    assert!(
        json_lines >= 2,
        "expected the startup line plus at least one more, got {json_lines}"
    );
}

#[test]
fn an_unreachable_database_exits_non_zero_promptly() {
    // A1: fail closed. A migration job that cannot reach its database must
    // not report success.
    //
    // And must say so while someone is still watching. sqlx retries a refused
    // connection until its pool acquire timeout, whose default is thirty
    // seconds, so this took thirty seconds before `connect_timeout_ms`
    // existed — an in-process retry loop nobody chose. Whether to retry is
    // the invoking deploy step's decision and it cannot make it until this
    // process returns.
    //
    // The bound is loose on purpose: it is here to catch a silent return to
    // the library default, not to measure the timeout. A slow machine has
    // room; thirty seconds does not fit.
    let scratch = Scratch::new("unreachable");
    let started = std::time::Instant::now();
    let output = Command::new(BIN)
        .arg("--config")
        .arg(scratch.unreachable_config())
        .arg("migrate")
        .output()
        .expect("binary runs");
    let elapsed = started.elapsed();
    assert!(
        !output.status.success(),
        "migrate must exit non-zero when the database is unreachable"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "took {elapsed:?}: the acquire timeout is not being applied"
    );
}

#[test]
fn a_missing_config_file_exits_non_zero() {
    let output = Command::new(BIN)
        .arg("--config")
        .arg("/nonexistent/config.toml")
        .arg("migrate")
        .output()
        .expect("binary runs");
    assert!(!output.status.success());
}

#[test]
fn no_log_line_contains_the_configured_password() {
    // A4 at the binary level: the connection string is built from the
    // password file and handed to the driver, and must not reach a log even
    // on the connection-failure path.
    let scratch = Scratch::new("nopassword");
    let output = Command::new(BIN)
        .arg("--config")
        .arg(scratch.unreachable_config())
        .arg("migrate")
        .output()
        .expect("binary runs");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("not-a-real-credential"),
        "the password must never appear in output"
    );
}
