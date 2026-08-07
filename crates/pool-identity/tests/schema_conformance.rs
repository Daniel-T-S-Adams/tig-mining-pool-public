//! Wire messages produced/accepted by the identity service must conform to
//! `schemas/member_protocol/v0.1.0` (issue #32: "issued identities and any
//! wire messages must conform to the relevant schemas").
//!
//! The two schema files cross-reference each other by relative `$id`
//! (`api.schema.json` → `common.schema.json`). For offline validation they
//! are merged into one document: common's `$defs` are inlined under a
//! `common__` prefix and every cross-file `$ref` is rewritten to point at
//! them. The rewrite is purely mechanical; the constraints are unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use pool_identity::keys::{
    encode_public_key, enrollment_signing_string, rotation_signing_string, sign_b64url,
};
use pool_identity::{EnrollRequest, IdentityService, RotateCredentialRequest};
use serde_json::Value;

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/member_protocol/v0.1.0")
}

/// One self-contained schema document: api.schema.json with common's
/// definitions inlined under `common__`.
fn merged_schema() -> Value {
    let api_text = std::fs::read_to_string(schema_dir().join("api.schema.json")).expect("api");
    let common_text =
        std::fs::read_to_string(schema_dir().join("common.schema.json")).expect("common");
    let record_text = std::fs::read_to_string(schema_dir().join("output-record.schema.json"))
        .expect("output-record");
    // Cross-file refs in api point into the inlined copies.
    let api_text = api_text
        .replace("common.schema.json#/$defs/", "#/$defs/common__")
        .replace(
            "\"output-record.schema.json\"",
            "\"#/$defs/output__record\"",
        );
    // The inlined files' own refs must follow their definitions.
    let common_text = common_text.replace("\"#/$defs/", "\"#/$defs/common__");
    let record_text = record_text.replace("common.schema.json#/$defs/", "#/$defs/common__");
    let mut api: Value = serde_json::from_str(&api_text).expect("api json");
    let common: Value = serde_json::from_str(&common_text).expect("common json");
    let mut record: Value = serde_json::from_str(&record_text).expect("record json");
    let record_obj = record.as_object_mut().expect("record object");
    record_obj.remove("$id");
    record_obj.remove("$schema");
    let record = record.clone();
    let api_defs = api["$defs"].as_object_mut().expect("api $defs");
    for (name, def) in common["$defs"].as_object().expect("common $defs") {
        api_defs.insert(format!("common__{name}"), def.clone());
    }
    api_defs.insert("output__record".to_owned(), record);
    // Relative $id values would otherwise anchor ref resolution.
    api.as_object_mut().expect("object").remove("$id");
    api
}

fn validator_for(def: &str) -> jsonschema::Validator {
    let mut schema = merged_schema();
    schema
        .as_object_mut()
        .expect("object")
        .insert("$ref".to_owned(), Value::String(format!("#/$defs/{def}")));
    jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("build validator")
}

fn assert_valid(def: &str, instance: &Value) {
    let validator = validator_for(def);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{e} at {}", e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "instance violates {def}: {errors:?}\n{instance:#}"
    );
}

const NOW: u64 = 1_754_000_000;

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// TEST-ONLY deterministic key bytes; never a real credential.
fn test_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn fresh_service(name: &str) -> IdentityService {
    let dir = std::env::temp_dir().join(format!(
        "pool-identity-schema-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    IdentityService::open(dir).expect("open")
}

#[test]
fn enroll_request_and_response_conform_to_schema() {
    let svc = fresh_service("enroll");
    let member_id = svc.create_member("alice", NOW).expect("member");
    let ticket = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    let key = test_key(1);
    let public_key = encode_public_key(&key.verifying_key());
    let proof = sign_b64url(
        &key,
        &enrollment_signing_string(&uuid(1), &ticket.secret, &public_key),
    );
    let request = EnrollRequest {
        enrollment_request_id: uuid(1),
        enrollment_ticket: ticket.secret.clone(),
        worker_name: "bench-box".to_owned(),
        ed25519_public_key: public_key,
        ed25519_key_proof: proof,
        supported_protocol_versions: vec!["0.1.0".to_owned()],
        supported_package_formats: vec!["proof-material-v1".to_owned()],
        member_agent_version: "spike-test-0".to_owned(),
    };
    assert_valid("EnrollRequest", &serde_json::to_value(&request).unwrap());

    let response = svc.enroll(&request, NOW).expect("enroll");
    assert_valid("EnrollResponse", &serde_json::to_value(&response).unwrap());
}

#[test]
fn rotation_request_and_response_conform_to_schema() {
    let svc = fresh_service("rotate");
    let member_id = svc.create_member("alice", NOW).expect("member");
    let ticket = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    let key = test_key(1);
    let public_key = encode_public_key(&key.verifying_key());
    let proof = sign_b64url(
        &key,
        &enrollment_signing_string(&uuid(1), &ticket.secret, &public_key),
    );
    let enrolled = svc
        .enroll(
            &EnrollRequest {
                enrollment_request_id: uuid(1),
                enrollment_ticket: ticket.secret.clone(),
                worker_name: "bench-box".to_owned(),
                ed25519_public_key: public_key,
                ed25519_key_proof: proof,
                supported_protocol_versions: vec!["0.1.0".to_owned()],
                supported_package_formats: vec!["proof-material-v1".to_owned()],
                member_agent_version: "spike-test-0".to_owned(),
            },
            NOW,
        )
        .expect("enroll");

    let new_key = test_key(2);
    let new_public_key = encode_public_key(&new_key.verifying_key());
    let request = RotateCredentialRequest {
        protocol_version: "0.1.0".to_owned(),
        rotation_id: uuid(2),
        new_ed25519_public_key: new_public_key.clone(),
        new_key_proof: sign_b64url(
            &new_key,
            &rotation_signing_string(&enrolled.worker_id, &uuid(2), &new_public_key),
        ),
    };
    assert_valid(
        "RotateCredentialRequest",
        &serde_json::to_value(&request).unwrap(),
    );

    let caller = pool_identity::VerifiedWorker {
        member_id: enrolled.member_id.clone(),
        worker_id: enrolled.worker_id.clone(),
        credential_id: enrolled.credential_id.clone(),
    };
    let response = svc
        .rotate(&caller, &enrolled.worker_id, &request, NOW)
        .expect("rotate");
    assert_valid(
        "RotateCredentialResponse",
        &serde_json::to_value(&response).unwrap(),
    );
}

#[test]
fn validator_rejects_nonconforming_documents() {
    // Harness sanity: the merged schema still bites. A response missing a
    // required field and an out-of-pattern key must fail.
    let validator = validator_for("EnrollResponse");
    assert!(!validator.is_valid(&serde_json::json!({})));
    let validator = validator_for("EnrollRequest");
    let mut bad = serde_json::json!({
        "enrollment_request_id": "not-a-uuid",
        "enrollment_ticket": "x",
        "worker_name": "w",
        "ed25519_public_key": "short",
        "ed25519_key_proof": "short",
        "supported_protocol_versions": ["0.1.0"],
        "supported_package_formats": ["proof-material-v1"],
        "member_agent_version": "v",
    });
    assert!(!validator.is_valid(&bad));
    // Unknown fields are rejected (member_protocol §4).
    bad["extra"] = serde_json::json!(1);
    assert!(!validator.is_valid(&bad));
}
