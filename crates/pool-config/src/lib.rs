//! Typed, fail-closed configuration for pool binaries.
//!
//! `docs/architecture.md` §9 owns the rules this implements: one explicit
//! non-secret TOML path per binary, parsed into typed structures, unknown
//! fields rejected, cross-field invariants validated, and a non-zero exit
//! before the process serves or claims any work. `network` has no
//! production default.
//!
//! Secrets never appear here. A config file names the *path* of a secret
//! file; the bytes are read at use time by the one process entitled to them
//! (`architecture.md` §2.2, §9).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Every way loading can fail. All of them are fatal: the caller exits
/// non-zero rather than starting with a partial configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration in {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

/// The pool's operating network.
///
/// Slice 1 accepts `testnet` only. `mainnet` parses — the name has to exist
/// for the rejection to be meaningful and for later slices to widen it — but
/// [`Config::validate`] refuses it, so no build of this slice can be pointed
/// at mainnet by editing a config file (`tig_integration.md` §13 check 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        })
    }
}

/// Connection facts for one PostgreSQL login role.
///
/// There is no `password` field and there never will be: the password lives
/// in `password_file`, outside version control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    /// The login role. Which one it may be depends on the binary; see
    /// [`Config::validate_for`].
    pub user: String,
    /// Path to a file containing only the password. Never committed.
    pub password_file: PathBuf,
    /// Statement timeout applied to every session, in milliseconds.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u32,
    /// How long to keep trying to obtain a connection before giving up, in
    /// milliseconds.
    ///
    /// sqlx retries a refused connection until its pool acquire timeout, so
    /// without this the effective answer is that library's default of thirty
    /// seconds — an in-process retry loop nobody chose and nobody can see. A1
    /// says a job that cannot reach its database must fail closed, and failing
    /// closed half a minute late is a worse answer than failing closed
    /// promptly: whether to retry is the invoking deploy step's decision, and
    /// it can only make it once this process has returned.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u32,
}

fn default_statement_timeout_ms() -> u32 {
    30_000
}

/// Long enough to cross a slow resolver or a proxy, short enough that a
/// refused connection is reported while an operator is still watching.
fn default_connect_timeout_ms() -> u32 {
    5_000
}

/// The log format.
///
/// One variant on purpose. `architecture.md` §10.1 states that *every*
/// process emits structured JSON, so a human-readable alternative would make
/// an invariant into a per-config choice — and a deployed process set to it
/// would lose the contract while every gate that checks §10.1 is keyed to
/// JSON output and would simply see no lines to check. Keeping the field
/// explicit (rather than dropping it) means a config states its format
/// rather than relying on a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Structured JSON, one object per line (`architecture.md` §10.1).
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    pub format: LogFormat,
    pub level: String,
    /// Deployment name carried on every log line and metric.
    pub deployment: String,
}

/// Where TIG is.
///
/// A separate configuration surface from `network`, and deliberately so.
/// `network` is pinned to exactly `testnet` in this build (A2), so it cannot
/// distinguish the real testnet API from a local `fake-tig` — and criterion
/// F4d needs exactly that distinction, to refuse a fabricated acceptance
/// record anywhere but against the fake. Two fields, because they answer two
/// questions: *which chain* and *which server*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TigConfig {
    /// Base URL of the TIG API. No default: `architecture.md` §9 gives
    /// `network` no production default for the same reason, and an endpoint
    /// that falls back to something is an endpoint reached when the operator
    /// forgot to choose one.
    pub base_url: String,
    /// The pool's own TIG player id on this endpoint.
    ///
    /// Public and safe to commit — it is an address, not a credential; the
    /// API key that goes with it lives only in `tig-gateway`'s secret file
    /// (§2.2). It is part of every §6.1 precommit body, so the controller
    /// needs it to digest a payload at admission and the gateway to rebuild
    /// the same bytes before sending. Beside `base_url` because it is an
    /// identity *on that endpoint*: the same pool has a different id on a
    /// different network.
    pub player_id: String,
    /// The upstream TIG source commit whoever deployed this says was
    /// acquired and reviewed, per `tig_integration.md` §15.
    ///
    /// §13 check 2 compares "the acquired upstream source commit" against the
    /// pin. The pin is compiled in — `config/tig_integration.json` reaches
    /// the binary through `include_str!` — so it cannot also be the *other*
    /// side of that comparison without the check being circular and proving
    /// nothing.
    ///
    /// Nothing in this repository acquires TIG's source: §15 makes the
    /// upgrade a reviewed human procedure that ends by editing the pinned
    /// file, restating this value in every deployment and rebuilding. The
    /// acquired commit is therefore a fact only a person holds, and
    /// this is where they state it. A deployment stating one the binary was
    /// not built against fails check 2 rather than writing to TIG under a
    /// snapshot nobody built it against.
    ///
    /// What this does **not** prove: that the person reviewed that commit.
    /// Only automated acquisition closes that, and until it exists check 2
    /// verifies the deployment against the build, not the build against
    /// upstream.
    pub acquired_upstream_commit: String,
    /// Why this deployment resolves no pinned container digests, if it does
    /// not — §13 check 3, and the deviation `tig_integration.md` §13.2
    /// records.
    ///
    /// Absent by default, and absence **fails** check 3: a deployment that
    /// has said nothing about the check has not passed it. Present, the
    /// gateway may write and the reason travels with the permission, so a
    /// deviation nobody can see from the outside does not outlive the reason
    /// for it.
    ///
    /// Not a boolean. A flag records that someone toggled something; a reason
    /// records what they believed, which is what the next operator needs in
    /// order to decide whether it still holds. Issue #20 owns removing this
    /// field along with the check it stands in for.
    #[serde(default)]
    pub unresolved_containers_acknowledged: Option<String>,
}

impl TigConfig {
    /// Whether this endpoint is a local `fake-tig`, for F4d's guard.
    ///
    /// A property of the **configuration**, not of the network at an instant.
    /// Resolving a hostname would be the obvious reading of "resolves to the
    /// local fake-tig target" and is the wrong one: a DNS answer can differ
    /// between the check and the write it guards, so a guard built on one
    /// says a thing that was true a moment ago. A loopback literal cannot
    /// change under the process.
    ///
    /// `localhost` is accepted because every dev and CI config in this
    /// repository writes it and refusing it would push people to hardcode an
    /// address; it is a name the host resolver owns, and an operator who has
    /// repointed it has already left the ground this guard stands on.
    ///
    /// Parsed with a real URL parser rather than split by hand. The hand-rolled
    /// version read the text before the first `:` as the host, so
    /// `http://127.0.0.1:8080@api.tig.foundation` answered *loopback* while
    /// every HTTP client sent the request to the real API — the guard passing
    /// in exactly the case it exists to catch. Userinfo is refused outright as
    /// well as ignored: a TIG endpoint has no legitimate use for it, and
    /// `architecture.md` §9 keeps credentials out of TOML.
    pub fn is_local_fake_tig(&self) -> bool {
        let Ok(url) = url::Url::parse(&self.base_url) else {
            return false;
        };
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        match url.host() {
            Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
            Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
            None => false,
        }
    }

    /// The endpoint's host, with no userinfo, port or path.
    ///
    /// For messages. A refusal should name what the process was pointed at,
    /// and `base_url` is the wrong thing to print: `Config::load` refuses
    /// userinfo, but a `TigConfig` built directly need not have come through
    /// it, and §9 keeps credentials out of logs whatever route they arrived
    /// by. One parser, here, because two parsers is how the loopback check
    /// came to disagree with every HTTP client.
    pub fn host(&self) -> Option<String> {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    }

    /// The endpoints `config/tig_integration.json` pins, as
    /// `(testnet, mainnet)`.
    ///
    /// Read from the pinned file rather than written here, so there is one
    /// place that says where TIG is. `tig_integration.md` §2.1 says the
    /// mainnet URL "must not be used as a fallback" and that enabling mainnet
    /// "requires an explicit reviewed configuration change" — which a free-form
    /// `base_url` would otherwise turn into a config edit.
    fn pinned_endpoints() -> Result<(String, String), String> {
        const PINNED: &str = include_str!("../../../config/tig_integration.json");
        let value: serde_json::Value = serde_json::from_str(PINNED)
            .map_err(|e| format!("config/tig_integration.json is not valid JSON: {e}"))?;
        let field = |key: &str| -> Result<String, String> {
            Ok(value
                .get("network")
                .and_then(|n| n.get(key))
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("config/tig_integration.json has no network.{key}"))?
                .trim_end_matches('/')
                .to_string())
        };
        Ok((field("api_base_url")?, field("mainnet_api_base_url")?))
    }
}

/// The gateway's own settings: the credential, the lease, and the scope §13
/// check 6 judges.
///
/// Gateway-only, the way `[orchestration]` is controller-only. A controller
/// config carrying it would read as though the controller held the API key,
/// which `architecture.md` §2.2 says it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Where the TIG API key is read from.
    ///
    /// The path, never the key. `architecture.md` §9 keeps secrets out of
    /// configuration and §2.2 makes this file the only place the key exists;
    /// naming the file is what lets check 8 judge who can read it.
    pub api_key_file: PathBuf,
    /// How long a transmit lease is held.
    ///
    /// No default: `run_once` refuses a lease shorter than a write's call
    /// timeout, because a claimant could otherwise take over an attempt whose
    /// sender is still waiting on TIG. A compiled fallback would be a value
    /// reached exactly when the operator did not think about it.
    pub lease_secs: i64,
    /// The platform the pinned container digests must resolve for.
    ///
    /// A fact about this host. §2 records the pinned images as multi-platform,
    /// so the file cannot answer which one this machine needs. `linux/<arch>`,
    /// matching what a registry reports.
    pub platform: String,
    /// The compute types this deployment serves, for §13 check 6.
    ///
    /// §13.5 is the contract: "considered by the decision engine" resolves to
    /// the compute this deployment serves, so this is what scopes the check.
    /// **Required, and required to be explicit** — a gateway that defaulted it
    /// to empty would pass check 6 by saying nothing. An empty list is
    /// therefore a deliberate statement that this deployment mines nothing,
    /// which is what slice 1 is.
    pub served_compute: Vec<String>,
}

/// `accounting.md` §3's canonical unsigned base-10 atom string.
///
/// Digits only, no sign, no exponent, no decimal point, and no redundant
/// leading zero — so one amount has exactly one spelling and two records of it
/// compare equal as text.
fn is_canonical_atoms(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

/// The controller's orchestration policy.
///
/// `mining_system.md` §11 lists `internal_pool_unverified_limit` among the
/// numerical values "required before full product implementation", as
/// *versioned policy*. So it is configuration with no default: a compiled
/// fallback would be a policy value reached exactly when the operator forgot
/// to set one, which is the case fail-closed exists for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationConfig {
    /// The §6.1 gate: `pool_unverified < internal_pool_unverified_limit`.
    pub internal_pool_unverified_limit: i64,
    /// How many `get-benchmark-data` reads one poll may spend warming the
    /// active-benchmark cache (`tig_integration.md` §5.2, §9 step 4).
    ///
    /// The reads come out of the controller's share of the per-IP budget
    /// (ADR-0006) alongside the poll and the snapshot assembly, and a block
    /// must still be taken in every block; this is where the operator says
    /// how much of what is left the warm-up may use. No default, for the
    /// same reason as the limit above: a compiled value here would be a
    /// rate decision made in the binary rather than in reviewed
    /// configuration.
    pub active_cache_fetches_per_poll: u32,
    /// `accounting.md` §11.4's per-benchmark failure charge `X`, in atoms.
    ///
    /// `pre_build_checklist.md` §5.2 still lists `X` as **unchosen** — the
    /// checklist item is open — so this is not the settled value and no
    /// deployment should read it as one. It exists because criterion D2c
    /// requires slice 1 to *record the reserve's inputs* per intent, and an
    /// input the binary supplied itself would be a policy value invented in
    /// code. No default, for the same reason as the two above.
    ///
    /// A canonical unsigned base-10 atom string, which `accounting.md` §3 is
    /// what amounts cross this boundary as.
    pub precommit_failure_charge_atoms: String,
    /// Which version of the reserve policy the value above came from.
    ///
    /// D2c names "the `X` policy version" among the inputs to record, because
    /// a recorded amount with no version cannot be re-derived once `X` is
    /// chosen and changed. Free-form: the versioning scheme is the accounting
    /// slice's to fix, and inventing one here would be the same mistake as
    /// inventing the number.
    pub reserve_policy_version: String,
    /// The compute the pool decides *for* while it has no members.
    ///
    /// In production an offer comes from a member's compute-availability
    /// offer. Slice 1 has no members (slice-1 plan §2), so a deployment that
    /// is to decide anything has to say what it is deciding for — the same
    /// bootstrap shape as criterion F6's pool-owned workflow owner.
    ///
    /// **Optional, and absent means the pool decides nothing.** That is the
    /// slice's ordinary posture and the honest default: it matches
    /// `gateway.served_compute = []` and `tig_integration.md` §13.5's "a
    /// deployment serving nothing has nothing to judge". A default offer here
    /// would make every controller start proposing work, which is a decision
    /// no operator made.
    ///
    /// This is **not** `gateway.served_compute`. That field scopes §13 check 6
    /// — which compute types the gateway must have pinned runtimes for — and
    /// the compute a decision is made for is a different question.
    ///
    /// **Nothing currently checks that the two agree, and the gate does not.**
    /// An earlier version of this comment claimed it did. §13.5 scopes check 6
    /// to the compute types "considered by the decision engine" and resolves
    /// that to `served_compute`, so with `served_compute = []` the check skips
    /// every challenge and passes vacuously — while this field could
    /// independently have the controller deciding for CPU. The two processes
    /// hold separate configurations and `pool-config` sees one at a time, so
    /// the comparison cannot happen here.
    ///
    /// Where it can happen is the write boundary: every intent names its
    /// `compute_type`, and the gateway could refuse to transmit one outside
    /// its served set. That is the real fix and it belongs to `tig-gateway`;
    /// until it lands, keeping the two in step is an operator's job and this
    /// comment says so rather than implying a gate that does not fire.
    #[serde(default)]
    pub bootstrap_offer: Option<BootstrapOffer>,
}

/// What the pool offers itself, before members exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapOffer {
    /// `mining_system.md` §6.2's CPU/GPU class, which selects challenges.
    pub compute_class: String,
    /// §6.7's alignment rule needs the core count, and only for CPU.
    #[serde(default)]
    pub cpu_cores: Option<u64>,
    /// `tig_integration.md` §3's protocol compute type — `aws_t4g`, `aws_c7i`
    /// — which is what the §6.1 body carries. A different vocabulary from the
    /// class above, kept separate for the reason `config/tig_integration.json`
    /// keeps them separate.
    ///
    /// Checked for shape here and against the pinned compatibility table by
    /// the controller, where the pin is read. §3 says a compute type outside
    /// that table is "ineligible rather than coerced".
    pub tig_compute_type: String,
}

/// The configuration shared by every pool binary.
///
/// Binary-specific sections are added by the slices that introduce those
/// binaries, not speculatively here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// No default. A file that omits this fails to parse
    /// (`architecture.md` §9: "network has no production default").
    pub network: Network,
    pub database: DatabaseConfig,
    pub telemetry: TelemetryConfig,
    /// Present for the binaries that talk to TIG — the controller reads and
    /// the gateway writes — and absent for `pool-admin migrate`, which does
    /// neither. `validate_for` enforces it in both directions.
    pub tig: Option<TigConfig>,
    /// Present for `pool-controller` and absent for every other binary, which
    /// `validate_for` enforces in both directions.
    pub orchestration: Option<OrchestrationConfig>,
    pub gateway: Option<GatewayConfig>,
}

/// Which binary is loading, so cross-field rules can differ where the
/// process boundary demands it (`architecture.md` §6, §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    /// The one-shot migration job; the only caller entitled to the
    /// DDL-bearing credential (`architecture.md` §9).
    ///
    /// There is deliberately no variant for the operator CLI's other
    /// commands. `architecture.md` §4 routes those through the controller's
    /// private endpoint, so they need no database credential — and adding a
    /// variant with no consumer meant inventing a rule about which role it
    /// may hold, which reinterpreted a settled boundary for nothing. The
    /// slice that adds such a subcommand makes that decision, with an ADR if
    /// it changes the boundary.
    PoolAdminMigrate,
    PoolController,
    TigGateway,
}

impl Binary {
    /// The one database role this caller may log in as. Enforcing it here
    /// is a guardrail, not the enforcement: grants in the database are what
    /// actually separate the roles.
    fn required_db_role(self) -> &'static str {
        match self {
            Binary::PoolAdminMigrate => "pool_migration",
            Binary::PoolController => "pool_controller",
            Binary::TigGateway => "pool_gateway",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Binary::PoolAdminMigrate => "pool-admin migrate",
            Binary::PoolController => "pool-controller",
            Binary::TigGateway => "tig-gateway",
        }
    }
}

impl Config {
    /// Read, parse, and validate the file at `path` for `binary`.
    ///
    /// Every failure is fatal by design; there is no partial success and no
    /// fallback to defaults for a field the operator got wrong.
    pub fn load(path: impl AsRef<Path>, binary: Binary) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate_for(binary, path)?;
        Ok(config)
    }

    /// Cross-field invariants. Kept separate from parsing so tests can drive
    /// it directly.
    pub fn validate_for(&self, binary: Binary, path: &Path) -> Result<(), ConfigError> {
        let invalid = |reason: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            reason,
        };

        // Slice 1 is testnet-only. Failure to reach testnet must never
        // redirect to mainnet (`tig_integration.md` §2.1).
        if self.network != Network::Testnet {
            return Err(invalid(format!(
                "network must be exactly \"testnet\" in this build, found \"{}\"",
                self.network
            )));
        }

        if self.database.host.trim().is_empty() {
            return Err(invalid("database.host must not be empty".into()));
        }
        if self.database.port == 0 {
            return Err(invalid("database.port must not be 0".into()));
        }
        if self.database.name.trim().is_empty() {
            return Err(invalid("database.name must not be empty".into()));
        }
        if self.database.statement_timeout_ms == 0 {
            return Err(invalid(
                "database.statement_timeout_ms must not be 0: an unbounded statement can hold a \
                 lock indefinitely"
                    .into(),
            ));
        }
        if self.database.connect_timeout_ms == 0 {
            return Err(invalid(
                "database.connect_timeout_ms must not be 0: a zero timeout cannot succeed, so \
                 the process would fail closed whatever the database was doing"
                    .into(),
            ));
        }

        // The caller and its database role must agree. A controller config
        // pointed at the migration role would silently grant it DDL.
        let required = binary.required_db_role();
        if self.database.user != required {
            return Err(invalid(format!(
                "{} must connect as database role \"{}\", found \"{}\"",
                binary.as_str(),
                required,
                self.database.user
            )));
        }

        // A password file that is absent, unreadable, or empty is a startup
        // failure, not a runtime surprise on the first query.
        // Check the TRIMMED contents, not the byte length. A file holding
        // only a newline is non-empty on disk but yields an empty password
        // from `database_url`, which would defeat the fail-closed intent —
        // the operator would get a runtime authentication failure instead of
        // a startup error naming the file.
        match std::fs::read_to_string(&self.database.password_file) {
            Ok(contents) if contents.trim().is_empty() => {
                return Err(invalid(format!(
                    "database.password_file {} is empty or whitespace only",
                    self.database.password_file.display()
                )));
            }
            Ok(_) => {}
            Err(source) => {
                return Err(invalid(format!(
                    "database.password_file {} is not readable: {source}",
                    self.database.password_file.display()
                )));
            }
        }

        if self.telemetry.deployment.trim().is_empty() {
            return Err(invalid("telemetry.deployment must not be empty".into()));
        }
        if self.telemetry.level.trim().is_empty() {
            return Err(invalid("telemetry.level must not be empty".into()));
        }

        // Required for the binaries that talk to TIG, refused for the one
        // that does not. `pool-admin migrate` holding an endpoint would read
        // as though the migration job could reach TIG, and §4 gives it one
        // job.
        match (binary, &self.tig) {
            (Binary::PoolController | Binary::TigGateway, None) => {
                return Err(invalid(format!(
                    "{} requires [tig] with base_url; there is no default, for the same reason network has none (architecture.md §9)",
                    binary.as_str()
                )));
            }
            (Binary::PoolController | Binary::TigGateway, Some(tig)) => {
                // Parsed enough to be an endpoint. A bare host would be read
                // as a relative path by most clients and produce a request to
                // somewhere unintended rather than a startup failure.
                // The scheme, never the URL. This message runs before
                // anything has checked for userinfo, so interpolating
                // `base_url` here would print `ftp://user:pw@host`'s
                // credential into a startup log — §9 and criterion A4 keep
                // secrets out of logs whatever put them in the config.
                if !tig.base_url.starts_with("http://") && !tig.base_url.starts_with("https://") {
                    let scheme = tig
                        .base_url
                        .split("://")
                        .next()
                        .filter(|s| s.len() < tig.base_url.len())
                        .unwrap_or("<none>");
                    return Err(invalid(format!(
                        "tig.base_url scheme \"{scheme}\" is not http or https"
                    )));
                }
                let parsed = url::Url::parse(&tig.base_url)
                    .map_err(|e| invalid(format!("tig.base_url is not a URL: {e}")))?;
                if parsed.host().is_none() {
                    return Err(invalid(format!(
                        "tig.base_url names no host: \"{}\"",
                        tig.base_url
                    )));
                }
                // §6.1 renders it as "<lowercase pool address>", and
                // `reconcile` compares `settings.player_id` byte for byte
                // against what TIG returns. A mixed-case or padded value
                // would go into the body and the digest verbatim; if TIG
                // normalises it, the pool's own confirmed precommit would
                // reconcile as NoCandidate — an accepted write reported as
                // absent, which is the precondition §10 forbids acting on.
                // Refused at load, where the operator can fix it.
                let hex = tig.player_id.strip_prefix("0x").unwrap_or("");
                let well_formed = hex.len() == 40
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if !well_formed {
                    return Err(invalid(
                        "tig.player_id must be 0x followed by 40 lowercase hex digits, exactly \
                         as tig_integration.md §6.1 renders it and as TIG compares it"
                            .into(),
                    ));
                }
                // §13 check 2 compares this against the compiled-in pin byte
                // for byte, so a shortened or mixed-case commit would fail
                // the check for the wrong reason — the operator would read
                // "you deployed the wrong binary" when they had only typed an
                // abbreviation. Refused at load, where it says what it is.
                let commit = &tig.acquired_upstream_commit;
                let well_formed = commit.len() == 40
                    && commit
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if !well_formed {
                    return Err(invalid(
                        "tig.acquired_upstream_commit must be a full 40-character lowercase \
                         hex commit, not an abbreviation: §13 check 2 compares it against the \
                         compiled-in pin byte for byte"
                            .into(),
                    ));
                }
                // An empty reason is the shape of an operator who wanted the
                // check to stop failing without saying why. It would satisfy
                // `Some(_)` and pass check 3 while recording nothing, which
                // is the outcome §13.2 exists to prevent.
                if tig
                    .unresolved_containers_acknowledged
                    .as_deref()
                    .is_some_and(|reason| reason.trim().is_empty())
                {
                    return Err(invalid(
                        "tig.unresolved_containers_acknowledged must say why, not just be \
                         present: it is what a later operator reads to decide whether the \
                         deviation still holds (tig_integration.md §13.2)"
                            .into(),
                    ));
                }
                // A TIG endpoint has no legitimate userinfo. One here would
                // put a credential in TOML against §9 — and be echoed back in
                // this very message — and userinfo before a real host is how
                // a loopback check gets fooled.
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(invalid("tig.base_url must not carry userinfo".into()));
                }

                // Where TIG is has one source of truth: the pinned
                // `config/tig_integration.json`. Without this, `[tig]` is a
                // second one — `tig_integration.md` §2.1 says the mainnet URL
                // "must not be used as a fallback" and that enabling mainnet
                // "requires an explicit reviewed configuration change", and
                // `Network`'s own doc says no build of this slice can be
                // pointed at mainnet by editing a config file. A free-form
                // endpoint made that false while `network` still read
                // "testnet".
                let (pinned_testnet, pinned_mainnet) =
                    TigConfig::pinned_endpoints().map_err(invalid)?;
                let given = tig.base_url.trim_end_matches('/');
                if given.eq_ignore_ascii_case(&pinned_mainnet) {
                    return Err(invalid(format!(
                        "tig.base_url is the pinned mainnet endpoint {pinned_mainnet}: \
                         §2.1 forbids it as a fallback, and enabling mainnet is a reviewed \
                         change, not a config edit"
                    )));
                }
                if !tig.is_local_fake_tig() && given != pinned_testnet {
                    return Err(invalid(format!(
                        "tig.base_url must be the pinned endpoint {pinned_testnet} or a \
                         local fake-tig, found \"{}\"",
                        tig.base_url
                    )));
                }
            }
            (Binary::PoolAdminMigrate, Some(_)) => {
                return Err(invalid(
                    "pool-admin migrate must not carry [tig]: it does not talk to TIG (architecture.md §4)"
                        .into(),
                ));
            }
            (Binary::PoolAdminMigrate, None) => {}
        }

        // Required for the controller and refused for anyone else. Both
        // directions matter: a controller with no limit could admit without
        // one, and a gateway config carrying an orchestration policy would
        // read as though the gateway enforced it, which `architecture.md` §3
        // says it does not.
        match (binary, &self.orchestration) {
            (Binary::PoolController, None) => {
                return Err(invalid(
                    "pool-controller requires [orchestration] with \
                     internal_pool_unverified_limit and \
                     active_cache_fetches_per_poll; neither has a default because \
                     mining_system.md §11 makes the limit versioned policy and the \
                     fetch budget is a share of the reviewed read budget"
                        .into(),
                ));
            }
            (Binary::PoolController, Some(orchestration)) => {
                if orchestration.internal_pool_unverified_limit < 1 {
                    return Err(invalid(format!(
                        "orchestration.internal_pool_unverified_limit must be at \
                         least 1, found {}: a limit of 0 admits nothing, which is \
                         a misconfiguration rather than a policy",
                        orchestration.internal_pool_unverified_limit
                    )));
                }
                if orchestration.active_cache_fetches_per_poll < 1 {
                    return Err(invalid(
                        "orchestration.active_cache_fetches_per_poll must be at \
                         least 1: with 0 the active-benchmark cache never warms and \
                         no snapshot ever becomes usable for a decision"
                            .into(),
                    ));
                }
                // `accounting.md` §3's atom form, checked here rather than at
                // the admission that records it: a value PostgreSQL would
                // silently reshape — `1.5` rounding, `1e3` expanding, a
                // negative passing a `>= 0` column check after the cast — is a
                // reserve input the record could not be re-derived from.
                if !is_canonical_atoms(&orchestration.precommit_failure_charge_atoms) {
                    return Err(invalid(format!(
                        "orchestration.precommit_failure_charge_atoms must be a \
                         canonical unsigned base-10 atom string (accounting.md §3), \
                         found {:?}",
                        orchestration.precommit_failure_charge_atoms
                    )));
                }
                if let Some(offer) = &orchestration.bootstrap_offer {
                    match (offer.compute_class.as_str(), offer.cpu_cores) {
                        // §6.7's CPU alignment divides by the core count, so a
                        // CPU offer without one cannot be sized and a count of
                        // zero cannot be divided by.
                        ("cpu", None) => {
                            return Err(invalid(
                                "orchestration.bootstrap_offer.cpu_cores is required for a \
                                 cpu offer: mining_system.md §6.7 aligns bundle counts to \
                                 the core count"
                                    .into(),
                            ));
                        }
                        ("cpu", Some(0)) => {
                            return Err(invalid(
                                "orchestration.bootstrap_offer.cpu_cores must be at least 1".into(),
                            ));
                        }
                        // Carried for a GPU offer it would be read as a CPU
                        // one's, and §6.7 gives GPU the minimum regardless.
                        ("gpu", Some(_)) => {
                            return Err(invalid(
                                "orchestration.bootstrap_offer.cpu_cores belongs to a cpu \
                                 offer; §6.7 gives a gpu offer the minimum bundle count"
                                    .into(),
                            ));
                        }
                        ("cpu" | "gpu", _) => {}
                        (other, _) => {
                            return Err(invalid(format!(
                                "orchestration.bootstrap_offer.compute_class must be \
                                 \"cpu\" or \"gpu\" (mining_system.md §6.2), found {other:?}"
                            )));
                        }
                    }
                    if offer.tig_compute_type.trim().is_empty() {
                        return Err(invalid(
                            "orchestration.bootstrap_offer.tig_compute_type must name \
                             tig_integration.md §3's protocol type, such as \"aws_t4g\"; \
                             it is what the §6.1 body carries and is not the compute class"
                                .into(),
                        ));
                    }
                }
                if orchestration.reserve_policy_version.trim().is_empty() {
                    return Err(invalid(
                        "orchestration.reserve_policy_version must name the policy \
                         version the failure charge came from: criterion D2c records \
                         it with every intent, and a recorded amount with no version \
                         cannot be re-derived once X is chosen and changed"
                            .into(),
                    ));
                }
            }
            (other, Some(_)) => {
                return Err(invalid(format!(
                    "[orchestration] belongs to pool-controller; {} must not carry it",
                    other.as_str()
                )));
            }
            (_, None) => {}
        }

        // The same shape for `[gateway]`, and both directions matter for the
        // same reason: a gateway with no credential path cannot load the key
        // §2.2 says only it holds, and a controller carrying one would read
        // as though it did.
        match (binary, &self.gateway) {
            (Binary::TigGateway, None) => {
                return Err(invalid(
                    "tig-gateway requires [gateway] with api_key_file, lease_secs, \
                     platform and served_compute; none has a default because each \
                     is a fact about this deployment that a fallback would answer \
                     on its behalf"
                        .into(),
                ));
            }
            (Binary::TigGateway, Some(gateway)) => {
                if gateway.lease_secs < 1 {
                    return Err(invalid(format!(
                        "gateway.lease_secs must be at least 1, found {}",
                        gateway.lease_secs
                    )));
                }
                // `linux/<arch>`, matching what a registry reports and what
                // §13 check 3 compares byte for byte. A bare "arm64" would
                // never equal a resolved platform and would fail check 3 for
                // a reason no operator could act on.
                if !gateway.platform.starts_with("linux/") || gateway.platform.len() < 8 {
                    return Err(invalid(format!(
                        "gateway.platform must be `linux/<arch>`, found {:?}: §13 \
                         check 3 compares it against what a registry reports",
                        gateway.platform
                    )));
                }
                if gateway
                    .served_compute
                    .iter()
                    .any(|compute| compute.trim().is_empty())
                {
                    return Err(invalid(
                        "gateway.served_compute must not contain an empty entry; an \
                         empty string matches no challenge and would scope §13 \
                         check 6 to nothing"
                            .into(),
                    ));
                }
            }
            (other, Some(_)) => {
                return Err(invalid(format!(
                    "[gateway] belongs to tig-gateway; {} must not carry it",
                    other.as_str()
                )));
            }
            (_, None) => {}
        }

        Ok(())
    }

    /// Digest of the decision-affecting configuration, stored with each
    /// decision and accounting batch (`architecture.md` §9).
    ///
    /// Telemetry and connection details are deliberately excluded: changing
    /// a log level or a database host does not change what the pool would
    /// decide, and including them would make every decision record look
    /// different for no protocol reason.
    ///
    /// The digest covers `network` and, when a `[tig]` section is present,
    /// `player_id` — the pool's identity is a field of every §6.1 body a
    /// decision commits to, so two decisions taken under different identities
    /// must not carry the same config digest. Fields join it as the slices
    /// that introduce decision-affecting configuration land; the domain
    /// string is versioned so a change of coverage is never mistaken for a
    /// change of value: `v2` added `player_id` and `v3` the bootstrap offer.
    ///
    /// The offer joins it because it decides and is **not otherwise
    /// recoverable**. `compute_class` selects §6.2's eligible set, and
    /// `cpu_cores` drives §6.7's alignment — and while the resulting
    /// `num_bundles` is on the decision record, the core count that produced
    /// it is not, so two decisions taken under different offered cores would
    /// otherwise be indistinguishable.
    ///
    /// `[orchestration]`'s reserve fields are deliberately **not** here, and
    /// the test is not "are they decision-affecting" — they are. It is whether
    /// the decision record already shows them: criterion D2c writes
    /// `precommit_failure_charge_atoms` and `reserve_policy_version` into
    /// every intent's `reserve_inputs`, so a decision made under a different
    /// charge is already distinguishable from one made under another. Digesting
    /// them as well would add a second, weaker record of the same fact and a
    /// digest version bump each time an open policy value moved.
    ///
    /// `base_url` is deliberately not included. Which server the pool talks
    /// to does not change what it would decide, and a digest that varied
    /// between the fake and the pinned endpoint would make every fake-tig
    /// decision record look unlike a live one for no protocol reason.
    pub fn decision_digest(&self) -> [u8; 32] {
        let player = self.tig.as_ref().map_or("", |t| t.player_id.as_str());
        // `v3` adds the bootstrap offer. Rendered as three fields rather than
        // one, so a record can be read back against the configuration that
        // produced it, and absent as a distinct string rather than as empty
        // ones — a deployment that offers nothing is not one that offers a
        // nameless class with no cores.
        let offer = self
            .orchestration
            .as_ref()
            .and_then(|o| o.bootstrap_offer.as_ref())
            .map_or_else(
                || "none".to_string(),
                |offer| {
                    format!(
                        "{}\n{}\n{}",
                        offer.compute_class,
                        offer
                            .cpu_cores
                            .map_or_else(|| "-".to_string(), |c| c.to_string()),
                        offer.tig_compute_type,
                    )
                },
            );
        let preimage = format!(
            "tig-pool-config-digest-v3\n{}\n{player}\n{offer}",
            self.network
        );
        *blake3::hash(preimage.as_bytes()).as_bytes()
    }

    /// The digest in lowercase hex, the form stored alongside a decision.
    pub fn decision_digest_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.decision_digest() {
            use fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// A `postgres://` URL with the password read from `password_file` at
    /// call time.
    ///
    /// The returned string contains a secret: it is passed straight to the
    /// driver and never logged, stored, or included in an error
    /// (`architecture.md` §2.2).
    pub fn database_url(&self) -> Result<String, ConfigError> {
        let password = std::fs::read_to_string(&self.database.password_file).map_err(|source| {
            ConfigError::Read {
                path: self.database.password_file.clone(),
                source,
            }
        })?;
        let password = password.trim_end_matches(['\n', '\r']);

        // Percent-encode every component that a value could otherwise break
        // out of. A password containing `@`, `:`, `/`, `?` or `#` would
        // otherwise re-point the client at a different host or database
        // rather than being treated as data.
        Ok(format!(
            "postgres://{}:{}@{}:{}/{}?options={}",
            percent_encode(&self.database.user),
            percent_encode(password),
            percent_encode(&self.database.host),
            self.database.port,
            percent_encode(&self.database.name),
            // Applies the configured bound to every session on this
            // connection, so `statement_timeout_ms` is enforced rather than
            // merely declared.
            percent_encode(&format!(
                "-c statement_timeout={}",
                self.database.statement_timeout_ms
            )),
        ))
    }
}

/// Percent-encode everything outside the unreserved set of RFC 3986.
///
/// Deliberately conservative: encoding a character that did not need it is
/// harmless, while missing one changes where the client connects.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => {
                use fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}
