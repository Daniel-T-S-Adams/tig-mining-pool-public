//! Slice-1 criteria D1 and D1a: TIG write-intent idempotency, proven against
//! a real PostgreSQL 18.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these tests skip, so a
//! developer without a database still gets a meaningful `make check`. CI
//! sets the URL and `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure.
//!
//! The throwaway-database harness is a trimmed copy of the one in
//! `pool-admin/tests/migrate.rs` and `pool-snapshot/tests/store.rs`. Three
//! copies is one too many; the next crate that needs it should extract a
//! shared test-support crate rather than copy it again.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use pool_domain::Network;
use pool_workflow::{
    IntentError, IntentState, NewIntent, PostgresIntentRepository, TigWriteIntentRepository,
    WriteKind,
};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool, Row};

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

const PROVISION_ROLES_SQL: &str = include_str!("../../../scripts/provision-db-roles.sql");

/// Serialises the cluster-wide role creation these tests share.
const PROVISION_LOCK: i64 = 0x7069_6f6f_6c5f_726f;

async fn exec(conn: &mut PgConnection, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

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
    async fn migrated(label: &str) -> Option<Self> {
        let superuser_url = superuser_url()?;
        let label: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let name = format!("pool_intent_{}_{label}", std::process::id());

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

        exec(&mut conn, PROVISION_ROLES_SQL.to_string())
            .await
            .unwrap();
        exec(
            &mut conn,
            format!(
                "GRANT CONNECT ON DATABASE {} TO pool_controller, pool_gateway, pool_readonly;",
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

    fn as_role(&self, role: &str) -> PgConnectOptions {
        self.as_superuser().options([("role", role)])
    }

    async fn pool_as(&self, role: &str) -> PgPool {
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

fn precommit(workflow: &str, generation: i32, digest: u8) -> NewIntent {
    NewIntent {
        network: Network::Testnet,
        workflow_id: workflow.to_string(),
        write_kind: WriteKind::Precommit,
        generation,
        benchmark_id: None,
        payload_digest: [digest; 32],
        payload_artifact_id: None,
    }
}

fn benchmark(workflow: &str, generation: i32, benchmark_id: &str, digest: u8) -> NewIntent {
    NewIntent {
        network: Network::Testnet,
        workflow_id: workflow.to_string(),
        write_kind: WriteKind::Benchmark,
        generation,
        benchmark_id: Some(benchmark_id.to_string()),
        payload_digest: [digest; 32],
        payload_artifact_id: None,
    }
}

#[tokio::test]
async fn concurrent_duplicate_intents_leave_exactly_one_row() {
    // D1, the constraint's whole purpose: sixteen tasks race to create the
    // same intent and the database, not the code, decides that one wins.
    let Some(db) = TempDb::migrated("concurrent").await else {
        return;
    };
    let repo = Arc::new(PostgresIntentRepository::new(
        db.pool_as("pool_controller").await,
    ));

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let repo = Arc::clone(&repo);
        tasks.push(tokio::spawn(async move {
            repo.create(precommit("w1", 1, 0xab)).await
        }));
    }

    let mut intent_ids = Vec::new();
    for task in tasks {
        let intent = task
            .await
            .unwrap()
            .expect("every racer sees the same intent");
        intent_ids.push(intent.intent_id);
    }

    // Every caller gets the same row back, not just one winner and fifteen
    // errors: an intent that already exists with the identical payload is
    // the crash-retry path, and failing it would turn a retry into an
    // incident.
    intent_ids.dedup();
    assert_eq!(intent_ids.len(), 1, "all racers must see one intent");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&db.pool_as("pool_controller").await)
        .await
        .unwrap();
    assert_eq!(rows, 1, "exactly one row survives");
}

#[tokio::test]
async fn the_same_key_with_a_different_payload_is_refused() {
    // §7.3: changing a canonical payload requires an explicit new
    // generation. Overwriting, or silently keeping the old row, would both
    // leave the caller believing it recorded something it did not.
    let Some(db) = TempDb::migrated("payload").await else {
        return;
    };
    let repo = PostgresIntentRepository::new(db.pool_as("pool_controller").await);

    repo.create(precommit("w1", 1, 0xab)).await.unwrap();
    match repo.create(precommit("w1", 1, 0xcd)).await {
        Err(IntentError::PayloadConflict { generation, .. }) => assert_eq!(generation, 1),
        other => panic!("expected PayloadConflict, got {other:?}"),
    }

    // The original is untouched.
    let stored = repo
        .find(Network::Testnet, "w1", WriteKind::Precommit, 1)
        .await
        .unwrap()
        .expect("the first intent is still there");
    assert_eq!(stored.payload_digest, [0xab; 32]);
}

#[tokio::test]
async fn a_new_generation_is_how_a_payload_changes() {
    let Some(db) = TempDb::migrated("generation").await else {
        return;
    };
    let repo = PostgresIntentRepository::new(db.pool_as("pool_controller").await);

    let first = repo.create(precommit("w1", 1, 0xab)).await.unwrap();
    let second = repo.create(precommit("w1", 2, 0xcd)).await.unwrap();
    assert_ne!(first.intent_id, second.intent_id);
    assert_eq!(first.state, IntentState::Prepared);
}

#[tokio::test]
async fn a_generation_cannot_be_reused_across_a_different_benchmark() {
    // D1a. The generation is bound to the TIG benchmark_id, so the same
    // generation naming a different benchmark is refused rather than
    // treated as the same intent — which is how a benchmark write could
    // otherwise be silently repointed.
    let Some(db) = TempDb::migrated("binding").await else {
        return;
    };
    let repo = PostgresIntentRepository::new(db.pool_as("pool_controller").await);

    repo.create(benchmark("w1", 1, "bench-a", 0xab))
        .await
        .unwrap();

    // Same key, same payload, different benchmark.
    match repo.create(benchmark("w1", 1, "bench-b", 0xab)).await {
        Err(IntentError::PayloadConflict { .. }) => {}
        other => panic!("expected PayloadConflict for a different benchmark, got {other:?}"),
    }

    // And re-creating the identical one is still idempotent.
    let again = repo
        .create(benchmark("w1", 1, "bench-a", 0xab))
        .await
        .expect("identical benchmark intent is idempotent");
    assert_eq!(again.benchmark_id.as_deref(), Some("bench-a"));
}

#[tokio::test]
async fn the_benchmark_binding_is_required_and_refused_by_kind() {
    let Some(db) = TempDb::migrated("kinds").await else {
        return;
    };
    let repo = PostgresIntentRepository::new(db.pool_as("pool_controller").await);

    // Both benchmark-bound kinds, not just one: §7.3 binds Proof exactly as
    // it binds Benchmark, and covering one would leave the other free to be
    // recorded without saying which benchmark it belongs to.
    for kind in [WriteKind::Benchmark, WriteKind::Proof] {
        let mut missing = benchmark("w1", 1, "bench-a", 0xab);
        missing.write_kind = kind;
        missing.benchmark_id = None;
        match repo.create(missing).await {
            Err(IntentError::BenchmarkBinding { write_kind, .. }) => {
                assert_eq!(write_kind, kind.as_str())
            }
            other => panic!("expected BenchmarkBinding for {kind:?}, got {other:?}"),
        }
    }

    let mut spurious = precommit("w1", 1, 0xab);
    spurious.benchmark_id = Some("bench-a".to_string());
    match repo.create(spurious).await {
        Err(IntentError::BenchmarkBinding { write_kind, .. }) => {
            assert_eq!(write_kind, "precommit")
        }
        other => panic!("expected BenchmarkBinding, got {other:?}"),
    }
}

#[tokio::test]
async fn the_schema_refuses_the_binding_even_without_the_repository() {
    // The repository's own check reports a caller's mistake clearly, but the
    // constraint is what enforces §7.3 against ANY writer — including
    // whatever writes to this table next. Asserted by going around the
    // repository entirely.
    let Some(db) = TempDb::migrated("constraint").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let bare = sqlx::query(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, benchmark_id, payload_digest)
         VALUES ('testnet', 'w1', 'benchmark', 1, NULL, decode(repeat('ab', 32), 'hex'))",
    )
    .execute(&pool)
    .await;
    assert!(
        bare.is_err(),
        "a benchmark intent without a benchmark_id must be refused by the schema"
    );

    let duplicate_setup = sqlx::query(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest)
         VALUES ('testnet', 'w2', 'precommit', 1, decode(repeat('ab', 32), 'hex'))",
    )
    .execute(&pool)
    .await;
    assert!(duplicate_setup.is_ok());

    let duplicate = sqlx::query(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest)
         VALUES ('testnet', 'w2', 'precommit', 1, decode(repeat('cd', 32), 'hex'))",
    )
    .execute(&pool)
    .await;
    assert!(
        duplicate.is_err(),
        "the §7.3 unique constraint must refuse a second row for one generation"
    );
}

#[tokio::test]
async fn identity_and_payload_cannot_be_edited() {
    // §7.3 calls the intent immutable, but state has to change — the gateway
    // records an ambiguous outcome and confirmation arrives later from reads
    // — so UPDATE cannot simply be withheld. The trigger is what keeps
    // "immutable" true for everything else.
    let Some(db) = TempDb::migrated("immutable").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    let repo = PostgresIntentRepository::new(pool.clone());
    repo.create(precommit("w1", 1, 0xab)).await.unwrap();

    // The state transition §7.3 needs is allowed.
    let state_change = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'OUTCOME_UNKNOWN' WHERE workflow_id = 'w1'",
    )
    .execute(&pool)
    .await;
    assert!(
        state_change.is_ok(),
        "state must be updatable: {state_change:?}"
    );

    for (column, value) in [
        ("payload_digest", "decode(repeat('cd', 32), 'hex')"),
        ("payload_artifact_id", "'somewhere-else'"),
        ("generation", "2"),
        ("workflow_id", "'w2'"),
        ("write_kind", "'proof'"),
    ] {
        let sql =
            format!("UPDATE pool.tig_write_intent SET {column} = {value} WHERE workflow_id = 'w1'");
        let result = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&pool).await;
        assert!(
            result.is_err(),
            "{column} must be immutable (architecture.md §7.3)"
        );
    }

    let stored = repo
        .find(Network::Testnet, "w1", WriteKind::Precommit, 1)
        .await
        .unwrap()
        .expect("still there");
    assert_eq!(stored.payload_digest, [0xab; 32]);
    assert_eq!(stored.state, IntentState::OutcomeUnknown);
}

#[tokio::test]
async fn the_gateway_records_outcomes_but_never_invents_a_write() {
    // architecture.md §7.3: the gateway transmits what already exists and
    // records the outcome. It holds UPDATE without INSERT, so a transmission
    // path cannot create a write nobody decided on.
    let Some(db) = TempDb::migrated("gateway").await else {
        return;
    };
    PostgresIntentRepository::new(db.pool_as("pool_controller").await)
        .create(precommit("w1", 1, 0xab))
        .await
        .unwrap();

    let gateway = db.pool_as("pool_gateway").await;
    let update = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'OUTCOME_UNKNOWN' WHERE workflow_id = 'w1'",
    )
    .execute(&gateway)
    .await;
    assert!(update.is_ok(), "the gateway records outcomes: {update:?}");

    // ...but only the ambiguous outcome. architecture.md §6 assigns
    // confirmation to the controller's reconciler, and tig_integration.md §7
    // makes a transport status no evidence at all — so the component that
    // received the response must not be able to mark its own write
    // confirmed.
    for state in ["CONFIRMED", "REJECTED", "PREPARED"] {
        let sql =
            format!("UPDATE pool.tig_write_intent SET state = '{state}' WHERE workflow_id = 'w1'");
        let refused = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&gateway).await;
        assert!(
            refused.is_err(),
            "the gateway must not set {state} (architecture.md §6)"
        );
    }

    // The controller's reconciler may, because it acts on confirmed reads.
    let controller = db.pool_as("pool_controller").await;
    let confirmed = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'CONFIRMED' WHERE workflow_id = 'w1'",
    )
    .execute(&controller)
    .await;
    assert!(
        confirmed.is_ok(),
        "the controller confirms from reads: {confirmed:?}"
    );

    let insert = sqlx::query(
        "INSERT INTO pool.tig_write_intent
             (network, workflow_id, write_kind, generation, payload_digest)
         VALUES ('testnet', 'w9', 'precommit', 1, decode(repeat('ab', 32), 'hex'))",
    )
    .execute(&gateway)
    .await
    .expect_err("the gateway must not create intents");
    assert_eq!(
        insert.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501"),
        "must be refused for lack of privilege, got: {insert}"
    );

    let delete = sqlx::query("DELETE FROM pool.tig_write_intent")
        .execute(&gateway)
        .await
        .expect_err("an intent is the record that a write was decided on");
    assert_eq!(
        delete.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501")
    );

    // And it cannot retract the confirmation. The ambiguity path firing on a
    // slow response for an intent the reconciler already confirmed would
    // otherwise drive a settled write back to ambiguous, re-entering §10
    // reconciliation and paging §10.3 for a write that in fact confirmed.
    let retract = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'OUTCOME_UNKNOWN' WHERE workflow_id = 'w1'",
    )
    .execute(&gateway)
    .await;
    assert!(
        retract.is_err(),
        "CONFIRMED is terminal; the gateway must not retract it"
    );

    // Not even the controller may re-arm a settled write: that is how a
    // restart silently duplicates a TIG write (§13 invariant 14).
    let rearm =
        sqlx::query("UPDATE pool.tig_write_intent SET state = 'PREPARED' WHERE workflow_id = 'w1'")
            .execute(&controller)
            .await;
    assert!(rearm.is_err(), "nothing returns to PREPARED");

    let still: String =
        sqlx::query_scalar("SELECT state FROM pool.tig_write_intent WHERE workflow_id = 'w1'")
            .fetch_one(&controller)
            .await
            .unwrap();
    assert_eq!(still, "CONFIRMED");
}

#[tokio::test]
async fn each_transition_guard_is_load_bearing_on_its_own() {
    // The three guards overlap on the obvious cases — a gateway retraction
    // of CONFIRMED is refused by the gateway clause AND by terminality AND,
    // if it targeted PREPARED, by the no-return rule. Tested only through
    // those, any two could be deleted and the suite would stay green. Each
    // case below is reachable by exactly one guard.
    let Some(db) = TempDb::migrated("guards").await else {
        return;
    };
    let controller = db.pool_as("pool_controller").await;
    let repo = PostgresIntentRepository::new(controller.clone());

    // Only terminality: the controller, moving CONFIRMED to a non-PREPARED
    // state. §7 makes confirmation the product of confirmed reads and §10
    // step 4 requires local state to advance monotonically.
    repo.create(precommit("terminal", 1, 0xab)).await.unwrap();
    sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'CONFIRMED' WHERE workflow_id = 'terminal'",
    )
    .execute(&controller)
    .await
    .unwrap();
    let retract = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'OUTCOME_UNKNOWN' WHERE workflow_id = 'terminal'",
    )
    .execute(&controller)
    .await;
    assert!(
        retract.is_err(),
        "CONFIRMED is terminal even for the controller"
    );

    // Only the no-return rule: OUTCOME_UNKNOWN back to PREPARED, by the
    // controller. Nothing is terminal here and the gateway is not involved.
    repo.create(precommit("rearm", 1, 0xab)).await.unwrap();
    sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'OUTCOME_UNKNOWN' WHERE workflow_id = 'rearm'",
    )
    .execute(&controller)
    .await
    .unwrap();
    let rearm = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'PREPARED' WHERE workflow_id = 'rearm'",
    )
    .execute(&controller)
    .await;
    assert!(
        rearm.is_err(),
        "re-arming an attempted write is how a restart duplicates it silently"
    );

    // Only the gateway clause: PREPARED straight to CONFIRMED. Nothing is
    // terminal and PREPARED is not the target.
    repo.create(precommit("author", 1, 0xab)).await.unwrap();
    let gateway = db.pool_as("pool_gateway").await;
    let confirm = sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'CONFIRMED' WHERE workflow_id = 'author'",
    )
    .execute(&gateway)
    .await;
    assert!(
        confirm.is_err(),
        "the gateway must not confirm from a transport response"
    );
    // The same transition IS allowed for the controller, so the case is
    // about who, not about the transition being illegal in general.
    sqlx::query(
        "UPDATE pool.tig_write_intent SET state = 'CONFIRMED' WHERE workflow_id = 'author'",
    )
    .execute(&controller)
    .await
    .expect("the controller confirms from reads");
}

#[tokio::test]
async fn a_different_artifact_pointer_is_a_conflict_not_a_silent_discard() {
    // §7.3 lists the artifact pointer as part of the intent and the trigger
    // makes it immutable, so a caller supplying a different one must be told
    // rather than handed back the stored intent with its pointer dropped.
    let Some(db) = TempDb::migrated("artifact").await else {
        return;
    };
    let repo = PostgresIntentRepository::new(db.pool_as("pool_controller").await);

    let mut first = precommit("w1", 1, 0xab);
    first.payload_artifact_id = Some("artifact-a".to_string());
    repo.create(first).await.unwrap();

    let mut changed = precommit("w1", 1, 0xab);
    changed.payload_artifact_id = Some("artifact-b".to_string());
    match repo.create(changed).await {
        Err(IntentError::PayloadConflict { .. }) => {}
        other => panic!("expected PayloadConflict for a changed artifact, got {other:?}"),
    }

    // Newly supplying one where there was none is the same discard.
    let mut added = precommit("w2", 1, 0xab);
    added.payload_artifact_id = None;
    repo.create(added).await.unwrap();
    let mut now_set = precommit("w2", 1, 0xab);
    now_set.payload_artifact_id = Some("artifact-c".to_string());
    match repo.create(now_set).await {
        Err(IntentError::PayloadConflict { .. }) => {}
        other => panic!("expected PayloadConflict for an added artifact, got {other:?}"),
    }
}

#[tokio::test]
async fn networks_do_not_share_a_generation_space() {
    let Some(db) = TempDb::migrated("networks").await else {
        return;
    };
    let repo = PostgresIntentRepository::new(db.pool_as("pool_controller").await);

    repo.create(precommit("w1", 1, 0xab)).await.unwrap();
    let mut mainnet = precommit("w1", 1, 0xcd);
    mainnet.network = Network::Mainnet;
    repo.create(mainnet)
        .await
        .expect("a different network is a different intent");

    let rows: i64 = sqlx::query("SELECT count(*) FROM pool.tig_write_intent")
        .fetch_one(&db.pool_as("pool_controller").await)
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 2);
}
