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
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use pool_config::{Config, MemberApiConfig};
use tower_http::limit::RequestBodyLimitLayer;

use crate::error::{ApiError, ProtocolShaped};
use crate::protocol::{ProtocolInfo, ServerTime};

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
///
/// Takes the member-API settings rather than the whole `Config` so there is
/// nothing to fall back to: `max_control_body_bytes` has no default, and a
/// builder that could supply one would be a limit nobody chose.
pub fn app(api: &MemberApiConfig, state: AppState) -> Router {
    assemble(
        control_routes(api.max_control_body_bytes),
        raw_body_routes(),
        state,
    )
}

/// The two groups, the fallback, and the shaping middleware.
///
/// Separate from `app` so a test can assemble the real thing around a probe
/// route in the raw group: the claim that the control limit does not reach
/// that group is otherwise unobservable while the group is empty, and an
/// unobservable claim is one that stops being true without anything failing.
fn assemble(control: Router<AppState>, raw: Router<AppState>, state: AppState) -> Router {
    control
        .merge(raw)
        // Outside both groups: an unrouted path is incompatible input (§14),
        // not an absence to report in the framework's own words, and it is
        // answered without reading whatever body arrived with it.
        .fallback(unknown_route)
        // Outermost, so a rejection from either group passes through here.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            protocol_shaped_errors,
        ))
        .with_state(state)
}

/// Routes whose body is a JSON control message.
///
/// `route_layer`, not `layer`: the bound belongs to these routes and to
/// nothing else. `member_protocol.md` §10.3 puts an upload chunk at 1–64 MiB,
/// so a chunk `PUT` registered under this layer would be capped at whatever a
/// deployment chose for control messages — a limit from the wrong contract,
/// and one no test would notice until a member's upload failed.
fn control_routes(max_control_body_bytes: u64) -> Router<AppState> {
    Router::new()
        .route("/member/v0/protocol", get(protocol))
        .route_layer(RequestBodyLimitLayer::new(
            usize::try_from(max_control_body_bytes).unwrap_or(usize::MAX),
        ))
}

/// Routes whose body is raw bytes, bounded by the upload session's own chunk
/// size rather than by `max_control_body_bytes`.
///
/// Empty until `PUT /member/v0/uploads/{upload_id}` lands (slice-2 criterion
/// G3). It exists now so that route has somewhere to go that is not under the
/// control limit, and so the separation is something a test can check today:
/// see `the_control_limit_does_not_reach_the_raw_body_group`.
fn raw_body_routes() -> Router<AppState> {
    Router::new()
}

/// §4's public read.
async fn protocol(State(state): State<AppState>) -> Response {
    let Some(server_time) = ServerTime::at((state.now)()) else {
        return no_server_time();
    };
    // `no-store` because the body carries server time, and §13 makes that the
    // value a member diagnoses skew against. A proxy replaying a cached answer
    // would hand an agent a stale clock and a confident explanation for it.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(ProtocolInfo::at(&server_time)),
    )
        .into_response()
}

async fn unknown_route(State(state): State<AppState>) -> Response {
    let Some(server_time) = ServerTime::at((state.now)()) else {
        return no_server_time();
    };
    ApiError::unknown_route().into_response_at(&server_time)
}

/// The one response this service cannot shape.
///
/// Every body the schema defines carries `server_time`, so a clock with no
/// RFC 3339 form leaves nothing conforming to send. §13 makes that value
/// authoritative, and a fabricated authoritative time is worse than an
/// unexplained failure: the member would diagnose skew against a lie. `503`,
/// because a clock is a thing an operator fixes.
fn no_server_time() -> Response {
    tracing::error!(
        event = "api.server_time_unrepresentable",
        "the server clock has no RFC 3339 form; answering without a body"
    );
    StatusCode::SERVICE_UNAVAILABLE.into_response()
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
    let Some(server_time) = ServerTime::at((state.now)()) else {
        return no_server_time();
    };
    error.into_response_at(&server_time)
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

    axum::serve(listener, app(api, AppState::default()))
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn empty(status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    /// A raw-body route stands in for the chunk `PUT`: it reads its body in
    /// full and reports the length, so a limit that reached it would show up
    /// as a rejection rather than as a number.
    async fn probe(body: axum::body::Bytes) -> String {
        body.len().to_string()
    }

    async fn post_bytes(router: Router, path: &str, len: usize) -> (StatusCode, String) {
        use tower::ServiceExt;

        let request = axum::http::Request::post(path)
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .header(axum::http::header::CONTENT_LENGTH, len.to_string())
            .body(axum::body::Body::from(vec![b'x'; len]))
            .expect("a well-formed request");
        let response = router.oneshot(request).await.expect("the router answers");
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("a readable body")
            .to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn the_control_limit_does_not_reach_the_raw_body_group() {
        // `member_protocol.md` §10.3 puts an upload chunk at 1-64 MiB, so a
        // chunk `PUT` bounded by `max_control_body_bytes` would be capped by
        // the wrong contract. The bound is a `route_layer` on the control
        // group for exactly that reason, and this is what says so.
        let limit = 64;
        let raw = Router::new().route("/probe/raw", axum::routing::post(probe));
        let router = assemble(control_routes(limit), raw, AppState::default());

        let (status, body) = post_bytes(router, "/probe/raw", limit as usize * 4).await;
        assert_eq!(status, StatusCode::OK, "the raw group must not be capped");
        assert_eq!(body, (limit * 4).to_string(), "the whole body must arrive");
    }

    #[tokio::test]
    async fn the_control_group_is_capped_by_the_same_number() {
        // The other half: without this, the test above would also pass with
        // the limit removed from both groups.
        let limit = 64;
        let control = control_routes(limit)
            .route("/probe/control", axum::routing::post(probe))
            // The same bound the group's own routes carry, applied the same
            // way, so the probe is inside the group rather than beside it.
            .route_layer(RequestBodyLimitLayer::new(limit as usize));
        let router = assemble(control, raw_body_routes(), AppState::default());

        let (status, _) = post_bytes(router, "/probe/control", limit as usize * 4).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
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
        let server_time =
            ServerTime::at(time::OffsetDateTime::UNIX_EPOCH).expect("the epoch is representable");
        for error in [ApiError::body_too_large(), ApiError::method_not_allowed()] {
            let written = error.into_response_at(&server_time);
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
