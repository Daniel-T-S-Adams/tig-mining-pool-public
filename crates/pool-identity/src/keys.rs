//! Ed25519 credential primitives and the exact protocol signing strings.
//!
//! Contract: `docs/member_protocol.md` §3.1 (enrollment proof), §3.2 (signed
//! requests), §3.3 (rotation proof). All signatures are unpadded base64url
//! Ed25519 signatures over UTF-8 strings with no final newline; public keys
//! are unpadded base64url of the 32 raw key bytes (43 characters, matching
//! `api.schema.json` `^[A-Za-z0-9_-]{43}$`).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use sha2::{Digest as _, Sha256};

use crate::{IdentityError, IdentityErrorCode, IdentityResult};

fn not_authenticated(detail: impl Into<String>) -> IdentityError {
    IdentityError::new(IdentityErrorCode::NotAuthenticated, detail)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn encode_public_key(key: &VerifyingKey) -> String {
    b64url(key.as_bytes())
}

pub fn decode_public_key(encoded: &str) -> IdentityResult<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| not_authenticated(format!("public key is not unpadded base64url: {e}")))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| not_authenticated("public key is not 32 bytes"))?;
    VerifyingKey::from_bytes(&arr)
        .map_err(|e| not_authenticated(format!("invalid Ed25519 public key: {e}")))
}

/// Sign `message` and return the unpadded base64url signature (86 chars).
pub fn sign_b64url(key: &SigningKey, message: &str) -> String {
    b64url(&key.sign(message.as_bytes()).to_bytes())
}

/// Verify an unpadded base64url Ed25519 signature over `message`.
pub fn verify_b64url(key: &VerifyingKey, message: &str, signature: &str) -> IdentityResult<()> {
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|e| not_authenticated(format!("signature is not unpadded base64url: {e}")))?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|e| not_authenticated(format!("signature is not 64 bytes: {e}")))?;
    key.verify_strict(message.as_bytes(), &sig)
        .map_err(|_| not_authenticated("Ed25519 signature verification failed"))
}

/// §3.1 enrollment proof-of-possession string (no final newline):
///
/// ```text
/// TIG-POOL-ENROLLMENT-V1
/// <enrollment_request_id>
/// <SHA-256 of the UTF-8 enrollment ticket as 64 lowercase hex characters>
/// <ed25519_public_key>
/// ```
pub fn enrollment_signing_string(
    enrollment_request_id: &str,
    enrollment_ticket: &str,
    ed25519_public_key: &str,
) -> String {
    format!(
        "TIG-POOL-ENROLLMENT-V1\n{enrollment_request_id}\n{}\n{ed25519_public_key}",
        sha256_hex(enrollment_ticket.as_bytes())
    )
}

/// §3.3 recovery proof-of-possession string, signed by the NEW key (no
/// final newline). Recovery has its own signing domain and never shares
/// the enrollment or rotation namespace; the HTTP route and schema remain
/// outside protocol 0.1.0 (§17):
///
/// ```text
/// TIG-POOL-RECOVERY-V1
/// <recovery_request_id>
/// <SHA-256 of the UTF-8 recovery ticket as 64 lowercase hex characters>
/// <ed25519_public_key>
/// ```
pub fn recovery_signing_string(
    recovery_request_id: &str,
    recovery_ticket: &str,
    ed25519_public_key: &str,
) -> String {
    format!(
        "TIG-POOL-RECOVERY-V1\n{recovery_request_id}\n{}\n{ed25519_public_key}",
        sha256_hex(recovery_ticket.as_bytes())
    )
}

/// §3.2 signed-request string (no final newline):
///
/// ```text
/// TIG-POOL-REQUEST-V1
/// <uppercase HTTP method>
/// <exact path, beginning with / and excluding scheme, host, and query>
/// <protocol version>
/// <worker_id>
/// <credential_id>
/// <request_id>
/// <request_timestamp>
/// <body_sha256>
/// ```
#[allow(clippy::too_many_arguments)]
pub fn request_signing_string(
    method: &str,
    path: &str,
    protocol_version: &str,
    worker_id: &str,
    credential_id: &str,
    request_id: &str,
    request_timestamp: u64,
    body_sha256: &str,
) -> String {
    format!(
        "TIG-POOL-REQUEST-V1\n{method}\n{path}\n{protocol_version}\n{worker_id}\n\
         {credential_id}\n{request_id}\n{request_timestamp}\n{body_sha256}"
    )
}

/// §3.3 rotation proof-of-possession string, signed by the NEW key
/// (no final newline):
///
/// ```text
/// TIG-POOL-ROTATION-V1
/// <worker_id>
/// <rotation_id>
/// <new_ed25519_public_key>
/// ```
pub fn rotation_signing_string(
    worker_id: &str,
    rotation_id: &str,
    new_ed25519_public_key: &str,
) -> String {
    format!("TIG-POOL-ROTATION-V1\n{worker_id}\n{rotation_id}\n{new_ed25519_public_key}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// TEST-ONLY fixed key bytes; never a real credential.
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn public_key_encoding_is_43_chars_and_round_trips() {
        let key = test_key();
        let encoded = encode_public_key(&key.verifying_key());
        assert_eq!(encoded.len(), 43);
        assert_eq!(decode_public_key(&encoded).unwrap(), key.verifying_key());
    }

    #[test]
    fn signature_is_86_chars_and_verifies() {
        let key = test_key();
        let msg = enrollment_signing_string(
            "11111111-1111-4111-8111-111111111111",
            "ticket-secret",
            &encode_public_key(&key.verifying_key()),
        );
        let sig = sign_b64url(&key, &msg);
        assert_eq!(sig.len(), 86);
        verify_b64url(&key.verifying_key(), &msg, &sig).unwrap();
        assert!(verify_b64url(&key.verifying_key(), "other message", &sig).is_err());
    }

    #[test]
    fn signing_strings_have_exact_shape() {
        assert_eq!(
            enrollment_signing_string("id", "t", "pk"),
            format!("TIG-POOL-ENROLLMENT-V1\nid\n{}\npk", sha256_hex(b"t"))
        );
        assert_eq!(
            request_signing_string("POST", "/member/v0/enroll", "0.1.0", "w", "c", "r", 5, "b"),
            "TIG-POOL-REQUEST-V1\nPOST\n/member/v0/enroll\n0.1.0\nw\nc\nr\n5\nb"
        );
        assert_eq!(
            rotation_signing_string("w", "r", "pk"),
            "TIG-POOL-ROTATION-V1\nw\nr\npk"
        );
    }
}
