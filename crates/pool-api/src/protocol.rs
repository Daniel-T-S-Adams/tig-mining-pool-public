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

/// Server time, already expressed as the schema's `DateTime`.
///
/// A type rather than a `String` because expressing the clock is the one step
/// that can fail, and `member_protocol.md` §13 makes the value authoritative:
/// "server time and TIG block height are authoritative". A response that
/// invented a timestamp when formatting failed would publish a wrong
/// authoritative value, which is worse than not answering. Constructing this
/// is therefore the single fallible step, and every body that carries server
/// time is built from one that already exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerTime(String);

impl ServerTime {
    /// Express `now` as RFC 3339 with whole seconds.
    ///
    /// Whole seconds because §3.2's freshness window is 300 seconds wide and
    /// sub-second precision states a confidence the protocol does not use.
    ///
    /// `None` when the instant has no RFC 3339 form at all — a year outside
    /// 0..=9999, which `OffsetDateTime::now_utc` cannot produce on a working
    /// clock. The caller answers without a body rather than with a wrong one.
    pub fn at(now: time::OffsetDateTime) -> Option<Self> {
        now.replace_nanosecond(0)
            .unwrap_or(now)
            .format(&time::format_description::well_known::Rfc3339)
            .ok()
            .map(Self)
    }

    /// The wire value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
    /// What this build accepts, as of `server_time`.
    pub fn at(server_time: &ServerTime) -> Self {
        Self {
            supported_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            supported_package_formats: vec![PACKAGE_FORMAT.to_owned()],
            server_time: server_time.as_str().to_owned(),
            request_clock_skew_seconds: REQUEST_CLOCK_SKEW_SECONDS,
            heartbeat_interval_seconds: HEARTBEAT_INTERVAL_SECONDS,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_normal_instant_has_a_wire_form_and_carries_whole_seconds() {
        let now = time::OffsetDateTime::from_unix_timestamp(1_774_000_000)
            .expect("a valid instant")
            .replace_nanosecond(123_456_789)
            .expect("a valid nanosecond");
        let server_time = ServerTime::at(now).expect("a representable instant");
        assert_eq!(server_time.as_str(), "2026-03-20T09:46:40Z");
    }

    #[test]
    fn an_instant_with_no_rfc_3339_form_has_none() {
        // The branch that used to answer with the Unix epoch. A year before
        // zero has no RFC 3339 form, so the only honest answers are "no value"
        // and a fabricated one; this is the test that pins which.
        let year_minus_one = time::Date::from_calendar_date(-1, time::Month::January, 1)
            .expect("a constructible date")
            .midnight()
            .assume_utc();
        assert_eq!(ServerTime::at(year_minus_one), None);
    }
}
