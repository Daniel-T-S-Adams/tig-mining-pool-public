//! Slice-2 criteria A1–A3: the member surface exists, states one protocol
//! version, and answers every failure in the shape the protocol pins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use pool_api::protocol::{
    HEARTBEAT_INTERVAL_SECONDS, PACKAGE_FORMAT, PROTOCOL_VERSION, ProtocolInfo,
    REQUEST_CLOCK_SKEW_SECONDS,
};
use pool_api::service::{AppState, app};
use pool_config::{Binary, Config};
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
        Self { dir }
    }

    /// A configuration `pool-api` starts on, with one value the caller picks.
    fn config(&self, max_control_body_bytes: u64) -> Config {
        let password_file = self.dir.join("db-password").display().to_string();
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

async fn send(config: &Config, request: Request<Body>) -> (StatusCode, Option<String>, Value) {
    let response = app(config, AppState { now: fixed_now })
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
    let config = scratch.config(262_144);

    // No credential, no signature: §4 makes this route public, and an agent
    // must be able to ask what the server speaks before it can enroll.
    let (status, content_type, body) = send(
        &config,
        Request::get("/member/v0/protocol")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    support::assert_conforms("api.schema.json#ProtocolInfoResponse", &body);

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
fn the_constants_this_build_serves_are_the_ones_the_schema_pins() {
    // `ProtocolVersion`, `PackageFormat` and the two `const` numbers are facts
    // of the protocol version rather than settings of this deployment
    // (`member_protocol.md` §4). Read out of the schema files so a build that
    // drifted from them fails here rather than at a member agent.
    let info = serde_json::to_value(ProtocolInfo::at(fixed_now())).unwrap();
    support::assert_conforms("api.schema.json#ProtocolInfoResponse", &info);

    assert_eq!(PROTOCOL_VERSION, "0.1.0");
    assert_eq!(PACKAGE_FORMAT, "proof-material-v1");
    assert_eq!(REQUEST_CLOCK_SKEW_SECONDS, 300);
    assert_eq!(HEARTBEAT_INTERVAL_SECONDS, 30);
}

#[tokio::test]
async fn an_unrouted_path_is_refused_in_the_protocols_own_error_shape() {
    let scratch = Scratch::new("unknown-route");
    let config = scratch.config(262_144);

    let (status, content_type, body) = send(
        &config,
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
    let config = scratch.config(262_144);

    // Axum answers this before any handler runs, with an empty body. A member
    // agent parses every failure through one parser, so an empty body is a
    // failure it can only report as "unknown".
    let (status, content_type, body) = send(
        &config,
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
    // A small limit so the oversized body is small too: the point is the
    // boundary, not the byte count.
    let config = scratch.config(64);

    let (status, content_type, body) = send(
        &config,
        Request::post("/member/v0/protocol")
            .header(header::CONTENT_TYPE, "application/json")
            // What a real client sends and what the limit reads: tower-http
            // refuses on the declared length before any body arrives.
            .header(header::CONTENT_LENGTH, "65")
            .body(Body::from(vec![b'x'; 65]))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    support::assert_conforms("common.schema.json#ErrorResponse", &body);
    assert_eq!(body["error_code"], "BODY_TOO_LARGE");

    // And a body at the limit is not refused by the limit — otherwise the
    // case above would pass with the layer rejecting everything.
    let (status, _, _) = send(
        &config,
        Request::post("/member/v0/protocol")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, "64")
            .body(Body::from(vec![b'x'; 64]))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
