//! `POST /member/v0/enroll` — a machine turns a ticket into a credential.
//!
//! `member_protocol.md` §3.1. The agent generates an Ed25519 key pair
//! locally, sends the public half with a proof of possession over the
//! `TIG-POOL-ENROLLMENT-V1` string, and presents the one-time ticket the
//! member obtained from the account system. "The private key must never
//! leave the member machine", so the pool sees a public key and a signature
//! and nothing else.
//!
//! This is the one member-protocol route with no worker credential to sign
//! with — there is no worker yet. What stands in for authentication is the
//! ticket: `security.md` §4.1 makes "ticket lookup, expiry check, one-time
//! consumption, and worker/credential creation" **one transaction**, so a
//! ticket buys exactly one worker even if two agents redeem it at once.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::{Json, Router};
use pool_identity::keys::{
    decode_public_key, enrollment_signing_string, sha256_hex, verify_b64url,
};
use serde::{Deserialize, Serialize};

use crate::account::AccountState;
use crate::error::ApiError;
use crate::protocol::{PACKAGE_FORMAT, PROTOCOL_VERSION, ServerTime};

/// §3.1's `EnrollRequest`, as `schemas/member_protocol/v0.1.0` pins it.
///
/// `deny_unknown_fields` because §14 makes a field this version does not
/// define incompatible input rather than something to ignore.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollRequest {
    pub(crate) enrollment_request_id: String,
    /// The bearer value. Hashed under the ticket key and never stored
    /// (`security.md` §4.1).
    pub(crate) enrollment_ticket: String,
    pub(crate) worker_name: String,
    pub(crate) ed25519_public_key: String,
    pub(crate) ed25519_key_proof: String,
    pub(crate) supported_protocol_versions: Vec<String>,
    pub(crate) supported_package_formats: Vec<String>,
    pub(crate) member_agent_version: String,
}

/// §3.1's `EnrollResponse`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EnrollResponse {
    pub(crate) protocol_version: &'static str,
    pub(crate) enrollment_request_id: String,
    pub(crate) package_format: &'static str,
    pub(crate) member_id: String,
    pub(crate) worker_id: String,
    pub(crate) credential_id: String,
    /// `const "ACTIVE"` in the schema: a worker that enrolled is active, and
    /// there is no other outcome this response can report.
    pub(crate) worker_status: &'static str,
    pub(crate) server_time: String,
}

pub(crate) fn routes(state: AccountState, max_control_body_bytes: u64) -> Router {
    crate::service::bounded(
        Router::new().route("/member/v0/enroll", axum::routing::post(enroll)),
        max_control_body_bytes,
    )
    .with_state(state)
}

/// Every way a ticket fails to buy a worker.
///
/// One answer, like every other authentication failure here: a caller who
/// learned that their ticket was known but expired would learn which tickets
/// exist. §4.1: failures "do not reveal whether a worker, credential, or
/// resource ID exists".
fn not_authenticated() -> ApiError {
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        error_code: "NOT_AUTHENTICATED",
        message: "that ticket does not enrol a worker".to_owned(),
        retryable: false,
        request_id: None,
    }
}

/// §4: "no common version produces HTTP 426 and INCOMPATIBLE_PROTOCOL".
fn incompatible_protocol() -> ApiError {
    ApiError {
        status: StatusCode::UPGRADE_REQUIRED,
        error_code: "INCOMPATIBLE_PROTOCOL",
        message: "this server speaks no version or package format you offered".to_owned(),
        retryable: false,
        request_id: None,
    }
}

/// §3.1: "changing any field for that ID is a conflict."
fn idempotency_conflict() -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        error_code: "IDEMPOTENCY_CONFLICT",
        message: "that enrollment_request_id was used for a different request".to_owned(),
        retryable: false,
        request_id: None,
    }
}

fn unavailable() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        error_code: "TEMPORARILY_UNAVAILABLE",
        message: "the pool could not complete this request".to_owned(),
        retryable: true,
        request_id: None,
    }
}

async fn enroll(
    State(state): State<AccountState>,
    body: Result<Json<EnrollRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(server_time) = ServerTime::at((state.now)()) else {
        return crate::service::no_server_time();
    };

    let Ok(Json(request)) = body else {
        return ApiError {
            status: StatusCode::BAD_REQUEST,
            error_code: "MALFORMED_REQUEST",
            message: "the request body is not the shape this route accepts".to_owned(),
            retryable: false,
            request_id: None,
        }
        .into_response_at(&server_time);
    };

    match enrol_worker(&state, &request, &server_time).await {
        Ok((status, response)) => (status, Json(response)).into_response(),
        // §3.2's echo is `request_id`, which this route has no header for;
        // `enrollment_request_id` is the identifier an enrolling agent has,
        // and §3.2 names it for exactly this case.
        Err(e) => e
            .echoing(&request.enrollment_request_id)
            .into_response_at(&server_time),
    }
}

async fn enrol_worker(
    state: &AccountState,
    request: &EnrollRequest,
    server_time: &ServerTime,
) -> Result<(StatusCode, EnrollResponse), ApiError> {
    // §14, before anything else: an identifier that is not one is
    // incompatible input, and `$1::uuid` would otherwise report the pool's
    // schema to a caller who sent a typo.
    let enrollment_request_id = canonical_uuid(&request.enrollment_request_id)?;

    // §4: the server "selects and returns one exact common protocol version
    // and package format". Exact, not compatible — §4 says the server "does
    // not guess compatibility from SemVer".
    if !request
        .supported_protocol_versions
        .iter()
        .any(|v| v == PROTOCOL_VERSION)
        || !request
            .supported_package_formats
            .iter()
            .any(|f| f == PACKAGE_FORMAT)
    {
        return Err(incompatible_protocol());
    }

    // The proof of possession, before the database is asked anything.
    // `security.md` §4.1 puts signature verification ahead of database work,
    // and here the signature is over the ticket's digest, so a caller who
    // does not hold the key cannot even probe whether a ticket is live.
    //
    // The **raw** ticket. `enrollment_signing_string` hashes it itself — §3.1
    // puts "SHA-256 of the UTF-8 enrollment ticket" in the string, and the
    // helper is what puts it there. Handing it a digest produced a proof over
    // SHA-256(hex(SHA-256(ticket))), which no conforming agent would ever
    // make, and the test helper reproduced the same mistake so the two agreed
    // with each other and with nothing else. The test now builds the string
    // literally from §3.1 instead of calling this function, which is what
    // makes it a client rather than a mirror.
    let signing_string = enrollment_signing_string(
        &enrollment_request_id,
        &request.enrollment_ticket,
        &request.ed25519_public_key,
    );
    let public_key = decode_public_key(&request.ed25519_public_key).map_err(|_| {
        tracing::info!(event = "enroll.rejected", reason = "MALFORMED_PUBLIC_KEY");
        not_authenticated()
    })?;
    verify_b64url(&public_key, &signing_string, &request.ed25519_key_proof).map_err(|_| {
        tracing::info!(event = "enroll.rejected", reason = "KEY_PROOF_FAILED");
        not_authenticated()
    })?;

    // §5: "repeating the same key and body returns the recorded result.
    // Repeating the key with a different body returns 409." The recorded
    // result is the worker row itself — §3.1's idempotency lives there rather
    // than in a separate ledger, so there is one thing to keep consistent
    // instead of two.
    let body_sha256 = canonical_body_sha256(request);
    if let Some(existing) = recorded_enrollment(state, &enrollment_request_id).await? {
        if existing.body_sha256 != body_sha256 {
            return Err(idempotency_conflict());
        }
        return Ok((
            StatusCode::OK,
            existing.into_response(&enrollment_request_id, server_time),
        ));
    }

    let mut tx = state.db.begin().await.map_err(|e| {
        tracing::error!(event = "enroll.begin_failed", error = %e);
        unavailable()
    })?;

    // §4.1's one transaction. It starts by *locking* the ticket rather than
    // consuming it, because `enrollment_ticket.consumed_by_worker_id` has to
    // name a worker that exists and the worker does not exist yet — the row
    // refuses a consumption recorded against nothing, which is the rule
    // keeping a consumed ticket attached to what it bought.
    //
    // `FOR UPDATE` is what makes the order safe: a second agent redeeming the
    // same ticket blocks here until this transaction commits, then reads
    // `consumed_at` set and gets nothing. One ticket, one worker, however
    // many arrive at once.
    //
    // Matched by HMAC, which is also the primary key — the bearer value is
    // never stored, so there is nothing else to match on.
    let ticket_hmac = state.key.hmac(request.enrollment_ticket.as_bytes());
    let member_id: Option<String> = sqlx::query_scalar(
        "SELECT member_id::text FROM pool.enrollment_ticket
          WHERE network = $1
            AND ticket_hmac = $2
            AND purpose = 'WORKER_ENROLLMENT'
            AND consumed_at IS NULL
            AND expires_at > now()
          FOR UPDATE",
    )
    .bind(&state.network)
    .bind(ticket_hmac.as_slice())
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "enroll.ticket_read_failed", error = %e);
        unavailable()
    })?;

    let Some(member_id) = member_id else {
        // Unknown, already consumed, expired, or the wrong purpose. One
        // answer for all four (§4.1).
        tracing::info!(event = "enroll.rejected", reason = "TICKET_NOT_REDEEMABLE");
        return Err(not_authenticated());
    };

    // §3.1: the response "records the exact negotiated protocol version", and
    // §4 keeps supporting it for an open assignment even after the pool stops
    // offering it — so it is a fact of the worker, written once.
    let worker_id: String = sqlx::query_scalar(
        "INSERT INTO pool.worker
             (network, worker_id, member_id, protocol_version,
              enrollment_request_id, enrollment_request_sha256)
         VALUES ($1, gen_random_uuid(), $2::uuid, $3, $4::uuid, $5)
         RETURNING worker_id::text",
    )
    .bind(&state.network)
    .bind(&member_id)
    .bind(PROTOCOL_VERSION)
    .bind(&enrollment_request_id)
    .bind(body_sha256.as_slice())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "enroll.worker_write_failed", error = %e);
        unavailable()
    })?;

    // "Worker public keys are ordinary database facts. A worker private key
    // is generated and retained only on the member machine" (§4.1).
    let credential_id: String = sqlx::query_scalar(
        "INSERT INTO pool.worker_credential (network, credential_id, worker_id, public_key)
         VALUES ($1, gen_random_uuid(), $2::uuid, $3)
         RETURNING credential_id::text",
    )
    .bind(&state.network)
    .bind(&worker_id)
    .bind(public_key.as_bytes().as_slice())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "enroll.credential_write_failed", error = %e);
        unavailable()
    })?;

    // And now the ticket is spent, naming what it bought.
    let consumed = sqlx::query(
        "UPDATE pool.enrollment_ticket
            SET consumed_at = now(), consumed_by_worker_id = $3::uuid
          WHERE network = $1 AND ticket_hmac = $2 AND consumed_at IS NULL",
    )
    .bind(&state.network)
    .bind(ticket_hmac.as_slice())
    .bind(&worker_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "enroll.ticket_consume_failed", error = %e);
        unavailable()
    })?;

    if consumed.rows_affected() != 1 {
        // Unreachable while the row above is locked, and checked anyway: an
        // UPDATE matching nothing reports success, so "the ticket was spent"
        // and "the ticket was already spent" look alike from the result.
        tracing::error!(
            event = "enroll.ticket_consume_lost",
            rows = consumed.rows_affected(),
            "the locked ticket was not the one consumed"
        );
        return Err(unavailable());
    }

    tx.commit().await.map_err(|e| {
        tracing::error!(event = "enroll.commit_failed", error = %e);
        unavailable()
    })?;

    tracing::info!(
        event = "enroll.worker_enrolled",
        member_id = %member_id,
        worker_id = %worker_id,
        credential_id = %credential_id,
        worker_name = %request.worker_name,
        member_agent_version = %request.member_agent_version,
        "a ticket was redeemed for a worker"
    );

    Ok((
        StatusCode::CREATED,
        EnrollResponse {
            protocol_version: PROTOCOL_VERSION,
            enrollment_request_id,
            package_format: PACKAGE_FORMAT,
            member_id,
            worker_id,
            credential_id,
            worker_status: "ACTIVE",
            server_time: server_time.as_str().to_owned(),
        },
    ))
}

/// What a previous enrollment under this `enrollment_request_id` produced.
struct Recorded {
    body_sha256: Vec<u8>,
    member_id: String,
    worker_id: String,
    credential_id: String,
}

impl Recorded {
    fn into_response(
        self,
        enrollment_request_id: &str,
        server_time: &ServerTime,
    ) -> EnrollResponse {
        EnrollResponse {
            protocol_version: PROTOCOL_VERSION,
            enrollment_request_id: enrollment_request_id.to_owned(),
            package_format: PACKAGE_FORMAT,
            member_id: self.member_id,
            worker_id: self.worker_id,
            credential_id: self.credential_id,
            worker_status: "ACTIVE",
            // The *current* server time, not the original's. §13 makes this
            // the value a member diagnoses skew against, so a replayed
            // timestamp would be a stale clock presented as the server's.
            server_time: server_time.as_str().to_owned(),
        }
    }
}

async fn recorded_enrollment(
    state: &AccountState,
    enrollment_request_id: &str,
) -> Result<Option<Recorded>, ApiError> {
    // The credential is the one enrollment created, which is the *first* for
    // this worker — §3.3's rotation adds later ones, and a retry of the
    // enrollment must not report a credential the member rotated to since.
    let row: Option<(Vec<u8>, String, String, String)> = sqlx::query_as(
        "SELECT w.enrollment_request_sha256,
                w.member_id::text,
                w.worker_id::text,
                (SELECT c.credential_id::text
                   FROM pool.worker_credential c
                  WHERE c.network = w.network AND c.worker_id = w.worker_id
                  ORDER BY c.created_at, c.credential_id
                  LIMIT 1)
           FROM pool.worker w
          WHERE w.network = $1 AND w.enrollment_request_id = $2::uuid",
    )
    .bind(&state.network)
    .bind(enrollment_request_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(event = "enroll.idempotency_read_failed", error = %e);
        unavailable()
    })?;

    Ok(row.map(
        |(body_sha256, member_id, worker_id, credential_id)| Recorded {
            body_sha256,
            member_id,
            worker_id,
            credential_id,
        },
    ))
}

/// The canonical hash of what was asked for.
///
/// Over the fields this version defines, in a fixed order, rather than over
/// the transmitted bytes: §5 decides "identical" by the canonical body, and
/// an agent that re-serialised its own request with the fields in a
/// different order has not changed what it asked for. `deny_unknown_fields`
/// means there is nothing outside this list to miss.
fn canonical_body_sha256(request: &EnrollRequest) -> Vec<u8> {
    let canonical = serde_json::json!({
        "enrollment_request_id": request.enrollment_request_id,
        // The ticket by its digest, never its value: this hash is stored.
        "enrollment_ticket_sha256": sha256_hex(request.enrollment_ticket.as_bytes()),
        "worker_name": request.worker_name,
        "ed25519_public_key": request.ed25519_public_key,
        "ed25519_key_proof": request.ed25519_key_proof,
        "supported_protocol_versions": request.supported_protocol_versions,
        "supported_package_formats": request.supported_package_formats,
        "member_agent_version": request.member_agent_version,
    });
    let hex = sha256_hex(canonical.to_string().as_bytes());
    (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A lowercase canonical UUID, as every identifier in the pinned schema is.
fn canonical_uuid(value: &str) -> Result<String, ApiError> {
    let shape_ok = value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        });
    if shape_ok {
        Ok(value.to_owned())
    } else {
        Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            error_code: "MALFORMED_REQUEST",
            message: "enrollment_request_id is not a canonical uuid".to_owned(),
            retryable: false,
            request_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    //! In this crate for the same reason `account`'s tests are: the state
    //! holds a `TicketKey`, which exists only through a crate-private loader.
    //!
    //! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
    //! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ed25519_dalek::SigningKey;
    use pool_identity::keys::{encode_public_key, sign_b64url};

    use super::*;
    use crate::account::tests::{ALICE_KEY, BOB_KEY, NONCE, issue_one_ticket, migrated, now};

    const REQUEST_ID: &str = "11111111-1111-4111-8111-111111111111";

    /// TEST-ONLY deterministic key bytes; never a real credential.
    fn agent_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// §3.1's proof string, written out here rather than built by calling the
    /// crate's own helper.
    ///
    /// ```text
    /// TIG-POOL-ENROLLMENT-V1
    /// <enrollment_request_id>
    /// <SHA-256 of the UTF-8 enrollment ticket as 64 lowercase hex characters>
    /// <ed25519_public_key>
    /// ```
    ///
    /// This is the whole point of the test: a conforming agent reads §3.1 and
    /// builds this, and the server has to accept *that*. Calling
    /// `enrollment_signing_string` on both sides made the test a mirror — the
    /// first version of this route passed the already-hashed ticket into a
    /// helper that hashes what it is given, the helper here did the same, and
    /// the two agreed with each other while rejecting every real agent.
    fn proof_string(request_id: &str, ticket: &str, public_key: &str) -> String {
        format!(
            "TIG-POOL-ENROLLMENT-V1\n{request_id}\n{}\n{public_key}",
            sha256_hex(ticket.as_bytes())
        )
    }

    /// A conforming `EnrollRequest` for `ticket`, signed by `key`.
    fn enrol_request(ticket: &str, key: &SigningKey, request_id: &str) -> EnrollRequest {
        let public = encode_public_key(&key.verifying_key());
        let proof = sign_b64url(key, &proof_string(request_id, ticket, &public));
        EnrollRequest {
            enrollment_request_id: request_id.to_owned(),
            enrollment_ticket: ticket.to_owned(),
            worker_name: "workshop-1".to_owned(),
            ed25519_public_key: public,
            ed25519_key_proof: proof,
            supported_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            supported_package_formats: vec![PACKAGE_FORMAT.to_owned()],
            member_agent_version: "0.1.0".to_owned(),
        }
    }

    fn server_time() -> ServerTime {
        ServerTime::at(now()).expect("a representable instant")
    }

    #[tokio::test]
    async fn a_ticket_buys_a_worker_and_its_first_credential() {
        let Some((_db, state)) = migrated("enroll_happy").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;

        let (status, response) = enrol_worker(
            &state,
            &enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect("a live ticket enrols a worker");

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(response.member_id, ticket.member_id);
        assert_eq!(response.worker_status, "ACTIVE");
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        assert_eq!(response.package_format, PACKAGE_FORMAT);

        // The credential holds the public half the agent sent, and the pool
        // has nothing else: "a worker private key is generated and retained
        // only on the member machine" (§4.1).
        let stored: Vec<u8> = sqlx::query_scalar(
            "SELECT public_key FROM pool.worker_credential WHERE credential_id = $1::uuid",
        )
        .bind(&response.credential_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(stored, agent_key(0xa1).verifying_key().as_bytes().to_vec());

        // And the ticket is spent, naming what it bought.
        let (consumed, by): (bool, String) = sqlx::query_as(
            "SELECT consumed_at IS NOT NULL, consumed_by_worker_id::text
               FROM pool.enrollment_ticket WHERE member_id = $1::uuid",
        )
        .bind(&ticket.member_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert!(consumed);
        assert_eq!(by, response.worker_id);
    }

    #[tokio::test]
    async fn one_ticket_buys_exactly_one_worker() {
        // §3.1: a ticket is "single-use". The second redemption is refused
        // with the same answer an unknown ticket gets.
        let Some((_db, state)) = migrated("enroll_single_use").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;

        enrol_worker(
            &state,
            &enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect("the first");

        let second = enrol_worker(
            &state,
            &enrol_request(
                &ticket.ticket,
                &agent_key(0xb2),
                "22222222-2222-4222-8222-222222222222",
            ),
            &server_time(),
        )
        .await
        .expect_err("the second must be refused");
        assert_eq!(second.error_code, "NOT_AUTHENTICATED");

        let workers: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.worker")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(workers, 1, "one ticket, one worker");
    }

    #[tokio::test]
    async fn a_ticket_that_is_not_redeemable_is_refused_the_same_way_as_one_that_never_existed() {
        // §4.1: authentication failures "do not reveal whether a worker,
        // credential, or resource ID exists". Expired, already spent, and
        // never issued must be one answer.
        let Some((_db, state)) = migrated("enroll_not_redeemable").await else {
            return;
        };
        let member_id = issue_one_ticket(&state, ALICE_KEY, NONCE).await.member_id;

        // A ticket that was issued already expired, rather than one edited
        // afterwards: `0018`'s `ticket_terms_are_fixed` trigger refuses to
        // move an issued ticket's expiry *for anyone*, including the table's
        // owner, and `pool_api`'s UPDATE grant covers only `consumed_at` and
        // `consumed_by_worker_id`. Two earlier versions of this test tried to
        // edit the row and were refused by each of those in turn — which is
        // the rule working, and why the row is written this way instead.
        let stale = "a-ticket-whose-moment-has-passed";
        sqlx::query(
            "INSERT INTO pool.enrollment_ticket
                 (network, ticket_hmac, member_id, purpose, expires_at)
             VALUES ('testnet', $1, $2::uuid, 'WORKER_ENROLLMENT',
                     now() - interval '1 second')",
        )
        .bind(state.key.hmac(stale.as_bytes()).as_slice())
        .bind(&member_id)
        .execute(&state.db)
        .await
        .expect("a ticket may be issued with any expiry within its ceiling");

        let expired = enrol_worker(
            &state,
            &enrol_request(stale, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect_err("an expired ticket must be refused");

        let unknown = enrol_worker(
            &state,
            &enrol_request("not-a-ticket-anyone-issued", &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect_err("an unknown ticket must be refused");

        assert_eq!(expired.error_code, unknown.error_code);
        assert_eq!(expired.status, unknown.status);
        assert_eq!(expired.message, unknown.message);
        assert_eq!(expired.error_code, "NOT_AUTHENTICATED");
    }

    #[tokio::test]
    async fn a_recovery_ticket_does_not_enrol_a_worker() {
        // §3.1 and §3.3 bind a ticket to one purpose, and the separation is
        // the point: consuming a *recovery* ticket attaches a new key to an
        // existing worker. An enrollment that accepted one would be a way to
        // take over a worker that already exists.
        let Some((_db, state)) = migrated("enroll_purpose").await else {
            return;
        };
        let member_id = issue_one_ticket(&state, ALICE_KEY, NONCE).await.member_id;

        // A worker for the recovery ticket to name, and the ticket itself.
        let worker_id: String = sqlx::query_scalar(
            "INSERT INTO pool.worker
                 (network, worker_id, member_id, protocol_version,
                  enrollment_request_id, enrollment_request_sha256)
             VALUES ('testnet', gen_random_uuid(), $1::uuid, '0.1.0',
                     gen_random_uuid(), $2)
             RETURNING worker_id::text",
        )
        .bind(&member_id)
        .bind(vec![0x7e_u8; 32])
        .fetch_one(&state.db)
        .await
        .unwrap();

        let recovery = "a-ticket-for-recovering-a-worker";
        sqlx::query(
            "INSERT INTO pool.enrollment_ticket
                 (network, ticket_hmac, member_id, purpose, worker_id, expires_at)
             VALUES ('testnet', $1, $2::uuid, 'WORKER_RECOVERY', $3::uuid,
                     now() + interval '15 minutes')",
        )
        .bind(state.key.hmac(recovery.as_bytes()).as_slice())
        .bind(&member_id)
        .bind(&worker_id)
        .execute(&state.db)
        .await
        .unwrap();

        let refused = enrol_worker(
            &state,
            &enrol_request(recovery, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect_err("a recovery ticket must not enrol");
        assert_eq!(refused.error_code, "NOT_AUTHENTICATED");

        // And it is still unspent, for the recovery route that owns it.
        let consumed: bool = sqlx::query_scalar(
            "SELECT consumed_at IS NOT NULL FROM pool.enrollment_ticket
              WHERE purpose = 'WORKER_RECOVERY'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert!(!consumed);
    }

    #[tokio::test]
    async fn a_worker_belongs_to_the_member_whose_ticket_bought_it() {
        // The member comes from the ticket, never from the request — there is
        // no field for it, and the worker a ticket buys belongs to whoever
        // the ticket was issued to.
        let Some((_db, state)) = migrated("enroll_owner").await else {
            return;
        };
        let alice = issue_one_ticket(&state, ALICE_KEY, NONCE).await;
        let bob = issue_one_ticket(&state, BOB_KEY, NONCE).await;
        assert_ne!(alice.member_id, bob.member_id, "two wallets, two accounts");

        let enrolled = enrol_worker(
            &state,
            &enrol_request(&bob.ticket, &agent_key(0xb2), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect("bob's ticket");

        assert_eq!(enrolled.1.member_id, bob.member_id);
        assert_ne!(enrolled.1.member_id, alice.member_id);

        let owner: String = sqlx::query_scalar(
            "SELECT member_id::text FROM pool.worker WHERE worker_id = $1::uuid",
        )
        .bind(&enrolled.1.worker_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(owner, bob.member_id);
    }

    // Two worker threads and two spawned tasks, so the redemptions genuinely
    // overlap. On the default current-thread runtime, and with `join!` rather
    // than `spawn`, both futures share one task and never run at once — a
    // race test that never raced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_agents_redeeming_one_ticket_produce_one_worker() {
        // `security.md` §4.1 makes lookup, expiry check, consumption and
        // creation one transaction, and this is the case that needs it: two
        // agents handed the same ticket, arriving together.
        //
        // **What actually holds the line is the row, not this code.**
        // `0018`'s `ticket_terms_are_fixed` refuses to re-consume a consumed
        // ticket — "a consumed ticket is never re-armed or re-attributed" —
        // so the loser's transaction raises and rolls back whatever the
        // application does. This test passes with the `FOR UPDATE`, the
        // `consumed_at IS NULL` guard and the `rows_affected` check all three
        // removed, and that is worth knowing rather than papering over: those
        // are a second statement of the rule, not the rule.
        //
        // They still earn their place. The lock makes the loser wait instead
        // of building a worker it will throw away, and the guard and the
        // check turn a trigger exception into a refusal this route can
        // explain. But the invariant is the schema's.
        let Some((_db, state)) = migrated("enroll_race").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;

        // Spawned, not `join!`ed. `join!` polls both futures from one task,
        // so they interleave at await points but never overlap — with that
        // shape this test passed even with the lock, the guard and the
        // `rows_affected` check all removed. Two tasks on two threads is
        // what actually races them.
        let now = server_time();
        let requests = [
            enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID),
            enrol_request(
                &ticket.ticket,
                &agent_key(0xb2),
                "22222222-2222-4222-8222-222222222222",
            ),
        ];
        let mut handles = Vec::new();
        for request in requests {
            let state = state.clone();
            let now = now.clone();
            handles.push(tokio::spawn(async move {
                enrol_worker(&state, &request, &now).await.is_ok()
            }));
        }
        let mut succeeded = 0_usize;
        for handle in handles {
            if handle.await.expect("the task did not panic") {
                succeeded += 1;
            }
        }

        assert_eq!(
            succeeded, 1,
            "exactly one of two concurrent redemptions may succeed"
        );

        let workers: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.worker")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(workers, 1);
    }

    #[tokio::test]
    async fn a_key_the_agent_cannot_prove_it_holds_buys_nothing() {
        // §3.1's proof of possession. Without it, anyone holding the ticket
        // could enrol *someone else's* public key and the member's agent
        // would hold a credential it cannot sign with.
        let Some((_db, state)) = migrated("enroll_proof").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;

        // A proof made by a different key than the one presented.
        let mut request = enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID);
        let other = agent_key(0xb2);
        request.ed25519_key_proof = sign_b64url(
            &other,
            &proof_string(REQUEST_ID, &ticket.ticket, &request.ed25519_public_key),
        );
        assert_eq!(
            enrol_worker(&state, &request, &server_time())
                .await
                .expect_err("a proof by another key")
                .error_code,
            "NOT_AUTHENTICATED"
        );

        // A proof over a *different ticket*, which is what a replay of one
        // enrollment's proof against another ticket would be.
        let mut request = enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID);
        request.ed25519_key_proof = sign_b64url(
            &agent_key(0xa1),
            &proof_string(REQUEST_ID, "some other ticket", &request.ed25519_public_key),
        );
        assert_eq!(
            enrol_worker(&state, &request, &server_time())
                .await
                .expect_err("a proof over another ticket")
                .error_code,
            "NOT_AUTHENTICATED"
        );

        // The ticket is untouched: a failed proof must not spend it.
        let consumed: bool =
            sqlx::query_scalar("SELECT consumed_at IS NOT NULL FROM pool.enrollment_ticket")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(!consumed, "a refused enrollment must not spend the ticket");
    }

    #[tokio::test]
    async fn the_same_request_twice_returns_the_same_worker_and_a_changed_one_conflicts() {
        // §3.1: "retrying an identical enrollment request with the same
        // `enrollment_request_id` returns the same response; changing any
        // field for that ID is a conflict."
        let Some((_db, state)) = migrated("enroll_idempotent").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;
        let request = enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID);

        let (first_status, first) = enrol_worker(&state, &request, &server_time())
            .await
            .expect("the first");
        let (second_status, second) = enrol_worker(&state, &request, &server_time())
            .await
            .expect("an identical retry");

        assert_eq!(first_status, StatusCode::CREATED);
        assert_eq!(second_status, StatusCode::OK, "a retry created nothing new");
        assert_eq!(first, second, "the same response, field for field");

        let workers: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.worker")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(workers, 1);

        // The same id, a different body.
        let mut changed = enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID);
        changed.worker_name = "somewhere-else".to_owned();
        assert_eq!(
            enrol_worker(&state, &changed, &server_time())
                .await
                .expect_err("a changed body")
                .error_code,
            "IDEMPOTENCY_CONFLICT"
        );
    }

    #[tokio::test]
    async fn an_agent_offering_no_version_this_build_speaks_is_refused() {
        // §4: "no common version produces HTTP 426 and
        // INCOMPATIBLE_PROTOCOL, with no state change."
        let Some((_db, state)) = migrated("enroll_versions").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;

        for (versions, formats) in [
            (vec!["0.2.0".to_owned()], vec![PACKAGE_FORMAT.to_owned()]),
            (vec![], vec![PACKAGE_FORMAT.to_owned()]),
            (vec![PROTOCOL_VERSION.to_owned()], vec!["tar-v9".to_owned()]),
            (vec![PROTOCOL_VERSION.to_owned()], vec![]),
        ] {
            let mut request = enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID);
            request.supported_protocol_versions = versions.clone();
            request.supported_package_formats = formats.clone();
            let refused = enrol_worker(&state, &request, &server_time())
                .await
                .expect_err("no common version");
            assert_eq!(refused.error_code, "INCOMPATIBLE_PROTOCOL");
            assert_eq!(refused.status, StatusCode::UPGRADE_REQUIRED);
        }

        // "With no state change": the ticket is still live and still buys a
        // worker for an agent that speaks this version.
        enrol_worker(
            &state,
            &enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect("the ticket was never spent");
    }

    #[tokio::test]
    async fn a_refused_enrollment_leaves_no_worker_and_no_credential() {
        // The transaction, tested by breaking its last write.
        let Some((db, state)) = migrated("enroll_atomic").await else {
            return;
        };
        let ticket = issue_one_ticket(&state, ALICE_KEY, NONCE).await;

        let mut owner = {
            use sqlx::Connection as _;
            sqlx::PgConnection::connect_with(&db.as_role("pool_migration"))
                .await
                .unwrap()
        };
        pool_test_support::exec(
            &mut owner,
            "REVOKE INSERT ON pool.worker_credential FROM pool_api".to_owned(),
        )
        .await
        .expect("the migration role owns the grant");

        let refused = enrol_worker(
            &state,
            &enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect_err("the credential write cannot run");
        assert_eq!(refused.error_code, "TEMPORARILY_UNAVAILABLE");

        for table in ["pool.worker", "pool.worker_credential"] {
            let rows: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&state.db)
                    .await
                    .unwrap();
            assert_eq!(rows, 0, "{table} kept a row from a failed enrollment");
        }
        let consumed: bool =
            sqlx::query_scalar("SELECT consumed_at IS NOT NULL FROM pool.enrollment_ticket")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(!consumed, "and the ticket is still the member's to use");

        // With the grant back, that same ticket still works.
        pool_test_support::exec(
            &mut owner,
            "GRANT INSERT ON pool.worker_credential TO pool_api".to_owned(),
        )
        .await
        .expect("restored");
        enrol_worker(
            &state,
            &enrol_request(&ticket.ticket, &agent_key(0xa1), REQUEST_ID),
            &server_time(),
        )
        .await
        .expect("nothing was spent");
    }
}
