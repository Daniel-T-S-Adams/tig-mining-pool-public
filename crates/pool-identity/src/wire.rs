//! Wire messages for enrollment, rotation, and signed requests.
//!
//! Field names and shapes follow `schemas/member_protocol/v0.1.0/`
//! (`api.schema.json#/$defs/EnrollRequest`, `EnrollResponse`,
//! `RotateCredentialRequest`, `RotateCredentialResponse`) and the §3.2 signed
//! header set. Conformance is checked against the JSON Schemas by
//! `tests/schema_conformance.rs`.

use serde::{Deserialize, Serialize};

/// `POST /member/v0/enroll` request (`api.schema.json#/$defs/EnrollRequest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollRequest {
    pub enrollment_request_id: String,
    pub enrollment_ticket: String,
    pub worker_name: String,
    pub ed25519_public_key: String,
    pub ed25519_key_proof: String,
    pub supported_protocol_versions: Vec<String>,
    pub supported_package_formats: Vec<String>,
    pub member_agent_version: String,
}

/// `POST /member/v0/enroll` response (`api.schema.json#/$defs/EnrollResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollResponse {
    pub protocol_version: String,
    pub enrollment_request_id: String,
    pub package_format: String,
    pub member_id: String,
    pub worker_id: String,
    pub credential_id: String,
    pub worker_status: String,
    pub server_time: String,
}

/// `POST /member/v0/workers/{worker_id}/credentials/rotate` request
/// (`api.schema.json#/$defs/RotateCredentialRequest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotateCredentialRequest {
    pub protocol_version: String,
    pub rotation_id: String,
    pub new_ed25519_public_key: String,
    pub new_key_proof: String,
}

/// Rotation response (`api.schema.json#/$defs/RotateCredentialResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotateCredentialResponse {
    pub protocol_version: String,
    pub rotation_id: String,
    pub worker_id: String,
    pub new_credential_id: String,
    pub old_credential_revokes_at: String,
    pub server_time: String,
}

/// The §3.2 signed request header set for one HTTP attempt.
///
/// ```text
/// X-Pool-Protocol-Version / X-Worker-Id / X-Worker-Credential-Id /
/// X-Request-Id / X-Request-Timestamp / X-Body-SHA256 / X-Worker-Signature
/// ```
///
/// `method` and `path` are taken from the HTTP request line; both are part of
/// the signed byte string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    pub method: String,
    pub path: String,
    pub protocol_version: String,
    pub worker_id: String,
    pub credential_id: String,
    pub request_id: String,
    pub request_timestamp: u64,
    pub body_sha256: String,
    pub signature: String,
}

/// The authenticated caller derived from a verified signed request.
///
/// `member_id` comes from the pool's stored worker binding, never from a
/// client claim (§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorker {
    pub member_id: String,
    pub worker_id: String,
    pub credential_id: String,
}
