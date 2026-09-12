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
            }
            (other, Some(_)) => {
                return Err(invalid(format!(
                    "[orchestration] belongs to pool-controller; {} must not carry it",
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
    /// change of value, and `v2` is the version that added `player_id`.
    ///
    /// `base_url` is deliberately not included. Which server the pool talks
    /// to does not change what it would decide, and a digest that varied
    /// between the fake and the pinned endpoint would make every fake-tig
    /// decision record look unlike a live one for no protocol reason.
    pub fn decision_digest(&self) -> [u8; 32] {
        let player = self.tig.as_ref().map_or("", |t| t.player_id.as_str());
        let preimage = format!("tig-pool-config-digest-v2\n{}\n{player}", self.network);
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
