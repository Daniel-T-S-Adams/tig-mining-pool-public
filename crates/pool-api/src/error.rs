//! The one error shape.
//!
//! `member_protocol.md` §14 makes "an unknown enum, missing required field,
//! lossy integer, invalid digest, or schema mismatch … incompatible input,
//! not a value to coerce", and the pinned `ErrorResponse` in
//! `schemas/member_protocol/v0.1.0/common.schema.json` is what an incompatible
//! request is answered with. A member agent parses errors before it parses
//! anything else, so every failing response from this service carries that
//! shape — including the ones the framework generates rather than a handler
//! (see `service::protocol_shaped_errors`).
//!
//! One exception, and it is structural rather than an omission: every body the
//! schema defines carries `server_time`, so a clock with no RFC 3339 form
//! leaves nothing conforming to send. `service::no_server_time` answers that
//! with a bodyless `503`, which §13's retry rules already have a member back
//! off from.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::protocol::{PROTOCOL_VERSION, ServerTime};

/// The pinned `ErrorResponse` body.
///
/// It carries the five properties the schema requires and no others yet. The
/// optional ones each belong to a route that does not exist: `request_id` and
/// `enrollment_request_id` to §3.2's echo, which lands with the authentication
/// PR (slice-2 criterion A8); `expected_state`, `expected_offset` and
/// `expected_event_seq` to the assignment, upload and event routes that have
/// something to say. The schema forbids unknown properties, so each arrives
/// with the code that populates it rather than as an always-absent field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub protocol_version: &'static str,
    pub error_code: &'static str,
    pub message: String,
    /// Whether repeating the identical request could succeed.
    ///
    /// §13 has the member back off on retryable failures and stop on the
    /// rest; getting this wrong either spins an agent against a permanent
    /// rejection or strands work that a retry would have completed.
    pub retryable: bool,
    /// §3.2: "an error echoes `request_id` when the request carried the
    /// standard signed header". Absent rather than null when it did not —
    /// the schema forbids unknown properties and `Uuid` has no null form, so
    /// a public read or a failure before the header could be parsed simply
    /// omits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// §3.2's other echo: "or `enrollment_request_id` for a decoded
    /// enrollment attempt". A separate field, not the same one under a
    /// different meaning — an enrolling agent has no `X-Request-Id` to match
    /// against, and the schema gives each its own key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrollment_request_id: Option<String>,
    pub server_time: String,
}

/// Placed on every response this service wrote itself.
///
/// The framework's own rejections — `405` from routing, `413` from the body
/// limit — are restated by `service::protocol_shaped_errors`, which needs to
/// tell them from a handler's response that happens to share their status.
/// A marker rather than a guess at headers: tower-http's limit rejection
/// carries a `text/plain` body, so "has no content type" would have called it
/// a handler's work and passed it through unshaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolShaped;

/// A failure this service knows how to state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub error_code: &'static str,
    pub message: String,
    pub retryable: bool,
    /// The caller's own `X-Request-Id`, once it has been read.
    ///
    /// Echoed rather than withheld: it is the caller's value, so returning it
    /// discloses nothing, and §3.2 has them match a failure to the attempt
    /// that caused it. `None` before the header is parsed, and on the routes
    /// that have no signed header at all.
    pub request_id: Option<String>,
    /// The enrolling agent's own identifier, for the one route with no signed
    /// header to carry a request id (§3.2).
    pub enrollment_request_id: Option<String>,
}

impl ApiError {
    /// The same error, echoing the attempt it refused (§3.2).
    #[must_use]
    pub fn echoing(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.to_owned());
        self
    }

    /// The same, for an enrollment attempt, which carries no signed header
    /// and so has no `request_id` to echo (§3.2).
    #[must_use]
    pub fn echoing_enrollment(mut self, enrollment_request_id: &str) -> Self {
        self.enrollment_request_id = Some(enrollment_request_id.to_owned());
        self
    }

    /// A request for something this protocol version does not route.
    /// Permanent: the route set is a fact of the version (§5), so retrying
    /// cannot make it appear.
    pub fn unknown_route() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error_code: "UNKNOWN_ROUTE",
            message: "no such route in this protocol version".to_owned(),
            retryable: false,
            request_id: None,
            enrollment_request_id: None,
        }
    }

    /// The right path with the wrong method — the same kind of mismatch as an
    /// unknown route, and permanent for the same reason.
    pub fn method_not_allowed() -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            error_code: "METHOD_NOT_ALLOWED",
            message: "that route does not accept this method".to_owned(),
            retryable: false,
            request_id: None,
            enrollment_request_id: None,
        }
    }

    /// A control body above `member_api.max_control_body_bytes`.
    ///
    /// Not retryable: the limit is a property of the deployment, so the
    /// identical request would be refused identically. A member with more to
    /// say uses the upload routes, which are bounded by the session's own
    /// chunk size rather than by this.
    pub fn body_too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error_code: "BODY_TOO_LARGE",
            message: "control message body exceeds this deployment's limit".to_owned(),
            retryable: false,
            request_id: None,
            enrollment_request_id: None,
        }
    }

    /// The wire body for this error, as of `server_time`.
    pub fn body(&self, server_time: &ServerTime) -> ErrorResponse {
        ErrorResponse {
            protocol_version: PROTOCOL_VERSION,
            error_code: self.error_code,
            message: self.message.clone(),
            retryable: self.retryable,
            request_id: self.request_id.clone(),
            enrollment_request_id: self.enrollment_request_id.clone(),
            server_time: server_time.as_str().to_owned(),
        }
    }

    /// The response, timestamped from the server's own clock (§13).
    pub fn into_response_at(self, server_time: &ServerTime) -> Response {
        let mut response = (self.status, Json(self.body(server_time))).into_response();
        response.extensions_mut().insert(ProtocolShaped);
        response
    }
}
