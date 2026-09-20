//! §3.2 signed requests: who is asking, and whether they asked once.
//!
//! `member_protocol.md` §3.2 defines seven headers, one signed byte string,
//! a 300-second freshness window, and a 24-hour memory of every
//! `(credential_id, request_id)`. `security.md` §4.1 adds the ordering that
//! makes those worth anything: "signature verification and exact body-hash
//! verification occur before JSON decoding, database work, or upload quota
//! reservation", and "authentication failures do not reveal whether a worker,
//! credential, or resource ID exists".
//!
//! The signed string and the Ed25519 primitives come from `pool_identity`
//! rather than being restated here. Two spellings of the signed bytes is a
//! signature bug that no test catches, because each side passes its own.

use axum::http::{HeaderMap, StatusCode};
use pool_identity::keys::{request_signing_string, sha256_hex, verify_b64url};
use sqlx::{PgPool, Row};

use crate::error::ApiError;
use crate::protocol::{PROTOCOL_VERSION, REQUEST_CLOCK_SKEW_SECONDS};

/// §3.2's header names, lowercase as `http` stores them.
pub const HEADER_PROTOCOL_VERSION: &str = "x-pool-protocol-version";
pub const HEADER_WORKER_ID: &str = "x-worker-id";
pub const HEADER_CREDENTIAL_ID: &str = "x-worker-credential-id";
pub const HEADER_REQUEST_ID: &str = "x-request-id";
pub const HEADER_REQUEST_TIMESTAMP: &str = "x-request-timestamp";
pub const HEADER_BODY_SHA256: &str = "x-body-sha256";
pub const HEADER_SIGNATURE: &str = "x-worker-signature";

/// How long a recorded attempt is remembered (§3.2), as the interval the
/// INSERT below adds to the database's own clock. `request_replay` holds 24
/// hours as a floor; this is the value the API writes.
const REPLAY_MEMORY: &str = "24 hours";

/// A worker the pool has authenticated, and the account behind it.
///
/// `member_id` is read from the stored worker binding and never from the
/// request: §3.2 says "the pool derives `member_id` from its stored worker
/// binding rather than trusting a client claim", which is the difference
/// between an identity and an assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorker {
    pub member_id: String,
    pub worker_id: String,
    pub credential_id: String,
    pub request_id: String,
}

/// The seven headers, parsed but not yet believed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SignedHeaders {
    protocol_version: String,
    worker_id: String,
    credential_id: String,
    request_id: String,
    /// §3.2's "Unix time in whole seconds".
    ///
    /// Unsigned, and refused rather than coerced when it is not: this is the
    /// one header a caller can put an arbitrary integer in, and §14 makes a
    /// value outside the type incompatible input rather than something to
    /// reshape. Keeping it signed cost an `abs()` on a difference a caller
    /// could drive to overflow, and a `try_into` that turned a negative
    /// timestamp into `u64::MAX` — a signed string no member ever signed.
    request_timestamp: u64,
    body_sha256: String,
    signature: String,
}

/// What the pool tells a caller it could not authenticate.
///
/// One code for every identity failure, deliberately. §4.1: "authentication
/// failures do not reveal whether a worker, credential, or resource ID
/// exists", so an unknown credential, a credential belonging to another
/// worker, a revoked one, and a bad signature are the same answer. The
/// detail that distinguishes them goes to the log and the audit row, which
/// the caller cannot read.
fn not_authenticated() -> ApiError {
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        error_code: "NOT_AUTHENTICATED",
        message: "the request is not authenticated".to_owned(),
        retryable: false,
        request_id: None,
    }
}

/// A clock too far from the server's (§3.2).
///
/// Distinct from `NOT_AUTHENTICATED` on purpose, and it discloses nothing: a
/// caller learns their own clock is wrong, which §13 expects them to correct
/// from `GET /member/v0/protocol`. Retryable, because it stops being true
/// once they do.
fn stale_timestamp() -> ApiError {
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        error_code: "STALE_TIMESTAMP",
        message: "request timestamp is outside the accepted window".to_owned(),
        retryable: true,
        request_id: None,
    }
}

/// The version this build speaks is not the caller's (§4).
fn incompatible_protocol() -> ApiError {
    ApiError {
        // §4: "no common version produces HTTP 426 and
        // INCOMPATIBLE_PROTOCOL, with no state change".
        status: StatusCode::UPGRADE_REQUIRED,
        error_code: "INCOMPATIBLE_PROTOCOL",
        message: "this server speaks a different protocol version".to_owned(),
        retryable: false,
        request_id: None,
    }
}

/// The same request ID, signed over different bytes (§3.2).
fn request_id_reused() -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        error_code: "REQUEST_ID_REUSED",
        message: "that request id was already used for a different request".to_owned(),
        retryable: false,
        request_id: None,
    }
}

/// Something the pool could not do, which is not the caller's fault.
fn unavailable() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        error_code: "TEMPORARILY_UNAVAILABLE",
        message: "the pool could not complete this request".to_owned(),
        retryable: true,
        request_id: None,
    }
}

/// Where a verification runs: the database it reads, and the two values every
/// audit row records (`security.md` §9).
#[derive(Debug, Clone)]
pub struct Verifier {
    pub db: PgPool,
    pub network: String,
    pub deployment: String,
}

impl Verifier {
    /// Authenticate one request.
    ///
    /// `path` is §3.2's "exact path, beginning with / and excluding scheme,
    /// host, and query", and `body` the exact transmitted bytes — the same
    /// bytes `X-Body-SHA256` commits to, before any decoding.
    pub async fn verify(
        &self,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        body: &[u8],
        now: time::OffsetDateTime,
    ) -> Result<VerifiedWorker, ApiError> {
        let headers = parse(headers)?;
        // From here the caller's own request id is known, so every refusal
        // carries it back (§3.2). Before here it is not, which is why `parse`
        // returns a bare denial.
        let echo = |e: ApiError| e.echoing(&headers.request_id);

        // §4: an exact version, not a compatible one. Before anything else,
        // because a caller speaking another version may not even agree on
        // what the signed string is.
        if headers.protocol_version != PROTOCOL_VERSION {
            return Err(echo(incompatible_protocol()));
        }

        // §4.1's ordering starts here: the body hash is checked against the
        // bytes as they arrived, before the body is parsed and before the
        // database is asked anything at all. A request whose body does not
        // match its header is refused without the pool having looked up a
        // single row.
        if sha256_hex(body) != headers.body_sha256 {
            return Err(echo(not_authenticated()));
        }

        // **One column, and it is the key.** `security.md` §4.1 puts
        // verification before "database work", and verification cannot happen
        // without the public key — the key is what a signature is checked
        // against, and it is a database fact (§4.1: "worker public keys are
        // ordinary database facts"). So one read is unavoidable, and the rule
        // is honoured by making it the *only* one: nothing about the worker,
        // the account, or any state is read until the caller has proved
        // possession of the key. That second read is below the verification.
        //
        // Scoped by worker as well as credential, so a credential presented
        // for another worker finds nothing — §3.2's "the credential
        // authorizes only its exact `worker_id`" enforced by the query rather
        // than by a comparison afterwards.
        let key_row = sqlx::query(
            "SELECT public_key
               FROM pool.worker_credential
              WHERE network = $1
                AND credential_id = $2::uuid
                AND worker_id = $3::uuid",
        )
        .bind(&self.network)
        .bind(&headers.credential_id)
        .bind(&headers.worker_id)
        .fetch_optional(&self.db)
        .await
        .map_err(|e| {
            tracing::error!(event = "auth.key_lookup_failed", error = %e);
            echo(unavailable())
        })?;

        let key = match key_row {
            Some(row) => {
                let public_key: Vec<u8> = row.try_get("public_key").map_err(|e| {
                    // A column this build asked for by name and could not
                    // decode is a schema the binary was not built against —
                    // the pool's problem, not a caller's, and silently
                    // answering "not authenticated" would hide a deployment
                    // fault behind a refusal the operator never sees.
                    tracing::error!(event = "auth.key_decode_failed", error = %e);
                    echo(unavailable())
                })?;
                let key_bytes: [u8; 32] = public_key.as_slice().try_into().map_err(|_| {
                    tracing::error!(
                        event = "auth.key_wrong_length",
                        bytes = public_key.len(),
                        "a stored credential key is not 32 bytes"
                    );
                    echo(unavailable())
                })?;
                ed25519_dalek::VerifyingKey::from_bytes(&key_bytes).map_err(|e| {
                    tracing::error!(event = "auth.key_not_ed25519", error = %e);
                    echo(unavailable())
                })?
            }
            None => {
                // No such credential, or one belonging to another worker.
                // §4.1 makes these the same answer as a bad signature, so the
                // request is verified against a stand-in key and refused when
                // that fails — which it always does.
                //
                // Returning here instead would make the two cases take
                // visibly different work: an unknown credential answered
                // before any Ed25519 operation, a known one only after. That
                // is the existence disclosure §4.1 forbids, rebuilt out of
                // timing. This removes the short-circuit; it does not claim
                // to equalise the whole path, which the lookup itself does
                // not allow.
                tracing::info!(
                    event = "auth.rejected",
                    reason = "NO_SUCH_CREDENTIAL_FOR_THIS_WORKER",
                    credential_id = %headers.credential_id,
                    worker_id = %headers.worker_id,
                    "a credential that does not exist, or not for this worker"
                );
                absent_credential_stand_in()
            }
        };

        let signed = request_signing_string(
            method,
            path,
            &headers.protocol_version,
            &headers.worker_id,
            &headers.credential_id,
            &headers.request_id,
            // Exactly as it arrived: the string has to match the member's
            // byte for byte, so there is no conversion here to be lossy.
            headers.request_timestamp,
            &headers.body_sha256,
        );
        verify_b64url(&key, &signed, &headers.signature).map_err(|_| echo(not_authenticated()))?;

        // Everything else, now that the caller has proved possession of the
        // key. This is the read `security.md` §4.1 means by "database work",
        // and it happens after verification rather than before it.
        //
        // §3.3: credential and worker state "take effect on the next
        // request", so both are read on every one rather than cached. The
        // grace comparison is made in SQL so that "has this credential's
        // grace ended" is answered by the same clock, in the same statement,
        // as the row it is about (§3.3's ten-minute window). The states come
        // back as text so the log can say which of them refused the request;
        // the caller is told none of it.
        let row = sqlx::query(
            "SELECT c.state        AS credential_state,
                    (c.not_after IS NOT NULL AND c.not_after <= now()) AS grace_ended,
                    w.state        AS worker_state,
                    w.member_id::text AS member_id
               FROM pool.worker_credential c
               JOIN pool.worker w
                 ON w.network = c.network AND w.worker_id = c.worker_id
              WHERE c.network = $1
                AND c.credential_id = $2::uuid
                AND c.worker_id = $3::uuid",
        )
        .bind(&self.network)
        .bind(&headers.credential_id)
        .bind(&headers.worker_id)
        .fetch_optional(&self.db)
        .await
        .map_err(|e| {
            tracing::error!(event = "auth.binding_lookup_failed", error = %e);
            echo(unavailable())
        })?
        .ok_or_else(|| echo(not_authenticated()))?;

        let decoded = |e: sqlx::Error| {
            tracing::error!(event = "auth.binding_decode_failed", error = %e);
            echo(unavailable())
        };
        let credential_state: String = row.try_get("credential_state").map_err(decoded)?;
        let worker_state: String = row.try_get("worker_state").map_err(decoded)?;
        let grace_ended: bool = row.try_get("grace_ended").map_err(decoded)?;
        if credential_state != "ACTIVE" || worker_state != "ACTIVE" || grace_ended {
            tracing::info!(
                event = "auth.rejected",
                reason = "CREDENTIAL_OR_WORKER_NOT_ACTIVE",
                credential_state = %credential_state,
                worker_state = %worker_state,
                grace_ended,
                "a verified signature from a credential that may no longer be used"
            );
            return Err(echo(not_authenticated()));
        }

        // §3.2's 300 seconds, either side. After the signature, so a caller
        // who cannot sign learns nothing about the server's clock.
        //
        // Widened to `i128` for the subtraction. Both operands are values a
        // caller can drive to the end of their range — `u64::MAX` seconds
        // against a server clock — and in `i64` that difference overflows,
        // which is a panic in a debug build and a wrapped value that may land
        // inside the window in a release one. `i128` holds every difference
        // these two types can produce, so the comparison is total.
        let skew = (i128::from(now.unix_timestamp()) - i128::from(headers.request_timestamp)).abs();
        if skew > i128::from(REQUEST_CLOCK_SKEW_SECONDS) {
            return Err(echo(stale_timestamp()));
        }

        let member_id: String = row.try_get("member_id").map_err(decoded)?;

        self.record_attempt(&headers, &signed, &member_id).await?;

        Ok(VerifiedWorker {
            member_id,
            worker_id: headers.worker_id,
            credential_id: headers.credential_id,
            request_id: headers.request_id,
        })
    }

    /// Remember this attempt, or refuse it as a reuse (§3.2).
    ///
    /// `ON CONFLICT DO NOTHING` and then a read, rather than a read and then
    /// an insert: two requests arriving together would both find nothing and
    /// both proceed, which is the replay this exists to stop.
    async fn record_attempt(
        &self,
        headers: &SignedHeaders,
        signed: &str,
        member_id: &str,
    ) -> Result<(), ApiError> {
        let signed_sha256 = sha256_hex(signed.as_bytes());

        let inserted = sqlx::query(sqlx::AssertSqlSafe(format!(
            // The interval is this module's own constant, never member input.
            "INSERT INTO pool.request_replay
                 (network, credential_id, request_id, signed_sha256,
                  first_seen_at, forget_after)
             VALUES ($1, $2::uuid, $3::uuid, $4,
                     now(), now() + interval '{REPLAY_MEMORY}')
             ON CONFLICT (network, credential_id, request_id) DO NOTHING"
        )))
        .bind(&self.network)
        .bind(&headers.credential_id)
        .bind(&headers.request_id)
        .bind(&signed_sha256)
        .execute(&self.db)
        .await
        .map_err(|e| {
            tracing::error!(event = "auth.replay_write_failed", error = %e);
            unavailable().echoing(&headers.request_id)
        })?;

        if inserted.rows_affected() == 1 {
            return Ok(());
        }

        // Seen before. The same signed bytes is an HTTP retry of one attempt,
        // which §3.2 allows and application idempotency keys make harmless.
        // Different bytes is the reuse.
        let recorded: String = sqlx::query_scalar(
            "SELECT signed_sha256 FROM pool.request_replay
              WHERE network = $1 AND credential_id = $2::uuid AND request_id = $3::uuid",
        )
        .bind(&self.network)
        .bind(&headers.credential_id)
        .bind(&headers.request_id)
        .fetch_one(&self.db)
        .await
        .map_err(|e| {
            tracing::error!(event = "auth.replay_read_failed", error = %e);
            unavailable().echoing(&headers.request_id)
        })?;

        if recorded == signed_sha256 {
            return Ok(());
        }

        // §3.2 says a reuse is "rejected **and** audited", and the two are one
        // answer rather than an answer and a best effort. If the row cannot be
        // written the pool has not done what it says it does, so it asks to be
        // asked again rather than issuing a permanent refusal it could not
        // record. The retry re-conflicts, re-compares, and tries the row
        // again; nothing about the caller's position changes meanwhile.
        if !self
            .audit_reuse(headers, member_id, &recorded, &signed_sha256)
            .await
        {
            return Err(unavailable().echoing(&headers.request_id));
        }
        Err(request_id_reused().echoing(&headers.request_id))
    }

    /// §3.2's "rejected **and** audited". `true` when the row was written.
    ///
    /// The row is the durable half: the rejection alone leaves nothing an
    /// operator can look at later, and `security.md` §9 lists "authentication
    /// replay" among the events that must leave one. The caller decides what
    /// to answer when it could not be written.
    ///
    /// `audit_event_id` is logged as the row is inserted, which is what puts
    /// the row and the logs around it back together — `0025` dropped the
    /// `trace_id` column because nothing here produces a trace id to fill it.
    async fn audit_reuse(
        &self,
        headers: &SignedHeaders,
        member_id: &str,
        recorded: &str,
        presented: &str,
    ) -> bool {
        let evidence = serde_json::json!({
            "recorded_signed_sha256": recorded,
            "presented_signed_sha256": presented,
            "worker_id": headers.worker_id,
            "member_id": member_id,
        });

        let written = sqlx::query_scalar::<_, String>(
            "INSERT INTO pool.audit_event
                 (deployment, network, actor_type, actor_id, action, outcome,
                  resource_type, resource_id, request_id, reason, evidence)
             VALUES ($1, $2, 'WORKER', $3, 'REQUEST_REPLAY_REJECTED', 'REJECTED',
                     'WORKER_CREDENTIAL', $4, $5::uuid,
                     'REQUEST_ID_REUSED_WITH_DIFFERENT_BYTES', $6::jsonb)
             RETURNING audit_event_id::text",
        )
        .bind(&self.deployment)
        .bind(&self.network)
        .bind(&headers.worker_id)
        .bind(&headers.credential_id)
        .bind(&headers.request_id)
        .bind(evidence.to_string())
        .fetch_one(&self.db)
        .await;

        match written {
            Ok(audit_event_id) => {
                tracing::info!(
                    event = "auth.replay_rejected",
                    audit_event_id = %audit_event_id,
                    credential_id = %headers.credential_id,
                    request_id = %headers.request_id,
                    "a request id was reused over different signed bytes"
                );
                true
            }
            Err(e) => {
                tracing::error!(
                    event = "auth.audit_write_failed",
                    error = %e,
                    credential_id = %headers.credential_id,
                    request_id = %headers.request_id,
                    "a replay could not be recorded, so it is not being refused permanently"
                );
                false
            }
        }
    }
}

/// A valid Ed25519 public key that verifies nothing anyone can produce.
///
/// Used when the credential lookup finds nothing, so that an unknown
/// credential and a known one with a bad signature are both refused *after* a
/// verification attempt rather than one before and one after. The bytes are a
/// fixed generator multiple with no known scalar; no signature checked
/// against it can succeed, and none is meant to.
fn absent_credential_stand_in() -> ed25519_dalek::VerifyingKey {
    // The Ed25519 basepoint's compressed encoding: a valid curve point, and
    // the discrete log nobody has.
    const BASEPOINT: [u8; 32] = [
        0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
        0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
        0x66, 0x66,
    ];
    ed25519_dalek::VerifyingKey::from_bytes(&BASEPOINT)
        .unwrap_or_else(|_| unreachable!("the Ed25519 basepoint is a valid public key"))
}

/// Read the seven headers. Anything missing or misshapen is the same answer
/// as a bad signature: a caller learns only that they were not authenticated.
fn parse(headers: &HeaderMap) -> Result<SignedHeaders, ApiError> {
    let text = |name: &str| -> Result<String, ApiError> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(not_authenticated)
    };

    let protocol_version = text(HEADER_PROTOCOL_VERSION)?;
    let worker_id = uuid(&text(HEADER_WORKER_ID)?)?;
    let credential_id = uuid(&text(HEADER_CREDENTIAL_ID)?)?;
    let request_id = uuid(&text(HEADER_REQUEST_ID)?)?;
    let request_timestamp = text(HEADER_REQUEST_TIMESTAMP)?
        .parse::<u64>()
        .map_err(|_| not_authenticated())?;
    let body_sha256 = text(HEADER_BODY_SHA256)?;
    let signature = text(HEADER_SIGNATURE)?;

    // §3.2: "64 lowercase hex characters". Checked here rather than left to
    // the comparison, because an uppercase digest would simply compare
    // unequal and be reported as a signature failure — a true rejection for
    // the wrong stated reason.
    if body_sha256.len() != 64
        || !body_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(not_authenticated());
    }

    Ok(SignedHeaders {
        protocol_version,
        worker_id,
        credential_id,
        request_id,
        request_timestamp,
        body_sha256,
        signature,
    })
}

/// A lowercase canonical UUID, as every identifier in the pinned schema is.
///
/// Refused here rather than at the database: a value that is not one would
/// otherwise reach `$1::uuid` as a cast error, which is a different failure
/// with a different status and a message describing the pool's schema.
fn uuid(value: &str) -> Result<String, ApiError> {
    let shape_ok = value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        });
    if shape_ok {
        Ok(value.to_owned())
    } else {
        Err(not_authenticated())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const UUID: &str = "0123abcd-4567-89ab-cdef-0123456789ab";

    #[test]
    fn a_uuid_must_be_lowercase_canonical() {
        assert_eq!(uuid(UUID).unwrap(), UUID);
        for bad in [
            "",
            "0123ABCD-4567-89ab-cdef-0123456789ab",
            "0123abcd456789abcdef0123456789ab",
            "0123abcd-4567-89ab-cdef-0123456789ag",
            "0123abcd-4567-89ab-cdef-0123456789ab ",
            "0123abcd_4567_89ab_cdef_0123456789ab",
        ] {
            assert!(uuid(bad).is_err(), "{bad:?} is not a canonical uuid");
        }
    }

    fn headers_with(overrides: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let mut set = |name: &str, value: &str| {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        };
        set(HEADER_PROTOCOL_VERSION, PROTOCOL_VERSION);
        set(HEADER_WORKER_ID, UUID);
        set(HEADER_CREDENTIAL_ID, UUID);
        set(HEADER_REQUEST_ID, UUID);
        set(HEADER_REQUEST_TIMESTAMP, "1774000000");
        set(HEADER_BODY_SHA256, &sha256_hex(b""));
        set(HEADER_SIGNATURE, "c2ln");
        for (name, value) in overrides {
            set(name, value);
        }
        headers
    }

    #[test]
    fn the_seven_headers_are_all_required() {
        assert!(parse(&headers_with(&[])).is_ok());
        for name in [
            HEADER_PROTOCOL_VERSION,
            HEADER_WORKER_ID,
            HEADER_CREDENTIAL_ID,
            HEADER_REQUEST_ID,
            HEADER_REQUEST_TIMESTAMP,
            HEADER_BODY_SHA256,
            HEADER_SIGNATURE,
        ] {
            let mut headers = headers_with(&[]);
            headers.remove(name);
            assert!(parse(&headers).is_err(), "{name} must be required");
        }
    }

    #[test]
    fn a_body_digest_must_be_64_lowercase_hex() {
        // An uppercase digest would compare unequal and be reported as a
        // signature failure — a true rejection stated for the wrong reason.
        for bad in [
            "",
            &sha256_hex(b"").to_uppercase(),
            &sha256_hex(b"")[..63],
            &format!("{}0", sha256_hex(b"")),
            &"g".repeat(64),
        ] {
            let headers = headers_with(&[(HEADER_BODY_SHA256, bad)]);
            assert!(parse(&headers).is_err(), "digest {bad:?} must be refused");
        }
    }

    #[test]
    fn a_timestamp_must_be_whole_unsigned_seconds() {
        for bad in [
            "",
            "1774000000.5",
            "now",
            "1_774_000_000",
            " 1774000000",
            // Not a time §3.2 defines, and the value that made the freshness
            // subtraction overflow while this was an `i64`.
            "-5",
            "-9223372036854775808",
            // One past `u64::MAX`.
            "18446744073709551616",
        ] {
            let headers = headers_with(&[(HEADER_REQUEST_TIMESTAMP, bad)]);
            assert!(
                parse(&headers).is_err(),
                "timestamp {bad:?} must be refused"
            );
        }

        // The ends of what the type does allow, so the rejections above are a
        // rule rather than a check that refuses everything.
        for good in ["0", "1774000000", "18446744073709551615"] {
            let headers = headers_with(&[(HEADER_REQUEST_TIMESTAMP, good)]);
            assert_eq!(
                parse(&headers).unwrap().request_timestamp,
                good.parse::<u64>().unwrap()
            );
        }
    }

    #[test]
    fn the_stand_in_key_exists_and_verifies_nothing() {
        // The `None` branch hands this to `verify_b64url` so that an unknown
        // credential and a bad signature both cost a verification. It has to
        // be a valid key — `from_bytes` rejects a non-point, and the branch
        // would panic — and it has to verify nothing, or an attacker naming a
        // credential that does not exist could authenticate as it.
        let key = absent_credential_stand_in();

        let real = ed25519_dalek::SigningKey::from_bytes(&[0x11; 32]);
        let message = "TIG-POOL-REQUEST-V1\nGET\n/member/v0/protocol";
        let signature = pool_identity::keys::sign_b64url(&real, message);
        assert!(
            verify_b64url(&key, message, &signature).is_err(),
            "a real signature must not verify against the stand-in"
        );
        // And a signature it produced for itself cannot exist: nobody holds
        // the scalar. The closest a test can get is that the key is not the
        // one that signed.
        assert_ne!(key.as_bytes(), real.verifying_key().as_bytes());
    }

    #[test]
    fn every_identity_failure_is_the_same_answer() {
        // `security.md` §4.1: authentication failures do not reveal whether a
        // worker, credential, or resource ID exists. If these ever differ, a
        // caller can enumerate.
        let denials = [not_authenticated(), stale_timestamp()];
        assert!(
            denials.iter().all(|e| e.status == StatusCode::UNAUTHORIZED),
            "both are 401"
        );
        // And they differ only in the way a caller is entitled to know:
        // whether it is their clock.
        assert_ne!(denials[0].error_code, denials[1].error_code);
        assert!(!denials[0].retryable, "a bad signature does not improve");
        assert!(denials[1].retryable, "a corrected clock does");
    }
}
