//! The service: what is served, where it listens, and how it stops.
//!
//! `architecture.md` §11.2 puts a TLS proxy in front of this process, so it
//! binds a private address and terminates plain HTTP behind it. `security.md`
//! §4.3's edge controls — TLS, header limits, connection limits, per-IP rate
//! limits — belong to that proxy; what lives here is everything the proxy
//! cannot know: who the caller is, and what they are allowed to touch.

use std::net::SocketAddr;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use pool_config::{Config, MemberApiConfig};
use tower_http::limit::RequestBodyLimitLayer;

use crate::error::{ApiError, ProtocolShaped};
use crate::protocol::{ProtocolInfo, ServerTime};
use crate::ticket_key::{self, TicketKey};

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
    bounded(
        Router::new().route("/member/v0/protocol", get(protocol)),
        max_control_body_bytes,
    )
}

/// Put `routes` under `limit`, and under nothing else.
///
/// `route_layer` wraps only the routes registered before it runs, so the
/// routes and the bound have to be applied together — which is why this takes
/// the group rather than being folded into its caller. A test that registered
/// a probe after the bound would be measuring the layer it installed itself,
/// and would pass with this one deleted.
///
/// Two layers, because two limits would otherwise apply. `axum` caps every
/// body-consuming extractor at 2 MiB unless `DefaultBodyLimit` says otherwise,
/// so any group wanting more than that would find its own number silently
/// replaced — a limit nobody chose, which is the thing `MemberApiConfig`
/// exists to prevent, and which matters most for the group whose contract
/// starts at 1 MiB and runs to 64.
fn bounded(routes: Router<AppState>, limit: u64) -> Router<AppState> {
    routes
        .route_layer(RequestBodyLimitLayer::new(
            usize::try_from(limit).unwrap_or(usize::MAX),
        ))
        .route_layer(DefaultBodyLimit::disable())
}

/// Routes whose body is raw bytes rather than a JSON control message.
///
/// **Empty, and deliberately unbounded, because there is nothing here to
/// bound.** axum panics on a `route_layer` applied to a router with no routes
/// — "adding a route_layer before any routes is a no-op" — so the group's
/// bound cannot be installed ahead of its first route. It arrives with that
/// route, which is `PUT /member/v0/uploads/{upload_id}` (slice-2 criterion
/// G3), and it must arrive as `bounded(routes, LARGEST_PROTOCOL_BODY_BYTES)`.
///
/// That ceiling is `member_protocol.md` §10.3's 64 MiB chunk, the largest
/// single body this protocol defines. It is an outer bound and not the one
/// that decides a request: §11 gives each upload session its own chunk size
/// within §10.3's 1–64 MiB range, which is a fact about that session rather
/// than about the router, so the handler checks the declared session itself.
///
/// The ceiling is needed rather than optional, because the alternative is not
/// "no limit" but axum's own 2 MiB — *below* the range §10.3 defines, so a
/// chunk route registered without it would be refused at 2 MiB by a limit
/// nobody chose. `the_raw_group_carries_the_protocols_own_ceiling` pins the
/// shape that route has to take, so the requirement is a failing test rather
/// than a paragraph someone has to remember.
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

    // Before the listener. `security.md` §4.1 hashes every enrollment and
    // recovery ticket under this key, so a deployment without a usable one
    // cannot issue or redeem a ticket — and a service that accepted
    // connections first would discover that at a member's first enrollment
    // rather than at startup, where an operator is watching.
    let _ticket_key = load_ticket_key(api)?;
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

/// Everything that must be true before this process accepts a connection.
///
/// Returns nothing. An earlier version returned the loaded [`TicketKey`], and
/// that was a public key-loading path: any crate linking `pool-api` could
/// call it and make its own process read the key file, which is precisely
/// what B9 and `security.md` §4.1 forbid — "the ticket HMAC key stays in the
/// Pool API alone". `load` and `hmac` being crate-private did not save it,
/// because a public function handing the key out is a public way to obtain
/// one. `tig-gateway` exposes nothing public returning a `TigApiKey`, which
/// is why its identical claim holds, and this now matches.
///
/// Public because `pool-api check` and the boundary script's positive
/// control need *something* public to call; what they need is the verdict,
/// not the key.
pub fn preflight(api: &MemberApiConfig) -> Result<(), String> {
    load_ticket_key(api).map(|_| ())
}

/// The same work, keeping the key. Crate-private: the key does not leave.
fn load_ticket_key(api: &MemberApiConfig) -> Result<TicketKey, String> {
    let key = ticket_key::load(&api.ticket_hmac_key_file).map_err(|e| e.to_string())?;
    // Reported as presence, never as the key or its length
    // (`TicketKey::Debug` prints neither).
    tracing::info!(
        event = "api.ticket_key.loaded",
        path = %api.ticket_hmac_key_file.display(),
        present = key.is_present(),
        "the ticket HMAC key is readable by this process alone"
    );
    Ok(key)
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

    // Only the tests name these: production code has no route to bound with
    // the protocol ceiling yet, and the control bound arrives as
    // configuration rather than as a constant.
    use pool_config::{LARGEST_CONFORMING_CONTROL_BODY_BYTES, LARGEST_PROTOCOL_BODY_BYTES};

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

    /// The body is the protocol's `ErrorResponse`, carrying `code`.
    ///
    /// Only `protocol_shaped_errors` produces this, so asserting it is what
    /// makes a test notice a router assembled without that layer — a status
    /// alone would look the same.
    fn assert_protocol_error(body: &str, code: &str) {
        let parsed: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("a protocol error body, got {body:?} ({e})"));
        assert_eq!(parsed["error_code"], code);
        assert_eq!(
            parsed["protocol_version"],
            crate::protocol::PROTOCOL_VERSION
        );
        assert!(parsed["server_time"].is_string());
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

    /// The raw group as it will look once it has a route: a body-reading
    /// handler under `LARGEST_PROTOCOL_BODY_BYTES` and nothing else. This is
    /// the shape `raw_body_routes` documents as required of the chunk `PUT`,
    /// built the one way a bound can be applied — with the routes.
    fn raw_group_with_probe() -> Router<AppState> {
        bounded(
            Router::new().route("/probe/raw", axum::routing::post(probe)),
            LARGEST_PROTOCOL_BODY_BYTES,
        )
    }

    #[tokio::test]
    async fn the_control_limit_does_not_reach_the_raw_body_group() {
        // `member_protocol.md` §10.3 puts an upload chunk at 1-64 MiB, so a
        // chunk `PUT` bounded by `max_control_body_bytes` would be capped by
        // the wrong contract. The bound is a `route_layer` on the control
        // group for exactly that reason, and this is what says so.
        //
        // Three MiB, not three hundred bytes. A small body would pass whatever
        // the raw group's bound turned out to be — including axum's own 2 MiB,
        // which is below the range §10.3 defines and would refuse a real
        // chunk. The number has to be one the wrong answer fails at.
        let control_limit = LARGEST_CONFORMING_CONTROL_BODY_BYTES;
        let router = assemble(
            control_routes(control_limit),
            raw_group_with_probe(),
            AppState::default(),
        );

        let chunk = 3 * 1024 * 1024;
        assert!(
            chunk > control_limit as usize,
            "the probe body must exceed the control limit, or this proves nothing"
        );
        let (status, body) = post_bytes(router, "/probe/raw", chunk).await;
        assert_eq!(status, StatusCode::OK, "the raw group must not be capped");
        assert_eq!(body, chunk.to_string(), "the whole body must arrive");
    }

    #[tokio::test]
    async fn the_raw_group_carries_the_protocols_own_ceiling() {
        // The other half. Disabling axum's default without putting the
        // protocol's ceiling in its place would leave the group unbounded, so
        // a route added there later would read whatever arrived — which is a
        // worse answer than the wrong limit it replaced.
        //
        // Declared rather than sent: tower-http refuses on the content length,
        // so the test does not have to allocate 64 MiB to find the edge.
        let router = assemble(
            control_routes(LARGEST_CONFORMING_CONTROL_BODY_BYTES),
            raw_group_with_probe(),
            AppState::default(),
        );

        let request = axum::http::Request::post("/probe/raw")
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .header(
                axum::http::header::CONTENT_LENGTH,
                (LARGEST_PROTOCOL_BODY_BYTES + 1).to_string(),
            )
            .body(axum::body::Body::empty())
            .expect("a well-formed request");
        let response = tower::ServiceExt::oneshot(router, request)
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("a readable body")
            .to_bytes();
        assert_protocol_error(&String::from_utf8_lossy(&bytes), "BODY_TOO_LARGE");
    }

    /// The probe, inside the control group and under the group's own bound —
    /// `bounded` is handed the routes, exactly as `control_routes` hands it
    /// the real ones. Registering the probe afterwards and applying a second
    /// layer would measure that layer instead, and would pass with the group's
    /// bound deleted.
    fn control_group_with_probe(limit: u64) -> Router<AppState> {
        bounded(
            Router::new().route("/probe/control", axum::routing::post(probe)),
            limit,
        )
    }

    #[tokio::test]
    async fn the_control_group_is_capped_by_the_same_number() {
        // The other half: without this, the test above would also pass with
        // the limit removed from both groups.
        let limit = 64;
        let router = assemble(
            control_group_with_probe(limit),
            raw_body_routes(),
            AppState::default(),
        );

        let (status, body) = post_bytes(router, "/probe/control", limit as usize * 4).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        // In the pinned shape, which only the shaping middleware supplies —
        // §14 has the member parse every failure through one parser, and a
        // router assembled some other way would answer the same status with a
        // body it cannot read.
        assert_protocol_error(&body, "BODY_TOO_LARGE");

        // And a body at the bound arrives whole, so the case above is a bound
        // rather than a layer that refuses everything.
        let router = assemble(
            control_group_with_probe(limit),
            raw_body_routes(),
            AppState::default(),
        );
        let (status, body) = post_bytes(router, "/probe/control", limit as usize).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, limit.to_string());
    }

    #[tokio::test]
    async fn the_configured_limit_is_the_only_one_that_applies() {
        // `axum` caps a body-consuming extractor at 2 MiB of its own accord.
        // A deployment naming more than that would otherwise be refused at
        // 2 MiB while its configuration said something else — and
        // `LARGEST_PROTOCOL_BODY_BYTES` lets it name up to 64 MiB.
        let limit = 4 * 1024 * 1024;
        let router = assemble(
            control_group_with_probe(limit),
            raw_body_routes(),
            AppState::default(),
        );

        let over_axums_default = 3 * 1024 * 1024;
        let (status, body) = post_bytes(router, "/probe/control", over_axums_default).await;
        assert_eq!(status, StatusCode::OK, "the configured limit is the bound");
        assert_eq!(body, over_axums_default.to_string());
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
