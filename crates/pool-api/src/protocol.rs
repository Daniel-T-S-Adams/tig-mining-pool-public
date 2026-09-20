//! `GET /member/v0/protocol` — the one public route besides enrollment.
//!
//! `member_protocol.md` §4: it "is public and returns the exact protocol and
//! package versions the server currently accepts", and §13 adds server time
//! "so a client can diagnose skew". Its shape is pinned as
//! `ProtocolInfoResponse` in `schemas/member_protocol/v0.1.0/api.schema.json`,
//! including two `const` fields — the 300-second freshness window and the
//! 30-second heartbeat interval — which are therefore facts of the protocol
//! version rather than settings of this deployment.

use serde::Serialize;

/// The protocol version this build speaks.
///
/// One exact string, not a range: §4 says "an exact supported version is
/// required; the server does not guess compatibility from SemVer". It is the
/// version the pinned schema directory carries, and a build that spoke another
/// would have no schema to validate against.
pub const PROTOCOL_VERSION: &str = "0.1.0";

/// The proof-material package format this build accepts
/// (`member_protocol.md` §10).
pub const PACKAGE_FORMAT: &str = "proof-material-v1";

/// §3.2's freshness window, in seconds. Pinned by the schema as a `const`, so
/// it is not configurable: a deployment that widened it would accept requests
/// the protocol calls stale, and one that narrowed it would reject requests a
/// conforming agent is entitled to make.
pub const REQUEST_CLOCK_SKEW_SECONDS: u32 = 300;

/// §8's heartbeat cadence, in seconds. Pinned the same way.
pub const HEARTBEAT_INTERVAL_SECONDS: u32 = 30;

/// The §4 response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtocolInfo {
    pub supported_protocol_versions: Vec<String>,
    pub supported_package_formats: Vec<String>,
    /// RFC 3339, from the server's clock. §13: "Server time and TIG block
    /// height are authoritative. Member wall clocks are used only for request
    /// freshness and display."
    pub server_time: String,
    pub request_clock_skew_seconds: u32,
    pub heartbeat_interval_seconds: u32,
}

impl ProtocolInfo {
    /// What this build accepts, as of `now`.
    pub fn at(now: time::OffsetDateTime) -> Self {
        Self {
            supported_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
            supported_package_formats: vec![PACKAGE_FORMAT.to_string()],
            server_time: rfc3339(now),
            request_clock_skew_seconds: REQUEST_CLOCK_SKEW_SECONDS,
            heartbeat_interval_seconds: HEARTBEAT_INTERVAL_SECONDS,
        }
    }
}

/// Server time as the schema's `DateTime` — RFC 3339, whole seconds.
///
/// Whole seconds because §3.2's freshness window is 300 seconds wide and
/// sub-second precision states a confidence the protocol does not use.
pub fn rfc3339(now: time::OffsetDateTime) -> String {
    now.replace_nanosecond(0)
        .unwrap_or(now)
        .format(&time::format_description::well_known::Rfc3339)
        // An unformattable timestamp is not a reason to fail a read, and the
        // epoch is visibly wrong rather than quietly plausible — a client
        // diagnosing skew sees a bad answer instead of a convincing one.
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
