//! The service: what is served, where it listens, and how it stops.
//!
//! `architecture.md` §11.2 puts a TLS proxy in front of this process, so it
//! binds a private address and terminates plain HTTP behind it. `security.md`
//! §4.3's edge controls — TLS, header limits, connection limits, per-IP rate
//! limits — belong to that proxy; what lives here is everything the proxy
//! cannot know: who the caller is, and what they are allowed to touch.

use std::net::SocketAddr;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use pool_config::Config;
use tower_http::limit::RequestBodyLimitLayer;

use crate::error::{ApiError, ProtocolShaped};
use crate::protocol::ProtocolInfo;

/// The body limit applied when a caller builds a router without configuration.
/// Only reachable from tests; a binary always has `[member_api]`, which
/// `pool-config` requires for `Binary::PoolApi`.
const FALLBACK_BODY_LIMIT: u64 = 262_144;

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    /// The clock this service answers with. A field rather than a call to
    /// `now()` inside each handler so a test can pin it: §13 makes server
    /// time authoritative for skew diagnosis, and a value no test can control
    /// is a value no test can check.
    pub now: fn() -> time::OffsetDateTime,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            now: time::OffsetDateTime::now_utc,
        }
    }
}

/// The router, without a listener, so tests drive it directly.
pub fn app(config: &Config, state: AppState) -> Router {
    let limit = config
        .member_api
        .as_ref()
        .map_or(FALLBACK_BODY_LIMIT, |api| api.max_control_body_bytes);

    Router::new()
        .route("/member/v0/protocol", get(protocol))
        // An unrouted path is incompatible input (§14), not an absence to
        // report in the framework's own words.
        .fallback(unknown_route)
        .layer(RequestBodyLimitLayer::new(
            usize::try_from(limit).unwrap_or(usize::MAX),
        ))
        // Outside the limit layer, so its rejection passes through here too.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            protocol_shaped_errors,
        ))
        .with_state(state)
}

/// §4's public read.
async fn protocol(State(state): State<AppState>) -> Json<ProtocolInfo> {
    Json(ProtocolInfo::at((state.now)()))
}

async fn unknown_route(State(state): State<AppState>) -> Response {
    ApiError::unknown_route().into_response_at((state.now)())
}

/// Restate the responses this service did not write itself.
///
/// Routing and the body limit answer before any handler runs, in the
/// framework's shapes: a bare `405` for a method the route does not take,
/// a `text/plain` `413` for an oversized body. A member agent reads every
/// failure through one parser (§14), so a response with no `error_code` is one
/// it can only report as "unknown". A response this service wrote carries
/// `ProtocolShaped` and is left exactly as the handler stated it.
async fn protocol_shaped_errors(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    let Some(error) = framework_error(&response) else {
        return response;
    };
    error.into_response_at((state.now)())
}

/// Which framework-generated status this response is, if any.
///
/// A free function rather than a branch inside the middleware so a test can
/// hand it a response and read the verdict, rather than inferring the rule
/// from an HTTP round trip.
fn framework_error(response: &Response) -> Option<ApiError> {
    // A response this service wrote has said something more specific than a
    // status code could.
    if response.extensions().get::<ProtocolShaped>().is_some() {
        return None;
    }
    match response.status() {
        StatusCode::METHOD_NOT_ALLOWED => Some(ApiError::method_not_allowed()),
        StatusCode::PAYLOAD_TOO_LARGE => Some(ApiError::body_too_large()),
        _ => None,
    }
}

/// Serve until the process is asked to stop.
///
/// The address is parsed at config load (`pool-config`'s `[member_api]`
/// rules), so a bad one fails before telemetry says "listening" — a service
/// that starts and then cannot bind is the fail-open shape `architecture.md`
/// §9 refuses, because it looks healthy.
pub async fn run(config: &Config) -> Result<(), String> {
    let api = config
        .member_api
        .as_ref()
        .ok_or_else(|| "pool-api requires [member_api]".to_owned())?;
    let addr: SocketAddr = api
        .listen
        .parse()
        .map_err(|e| format!("member_api.listen is not an address: {e}"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;

    tracing::info!(
        event = "api.listening",
        address = %addr,
        protocol_version = crate::protocol::PROTOCOL_VERSION,
        max_control_body_bytes = api.max_control_body_bytes,
        "member api is serving"
    );

    axum::serve(listener, app(config, AppState::default()))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|e| format!("serve failed: {e}"))
}

/// SIGTERM or Ctrl-C. A member's upload is a long request, so the server
/// drains rather than dropping: §13's retry rules make a dropped control
/// request cheap and a dropped chunk expensive.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // Without a handler the default disposition still terminates the
            // process; there is nothing to fall back to, and nothing to log
            // that an operator could act on.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }

    tracing::info!(event = "api.shutdown", "draining and stopping");
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;

    use super::*;

    fn empty(status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    #[test]
    fn the_two_framework_statuses_are_restated_and_nothing_else_is() {
        assert_eq!(
            framework_error(&empty(StatusCode::METHOD_NOT_ALLOWED)).map(|e| e.error_code),
            Some("METHOD_NOT_ALLOWED")
        );
        assert_eq!(
            framework_error(&empty(StatusCode::PAYLOAD_TOO_LARGE)).map(|e| e.error_code),
            Some("BODY_TOO_LARGE")
        );
        // A success is not an error, and a status this service does state
        // itself is not the framework's to restate.
        for status in [
            StatusCode::OK,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNAUTHORIZED,
        ] {
            assert!(
                framework_error(&empty(status)).is_none(),
                "{status} must pass through"
            );
        }
    }

    #[test]
    fn a_handlers_own_error_is_left_alone() {
        // The same statuses, but written here: rewriting one would replace a
        // specific `error_code` with a generic one.
        for error in [ApiError::body_too_large(), ApiError::method_not_allowed()] {
            let written = error.into_response_at(time::OffsetDateTime::UNIX_EPOCH);
            assert!(framework_error(&written).is_none());
        }
    }

    #[test]
    fn a_framework_rejection_with_a_body_is_still_restated() {
        // tower-http's limit rejection is `text/plain`, not empty. Telling
        // framework from handler by "carries a body" would pass this one
        // through in a shape no member agent parses.
        let rejection = Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header(axum::http::header::CONTENT_TYPE, "text/plain")
            .body(axum::body::Body::from("length limit exceeded"))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        assert_eq!(
            framework_error(&rejection).map(|e| e.error_code),
            Some("BODY_TOO_LARGE")
        );
    }
}
