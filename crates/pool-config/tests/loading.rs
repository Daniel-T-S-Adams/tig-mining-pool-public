//! Slice-1 criteria A1–A3: fail-closed configuration loading.
//!
//! Each rejection case is exercised in isolation, so a change that loosens
//! one guard fails one named test rather than quietly widening what the
//! binaries accept.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pool_config::{
    AUDIT_DEPLOYMENT_MAX_BYTES, Binary, Config, ConfigError, LARGEST_CONFORMING_CONTROL_BODY_BYTES,
    LARGEST_PROTOCOL_BODY_BYTES,
};

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
        // Both TIG-facing binaries require an endpoint; the migration job
        // must not carry one.
        if matches!(binary, Binary::PoolController | Binary::TigGateway) {
            toml.push_str(TIG);
        }
        // The gateway's own required section, and only its own: a controller
        // carrying it would read as though the controller held the API key.
        if matches!(binary, Binary::TigGateway) {
            toml.push_str(GATEWAY);
        }
        let path = scratch.write(&toml);
        Config::load(&path, binary)
            .unwrap_or_else(|e| panic!("{} with role {role} must load: {e}", binary.as_str()));
    }
}

/// `mining_system.md` §11 makes `internal_pool_unverified_limit` versioned
/// policy with no settled value, so this is a test fixture and never a
/// production constant.
const ORCHESTRATION: &str = "\n[orchestration]\ninternal_pool_unverified_limit = 8\nactive_cache_fetches_per_poll = 20\nprecommit_failure_charge_atoms = \"1000\"\nreserve_policy_version = \"unset-slice-1\"\n";

/// Gateway-only: the section carries the API key path, so no other binary may
/// present it (`architecture.md` §2.2). `served_compute` is explicitly empty
/// because the pool serves no compute of its own in slice 1
/// (`tig_integration.md` §13.5), and an absent list would be indistinguishable
/// from an unanswered question.
///
/// `lease_secs` outlasts the pinned 60s write call timeout, which `tig-gateway`
/// requires at startup (§7.5). `pool-config` cannot check that — the timeout is
/// pinned in `tig-gateway`, not configured — so a fixture that looked valid
/// here but that no gateway would start on would teach a reader the wrong
/// shape.
const GATEWAY: &str = "\n[gateway]\napi_key_file = \"/dev/null\"\nlease_secs = 120\nplatform = \"linux/arm64\"\nserved_compute = []\n";

/// `pool-api`-only: `architecture.md` §3 gives member traffic to one process.
const MEMBER_API: &str = "\n[member_api]\nlisten = \"127.0.0.1:8081\"\nticket_hmac_key_file = \"/dev/null\"\nmax_control_body_bytes = 524288\n";

/// The endpoint the controller and gateway both require. A local `fake-tig`
/// here, which is also what F4d's guard reads.
const TIG: &str = "\n[tig]\nbase_url = \"http://127.0.0.1:8080\"\nplayer_id = \"0x2935a721068da756b28cba896efdb64e8909dfae\"\nacquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n";

#[test]
fn the_reserve_policy_values_are_checked_rather_than_carried() {
    // Criterion D2c records `X` and its policy version with every intent, so
    // a value PostgreSQL would silently reshape — `1.5` rounding, `1e3`
    // expanding, a negative passing a `>= 0` column check after the cast — is
    // a reserve input the record could not be re-derived from
    // (`accounting.md` §3).
    let scratch = Scratch::new("reserve-policy");
    let load = |orchestration: &str| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
        toml.push_str(TIG);
        toml.push_str(orchestration);
        Config::load(scratch.write(&toml), Binary::PoolController)
    };
    let with = |charge: &str, version: &str| {
        format!(
            "\n[orchestration]\ninternal_pool_unverified_limit = 8\n\
             active_cache_fetches_per_poll = 20\n\
             precommit_failure_charge_atoms = \"{charge}\"\n\
             reserve_policy_version = \"{version}\"\n"
        )
    };

    // The two that are allowed: a real amount, and zero.
    assert!(load(&with("1000", "v1")).is_ok());
    assert!(
        load(&with("0", "unchosen-pre-build-5.2")).is_ok(),
        "zero is a value; `X` being unchosen is what the version names"
    );

    for (charge, why) in [
        ("", "empty"),
        ("1.5", "a decimal point"),
        ("1e3", "an exponent"),
        ("-1", "a sign"),
        ("0100", "a redundant leading zero"),
        ("1 000", "a separator"),
        ("NaN", "not a number at all"),
    ] {
        let err = load(&with(charge, "v1")).expect_err(why);
        assert_invalid(err, "canonical unsigned base-10 atom string");
    }

    // A version is what makes the amount re-derivable once `X` moves.
    for version in ["", "   "] {
        let err = load(&with("1000", version)).expect_err("a version is required");
        assert_invalid(err, "reserve_policy_version must name the policy version");
    }
}

#[test]
fn the_offer_the_pool_decides_for_enters_the_digest() {
    // A3, and the digest's own rule that a decision-affecting field joins it
    // unless the decision record already shows the field. `compute_class`
    // selects §6.2's eligible set and `cpu_cores` drives §6.7's alignment —
    // and while the resulting `num_bundles` reaches the record, the core count
    // that produced it does not, so two decisions taken under different
    // offered cores would otherwise be indistinguishable.
    let scratch = Scratch::new("digest-offer");
    let controller = |offer: Option<&str>| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
        toml.push_str(TIG);
        toml.push_str(ORCHESTRATION);
        if let Some(offer) = offer {
            toml.push_str(offer);
        }
        Config::load(scratch.write(&toml), Binary::PoolController).unwrap()
    };
    let offer = |class: &str, cores: &str, compute_type: &str| {
        format!(
            "\n[orchestration.bootstrap_offer]\ncompute_class = \"{class}\"\n\
             {cores}tig_compute_type = \"{compute_type}\"\n"
        )
    };

    let none = controller(None);
    let eight = controller(Some(&offer("cpu", "cpu_cores = 8\n", "aws_t4g")));
    let sixteen = controller(Some(&offer("cpu", "cpu_cores = 16\n", "aws_t4g")));
    let gpu = controller(Some(&offer("gpu", "", "aws_g4dn")));
    let other_type = controller(Some(&offer("cpu", "cpu_cores = 8\n", "aws_c7g")));

    // Each differs from `eight` in exactly one field, and each must differ in
    // the digest.
    for (other, which) in [
        (&none, "no offer at all"),
        (&sixteen, "a different core count"),
        (&gpu, "a different class"),
        (&other_type, "a different protocol compute type"),
    ] {
        assert_ne!(
            eight.decision_digest(),
            other.decision_digest(),
            "{which} must not carry the same digest"
        );
    }

    // A deployment that offers nothing is not one that offers a nameless
    // class: the absent case is its own value, not empty fields.
    assert_ne!(none.decision_digest(), gpu.decision_digest());
    // And it is still a pure function of the configuration.
    assert_eq!(eight.decision_digest(), eight.decision_digest());
    assert_eq!(
        eight.decision_digest(),
        controller(Some(&offer("cpu", "cpu_cores = 8\n", "aws_t4g"))).decision_digest()
    );
}

#[test]
fn a_bootstrap_offer_is_refused_unless_it_can_actually_be_sized() {
    // §6.7 aligns CPU bundle counts to the core count, so a cpu offer without
    // one cannot be sized and a count of zero cannot be divided by. A gpu
    // offer carrying cores would have them read as a cpu offer's, and §6.7
    // gives gpu the minimum regardless.
    //
    // The offer itself is optional: absent means the deployment decides
    // nothing, which is slice 1's ordinary posture.
    let scratch = Scratch::new("bootstrap-offer");
    let load = |offer: Option<&str>| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
        toml.push_str(TIG);
        toml.push_str(ORCHESTRATION);
        if let Some(offer) = offer {
            toml.push_str(offer);
        }
        Config::load(scratch.write(&toml), Binary::PoolController)
    };

    let config = load(None).expect("no offer is a valid configuration");
    assert!(
        config
            .orchestration
            .as_ref()
            .expect("the section is there")
            .bootstrap_offer
            .is_none(),
        "absent means this deployment decides nothing"
    );

    assert!(
        load(Some(
            "\n[orchestration.bootstrap_offer]\ncompute_class = \"cpu\"\n\
             cpu_cores = 8\ntig_compute_type = \"aws_t4g\"\n"
        ))
        .is_ok()
    );
    assert!(
        load(Some(
            "\n[orchestration.bootstrap_offer]\ncompute_class = \"gpu\"\n\
             tig_compute_type = \"aws_g4dn\"\n"
        ))
        .is_ok(),
        "a gpu offer needs no core count"
    );

    let err = load(Some(
        "\n[orchestration.bootstrap_offer]\ncompute_class = \"cpu\"\n\
         tig_compute_type = \"aws_t4g\"\n",
    ))
    .expect_err("a cpu offer needs cores");
    assert_invalid(err, "cpu_cores is required for a cpu offer");

    let err = load(Some(
        "\n[orchestration.bootstrap_offer]\ncompute_class = \"cpu\"\n\
         cpu_cores = 0\ntig_compute_type = \"aws_t4g\"\n",
    ))
    .expect_err("zero cores cannot be divided by");
    assert_invalid(err, "cpu_cores must be at least 1");

    let err = load(Some(
        "\n[orchestration.bootstrap_offer]\ncompute_class = \"gpu\"\n\
         cpu_cores = 8\ntig_compute_type = \"aws_g4dn\"\n",
    ))
    .expect_err("a gpu offer has no cores");
    assert_invalid(err, "cpu_cores belongs to a cpu offer");

    let err = load(Some(
        "\n[orchestration.bootstrap_offer]\ncompute_class = \"quantum\"\n\
         tig_compute_type = \"aws_t4g\"\n",
    ))
    .expect_err("an unknown class");
    assert_invalid(err, "compute_class must be");

    // The class and the protocol type are different vocabularies. An empty
    // type would send nothing where TIG expects `aws_t4g`.
    let err = load(Some(
        "\n[orchestration.bootstrap_offer]\ncompute_class = \"cpu\"\n\
         cpu_cores = 8\ntig_compute_type = \"\"\n",
    ))
    .expect_err("an empty compute type");
    assert_invalid(err, "tig_compute_type must name");
}

#[test]
fn a_controller_without_an_unverified_limit_does_not_load() {
    // No default: a compiled fallback would be a policy value reached exactly
    // when the operator forgot to set one.
    let scratch = Scratch::new("no-limit");
    let mut toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
    toml.push_str(TIG);
    toml.push_str(GATEWAY);
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
    toml.push_str(TIG);
    toml.push_str(GATEWAY);
    toml.push_str(
        "\n[orchestration]\ninternal_pool_unverified_limit = 0\nactive_cache_fetches_per_poll = 20\nprecommit_failure_charge_atoms = \"1000\"\nreserve_policy_version = \"unset-slice-1\"\n",
    );
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolController).expect_err("0 admits nothing");
    assert_invalid(err, "at least 1");
}

#[test]
fn a_zero_cache_fetch_budget_is_rejected() {
    // With no fetches the active-benchmark cache never warms, so no
    // snapshot ever becomes usable for a decision — a controller that
    // silently decides nothing forever.
    let scratch = Scratch::new("zero-fetches");
    let mut toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_controller\"");
    toml.push_str(TIG);
    toml.push_str(GATEWAY);
    toml.push_str(
        "\n[orchestration]\ninternal_pool_unverified_limit = 8\nactive_cache_fetches_per_poll = 0\nprecommit_failure_charge_atoms = \"1000\"\nreserve_policy_version = \"unset-slice-1\"\n",
    );
    let path = scratch.write(&toml);
    let err = Config::load(&path, Binary::PoolController).expect_err("0 never warms");
    assert_invalid(err, "active_cache_fetches_per_poll");
}

#[test]
fn another_binary_may_not_carry_the_orchestration_policy() {
    // A gateway config naming an orchestration limit reads as though the
    // gateway enforced it, which `architecture.md` §3 says it does not.
    let scratch = Scratch::new("gateway-orchestration");
    let mut toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
    toml.push_str(TIG);
    toml.push_str(GATEWAY);
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
fn a_deployment_too_long_for_an_audit_row_is_rejected() {
    // Every `pool.audit_event` row carries this name, and that column is 64
    // characters. A longer one loads cleanly and then fails every audit
    // INSERT, which turns `member_protocol.md` §3.2's "rejected and audited"
    // into rejected-and-logged without anything saying so.
    let scratch = Scratch::new("deployment-length");
    let load = |deployment: &str| {
        let toml = valid_toml(&scratch.password_file()).replace(
            "deployment = \"test\"",
            &format!("deployment = \"{deployment}\""),
        );
        Config::load(scratch.write(&toml), Binary::PoolAdminMigrate)
    };

    let Err(err) = load(&"n".repeat(AUDIT_DEPLOYMENT_MAX_BYTES + 1)) else {
        panic!("a deployment longer than an audit row must not load");
    };
    assert_invalid(err, "telemetry.deployment must be 1 to 64 bytes");

    // The boundary itself loads, so the rejection is a bound rather than a
    // check that refuses every long name.
    load(&"n".repeat(AUDIT_DEPLOYMENT_MAX_BYTES)).expect("64 bytes is the limit, not past it");
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

    // And it must change when something that changes the write changes.
    // `player_id` is a field of every §6.1 body, so two decisions taken under
    // different identities must not carry the same config digest — while the
    // endpoint, which changes nothing about what is decided, must not enter.
    let gateway = |player: &str, base_url: &str| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!(
            "\n[tig]\nbase_url = \"{base_url}\"\nplayer_id = \"{player}\"\nacquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n"
        ));
        toml.push_str(GATEWAY);
        Config::load(scratch.write(&toml), Binary::TigGateway).unwrap()
    };
    let a = gateway(
        "0x2935a721068da756b28cba896efdb64e8909dfae",
        "http://127.0.0.1:8080",
    );
    let b = gateway(
        "0x0000000000000000000000000000000000000001",
        "http://127.0.0.1:8080",
    );
    let c = gateway(
        "0x2935a721068da756b28cba896efdb64e8909dfae",
        "https://testnet-api.tig.foundation",
    );
    assert_ne!(
        a.decision_digest(),
        b.decision_digest(),
        "a different pool identity is a different decision context"
    );
    assert_eq!(
        a.decision_digest(),
        c.decision_digest(),
        "which server the pool talks to does not change what it decides"
    );
}

#[test]
fn a_player_id_that_is_not_a_lowercase_address_does_not_load() {
    // §6.1 renders `player_id` as "<lowercase pool address>", and `reconcile`
    // compares it byte for byte against what TIG returns. A value TIG would
    // normalise differently would put the pool's own confirmed precommit out
    // of reach of its own search.
    let scratch = Scratch::new("player-shape");
    for bad in [
        "0x2935A721068DA756B28CBA896EFDB64E8909DFAE",
        " 0x2935a721068da756b28cba896efdb64e8909dfae",
        "0x2935a721068da756b28cba896efdb64e8909dfae ",
        "2935a721068da756b28cba896efdb64e8909dfae",
        "0x2935a721068da756b28cba896efdb64e8909dfa",
        "0xzz35a721068da756b28cba896efdb64e8909dfae",
        "",
    ] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!(
            "\n[tig]\nbase_url = \"http://127.0.0.1:8080\"\nplayer_id = \"{bad}\"\nacquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n"
        ));
        toml.push_str(GATEWAY);
        let Err(err) = Config::load(scratch.write(&toml), Binary::TigGateway) else {
            panic!("{bad:?} must not load");
        };
        assert_invalid(err, "40 lowercase hex digits");
    }
}

#[test]
fn an_abbreviated_or_malformed_upstream_commit_does_not_load() {
    // §13 check 2 compares this against the pin compiled into the binary,
    // byte for byte. An abbreviation is the value a person would naturally
    // paste — git prints short hashes everywhere — and it would fail the
    // check saying "you deployed the wrong binary" when the operator had
    // only typed seven characters. Refused here, where the message is about
    // what is actually wrong.
    let scratch = Scratch::new("upstream-shape");
    for bad in [
        "ad08d1e",
        "AD08D1EA001A73FF5AAB3B556D7F59246FECE14E",
        "ad08d1ea001a73ff5aab3b556d7f59246fece14",
        " ad08d1ea001a73ff5aab3b556d7f59246fece14e",
        "",
    ] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!(
            "\n[tig]\nbase_url = \"http://127.0.0.1:8080\"\nplayer_id = \
             \"0x2935a721068da756b28cba896efdb64e8909dfae\"\n\
             acquired_upstream_commit = \"{bad}\"\n"
        ));
        toml.push_str(GATEWAY);
        let Err(err) = Config::load(scratch.write(&toml), Binary::TigGateway) else {
            panic!("{bad:?} must not load");
        };
        assert_invalid(err, "40-character lowercase hex commit");
    }
}

#[test]
fn an_acknowledgement_that_says_nothing_does_not_load() {
    // §13.2: the acknowledgement that lets check 3 pass is a reason, not a
    // flag, because it is what a later operator reads to decide whether the
    // deviation still holds. An empty string satisfies `Some(_)` and would
    // pass the check while recording nothing — the shape of someone who
    // wanted the failure to stop rather than to be understood.
    let scratch = Scratch::new("ack-empty");
    for bad in ["", "   ", "\t"] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!(
            "\n[tig]\nbase_url = \"http://127.0.0.1:8080\"\nplayer_id = \
             \"0x2935a721068da756b28cba896efdb64e8909dfae\"\n\
             acquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n\
             unresolved_containers_acknowledged = \"{bad}\"\n"
        ));
        toml.push_str(GATEWAY);
        let Err(err) = Config::load(scratch.write(&toml), Binary::TigGateway) else {
            panic!("{bad:?} must not load");
        };
        assert_invalid(err, "must say why");
    }

    // A real reason loads, and absence loads too — absence is the default,
    // and it is `evaluate` that turns it into a failed check rather than
    // configuration refusing to start.
    for ok in [Some("slice 1 runs no pinned container; issue #20"), None] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(GATEWAY);
        toml.push_str(
            "\n[tig]\nbase_url = \"http://127.0.0.1:8080\"\nplayer_id = \
             \"0x2935a721068da756b28cba896efdb64e8909dfae\"\n\
             acquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n",
        );
        if let Some(reason) = ok {
            toml.push_str(&format!(
                "unresolved_containers_acknowledged = \"{reason}\"\n"
            ));
        }
        let config = Config::load(scratch.write(&toml), Binary::TigGateway)
            .unwrap_or_else(|e| panic!("{ok:?} must load: {e}"));
        assert_eq!(
            config
                .tig
                .as_ref()
                .unwrap()
                .unresolved_containers_acknowledged
                .as_deref(),
            ok
        );
    }
}

#[test]
fn the_gateway_section_is_required_by_the_gateway_and_forbidden_elsewhere() {
    let scratch = Scratch::new("gateway-section");
    let base = |role: &str| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", &format!("user = \"{role}\""));
        toml.push_str(TIG);
        toml
    };

    // Required. Every field is a fact about this deployment, and a gateway
    // without them cannot load the key §2.2 says only it holds.
    let Err(err) = Config::load(scratch.write(&base("pool_gateway")), Binary::TigGateway) else {
        panic!("a gateway with no [gateway] must not load");
    };
    assert_invalid(err, "tig-gateway requires [gateway]");

    // And forbidden elsewhere: a controller carrying it would read as though
    // the controller held the API key.
    let mut toml = base("pool_controller");
    toml.push_str(ORCHESTRATION);
    toml.push_str(GATEWAY);
    let Err(err) = Config::load(scratch.write(&toml), Binary::PoolController) else {
        panic!("a controller carrying [gateway] must not load");
    };
    assert_invalid(err, "[gateway] belongs to tig-gateway");
}

#[test]
fn a_gateway_section_with_an_unusable_value_does_not_load() {
    let scratch = Scratch::new("gateway-values");
    let load = |section: &str| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(TIG);
        toml.push_str(section);
        Config::load(scratch.write(&toml), Binary::TigGateway)
    };

    // A lease of zero can hold nothing, which is a misconfiguration rather
    // than a policy.
    let Err(err) = load(
        "\n[gateway]\napi_key_file = \"/dev/null\"\nlease_secs = 0\n\
         platform = \"linux/arm64\"\nserved_compute = []\n",
    ) else {
        panic!("lease_secs = 0 must not load");
    };
    assert_invalid(err, "lease_secs must be at least 1");

    // §13 check 3 compares the platform byte for byte against what a registry
    // reports, so a bare architecture would fail it for a reason no operator
    // could act on.
    for bad in ["arm64", "linux/", ""] {
        let Err(err) = load(&format!(
            "\n[gateway]\napi_key_file = \"/dev/null\"\nlease_secs = 120\n\
             platform = \"{bad}\"\nserved_compute = []\n"
        )) else {
            panic!("platform {bad:?} must not load");
        };
        assert_invalid(err, "must be `linux/<arch>`");
    }

    // An empty served entry matches no challenge and would scope §13 check 6
    // to nothing — the failure §13.5 warns about, reached by a stray comma.
    let Err(err) = load(
        "\n[gateway]\napi_key_file = \"/dev/null\"\nlease_secs = 120\n\
         platform = \"linux/arm64\"\nserved_compute = [\"cpu\", \"\"]\n",
    ) else {
        panic!("an empty served_compute entry must not load");
    };
    assert_invalid(err, "must not contain an empty entry");

    // And the shape that does load, so the rejections above are not passing
    // for some unrelated reason.
    load(
        "\n[gateway]\napi_key_file = \"/dev/null\"\nlease_secs = 120\n\
         platform = \"linux/arm64\"\nserved_compute = []\n",
    )
    .expect("a well-formed [gateway] loads");
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

#[test]
fn a_tig_facing_binary_requires_an_endpoint_and_the_migration_job_must_not_have_one() {
    // Both directions. A controller or gateway with no endpoint would have to
    // invent one at the first call; `pool-admin migrate` carrying one reads as
    // though the migration job could reach TIG, and `architecture.md` §4 gives
    // it exactly one job.
    let scratch = Scratch::new("tig-presence");

    for (binary, role) in [
        (Binary::PoolController, "pool_controller"),
        (Binary::TigGateway, "pool_gateway"),
    ] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", &format!("user = \"{role}\""));
        if matches!(binary, Binary::PoolController) {
            toml.push_str(ORCHESTRATION);
        }
        let path = scratch.write(&toml);
        let err = Config::load(&path, binary).expect_err("no endpoint");
        assert_invalid(err, "[tig]");
    }

    let mut toml = valid_toml(&scratch.password_file());
    toml.push_str(TIG);
    toml.push_str(GATEWAY);
    let path = scratch.write(&toml);
    let err =
        Config::load(&path, Binary::PoolAdminMigrate).expect_err("migrate does not talk to TIG");
    assert_invalid(err, "must not carry [tig]");
}

#[test]
fn an_endpoint_that_is_not_a_url_does_not_load() {
    // A bare host is read as a relative path by most clients, so it would
    // produce a request to somewhere unintended rather than a startup failure.
    let scratch = Scratch::new("tig-shape");
    for (bad, needle) in [
        ("api.tig.foundation", "is not http or https"),
        ("ftp://api.tig.foundation", "is not http or https"),
        // The URL parser reaches this one first and says it better than a
        // hand-rolled check could.
        ("http://", "empty host"),
    ] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!("\n[tig]\nbase_url = \"{bad}\"\nplayer_id = \"0x2935a721068da756b28cba896efdb64e8909dfae\"\nacquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n"));
        toml.push_str(GATEWAY);
        let path = scratch.write(&toml);
        let Err(err) = Config::load(&path, Binary::TigGateway) else {
            panic!("{bad} must not load");
        };
        assert_invalid(err, needle);
    }
}

#[test]
fn the_fake_tig_test_reads_the_endpoint_and_not_the_network() {
    // Criterion F4d. `network` is pinned to exactly "testnet" (A2), so it
    // cannot tell the real testnet API from a local fake — which is the whole
    // distinction the stub-acceptance guard turns on.
    //
    // A loopback *literal*, not a resolved name: a DNS answer can differ
    // between the check and the write it guards, so a guard built on one
    // states something that was true a moment ago.
    for local in [
        "http://127.0.0.1:8080",
        "http://localhost:3000",
        "http://[::1]:8080",
        "http://127.0.0.1",
        "http://localhost/api",
    ] {
        assert!(
            pool_config::TigConfig {
                acquired_upstream_commit: "ad08d1ea001a73ff5aab3b556d7f59246fece14e".to_string(),
                unresolved_containers_acknowledged: None,
                base_url: local.to_string(),
                player_id: "0x2935a721068da756b28cba896efdb64e8909dfae".to_string(),
            }
            .is_local_fake_tig(),
            "{local} is a local fake"
        );
    }
    for remote in [
        "https://testnet-api.tig.foundation",
        "http://10.0.0.1:8080",
        "https://127.0.0.1.example.com",
        "http://not-localhost:8080",
        "https://localhost.evil.test",
        "",
        // Userinfo posing as the host. Every HTTP client sends these to the
        // domain after the `@`; a parser that reads up to the first `:` or
        // `]` sees a loopback literal and answers yes — the guard passing in
        // exactly the case it exists to catch.
        "http://127.0.0.1:8080@testnet-api.tig.foundation",
        "https://[::1]@testnet-api.tig.foundation",
        "http://localhost@testnet-api.tig.foundation/",
        "http://user:127.0.0.1@testnet-api.tig.foundation",
    ] {
        assert!(
            !pool_config::TigConfig {
                acquired_upstream_commit: "ad08d1ea001a73ff5aab3b556d7f59246fece14e".to_string(),
                unresolved_containers_acknowledged: None,
                base_url: remote.to_string(),
                player_id: "0x2935a721068da756b28cba896efdb64e8909dfae".to_string(),
            }
            .is_local_fake_tig(),
            "{remote} is not a local fake"
        );
    }
}

#[test]
fn every_shipped_dev_config_parses_into_the_typed_shape() {
    // The dev configs are the ones an operator runs first, and nothing checked
    // them. `deny_unknown_fields` means a renamed or misspelled key is a hard
    // parse failure — which is the behaviour we want, but only useful if
    // something notices before the operator does.
    //
    // Parsed, not `Config::load`ed: loading reads the password file, which
    // `scripts/dev-db.sh` generates into the untracked `secrets/` directory
    // and CI does not have. Everything that can be checked without a secret is
    // checked here, and that is the part that goes stale — sections, key
    // names, and the role each binary must connect as.
    for (file, binary, role) in [
        (
            include_str!("../../../config/pool-admin.dev.toml"),
            Binary::PoolAdminMigrate,
            "pool_migration",
        ),
        (
            include_str!("../../../config/pool-controller.dev.toml"),
            Binary::PoolController,
            "pool_controller",
        ),
        (
            include_str!("../../../config/tig-gateway.dev.toml"),
            Binary::TigGateway,
            "pool_gateway",
        ),
        (
            include_str!("../../../config/pool-api.dev.toml"),
            Binary::PoolApi,
            "pool_api",
        ),
    ] {
        let config: Config = toml::from_str(file)
            .unwrap_or_else(|e| panic!("{} dev config does not parse: {e}", binary.as_str()));

        assert_eq!(config.network, pool_config::Network::Testnet);
        assert_eq!(
            config.database.user,
            role,
            "{} must connect as {role}",
            binary.as_str()
        );
        assert!(
            config.database.password_file.starts_with("secrets/"),
            "a dev config names a password file under secrets/, never a password: {:?}",
            config.database.password_file
        );

        // The presence rules `validate_for` enforces, checked against the
        // files that have to satisfy them.
        match binary {
            Binary::PoolController => {
                let tig = config
                    .tig
                    .as_ref()
                    .expect("the controller needs an endpoint");
                assert!(
                    tig.is_local_fake_tig(),
                    "a dev config points at a local fake-tig, or F4d's guard refuses the stub: {}",
                    tig.base_url
                );
                assert!(config.orchestration.is_some());
            }
            Binary::TigGateway => {
                let tig = config.tig.as_ref().expect("the gateway needs an endpoint");
                assert!(
                    tig.is_local_fake_tig(),
                    "a dev config points at a local fake-tig: {}",
                    tig.base_url
                );
                let gateway = config
                    .gateway
                    .as_ref()
                    .expect("the gateway section is what makes this binary runnable");
                assert!(
                    gateway.api_key_file.starts_with("secrets/"),
                    "a dev config names a key file under secrets/, never a key: {:?}",
                    gateway.api_key_file
                );
                // The one cross-field rule `pool-config` cannot check, because
                // the call timeout is pinned in `tig-gateway` rather than
                // configured: a file that parses here but that no gateway would
                // start on teaches the wrong shape (drive::lease_outlasts_call).
                assert!(
                    gateway.lease_secs > 60,
                    "a dev lease must outlast the pinned 60s write call timeout, or \
                     `tig-gateway run` refuses to start: {}",
                    gateway.lease_secs
                );
                assert!(
                    config.orchestration.is_none(),
                    "the gateway chooses no work"
                );
            }
            Binary::PoolApi => {
                // §2.2 and §3: no endpoint, no key, no orchestration policy —
                // this process terminates member traffic and nothing else.
                assert!(config.tig.is_none(), "pool-api does not talk to TIG");
                assert!(config.gateway.is_none());
                assert!(config.orchestration.is_none());
                let api = config
                    .member_api
                    .as_ref()
                    .expect("the member_api section is what makes this binary runnable");
                assert!(
                    api.listen.parse::<std::net::SocketAddr>().is_ok(),
                    "a dev config names an address literal, not a hostname: {}",
                    api.listen
                );
                // Checked here because this test parses rather than loads, so
                // `validate_for`'s bounds do not run: a shipped file under the
                // floor would answer `413` to a conforming heartbeat, and an
                // operator would meet that before anything told them why.
                assert!(
                    (LARGEST_CONFORMING_CONTROL_BODY_BYTES..=LARGEST_PROTOCOL_BODY_BYTES)
                        .contains(&api.max_control_body_bytes),
                    "a dev config names a body limit validate_for accepts: {}",
                    api.max_control_body_bytes
                );
            }
            _ => {
                assert!(config.tig.is_none(), "migrate does not talk to TIG");
                assert!(config.orchestration.is_none());
                assert!(config.member_api.is_none());
            }
        }
    }
}

#[test]
fn the_endpoint_cannot_be_pointed_at_mainnet_or_anywhere_unpinned() {
    // `tig_integration.md` §2.1: the mainnet URL "must not be used as a
    // fallback", and enabling mainnet "requires an explicit reviewed
    // configuration change". `Network`'s own doc says no build of this slice
    // can be pointed at mainnet by editing a config file — which a free-form
    // `[tig].base_url` made false, since A2 only constrains `network`.
    //
    // So where TIG is keeps one source of truth: the pinned
    // `config/tig_integration.json`, or a local fake.
    let scratch = Scratch::new("tig-pinned");
    let gateway = |toml: &str| {
        let path = scratch.write(toml);
        Config::load(&path, Binary::TigGateway)
    };
    let with = |base: &str| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!("\n[tig]\nbase_url = \"{base}\"\nplayer_id = \"0x2935a721068da756b28cba896efdb64e8909dfae\"\nacquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n"));
        toml.push_str(GATEWAY);
        toml
    };

    let err = gateway(&with("https://mainnet-api.tig.foundation")).expect_err("mainnet");
    assert_invalid(err, "pinned mainnet endpoint");

    let err = gateway(&with("https://api.example.com")).expect_err("somewhere else entirely");
    assert_invalid(err, "must be the pinned endpoint");

    // Userinfo is refused outright, not merely treated as non-local: a TIG
    // endpoint needs none, and one in TOML is a credential in TOML.
    let err = gateway(&with("https://user:pw@testnet-api.tig.foundation")).expect_err("userinfo");
    assert_invalid(err, "userinfo");

    // The two that are allowed.
    gateway(&with("https://testnet-api.tig.foundation")).expect("the pinned testnet endpoint");
    gateway(&with("http://127.0.0.1:8080")).expect("a local fake-tig");
}

#[test]
fn a_rejected_endpoint_never_echoes_its_userinfo() {
    // §9 and criterion A4: no secret readable from a config file, argument,
    // log or trace. The scheme check runs before anything has looked for
    // userinfo, so a message that interpolated `base_url` would print the
    // credential of a URL it was in the middle of rejecting.
    let scratch = Scratch::new("tig-redact");
    for bad in [
        "ftp://user:hunter2@testnet-api.tig.foundation",
        "https://user:hunter2@testnet-api.tig.foundation",
        "http://user:hunter2@127.0.0.1:8080",
    ] {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_gateway\"");
        toml.push_str(&format!("\n[tig]\nbase_url = \"{bad}\"\nplayer_id = \"0x2935a721068da756b28cba896efdb64e8909dfae\"\nacquired_upstream_commit = \"ad08d1ea001a73ff5aab3b556d7f59246fece14e\"\n"));
        toml.push_str(GATEWAY);
        let path = scratch.write(&toml);
        let Err(err) = Config::load(&path, Binary::TigGateway) else {
            panic!("{bad} must not load");
        };
        let rendered = err.to_string();
        assert!(
            !rendered.contains("hunter2"),
            "the refusal printed the credential it was rejecting: {rendered}"
        );
    }
}

#[test]
fn the_member_api_section_is_required_by_pool_api_and_forbidden_elsewhere() {
    let scratch = Scratch::new("member-api-section");
    let base = |role: &str| {
        valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", &format!("user = \"{role}\""))
    };

    // Required. `listen` says where the TLS proxy in front of this process
    // reaches it and `max_control_body_bytes` says how much a member may say
    // in one control message; a default for either would answer a deployment
    // question on the operator's behalf.
    let Err(err) = Config::load(scratch.write(&base("pool_api")), Binary::PoolApi) else {
        panic!("pool-api with no [member_api] must not load");
    };
    assert_invalid(err, "pool-api requires [member_api]");

    // And forbidden elsewhere: a controller carrying it would read as though
    // the controller terminated member traffic.
    let mut toml = base("pool_controller");
    toml.push_str(TIG);
    toml.push_str(ORCHESTRATION);
    toml.push_str(MEMBER_API);
    let Err(err) = Config::load(scratch.write(&toml), Binary::PoolController) else {
        panic!("a controller carrying [member_api] must not load");
    };
    assert_invalid(err, "[member_api] belongs to pool-api");

    // The shape that does load, so the rejections above are not passing for
    // some unrelated reason.
    let mut toml = base("pool_api");
    toml.push_str(MEMBER_API);
    let config = Config::load(scratch.write(&toml), Binary::PoolApi).expect("a pool-api config");
    let api = config.member_api.expect("the section is kept");
    assert_eq!(api.listen, "127.0.0.1:8081");
    assert_eq!(api.max_control_body_bytes, 524_288);
}

#[test]
fn pool_api_must_not_carry_a_tig_endpoint() {
    // `architecture.md` §2.2 puts the TIG credential in the gateway alone and
    // §3 gives this process no reason to reach TIG at all. A config that named
    // an endpoint here would read as though it did.
    let scratch = Scratch::new("member-api-tig");
    let mut toml = valid_toml(&scratch.password_file())
        .replace("user = \"pool_migration\"", "user = \"pool_api\"");
    toml.push_str(MEMBER_API);
    toml.push_str(TIG);
    let Err(err) = Config::load(scratch.write(&toml), Binary::PoolApi) else {
        panic!("pool-api carrying [tig] must not load");
    };
    assert_invalid(err, "must not carry [tig]");
}

#[test]
fn a_member_api_section_with_an_unusable_value_does_not_load() {
    let scratch = Scratch::new("member-api-values");
    let load = |section: &str| {
        let mut toml = valid_toml(&scratch.password_file())
            .replace("user = \"pool_migration\"", "user = \"pool_api\"");
        toml.push_str(section);
        Config::load(scratch.write(&toml), Binary::PoolApi)
    };

    // Parsed at load rather than at bind: a service that starts, logs
    // "listening", and only then fails to bind looks healthy while serving
    // nobody, which is the fail-open shape `architecture.md` §9 refuses.
    for bad in [
        "",
        "   ",
        "127.0.0.1",
        ":8081",
        "localhost:8081",
        "127.0.0.1:0x1f",
    ] {
        let Err(err) = load(&format!(
            "\n[member_api]\nlisten = \"{bad}\"\nticket_hmac_key_file = \"/dev/null\"\nmax_control_body_bytes = 524288\n"
        )) else {
            panic!("listen {bad:?} must not load");
        };
        assert_invalid(err, "member_api.listen");
    }

    // A hostname is refused deliberately: a DNS answer can change between
    // startup and a restart, so a bind address that resolves is a different
    // address over time. An address literal is the one that cannot move.
    //
    // The body limit here is a *valid* one on purpose. With an invalid value
    // this case would pass only because `validate_for` happens to check
    // `listen` first, and reordering the two checks would quietly turn it into
    // a test of the other rejection.
    let Err(err) = load(&format!(
        "\n[member_api]\nlisten = \"localhost:8081\"\n\
         ticket_hmac_key_file = \"/dev/null\"\n\
         max_control_body_bytes = {LARGEST_CONFORMING_CONTROL_BODY_BYTES}\n"
    )) else {
        panic!("a hostname must not load");
    };
    assert_invalid(err, "must be `address:port`");

    // Below the floor, a maximal `HeartbeatRequest` the pinned schemas permit
    // would be answered `413` — a refusal `member_protocol.md` §14 reserves
    // for incompatible input. Above §10.3's chunk ceiling, the value describes
    // a body no route of this protocol accepts.
    for bad in [
        0_u64,
        1,
        262_144,
        LARGEST_CONFORMING_CONTROL_BODY_BYTES - 1,
        LARGEST_PROTOCOL_BODY_BYTES + 1,
        u64::MAX,
    ] {
        let Err(err) = load(&format!(
            "\n[member_api]\nlisten = \"127.0.0.1:8081\"\nticket_hmac_key_file = \"/dev/null\"\nmax_control_body_bytes = {bad}\n"
        )) else {
            panic!("max_control_body_bytes {bad} must not load");
        };
        assert_invalid(err, "max_control_body_bytes must be between");
    }

    // Both ends of the accepted range, so the rejections above are bounds
    // rather than a check that refuses everything.
    for good in [
        LARGEST_CONFORMING_CONTROL_BODY_BYTES,
        LARGEST_PROTOCOL_BODY_BYTES,
    ] {
        load(&format!(
            "\n[member_api]\nlisten = \"[::1]:8081\"\nticket_hmac_key_file = \"/dev/null\"\nmax_control_body_bytes = {good}\n"
        ))
        .unwrap_or_else(|e| panic!("max_control_body_bytes {good} must load: {e:?}"));
    }
}
