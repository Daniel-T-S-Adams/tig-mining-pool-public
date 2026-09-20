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

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::protocol::PROTOCOL_VERSION;

/// The pinned `ErrorResponse` body.
///
/// The optional fields are the protocol's diagnostic hints — §5's
/// `expected_offset` for a resumed upload, §8's `expected_event_seq` for an
/// out-of-order event. They are `None` here and carried by the routes that
/// have something to say; the schema forbids unknown properties, so a field
/// that does not apply must be absent rather than null.
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
}

impl ApiError {
    /// A request for something this protocol version does not route.
    /// Permanent: the route set is a fact of the version (§5), so retrying
    /// cannot make it appear.
    pub fn unknown_route() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error_code: "UNKNOWN_ROUTE",
            message: "no such route in this protocol version".to_owned(),
            retryable: false,
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
        }
    }

    /// The wire body for this error, as of `now`.
    pub fn body(&self, now: time::OffsetDateTime) -> ErrorResponse {
        ErrorResponse {
            protocol_version: PROTOCOL_VERSION,
            error_code: self.error_code,
            message: self.message.clone(),
            retryable: self.retryable,
            server_time: crate::protocol::rfc3339(now),
        }
    }

    /// The response, timestamped from the server's own clock (§13).
    pub fn into_response_at(self, now: time::OffsetDateTime) -> Response {
        let mut response = (self.status, Json(self.body(now))).into_response();
        response.extensions_mut().insert(ProtocolShaped);
        response
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.into_response_at(time::OffsetDateTime::now_utc())
    }
}
