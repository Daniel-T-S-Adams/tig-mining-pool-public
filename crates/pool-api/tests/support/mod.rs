//! Validating this service's wire bodies against the pinned schemas.
//!
//! `schemas/member_protocol/v0.1.0` is two files that reference each other by
//! relative `$id` (`api.schema.json` → `common.schema.json`). Rather than
//! merge them, each is registered as a resource under an absolute base URI and
//! the api document's `$id` is rewritten to match. Only the `$id` changes; not
//! one constraint is touched, so a body that validates here validates against
//! the files as shipped.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use serde_json::Value;

/// An absolute base the two relative `$id`s hang off. `.invalid` is reserved
/// by RFC 2606, so nothing resolves it and no validation can reach the
/// network.
const BASE: &str = "https://schemas.pool.invalid/member/v0.1.0/";

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/member_protocol/v0.1.0")
}

fn read(name: &str) -> Value {
    let text = std::fs::read_to_string(schema_dir().join(name))
        .unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

/// A validator for one definition, named `file#Definition` — for example
/// `api.schema.json#ProtocolInfoResponse` or `common.schema.json#ErrorResponse`.
pub fn validator_for(reference: &str) -> jsonschema::Validator {
    let (file, def) = reference
        .split_once('#')
        .unwrap_or_else(|| panic!("{reference:?} is not `file#Definition`"));

    // A root that refers into the shipped files rather than containing them,
    // so both are validated exactly as written.
    let root = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{BASE}root.json"),
        "$ref": format!("{BASE}{file}#/$defs/{def}"),
    });

    jsonschema::options()
        .should_validate_formats(true)
        .with_resources(
            [
                "api.schema.json",
                "common.schema.json",
                "output-record.schema.json",
            ]
            .into_iter()
            .map(|name| {
                let mut schema = read(name);
                // The shipped `$id` is relative; anchor it so the
                // cross-file `$ref`s resolve to the copy registered here.
                schema
                    .as_object_mut()
                    .expect("an object")
                    .insert("$id".to_owned(), Value::String(format!("{BASE}{name}")));
                (
                    format!("{BASE}{name}"),
                    jsonschema::Resource::from_contents(schema)
                        .unwrap_or_else(|e| panic!("{name} as a resource: {e}")),
                )
            }),
        )
        .build(&root)
        .unwrap_or_else(|e| panic!("build a validator for {reference}: {e}"))
}

/// Assert an instance satisfies a pinned definition, naming every violation.
pub fn assert_conforms(reference: &str, instance: &Value) {
    let validator = validator_for(reference);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{e} at {}", e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "instance violates {reference}: {errors:?}\n{instance:#}"
    );
}
