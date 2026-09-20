//! Slice-2 criteria A1–A3: the member surface exists, states one protocol
//! version, and answers every failure in the shape the protocol pins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use pool_api::protocol::{
    HEARTBEAT_INTERVAL_SECONDS, PACKAGE_FORMAT, PROTOCOL_VERSION, ProtocolInfo,
    REQUEST_CLOCK_SKEW_SECONDS, ServerTime,
};
use pool_api::service::{AppState, app};
use pool_config::{
    Binary, Config, LARGEST_CONFORMING_CONTROL_BODY_BYTES, LARGEST_PROTOCOL_BODY_BYTES,
    MemberApiConfig,
};
use serde_json::Value;
use tower::ServiceExt;

mod support;

/// A pinned clock, so `server_time` is a value the test states rather than
/// one it reads back from the thing under test.
const FIXED_NOW: i64 = 1_774_000_000;

fn fixed_now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(FIXED_NOW).expect("a valid instant")
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pool-api-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("db-password"), b"local-dev-only").unwrap();
        // Synthetic, never a real key: `validate_for` only checks that a
        // path is named, and these tests never load it.
        std::fs::write(dir.join("ticket-key"), [0x5a; 32]).unwrap();
        Self { dir }
    }

    /// The member-API settings `pool-api` starts on, with one value the
    /// caller picks. Built through `Config::load` rather than by hand, so a
    /// value these tests use is one a deployment could actually name.
    fn member_api(&self, max_control_body_bytes: u64) -> MemberApiConfig {
        self.config(max_control_body_bytes)
            .member_api
            .expect("validate_for requires the section for pool-api")
    }

    /// A configuration `pool-api` starts on, with one value the caller picks.
    fn config(&self, max_control_body_bytes: u64) -> Config {
        let password_file = self.dir.join("db-password").display().to_string();
        let key_file = self.dir.join("ticket-key").display().to_string();
        let path = self.dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
network = "testnet"

[database]
host = "127.0.0.1"
port = 5433
name = "pool_dev"
user = "pool_api"
password_file = "{password_file}"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "test"

[member_api]
listen = "127.0.0.1:8081"
ticket_hmac_key_file = "{key_file}"
pool_domain = "test.invalid"
login_chain_id = 84532
max_control_body_bytes = {max_control_body_bytes}
"#
            ),
        )
        .unwrap();
        Config::load(&path, Binary::PoolApi).expect("a pool-api config loads")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn send(
    api: &MemberApiConfig,
    request: Request<Body>,
) -> (StatusCode, Option<String>, Value) {
    send_with(api, AppState { now: fixed_now }, request).await
}

/// The same round trip, also reporting `Cache-Control`.
async fn send_reading_cache_control(
    api: &MemberApiConfig,
    request: Request<Body>,
) -> (StatusCode, Option<String>, Option<String>, Value) {
    let response = app(api, AppState { now: fixed_now })
        .oneshot(request)
        .await
        .expect("the router answers");
    let header_value = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap_or_default().to_owned())
    };
    let content_type = header_value(header::CONTENT_TYPE);
    let cache_control = header_value(header::CACHE_CONTROL);
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).expect("a JSON body");
    (status, content_type, cache_control, body)
}

async fn send_with(
    api: &MemberApiConfig,
    state: AppState,
    request: Request<Body>,
) -> (StatusCode, Option<String>, Value) {
    let response = app(api, state)
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap_or_default().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "body is not JSON ({e}): {:?}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, content_type, body)
}

#[tokio::test]
async fn the_protocol_read_is_public_and_conforms_to_the_pinned_schema() {
    let scratch = Scratch::new("protocol");
    let api = scratch.member_api(524_288);

    // No credential, no signature: §4 makes this route public, and an agent
    // must be able to ask what the server speaks before it can enroll.
    let (status, content_type, cache_control, body) = send_reading_cache_control(
        &api,
        Request::get("/member/v0/protocol")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    support::assert_conforms("api.schema.json#ProtocolInfoResponse", &body);
    // The body carries server time, and §13 makes that the value a member
    // diagnoses skew against, so a cached copy is a stale clock presented as
    // the server's own.
    assert_eq!(cache_control.as_deref(), Some("no-store"));

    // The schema pins the two `const` fields; this pins the rest, including
    // that server time is the server's and not the caller's.
    assert_eq!(
        body["supported_protocol_versions"],
        serde_json::json!(["0.1.0"])
    );
    assert_eq!(
        body["supported_package_formats"],
        serde_json::json!(["proof-material-v1"])
    );
    assert_eq!(body["server_time"], "2026-03-20T09:46:40Z");
    assert_eq!(body["request_clock_skew_seconds"], 300);
    assert_eq!(body["heartbeat_interval_seconds"], 30);
}

#[test]
fn an_error_conforms_with_and_without_the_echoed_request_id() {
    // §3.2 has an error echo `request_id` when the request carried the signed
    // header, and omit it otherwise. `ErrorResponse` forbids unknown
    // properties and gives `request_id` no null form, so "omit" has to mean
    // absent rather than null — which is a serialization detail the schema is
    // the only thing that checks.
    let server_time = ServerTime::at(fixed_now()).expect("a representable instant");
    let request_id = "0123abcd-4567-89ab-cdef-0123456789ab";

    let bare = pool_api::error::ApiError::unknown_route().body(&server_time);
    let bare = serde_json::to_value(&bare).expect("serializable");
    support::assert_conforms("common.schema.json#ErrorResponse", &bare);
    assert!(
        bare.get("request_id").is_none(),
        "absent, not null: {bare:#}"
    );

    let echoed = pool_api::error::ApiError::unknown_route()
        .echoing(request_id)
        .body(&server_time);
    let echoed = serde_json::to_value(&echoed).expect("serializable");
    support::assert_conforms("common.schema.json#ErrorResponse", &echoed);
    assert_eq!(echoed["request_id"], request_id);
}

#[test]
fn the_constants_this_build_serves_are_the_ones_the_schema_pins() {
    // `ProtocolVersion`, `PackageFormat` and the two `const` numbers are facts
    // of the protocol version rather than settings of this deployment
    // (`member_protocol.md` §4). Read out of the schema files so a build that
    // drifted from them fails here rather than at a member agent.
    let server_time = ServerTime::at(fixed_now()).expect("a representable instant");
    let info = serde_json::to_value(ProtocolInfo::at(&server_time)).unwrap();
    support::assert_conforms("api.schema.json#ProtocolInfoResponse", &info);

    assert_eq!(PROTOCOL_VERSION, "0.1.0");
    assert_eq!(PACKAGE_FORMAT, "proof-material-v1");
    assert_eq!(REQUEST_CLOCK_SKEW_SECONDS, 300);
    assert_eq!(HEARTBEAT_INTERVAL_SECONDS, 30);
}

#[tokio::test]
async fn an_unrouted_path_is_refused_in_the_protocols_own_error_shape() {
    let scratch = Scratch::new("unknown-route");
    let api = scratch.member_api(524_288);

    let (status, content_type, body) = send(
        &api,
        Request::get("/member/v0/nope").body(Body::empty()).unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    support::assert_conforms("common.schema.json#ErrorResponse", &body);
    assert_eq!(body["error_code"], "UNKNOWN_ROUTE");
    // §14: the route set is a fact of the version, so retrying cannot make
    // this one appear.
    assert_eq!(body["retryable"], false);
    assert_eq!(body["protocol_version"], "0.1.0");
    assert_eq!(body["server_time"], "2026-03-20T09:46:40Z");
}

#[tokio::test]
async fn the_wrong_method_on_a_real_route_is_refused_in_the_same_shape() {
    let scratch = Scratch::new("method");
    let api = scratch.member_api(524_288);

    // Axum answers this before any handler runs, with an empty body. A member
    // agent parses every failure through one parser, so an empty body is a
    // failure it can only report as "unknown".
    let (status, content_type, body) = send(
        &api,
        Request::post("/member/v0/protocol")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    support::assert_conforms("common.schema.json#ErrorResponse", &body);
    assert_eq!(body["error_code"], "METHOD_NOT_ALLOWED");
}

#[tokio::test]
async fn a_body_above_the_configured_limit_is_refused_in_the_same_shape() {
    let scratch = Scratch::new("limit");
    // The floor, so the refused body is as small as a refusable body can be:
    // the point is the boundary, not the byte count.
    let over_the_limit = LARGEST_CONFORMING_CONTROL_BODY_BYTES;
    let api = scratch.member_api(over_the_limit);

    let (status, content_type, body) = send(
        &api,
        control_post("/member/v0/protocol", over_the_limit + 1),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    support::assert_conforms("common.schema.json#ErrorResponse", &body);
    assert_eq!(body["error_code"], "BODY_TOO_LARGE");

    // And a body at the limit is not refused by the limit — otherwise the
    // case above would pass with the layer rejecting everything.
    let (status, _, _) = send(&api, control_post("/member/v0/protocol", over_the_limit)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_route_outside_the_control_group_is_not_capped_by_the_control_limit() {
    // The control limit is a `route_layer` on the control routes, so a route
    // registered outside that group does not inherit it. That matters for a
    // route this PR does not add: `member_protocol.md` §10.3 puts an upload
    // chunk at 1-64 MiB, and a chunk `PUT` under the control limit would be
    // capped by the wrong contract.
    //
    // The fallback stands in for such a route, because it is the one thing
    // outside the group today. An oversized body to an unrouted path is
    // answered `404` — the route does not exist — rather than `413`, which
    // would mean the limit had reached it.
    let scratch = Scratch::new("scope");
    let api = scratch.member_api(LARGEST_CONFORMING_CONTROL_BODY_BYTES);

    let (status, _, body) = send(
        &api,
        control_post(
            "/member/v0/uploads/00000000-0000-4000-8000-000000000000",
            LARGEST_PROTOCOL_BODY_BYTES,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_code"], "UNKNOWN_ROUTE");
}

#[tokio::test]
async fn a_clock_with_no_wire_form_answers_without_a_body_rather_than_a_wrong_one() {
    // §13 makes server time authoritative. Every body the schema defines
    // carries it, so when it cannot be expressed there is nothing conforming
    // to send — and a fabricated authoritative time is worse than a failure,
    // because a member would diagnose skew against it.
    let scratch = Scratch::new("clockless");
    let api = scratch.member_api(LARGEST_CONFORMING_CONTROL_BODY_BYTES);

    for path in ["/member/v0/protocol", "/member/v0/nope"] {
        let (status, content_type, body) = send_with(
            &api,
            AppState {
                now: year_minus_one,
            },
            Request::get(path).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(content_type, None, "{path}");
        assert_eq!(body, Value::Null, "{path}");
    }
}

#[test]
fn the_derived_floor_is_what_the_pinned_schemas_can_produce() {
    // `LARGEST_CONFORMING_CONTROL_BODY_BYTES` is the one number that decides
    // whether a conforming member agent can be answered at all, so it is
    // derived here rather than asserted: build the largest body the pinned
    // request schemas admit, check the shipped schema accepts it, and compare
    // its length against the constant.
    //
    // `HeartbeatRequest` binds. Its `resources` array is capped at 1024, and
    // every other request definition is bounded by short strings and small
    // arrays — the next largest, `RegisterSlotRequest`, is a few kilobytes.
    let uuid = "0123abcd-4567-89ab-cdef-0123456789ab";
    // Every optional field present, the longest `SlotState`, and `UInt64` at
    // its maximum: the widest a conforming entry can be.
    let resource = serde_json::json!({
        "slot_id": uuid,
        "slot_state": "PACKAGING",
        "offer_id": uuid,
        "ready_check_id": uuid,
        "assignment_id": uuid,
        "last_accepted_event_seq": 9_007_199_254_740_991_u64,
        "upload_id": uuid,
        "committed_upload_offset": 9_007_199_254_740_991_u64,
    });
    let heartbeat = serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "heartbeat_id": uuid,
        "worker_id": uuid,
        "sent_at": "2026-03-20T09:46:40Z",
        "resources": vec![resource; 1024],
    });

    support::assert_conforms("api.schema.json#HeartbeatRequest", &heartbeat);

    let compact = serde_json::to_vec(&heartbeat).expect("a serializable body");
    assert_eq!(
        compact.len() as u64,
        LARGEST_CONFORMING_CONTROL_BODY_BYTES,
        "the floor must be what the pinned schemas can produce"
    );

    // And one more entry than the schema allows is not a body this bound has
    // to hold — otherwise the number above would be a floor with nothing
    // under it.
    let mut too_many = heartbeat.clone();
    let resources = too_many["resources"].as_array_mut().expect("an array");
    resources.push(resources[0].clone());
    let validator = support::validator_for("api.schema.json#HeartbeatRequest");
    assert!(!validator.is_valid(&too_many));
}

/// A `POST` carrying `content_length` bytes, declared the way a real client
/// declares it: tower-http refuses on the declared length before any body
/// arrives.
fn control_post(path: &str, content_length: u64) -> Request<Body> {
    let body = vec![b'x'; usize::try_from(content_length).expect("a testable size")];
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, content_length.to_string())
        .body(Body::from(body))
        .unwrap()
}

/// A clock whose instants have no RFC 3339 form.
fn year_minus_one() -> time::OffsetDateTime {
    time::Date::from_calendar_date(-1, time::Month::January, 1)
        .expect("a constructible date")
        .midnight()
        .assume_utc()
}
