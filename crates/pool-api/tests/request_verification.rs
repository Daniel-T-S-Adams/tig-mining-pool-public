//! Slice-2 criteria A3, A4, A5 (the binding half) and A8: what `pool-api`
//! does with a signed request.
//!
//! Against a real database and the real `pool_api` grants. `security.md` §4.1
//! makes claims about *what the pool is asked and in what order* — "signature
//! verification and exact body-hash verification occur before JSON decoding,
//! database work, or upload quota reservation" — and a fake database would
//! only confirm the test's own model of that.
//!
//! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
//! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use ed25519_dalek::SigningKey;
use pool_api::auth::{
    HEADER_BODY_SHA256, HEADER_CREDENTIAL_ID, HEADER_PROTOCOL_VERSION, HEADER_REQUEST_ID,
    HEADER_REQUEST_TIMESTAMP, HEADER_SIGNATURE, HEADER_WORKER_ID, Verifier,
};
use pool_api::protocol::PROTOCOL_VERSION;
use pool_identity::keys::{request_signing_string, sha256_hex, sign_b64url};
use pool_test_support::{MIGRATOR, TempDb};
use sqlx::{Connection, PgPool};

const NETWORK: &str = "testnet";
const DEPLOYMENT: &str = "test";
const ALICE: &str = "0x1111111111111111111111111111111111111111";
const BOB: &str = "0x2222222222222222222222222222222222222222";
const SHA: [u8; 32] = [0x7e; 32];

/// A pinned clock: `server_time` and freshness are values these tests state.
const NOW: i64 = 1_774_000_000;

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(NOW).expect("a valid instant")
}

/// TEST-ONLY deterministic key bytes; never a real credential.
fn test_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

struct Enrolled {
    member_id: String,
    worker_id: String,
    credential_id: String,
    key: SigningKey,
}

/// A migrated database and a pool connected as `pool_api`.
async fn migrated(name: &str) -> Option<(TempDb, PgPool)> {
    let db = TempDb::create(name).await?;
    let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
        .await
        .unwrap();
    MIGRATOR.run(&migration_pool).await.unwrap();
    let api = PgPool::connect_with(db.as_role("pool_api")).await.unwrap();
    Some((db, api))
}

/// An account, a worker, and an active credential holding `key`'s public half.
async fn enrol(api: &PgPool, wallet: &str, seed: u8) -> Enrolled {
    let key = test_key(seed);
    let member_id: String = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ($1, gen_random_uuid(), $2) RETURNING member_id::text",
    )
    .bind(NETWORK)
    .bind(wallet)
    .fetch_one(api)
    .await
    .unwrap();

    let worker_id: String = sqlx::query_scalar(
        "INSERT INTO pool.worker
             (network, worker_id, member_id, protocol_version,
              enrollment_request_id, enrollment_request_sha256)
         VALUES ($1, gen_random_uuid(), $2::uuid, $3, gen_random_uuid(), $4)
         RETURNING worker_id::text",
    )
    .bind(NETWORK)
    .bind(&member_id)
    .bind(PROTOCOL_VERSION)
    .bind(SHA.as_slice())
    .fetch_one(api)
    .await
    .unwrap();

    let credential_id: String = sqlx::query_scalar(
        "INSERT INTO pool.worker_credential (network, credential_id, worker_id, public_key)
         VALUES ($1, gen_random_uuid(), $2::uuid, $3)
         RETURNING credential_id::text",
    )
    .bind(NETWORK)
    .bind(&worker_id)
    .bind(key.verifying_key().as_bytes().as_slice())
    .fetch_one(api)
    .await
    .unwrap();

    Enrolled {
        member_id,
        worker_id,
        credential_id,
        key,
    }
}

fn verifier(db: &PgPool) -> Verifier {
    Verifier {
        db: db.clone(),
        network: NETWORK.to_owned(),
        deployment: DEPLOYMENT.to_owned(),
    }
}

/// One request id per attempt, as §3.2 describes.
fn request_id(n: u8) -> String {
    format!("00000000-0000-4000-8000-0000000000{n:02x}")
}

/// A request signed exactly as §3.2 specifies, with whatever overrides the
/// caller wants applied *after* signing — so a test can change one field and
/// see the signature stop matching.
struct Signed {
    method: String,
    path: String,
    body: Vec<u8>,
    headers: HeaderMap,
}

fn sign(
    who: &Enrolled,
    method: &str,
    path: &str,
    body: &[u8],
    request_id: &str,
    timestamp: i64,
) -> Signed {
    sign_at(
        who,
        method,
        path,
        body,
        request_id,
        u64::try_from(timestamp).expect("a non-negative test timestamp"),
    )
}

/// The same, with the §3.2 timestamp given as the unsigned value it is — so a
/// test can sign one at the end of the range rather than only put it in the
/// header, where the signature would refuse it before the freshness check
/// ever ran.
fn sign_at(
    who: &Enrolled,
    method: &str,
    path: &str,
    body: &[u8],
    request_id: &str,
    timestamp: u64,
) -> Signed {
    sign_as(
        &who.key,
        &who.worker_id,
        &who.credential_id,
        method,
        path,
        body,
        request_id,
        timestamp,
    )
}

/// Sign as `key`, claiming `worker_id` and `credential_id`.
///
/// Separate from `sign` so a test can claim an identity its key does not
/// belong to — the substitution being *inside* the signed string, which is
/// what a member with their own key can actually do.
#[allow(clippy::too_many_arguments)]
fn sign_as(
    key: &SigningKey,
    worker_id: &str,
    credential_id: &str,
    method: &str,
    path: &str,
    body: &[u8],
    request_id: &str,
    timestamp: u64,
) -> Signed {
    let body_sha256 = sha256_hex(body);
    let signing_string = request_signing_string(
        method,
        path,
        PROTOCOL_VERSION,
        worker_id,
        credential_id,
        request_id,
        timestamp,
        &body_sha256,
    );
    let signature = sign_b64url(key, &signing_string);

    let mut headers = HeaderMap::new();
    for (name, value) in [
        (HEADER_PROTOCOL_VERSION, PROTOCOL_VERSION.to_owned()),
        (HEADER_WORKER_ID, worker_id.to_owned()),
        (HEADER_CREDENTIAL_ID, credential_id.to_owned()),
        (HEADER_REQUEST_ID, request_id.to_owned()),
        (HEADER_REQUEST_TIMESTAMP, timestamp.to_string()),
        (HEADER_BODY_SHA256, body_sha256),
        (HEADER_SIGNATURE, signature),
    ] {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(&value).unwrap(),
        );
    }

    Signed {
        method: method.to_owned(),
        path: path.to_owned(),
        body: body.to_vec(),
        headers,
    }
}

impl Signed {
    fn with(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        self
    }

    fn body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }
}

async fn verify(
    v: &Verifier,
    req: &Signed,
) -> Result<pool_api::auth::VerifiedWorker, pool_api::error::ApiError> {
    v.verify(&req.method, &req.path, &req.headers, &req.body, now())
        .await
}

const GET: &str = "GET";
const PATH: &str = "/member/v0/slots/00000000-0000-4000-8000-000000000001/qualification/status";

#[tokio::test]
async fn a_correctly_signed_request_names_the_account_behind_the_worker() {
    let Some((_db, api)) = migrated("verify_ok").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let bob = enrol(&api, BOB, 0xb2).await;

    let verified = verify(
        &verifier(&api),
        &sign(&alice, GET, PATH, b"", &request_id(1), NOW),
    )
    .await
    .expect("a conforming request verifies");

    assert_eq!(verified.worker_id, alice.worker_id);
    assert_eq!(verified.credential_id, alice.credential_id);
    assert_eq!(verified.request_id, request_id(1));

    // §3.2: "the pool derives `member_id` from its stored worker binding
    // rather than trusting a client claim". There is no member header to
    // trust, and the answer is the binding's — not the other account's.
    assert_eq!(verified.member_id, alice.member_id);
    assert_ne!(verified.member_id, bob.member_id);
}

#[tokio::test]
async fn a_body_that_does_not_match_its_digest_is_refused_before_any_database_work() {
    // Criterion A3, and `security.md` §4.1's ordering: "signature
    // verification and exact body-hash verification occur before JSON
    // decoding, database work, or upload quota reservation".
    //
    // Proved by closing the pool. Any query at all would then fail and be
    // reported as `TEMPORARILY_UNAVAILABLE`; a plain `NOT_AUTHENTICATED`
    // means the database was never asked.
    let Some((_db, api)) = migrated("verify_body_first").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let request = sign(&alice, GET, PATH, b"{}", &request_id(1), NOW).body(b"{ }");

    api.close().await;

    let denied = verify(&verifier(&api), &request)
        .await
        .expect_err("a mismatched body is refused");
    assert_eq!(denied.error_code, "NOT_AUTHENTICATED");
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn nothing_about_the_worker_is_read_until_the_signature_verifies() {
    // `security.md` §4.1 puts verification before "database work". One read
    // is unavoidable — a signature is checked against a public key, and §4.1
    // itself calls worker public keys "ordinary database facts" — so the rule
    // is kept by making that the *only* read before verification: the worker,
    // the account binding and every state are read afterwards.
    //
    // Observable by taking the second read away. With `SELECT` on
    // `pool.worker` revoked, a request the pool cannot authenticate must
    // still answer `NOT_AUTHENTICATED` — it never got that far — while one it
    // can authenticate reaches the missing privilege and reports the pool's
    // own failure. A single joined query would answer the second way to both.
    let Some((db, api)) = migrated("verify_read_order").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    let mut owner = sqlx::PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    pool_test_support::exec(
        &mut owner,
        "REVOKE SELECT ON pool.worker FROM pool_api".to_owned(),
    )
    .await
    .expect("the migration role owns the grant");

    let forged =
        sign(&alice, GET, PATH, b"", &request_id(1), NOW).with(HEADER_SIGNATURE, &"A".repeat(86));
    let denied = verify(&v, &forged).await.expect_err("a bad signature");
    assert_eq!(
        denied.error_code, "NOT_AUTHENTICATED",
        "an unverifiable request must not reach the worker table"
    );

    let genuine = sign(&alice, GET, PATH, b"", &request_id(2), NOW);
    let blocked = verify(&v, &genuine)
        .await
        .expect_err("the binding read cannot run");
    assert_eq!(
        blocked.error_code, "TEMPORARILY_UNAVAILABLE",
        "a verified request does reach it"
    );
}

#[tokio::test]
async fn a_closed_database_is_the_pools_failure_and_not_the_callers() {
    // The control for the test above: with the body hash correct, the same
    // closed pool produces the other answer. Without this, that test would
    // pass even if `NOT_AUTHENTICATED` were returned for every database
    // failure too.
    let Some((_db, api)) = migrated("verify_db_down").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let request = sign(&alice, GET, PATH, b"{}", &request_id(1), NOW);

    api.close().await;

    let denied = verify(&verifier(&api), &request)
        .await
        .expect_err("the lookup cannot run");
    assert_eq!(denied.error_code, "TEMPORARILY_UNAVAILABLE");
    assert_eq!(denied.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(denied.retryable, "the pool asks to be asked again");
}

#[tokio::test]
async fn the_signature_covers_the_method_the_path_and_the_body() {
    // §3.2's signed string names all three. If any were left out, a captured
    // signature would authorise a different request.
    let Some((_db, api)) = migrated("verify_binding").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    let original = sign(&alice, GET, PATH, b"{}", &request_id(1), NOW);

    // Method changed after signing: the digest still matches the body, so
    // only the signature can catch this.
    let mut moved = sign(&alice, GET, PATH, b"{}", &request_id(2), NOW);
    moved.method = "POST".to_owned();
    assert_eq!(
        verify(&v, &moved).await.expect_err("method").error_code,
        "NOT_AUTHENTICATED"
    );

    let mut elsewhere = sign(&alice, GET, PATH, b"{}", &request_id(3), NOW);
    elsewhere.path = "/member/v0/heartbeats".to_owned();
    assert_eq!(
        verify(&v, &elsewhere).await.expect_err("path").error_code,
        "NOT_AUTHENTICATED"
    );

    // A different body, with its digest header updated to match it: the body
    // hash check passes and the signature is what refuses.
    let other_body = b"{\"x\":1}";
    let swapped = sign(&alice, GET, PATH, b"{}", &request_id(4), NOW)
        .body(other_body)
        .with(HEADER_BODY_SHA256, &sha256_hex(other_body));
    assert_eq!(
        verify(&v, &swapped).await.expect_err("body").error_code,
        "NOT_AUTHENTICATED"
    );

    // And the untouched request verifies, so the three rejections above are
    // about what changed rather than about the fixture.
    verify(&v, &original).await.expect("the original verifies");
}

#[tokio::test]
async fn a_credential_does_not_authorise_another_worker() {
    // §3.2: "the credential authorizes only its exact `worker_id`".
    // `security.md` §4.1: the refusal must not say whether the worker or the
    // credential exists — so this is the same answer as a credential that was
    // never issued.
    let Some((_db, api)) = migrated("verify_cross_worker").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let bob = enrol(&api, BOB, 0xb2).await;
    let v = verifier(&api);

    // Alice's credential, Bob's worker id, signed by Alice over a string that
    // *names Bob's worker id*.
    //
    // Signing the substitution matters. Overriding only the header would
    // leave the signed string disagreeing with it, and the signature check
    // would refuse — so the test would pass with the query's worker scope
    // removed. Alice holds her own key and can sign whatever she likes, which
    // is precisely why the scope has to be in the lookup.
    let borrowed = sign_as(
        &alice.key,
        &bob.worker_id,
        &alice.credential_id,
        GET,
        PATH,
        b"",
        &request_id(1),
        NOW as u64,
    );
    let cross = verify(&v, &borrowed).await.expect_err("cross-worker");

    // A credential that does not exist at all.
    let unknown = sign(&alice, GET, PATH, b"", &request_id(2), NOW)
        .with(HEADER_CREDENTIAL_ID, "00000000-0000-4000-8000-00000000ffff");
    let absent = verify(&v, &unknown).await.expect_err("unknown credential");

    assert_eq!(cross.error_code, absent.error_code);
    assert_eq!(cross.status, absent.status);
    assert_eq!(cross.message, absent.message);
    assert_eq!(cross.error_code, "NOT_AUTHENTICATED");
}

#[tokio::test]
async fn a_credential_or_worker_that_is_no_longer_active_is_refused() {
    // §3.3: credential and worker state "take effect on the next request", so
    // both are read on every one. The signature is valid in each case here —
    // it is the state that refuses.
    let Some((_db, api)) = migrated("verify_state").await else {
        return;
    };
    let v = verifier(&api);

    let revoked_credential = enrol(&api, ALICE, 0xa1).await;
    sqlx::query(
        "UPDATE pool.worker_credential SET state = 'REVOKED', revoked_at = now()
          WHERE credential_id = $1::uuid",
    )
    .bind(&revoked_credential.credential_id)
    .execute(&api)
    .await
    .unwrap();
    assert_eq!(
        verify(
            &v,
            &sign(&revoked_credential, GET, PATH, b"", &request_id(1), NOW)
        )
        .await
        .expect_err("revoked credential")
        .error_code,
        "NOT_AUTHENTICATED"
    );

    let revoked_worker = enrol(&api, BOB, 0xb2).await;
    sqlx::query(
        "UPDATE pool.worker
            SET state = 'REVOKED', revoked_at = now(), revocation_reason = 'MEMBER_REQUEST'
          WHERE worker_id = $1::uuid",
    )
    .bind(&revoked_worker.worker_id)
    .execute(&api)
    .await
    .unwrap();
    assert_eq!(
        verify(
            &v,
            &sign(&revoked_worker, GET, PATH, b"", &request_id(2), NOW)
        )
        .await
        .expect_err("revoked worker")
        .error_code,
        "NOT_AUTHENTICATED"
    );
}

#[tokio::test]
async fn a_credential_whose_rotation_grace_has_ended_is_refused() {
    // §3.3 bounds the grace at ten minutes from the rotation. A credential
    // still marked ACTIVE but past `not_after` must stop working, or rotation
    // would never actually retire anything.
    let Some((_db, api)) = migrated("verify_grace").await else {
        return;
    };
    let inside = enrol(&api, ALICE, 0xa1).await;
    let ended = enrol(&api, BOB, 0xb2).await;
    let v = verifier(&api);

    // Two credentials rather than one moved twice: `0018`'s
    // `credential_identity_is_immutable` refuses to move a grace once it is
    // declared, which is the rule that stops ten minutes becoming an hour one
    // UPDATE at a time. Each grace here is therefore set exactly once.
    let declare_grace = |credential_id: String, started: &'static str, ends: &'static str| {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE pool.worker_credential
                SET grace_started_at = now() - interval '{started}',
                    not_after       = now() + interval '{ends}'
              WHERE credential_id = $1::uuid"
        )))
        .bind(credential_id)
        .execute(&api)
    };

    declare_grace(inside.credential_id.clone(), "0 minutes", "5 minutes")
        .await
        .unwrap();
    verify(&v, &sign(&inside, GET, PATH, b"", &request_id(1), NOW))
        .await
        .expect("a credential inside its grace still signs");

    // Past it, and still marked ACTIVE — which is the whole point: nothing
    // sweeps the row, so the read is what has to notice.
    declare_grace(ended.credential_id.clone(), "11 minutes", "-1 minute")
        .await
        .unwrap();
    let state: String = sqlx::query_scalar(
        "SELECT state FROM pool.worker_credential WHERE credential_id = $1::uuid",
    )
    .bind(&ended.credential_id)
    .fetch_one(&api)
    .await
    .unwrap();
    assert_eq!(state, "ACTIVE", "the row was never swept");

    assert_eq!(
        verify(&v, &sign(&ended, GET, PATH, b"", &request_id(2), NOW))
            .await
            .expect_err("grace ended")
            .error_code,
        "NOT_AUTHENTICATED"
    );
}

#[tokio::test]
async fn a_timestamp_outside_the_window_is_refused_and_the_edges_are_not() {
    // §3.2: "the server accepts a timestamp within 300 seconds of server
    // time". Both edges, so the check is a window rather than a formality.
    let Some((_db, api)) = migrated("verify_freshness").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    for (offset, n) in [(-300_i64, 1_u8), (300, 2), (0, 3)] {
        verify(
            &v,
            &sign(&alice, GET, PATH, b"", &request_id(n), NOW + offset),
        )
        .await
        .unwrap_or_else(|e| panic!("{offset}s must be inside the window: {e:?}"));
    }

    for (offset, n) in [(-301_i64, 4_u8), (301, 5)] {
        let denied = verify(
            &v,
            &sign(&alice, GET, PATH, b"", &request_id(n), NOW + offset),
        )
        .await
        .unwrap_err();
        assert_eq!(denied.error_code, "STALE_TIMESTAMP", "{offset}s");
        // A caller is entitled to know their own clock is wrong, and to try
        // again once it is not.
        assert!(denied.retryable);
    }
}

#[tokio::test]
async fn an_identical_retry_is_accepted_and_a_reuse_is_rejected_and_audited() {
    // §3.2: "an HTTP retry normally uses a new request ID but retains the
    // message's application idempotency key" — and reusing a request ID "with
    // different signed bytes is rejected and audited".
    let Some((_db, api)) = migrated("verify_replay").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    let attempt = sign(&alice, GET, PATH, b"", &request_id(1), NOW);
    verify(&v, &attempt).await.expect("the first attempt");
    verify(&v, &attempt)
        .await
        .expect("the identical retry of one attempt");

    // The same request id over different bytes: a different path, signed
    // correctly for that path.
    let elsewhere = sign(
        &alice,
        GET,
        "/member/v0/assignments/00000000-0000-4000-8000-000000000009/status",
        b"",
        &request_id(1),
        NOW,
    );
    let denied = verify(&v, &elsewhere).await.expect_err("a reuse");
    assert_eq!(denied.error_code, "REQUEST_ID_REUSED");
    assert_eq!(denied.status, StatusCode::CONFLICT);
    assert!(!denied.retryable, "the same request will never be accepted");

    // The durable half. `security.md` §9 makes the audit an append-only fact,
    // not a log line that rotates away.
    let (action, outcome, resource_id, reason): (String, String, String, String) = sqlx::query_as(
        "SELECT action, outcome, resource_id, reason FROM pool.audit_event
          WHERE request_id = $1::uuid",
    )
    .bind(request_id(1))
    .fetch_one(&api)
    .await
    .expect("the rejection left an audit row");
    assert_eq!(action, "REQUEST_REPLAY_REJECTED");
    assert_eq!(outcome, "REJECTED");
    assert_eq!(resource_id, alice.credential_id);
    assert_eq!(reason, "REQUEST_ID_REUSED_WITH_DIFFERENT_BYTES");

    // And the evidence names both digests, so an operator can see they really
    // differed rather than taking the row's word for it.
    let evidence: String = sqlx::query_scalar(
        "SELECT evidence::text FROM pool.audit_event WHERE request_id = $1::uuid",
    )
    .bind(request_id(1))
    .fetch_one(&api)
    .await
    .unwrap();
    let evidence: serde_json::Value = serde_json::from_str(&evidence).unwrap();
    assert_ne!(
        evidence["recorded_signed_sha256"],
        evidence["presented_signed_sha256"]
    );
    assert_eq!(evidence["member_id"], alice.member_id);
}

#[tokio::test]
async fn a_reuse_that_cannot_be_audited_is_not_refused_permanently() {
    // §3.2 says a reuse is "rejected **and** audited", and `security.md` §9
    // lists "authentication replay" among the events that must leave a
    // durable fact. The two are one answer: if the row cannot be written the
    // pool has not done what it says it does, so it must not hand back a
    // permanent `409` that no retry would ever turn into a record.
    let Some((db, api)) = migrated("verify_audit_required").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    let first = sign(&alice, GET, PATH, b"", &request_id(1), NOW);
    verify(&v, &first).await.expect("the first attempt");

    let mut owner = sqlx::PgConnection::connect_with(&db.as_role("pool_migration"))
        .await
        .unwrap();
    pool_test_support::exec(
        &mut owner,
        "REVOKE INSERT ON pool.audit_event FROM pool_api".to_owned(),
    )
    .await
    .expect("the migration role owns the grant");

    let elsewhere = sign(
        &alice,
        GET,
        "/member/v0/heartbeats",
        b"",
        &request_id(1),
        NOW,
    );
    let denied = verify(&v, &elsewhere).await.expect_err("a reuse");
    assert_eq!(
        denied.error_code, "TEMPORARILY_UNAVAILABLE",
        "an unrecordable reuse asks to be asked again"
    );
    assert!(denied.retryable);

    // And with the grant back, the same reuse is refused permanently and
    // leaves the row — so the case above is about the audit write and not
    // about reuses in general.
    pool_test_support::exec(
        &mut owner,
        "GRANT INSERT ON pool.audit_event TO pool_api".to_owned(),
    )
    .await
    .expect("restored");

    let denied = verify(&v, &elsewhere).await.expect_err("a reuse");
    assert_eq!(denied.error_code, "REQUEST_ID_REUSED");
    assert!(!denied.retryable);

    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pool.audit_event WHERE request_id = $1::uuid")
            .bind(request_id(1))
            .fetch_one(&api)
            .await
            .unwrap();
    assert_eq!(audited, 1, "one rejection, one row");
}

#[tokio::test]
async fn one_worker_cannot_spend_another_workers_request_ids() {
    // The replay memory is per credential, not global: two workers choosing
    // the same request ID are two attempts, not a replay. A global key would
    // let one member deny another service by guessing IDs.
    let Some((_db, api)) = migrated("verify_replay_scope").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let bob = enrol(&api, BOB, 0xb2).await;
    let v = verifier(&api);

    // Different signed bytes under the same request id, so a memory keyed on
    // the request id alone would see a reuse.
    let alices = sign(&alice, GET, PATH, b"", &request_id(1), NOW);
    let bobs = sign(&bob, GET, "/member/v0/heartbeats", b"", &request_id(1), NOW);

    verify(&v, &alices).await.expect("alice");
    verify(&v, &bobs)
        .await
        .expect("bob's identical request id is his own");

    // And each may still retry their own attempt. This is what needs the
    // *read* to be scoped too: the insert conflicts on the full key, so the
    // digest comparison is the only place a cross-credential lookup shows up,
    // and it would reject one of these two as a reuse of the other's bytes.
    verify(&v, &alices).await.expect("alice retries her own");
    verify(&v, &bobs).await.expect("bob retries his own");

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pool.request_replay WHERE request_id = $1::uuid")
            .bind(request_id(1))
            .fetch_one(&api)
            .await
            .unwrap();
    assert_eq!(rows, 2, "one memory each, not one between them");
}

#[tokio::test]
async fn a_timestamp_at_the_ends_of_its_range_is_refused_rather_than_overflowing() {
    // `X-Request-Timestamp` is the one header a caller can put an arbitrary
    // integer in. While it was parsed as `i64`, the freshness check subtracted
    // it from server time and took `abs()` — so `i64::MIN` overflowed, which
    // is a panic in a debug build and a wrapped value in a release one, and
    // `abs()` on `i64::MIN` panics outright. A member could have taken the
    // process down with one header.
    //
    // It is unsigned now, and the difference is taken in `i128`, which holds
    // every value the two types can produce. These are the inputs that used
    // to reach the arithmetic.
    let Some((_db, api)) = migrated("verify_timestamp_range").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    // Values the header can express but the type cannot hold. These are
    // refused when the header is read, before a signature is even considered,
    // so a header override is the only way to send one.
    for (n, header) in [
        (1_u8, "-9223372036854775808"),
        (2, "-1"),
        (3, "18446744073709551616"),
    ] {
        let request = sign(&alice, GET, PATH, b"", &request_id(n), NOW)
            .with(HEADER_REQUEST_TIMESTAMP, header);
        let denied = verify(&v, &request)
            .await
            .err()
            .unwrap_or_else(|| panic!("{header} must be refused"));
        assert_eq!(denied.error_code, "NOT_AUTHENTICATED", "{header}");
    }

    // Values the type does hold, at the ends of its range, and **signed** —
    // so verification reaches the freshness arithmetic instead of stopping at
    // the signature. Overriding only the header would leave the signed string
    // disagreeing with it, and this test would pass with the widening undone.
    //
    // 2^63 is the one that matters: cast back down it is `i64::MIN`, and
    // subtracting that from server time overflows whatever the header was
    // parsed as. That is why the arithmetic is widened rather than the parse
    // alone being fixed.
    for (n, timestamp) in [(4_u8, i64::MAX as u64), (5, 1_u64 << 63), (6, u64::MAX)] {
        let request = sign_at(&alice, GET, PATH, b"", &request_id(n), timestamp);
        let denied = verify(&v, &request)
            .await
            .err()
            .unwrap_or_else(|| panic!("{timestamp} must be refused"));
        // Stale, by a stated rule — not a panic, and not accepted as fresh.
        assert_eq!(denied.error_code, "STALE_TIMESTAMP", "{timestamp}");
    }
}

#[tokio::test]
async fn a_refusal_echoes_the_attempt_it_refused_once_the_header_is_readable() {
    // §3.2: "an error echoes `request_id` when the request carried the
    // standard signed header. The public protocol-discovery endpoint and
    // failures that occur before either ID can be decoded may omit both."
    //
    // It is the caller's own value, so echoing it discloses nothing — and it
    // is how a member matches a failure to the attempt that caused it when
    // several are in flight.
    let Some((_db, api)) = migrated("verify_echo").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    // A failure after the headers parse: the id comes back.
    let stale = verify(
        &v,
        &sign(&alice, GET, PATH, b"", &request_id(1), NOW - 4000),
    )
    .await
    .expect_err("stale");
    assert_eq!(stale.request_id.as_deref(), Some(request_id(1).as_str()));

    let wrong_signature =
        sign(&alice, GET, PATH, b"", &request_id(2), NOW).with(HEADER_SIGNATURE, &"A".repeat(86));
    let denied = verify(&v, &wrong_signature)
        .await
        .expect_err("bad signature");
    assert_eq!(denied.request_id.as_deref(), Some(request_id(2).as_str()));

    // A failure *before* they parse has no id to echo, and must not invent
    // one: the header the pool could not read is not a value it may quote.
    let unreadable =
        sign(&alice, GET, PATH, b"", &request_id(3), NOW).with(HEADER_REQUEST_ID, "not-a-uuid");
    let malformed = verify(&v, &unreadable).await.expect_err("malformed");
    assert_eq!(malformed.request_id, None);
    assert_eq!(malformed.error_code, "NOT_AUTHENTICATED");
}

#[tokio::test]
async fn another_protocol_version_is_refused_with_no_state_change() {
    // §4: "no common version produces HTTP 426 and INCOMPATIBLE_PROTOCOL,
    // with no state change" — so it must be refused before the attempt is
    // recorded, or the caller's request ID would be spent on a request the
    // pool never considered.
    let Some((_db, api)) = migrated("verify_version").await else {
        return;
    };
    let alice = enrol(&api, ALICE, 0xa1).await;
    let v = verifier(&api);

    let wrong =
        sign(&alice, GET, PATH, b"", &request_id(1), NOW).with(HEADER_PROTOCOL_VERSION, "0.2.0");
    let denied = verify(&v, &wrong).await.expect_err("another version");
    assert_eq!(denied.error_code, "INCOMPATIBLE_PROTOCOL");
    assert_eq!(denied.status, StatusCode::UPGRADE_REQUIRED);

    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.request_replay")
        .fetch_one(&api)
        .await
        .unwrap();
    assert_eq!(recorded, 0, "no state change");
}
