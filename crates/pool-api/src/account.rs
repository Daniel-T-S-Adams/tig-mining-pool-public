//! The account system: what a member does before they have a worker.
//!
//! `architecture.md` §9 already places this in the Pool API ("Pool API
//! account system"), and ADR 0011 says what an account *is*: "a member signs
//! in by connecting a wallet, and that wallet is where their money goes."
//! There is no password, no session secret, and no recovery — the address is
//! the identity.
//!
//! This is a **second surface on the same process**, deliberately outside
//! `/member/v0`. `member_protocol.md` §3.1 puts the thing that creates
//! enrollment tickets "outside this protocol", and §5's route table has no
//! entry for it: a member's browser asks for a ticket, a member's *machine*
//! speaks the member protocol, and the two have different callers, different
//! authentication, and different lifecycles. Mixing them would put a
//! browser-facing route into the contract an agent is written against.
//!
//! The slice-2 plan defers the interface and not the proof — "slice 2
//! implements the proof rather than an interim stand-in", and "what waits for
//! step 7 is the user interface that calls it". This is what it will call.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use axum::{Json, Router};
use pool_identity::wallet::{LoginExpectation, LoginPurpose, recover_login_address};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::ApiError;
use crate::protocol::ServerTime;
use crate::ticket_key::TicketKey;

/// §3.1: a ticket "expires after 15 minutes".
const TICKET_LIFETIME: &str = "15 minutes";

/// §3.1: "at least 256 bits of entropy". Thirty-two bytes, rendered as
/// unpadded base64url so a member can paste it.
const BEARER_BYTES: usize = 32;

/// How far ahead of server time a login signature may claim to be valid.
///
/// `accounting.md` §12.2 requires an expiry but names no bound, and the
/// signature carries whatever the member signed — so without this a member
/// could sign one good for years. Two things go wrong then: a stolen
/// signature is worth something for that whole time, and the pool must
/// remember its nonce for just as long, because §12.2 keeps a spent nonce
/// "until the signature that spent it has expired".
///
/// Fifteen minutes, which is what a signature actually needs: the round trip
/// between a wallet signing and a browser posting.
const LOGIN_MAX_LIFETIME_SECONDS: u64 = 15 * 60;

/// What the account routes need. Crate-private, because it holds the ticket
/// key and `security.md` §4.1 keeps that here.
#[derive(Clone)]
pub(crate) struct AccountState {
    pub(crate) db: PgPool,
    pub(crate) key: std::sync::Arc<TicketKey>,
    pub(crate) network: String,
    pub(crate) pool_domain: String,
    pub(crate) login_chain_id: u64,
    pub(crate) now: fn() -> time::OffsetDateTime,
}

/// A member asking for an enrollment ticket, proving who they are by signing.
///
/// The address is **not** a field. §12.2: it "is the member identity, so it
/// is not carried separately in the signed payload" — it is recovered from
/// the signature, so there is nothing here for a caller to claim.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollmentTicketRequest {
    /// The one-time value inside the signed text.
    pub(crate) nonce: String,
    /// The expiry inside the signed text, which the pool compares to its own
    /// clock.
    pub(crate) expires_at: String,
    /// `r || s || v` as hex, with or without `0x`.
    pub(crate) signature: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct EnrollmentTicketResponse {
    /// The bearer value, returned **once**. The pool keeps only its HMAC
    /// (`security.md` §4.1), so it cannot show this again and does not try.
    pub(crate) ticket: String,
    /// When the ticket stops being redeemable.
    pub(crate) expires_at: String,
    /// The pool-issued, opaque account id (`member_protocol.md` §2). Returned
    /// so an interface can show a member which account they are looking at;
    /// nothing authenticates with it.
    pub(crate) member_id: String,
}

pub(crate) fn routes(state: AccountState, max_control_body_bytes: u64) -> Router {
    crate::service::bounded(
        Router::new().route("/account/v0/enrollment-tickets", post(issue)),
        max_control_body_bytes,
    )
    .with_state(state)
}

/// A refusal that says no more than it must.
///
/// One code for every way a login fails. A caller who learns that their
/// signature was fine but their nonce was spent learns which nonces are
/// spent, and a caller who learns an address is unknown learns which
/// addresses are members.
fn not_authenticated() -> ApiError {
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        error_code: "NOT_AUTHENTICATED",
        message: "that signature does not authorise a ticket".to_owned(),
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

async fn issue(
    State(state): State<AccountState>,
    body: Result<Json<EnrollmentTicketRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(server_time) = ServerTime::at((state.now)()) else {
        return crate::service::no_server_time();
    };

    // §14: a body this version does not define is incompatible input, not a
    // value to coerce. `deny_unknown_fields` does the coercing half; this is
    // the answer.
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

    match issue_ticket(&state, &request).await {
        Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
        Err(e) => e.into_response_at(&server_time),
    }
}

async fn issue_ticket(
    state: &AccountState,
    request: &EnrollmentTicketRequest,
) -> Result<EnrollmentTicketResponse, ApiError> {
    // The pool supplies the domain, the chain and the purpose. A caller that
    // could choose any of them could obtain a signature for one thing and
    // spend it on another.
    let expectation = LoginExpectation {
        pool_domain: &state.pool_domain,
        chain_id: state.login_chain_id,
        purpose: LoginPurpose::WorkerEnrollment,
        nonce: &request.nonce,
        expires_at_rfc3339: &request.expires_at,
    };

    let now = (state.now)();
    let now_unix = u64::try_from(now.unix_timestamp()).unwrap_or(0);
    let proved = recover_login_address(&expectation, &request.signature, now_unix)
        .map_err(|_| not_authenticated())?;

    // The signature is inside its own expiry — `recover_login_address`
    // checked that — but the expiry is the member's to choose, so the pool
    // bounds how far ahead it may reach. Refused as an authentication
    // failure, like every other way a login does not authorise a ticket.
    if proved.expires_at_unix > now_unix.saturating_add(LOGIN_MAX_LIFETIME_SECONDS) {
        tracing::info!(
            event = "account.login_rejected",
            reason = "EXPIRY_TOO_FAR_AHEAD",
            "a login signature claimed a validity this pool does not grant"
        );
        return Err(not_authenticated());
    }
    let address = proved.address;

    // One transaction from here. `security.md` §4.1 puts "ticket lookup,
    // expiry check, one-time consumption, and worker/credential creation" in
    // one for redemption; issuance has the same shape and the same reason —
    // a nonce spent without a ticket to show for it costs a member their
    // signature, and a ticket issued without spending the nonce lets one
    // signature buy as many as the caller likes.
    let mut tx = state.db.begin().await.map_err(|e| {
        tracing::error!(event = "account.begin_failed", error = %e);
        unavailable()
    })?;

    // Spending the nonce first is what makes the rest at-most-once. The
    // primary key is `(network, wallet_address, nonce)`, so a second attempt
    // with the same signature conflicts here rather than further down.
    //
    // `signature_expires_at` is **the expiry the signature carries**, not the
    // ticket's lifetime. That column decides when the row may be pruned, and
    // §12.2 keeps a spent nonce "until the signature that spent it has
    // expired" — writing anything shorter would let the nonce be forgotten
    // while the signature it spent was still acceptable, which is the replay
    // the nonce exists to prevent. Writing the ticket's fifteen minutes here
    // was this route's first version, and the bound above is what keeps the
    // honest value from being unboundedly far away.
    let spent = sqlx::query(
        "INSERT INTO pool.wallet_login_nonce
             (network, wallet_address, nonce, purpose, signature_expires_at)
         VALUES ($1, $2, $3, 'WORKER_ENROLLMENT', to_timestamp($4::bigint))
         ON CONFLICT DO NOTHING",
    )
    .bind(&state.network)
    .bind(address.as_str())
    .bind(&request.nonce)
    .bind(i64::try_from(proved.expires_at_unix).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "account.nonce_write_failed", error = %e);
        unavailable()
    })?;

    if spent.rows_affected() != 1 {
        // The same signature, a second time. Refused as an authentication
        // failure rather than as a conflict, so a caller cannot use this
        // route to ask which nonces an address has spent.
        tracing::info!(
            event = "account.login_rejected",
            reason = "NONCE_ALREADY_SPENT",
            "a login signature was presented twice"
        );
        return Err(not_authenticated());
    }

    // ADR 0011: the wallet *is* the account, so a first login is a signup.
    // There is no separate registration step to get wrong, and no unset
    // payout destination to handle.
    //
    // `DO NOTHING` and then a read, not `DO UPDATE`. `pool_api` holds INSERT
    // and SELECT on this table and no UPDATE at all — `migrations/0018`
    // states that as "the API may create an account and may not change one",
    // and the wallet is the identity, so there is nothing on the row to
    // change anyway. An upsert would have been a design error the grant
    // caught: it asks for a privilege the role is deliberately denied.
    let existing: Option<String> = sqlx::query_scalar(
        "INSERT INTO pool.member (network, member_id, wallet_address)
         VALUES ($1, gen_random_uuid(), $2)
         ON CONFLICT (network, wallet_address) DO NOTHING
         RETURNING member_id::text",
    )
    .bind(&state.network)
    .bind(address.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "account.member_write_failed", error = %e);
        unavailable()
    })?;

    let member_id = match existing {
        Some(created) => created,
        // The account was already there, which is every login after the
        // first.
        None => sqlx::query_scalar(
            "SELECT member_id::text FROM pool.member
              WHERE network = $1 AND wallet_address = $2",
        )
        .bind(&state.network)
        .bind(address.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(event = "account.member_read_failed", error = %e);
            unavailable()
        })?,
    };

    let bearer = mint_bearer()?;
    let (expires_at,): (String,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO pool.enrollment_ticket
             (network, ticket_hmac, member_id, purpose, expires_at)
         VALUES ($1, $2, $3::uuid, 'WORKER_ENROLLMENT',
                 now() + interval '{TICKET_LIFETIME}')
         RETURNING to_char(expires_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
    )))
    .bind(&state.network)
    .bind(state.key.hmac(bearer.as_bytes()).as_slice())
    .bind(&member_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(event = "account.ticket_write_failed", error = %e);
        unavailable()
    })?;

    tx.commit().await.map_err(|e| {
        tracing::error!(event = "account.commit_failed", error = %e);
        unavailable()
    })?;

    // The address, not the bearer and not the hash. §4.1 keeps the bearer out
    // of everything the pool retains, and a log is something the pool
    // retains.
    tracing::info!(
        event = "account.enrollment_ticket_issued",
        member_id = %member_id,
        wallet_address = %address.as_str(),
        "a member proved their wallet and holds a ticket"
    );

    Ok(EnrollmentTicketResponse {
        ticket: bearer,
        expires_at,
        member_id,
    })
}

/// §3.1's "cryptographically random, single-use bearer value with at least
/// 256 bits of entropy".
fn mint_bearer() -> Result<String, ApiError> {
    use base64::Engine as _;

    let mut bytes = [0u8; BEARER_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| {
        // The pool cannot mint a ticket it cannot make unguessable, and a
        // weaker source is not a fallback — it is the same failure with the
        // evidence removed.
        tracing::error!(event = "account.entropy_unavailable", error = %e);
        unavailable()
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
pub(crate) mod tests {
    //! In this crate, not in `tests/`, for the reason
    //! `tig_gateway::drive`'s tests give for the same choice: an
    //! `AccountState` holds a `TicketKey`, and a `TicketKey` exists only
    //! through `ticket_key::load`, which is crate-private on purpose. An
    //! external test could not build one — which is the boundary working.
    //!
    //! Requires `POOL_TEST_SUPERUSER_URL`; without it these skip, and
    //! `POOL_REQUIRE_DB_TESTS=1` turns a skip into a failure in CI.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::PermissionsExt as _;

    use k256::ecdsa::{RecoveryId, Signature, SigningKey};
    use pool_identity::wallet::login_signing_string;
    use pool_test_support::{MIGRATOR, TempDb};
    use sha3::{Digest as _, Keccak256};

    use super::*;

    const POOL_DOMAIN: &str = "test.bench-pool.invalid";
    const CHAIN: u64 = 84_532;
    pub(crate) const NONCE: &str = "3f1a9c0e5b2d47a8bc6f91e0d3247a5b";
    /// Five minutes after `NOW`.
    const EXPIRES: &str = "2026-03-20T09:51:40Z";
    const NOW: i64 = 1_774_000_000;

    /// TEST-ONLY, and the most published secp256k1 key there is: the scalar
    /// 1. Its address is in every weak-key list.
    pub(crate) const ALICE_KEY: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";
    const ALICE: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
    pub(crate) const BOB_KEY: &str =
        "0000000000000000000000000000000000000000000000000000000000000002";
    const BOB: &str = "0x2b5ad5c4795c026514f8317c7a215e218dccd6cf";

    pub(crate) fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(NOW).expect("a valid instant")
    }

    fn signing_key(hex: &str) -> SigningKey {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// A synthetic ticket key. Never a real secret.
    fn ticket_key() -> std::sync::Arc<TicketKey> {
        let dir = std::env::temp_dir().join(format!("pool-api-account-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, [0x5a; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::sync::Arc::new(crate::ticket_key::load(&path).expect("a usable key"))
    }

    pub(crate) async fn migrated(name: &str) -> Option<(TempDb, AccountState)> {
        let db = TempDb::create(name).await?;
        let migration_pool = sqlx::PgPool::connect_with(db.as_role("pool_migration"))
            .await
            .unwrap();
        MIGRATOR.run(&migration_pool).await.unwrap();
        let api = sqlx::PgPool::connect_with(db.as_role("pool_api"))
            .await
            .unwrap();
        Some((
            db,
            AccountState {
                db: api,
                key: ticket_key(),
                network: "testnet".to_owned(),
                pool_domain: POOL_DOMAIN.to_owned(),
                login_chain_id: CHAIN,
                now,
            },
        ))
    }

    /// `personal_sign` as a wallet does it.
    fn personal_sign(key: &SigningKey, message: &str) -> String {
        let mut hasher = Keccak256::new();
        hasher.update(b"\x19Ethereum Signed Message:\n");
        hasher.update(message.len().to_string().as_bytes());
        hasher.update(message.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let (signature, recovery): (Signature, RecoveryId) =
            key.sign_prehash_recoverable(&digest).unwrap();
        let mut out = String::from("0x");
        for byte in signature.to_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
        out.push_str(&format!("{:02x}", recovery.to_byte() + 27));
        out
    }

    fn request_from(key_hex: &str, nonce: &str) -> EnrollmentTicketRequest {
        request_for(
            key_hex,
            POOL_DOMAIN,
            CHAIN,
            LoginPurpose::WorkerEnrollment,
            nonce,
        )
    }

    fn request_for(
        key_hex: &str,
        domain: &str,
        chain: u64,
        purpose: LoginPurpose,
        nonce: &str,
    ) -> EnrollmentTicketRequest {
        let message = login_signing_string(domain, chain, purpose, nonce, EXPIRES);
        EnrollmentTicketRequest {
            nonce: nonce.to_owned(),
            expires_at: EXPIRES.to_owned(),
            signature: personal_sign(&signing_key(key_hex), &message),
        }
    }

    /// One issued ticket, for the tests that redeem rather than issue.
    pub(crate) async fn issue_one_ticket(
        state: &AccountState,
        key_hex: &str,
        nonce: &str,
    ) -> EnrollmentTicketResponse {
        issue_ticket(state, &request_from(key_hex, nonce))
            .await
            .expect("a valid signature buys a ticket")
    }

    #[tokio::test]
    async fn a_first_login_creates_the_account_and_hands_back_one_ticket() {
        // ADR 0011: "a member signs in by connecting a wallet". There is no
        // registration step, so the first proof of an address is the signup.
        let Some((_db, state)) = migrated("account_first_login").await else {
            return;
        };

        let issued = issue_ticket(&state, &request_from(ALICE_KEY, NONCE))
            .await
            .expect("a valid signature buys a ticket");

        // The account is the address that signed, and nothing else.
        let address: String =
            sqlx::query_scalar("SELECT wallet_address FROM pool.member WHERE member_id = $1::uuid")
                .bind(&issued.member_id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(address, ALICE);

        // §3.1: "at least 256 bits of entropy", and the pool stores only the
        // keyed hash — never the bearer.
        assert_eq!(issued.ticket.len(), 43, "32 bytes as unpadded base64url");
        let stored: Vec<u8> = sqlx::query_scalar(
            "SELECT ticket_hmac FROM pool.enrollment_ticket WHERE member_id = $1::uuid",
        )
        .bind(&issued.member_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(stored, state.key.hmac(issued.ticket.as_bytes()).to_vec());
        assert_ne!(
            stored,
            issued.ticket.as_bytes().to_vec(),
            "the bearer value is not what is stored"
        );
    }

    #[tokio::test]
    async fn two_logins_from_one_wallet_are_one_account() {
        let Some((_db, state)) = migrated("account_same_wallet").await else {
            return;
        };

        let first = issue_ticket(&state, &request_from(ALICE_KEY, NONCE))
            .await
            .expect("first");
        let second = issue_ticket(
            &state,
            &request_from(ALICE_KEY, "aaaa9c0e5b2d47a8bc6f91e0d3247a5b"),
        )
        .await
        .expect("second, with a fresh nonce");

        assert_eq!(first.member_id, second.member_id, "one wallet, one account");
        assert_ne!(first.ticket, second.ticket, "and two different tickets");

        // Two wallets are two accounts.
        let bob = issue_ticket(&state, &request_from(BOB_KEY, NONCE))
            .await
            .expect("bob may use alice's nonce value; it is his own");
        assert_ne!(bob.member_id, first.member_id);
        let address: String =
            sqlx::query_scalar("SELECT wallet_address FROM pool.member WHERE member_id = $1::uuid")
                .bind(&bob.member_id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(address, BOB);
    }

    #[tokio::test]
    async fn the_same_signature_buys_exactly_one_ticket() {
        // §12.2's one-time nonce, and the reason the nonce is spent before
        // anything else: a signature that could be replayed is a signature
        // that buys as many tickets as the holder cares to ask for.
        let Some((_db, state)) = migrated("account_replay").await else {
            return;
        };
        let request = request_from(ALICE_KEY, NONCE);

        issue_ticket(&state, &request).await.expect("the first");
        let replayed = issue_ticket(&state, &request)
            .await
            .expect_err("the second must be refused");
        assert_eq!(replayed.error_code, "NOT_AUTHENTICATED");

        let tickets: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.enrollment_ticket")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(tickets, 1, "one signature, one ticket");
    }

    #[tokio::test]
    async fn a_signature_for_another_pool_chain_or_purpose_buys_nothing() {
        // `accounting.md` §12.2's exact-domain validation. Each of these is a
        // real signature by a real key — over text this pool did not write.
        let Some((_db, state)) = migrated("account_wrong_terms").await else {
            return;
        };

        let wrong = [
            request_for(
                ALICE_KEY,
                "other-pool.invalid",
                CHAIN,
                LoginPurpose::WorkerEnrollment,
                NONCE,
            ),
            // A domain this one merely ends with: suffix matching would take it.
            request_for(
                ALICE_KEY,
                &format!("attacker.invalid.{POOL_DOMAIN}"),
                CHAIN,
                LoginPurpose::WorkerEnrollment,
                NONCE,
            ),
            request_for(
                ALICE_KEY,
                POOL_DOMAIN,
                8453,
                LoginPurpose::WorkerEnrollment,
                NONCE,
            ),
            request_for(
                ALICE_KEY,
                POOL_DOMAIN,
                CHAIN,
                LoginPurpose::WorkerRecovery,
                NONCE,
            ),
        ];

        for (i, request) in wrong.iter().enumerate() {
            // Recovery succeeds — it yields a *different* address, which is
            // no member — so the ticket is not refused by an error in the
            // signature but by there being no such account to issue one to.
            // Either way nothing is issued to Alice.
            let _ = issue_ticket(&state, request).await;
            let alices: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pool.enrollment_ticket t
                   JOIN pool.member m ON m.member_id = t.member_id
                  WHERE m.wallet_address = $1",
            )
            .bind(ALICE)
            .fetch_one(&state.db)
            .await
            .unwrap();
            assert_eq!(alices, 0, "case {i} issued a ticket to Alice");
        }
    }

    #[tokio::test]
    async fn an_expired_signature_buys_nothing() {
        let Some((_db, mut state)) = migrated("account_expired").await else {
            return;
        };
        // A clock past the expiry the member signed.
        state.now =
            || time::OffsetDateTime::from_unix_timestamp(1_774_000_301).expect("a valid instant");

        let refused = issue_ticket(&state, &request_from(ALICE_KEY, NONCE))
            .await
            .expect_err("an expired signature must be refused");
        assert_eq!(refused.error_code, "NOT_AUTHENTICATED");

        // And nothing was spent on its behalf: the nonce is still available
        // for the signature the member will make next.
        let spent: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.wallet_login_nonce")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(spent, 0);
    }

    #[tokio::test]
    async fn the_nonce_is_remembered_until_the_signature_it_spent_expires() {
        // `accounting.md` §12.2: a spent nonce is kept "until the signature
        // that spent it has expired". The first version of this route wrote
        // the *ticket's* fifteen minutes into that column instead of the
        // expiry the signature carries — so a signature good for longer would
        // have had its nonce pruned while it was still acceptable, and could
        // then be replayed.
        let Some((_db, mut state)) = migrated("account_nonce_retention").await else {
            return;
        };

        // Real time here, not the pinned clock the other tests use. The
        // database's `now()` is real, and `migrations/0026` compares this
        // column against it — so a signature dated 2026-03 would be prunable
        // whatever the code wrote, and the assertion below would pass for the
        // wrong reason.
        state.now = time::OffsetDateTime::now_utc;
        let five_minutes_on = time::OffsetDateTime::now_utc() + time::Duration::minutes(5);
        let expires_at = five_minutes_on
            .replace_nanosecond(0)
            .expect("a valid nanosecond")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("a representable instant");

        let message = login_signing_string(
            POOL_DOMAIN,
            CHAIN,
            LoginPurpose::WorkerEnrollment,
            NONCE,
            &expires_at,
        );
        let request = EnrollmentTicketRequest {
            nonce: NONCE.to_owned(),
            expires_at: expires_at.clone(),
            signature: personal_sign(&signing_key(ALICE_KEY), &message),
        };
        issue_ticket(&state, &request).await.expect("a ticket");

        // The stored instant is the one inside the signed text, to the
        // second, and not `now() + 15 minutes`.
        let stored: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM signature_expires_at)::bigint
               FROM pool.wallet_login_nonce WHERE wallet_address = $1",
        )
        .bind(ALICE)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(
            stored,
            five_minutes_on.unix_timestamp(),
            "the signed expiry, not the ticket's lifetime"
        );

        // And `migrations/0026` therefore refuses to prune it: the signature
        // is still inside its own window. This is the assertion the first
        // version of this route would have failed — it wrote fifteen minutes
        // where the signature said five, and for a signature saying an hour
        // the row would have been prunable while the signature still worked.
        let early = sqlx::query("DELETE FROM pool.wallet_login_nonce")
            .execute(&state.db)
            .await;
        assert!(early.is_err(), "a live signature's nonce must stay");
    }

    #[tokio::test]
    async fn a_signature_good_for_longer_than_the_pool_grants_buys_nothing() {
        // The expiry is the member's to choose, so without a bound one could
        // sign a login good for years — a stolen signature worth something
        // for all of it, and a nonce the pool must remember just as long.
        let Some((_db, state)) = migrated("account_long_expiry").await else {
            return;
        };

        // Fifteen minutes exactly is granted; a second more is not.
        let at_the_limit = "2026-03-20T10:01:40Z";
        let past_it = "2026-03-20T10:01:41Z";

        let mut request = request_from(ALICE_KEY, NONCE);
        let message = login_signing_string(
            POOL_DOMAIN,
            CHAIN,
            LoginPurpose::WorkerEnrollment,
            NONCE,
            past_it,
        );
        request.expires_at = past_it.to_owned();
        request.signature = personal_sign(&signing_key(ALICE_KEY), &message);

        let refused = issue_ticket(&state, &request)
            .await
            .expect_err("a signature reaching too far ahead must be refused");
        assert_eq!(refused.error_code, "NOT_AUTHENTICATED");

        // Nothing was spent on its behalf.
        let spent: i64 = sqlx::query_scalar("SELECT count(*) FROM pool.wallet_login_nonce")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(spent, 0);

        // The boundary itself is granted, so the rejection is a bound rather
        // than a refusal of anything beyond the default.
        let message = login_signing_string(
            POOL_DOMAIN,
            CHAIN,
            LoginPurpose::WorkerEnrollment,
            NONCE,
            at_the_limit,
        );
        let at_limit = EnrollmentTicketRequest {
            nonce: NONCE.to_owned(),
            expires_at: at_the_limit.to_owned(),
            signature: personal_sign(&signing_key(ALICE_KEY), &message),
        };
        issue_ticket(&state, &at_limit)
            .await
            .expect("fifteen minutes exactly is granted");
    }

    #[tokio::test]
    async fn a_refused_login_leaves_no_account_and_no_ticket() {
        // The transaction. A nonce spent without a ticket costs a member
        // their signature; an account created for a signature that bought
        // nothing is a row nobody asked for.
        let Some((_db, state)) = migrated("account_atomic").await else {
            return;
        };

        let mut garbage = request_from(ALICE_KEY, NONCE);
        garbage.signature = format!("0x{}", "11".repeat(65));
        let _ = issue_ticket(&state, &garbage).await;

        for table in [
            "pool.member",
            "pool.enrollment_ticket",
            "pool.wallet_login_nonce",
        ] {
            let rows: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&state.db)
                    .await
                    .unwrap();
            assert_eq!(rows, 0, "{table} gained a row from a refused login");
        }
    }

    #[tokio::test]
    async fn a_login_that_fails_partway_spends_nothing() {
        // The transaction, tested by breaking its last write. A nonce spent
        // without a ticket to show for it costs a member their signature and
        // they cannot make the same one again; an account created for a
        // signature that bought nothing is a row nobody asked for.
        let Some((db, state)) = migrated("account_partial").await else {
            return;
        };

        let mut owner = {
            use sqlx::Connection as _;
            sqlx::PgConnection::connect_with(&db.as_role("pool_migration"))
                .await
                .unwrap()
        };
        pool_test_support::exec(
            &mut owner,
            "REVOKE INSERT ON pool.enrollment_ticket FROM pool_api".to_owned(),
        )
        .await
        .expect("the migration role owns the grant");

        let refused = issue_ticket(&state, &request_from(ALICE_KEY, NONCE))
            .await
            .expect_err("the ticket write cannot run");
        assert_eq!(refused.error_code, "TEMPORARILY_UNAVAILABLE");
        assert!(refused.retryable, "the member may sign again");

        // And the two earlier writes went with it.
        for table in ["pool.member", "pool.wallet_login_nonce"] {
            let rows: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&state.db)
                    .await
                    .unwrap();
            assert_eq!(rows, 0, "{table} kept a row from a login that failed");
        }

        // With the grant back, the same nonce still works — which is what
        // "spends nothing" has to mean for the member.
        pool_test_support::exec(
            &mut owner,
            "GRANT INSERT ON pool.enrollment_ticket TO pool_api".to_owned(),
        )
        .await
        .expect("restored");
        issue_ticket(&state, &request_from(ALICE_KEY, NONCE))
            .await
            .expect("the signature was never spent");
    }

    #[tokio::test]
    async fn a_ticket_lasts_fifteen_minutes_and_no_longer() {
        // §3.1. The row enforces the ceiling; this checks the value the API
        // actually writes, which is the one a member sees.
        let Some((_db, state)) = migrated("account_lifetime").await else {
            return;
        };
        let issued = issue_ticket(&state, &request_from(ALICE_KEY, NONCE))
            .await
            .expect("a ticket");

        let seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (expires_at - created_at))::bigint
               FROM pool.enrollment_ticket WHERE member_id = $1::uuid",
        )
        .bind(&issued.member_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(seconds, 900);
        assert!(
            issued.expires_at.ends_with('Z') && issued.expires_at.len() == 20,
            "the member is told when, in the shape the protocol uses: {}",
            issued.expires_at
        );
    }

    /// The real router: the account group merged through `app_with`, which is
    /// how `run` builds it.
    fn router(state: AccountState, limit: u64) -> Router {
        let api = pool_config::MemberApiConfig {
            listen: "127.0.0.1:0".to_owned(),
            ticket_hmac_key_file: std::path::PathBuf::from("/dev/null"),
            pool_domain: POOL_DOMAIN.to_owned(),
            login_chain_id: CHAIN,
            max_control_body_bytes: limit,
        };
        crate::service::app_with(&api, crate::service::AppState { now }, routes(state, limit))
    }

    async fn post_json(router: Router, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
        use tower::ServiceExt as _;

        let request = axum::http::Request::post(path)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .expect("a well-formed request");
        let response = router.oneshot(request).await.expect("the router answers");
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("a readable body")
            .to_bytes();
        let value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("a JSON body")
        };
        (status, value)
    }

    #[tokio::test]
    async fn the_route_answers_over_http_and_refuses_in_the_protocols_shape() {
        // Everything above calls `issue_ticket` directly. This is the layer
        // between that and a member's browser: routing, decoding, status, and
        // — because the group is merged before `assemble`'s shaping layer —
        // the one error shape §14 has a caller parse everything through.
        let Some((_db, state)) = migrated("account_http").await else {
            return;
        };
        let limit = pool_config::LARGEST_CONFORMING_CONTROL_BODY_BYTES;

        let request = request_from(ALICE_KEY, NONCE);
        let body = format!(
            r#"{{"nonce":"{}","expires_at":"{}","signature":"{}"}}"#,
            request.nonce, request.expires_at, request.signature
        );

        let (status, issued) = post_json(
            router(state.clone(), limit),
            "/account/v0/enrollment-tickets",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(issued["ticket"].as_str().map(str::len), Some(43));
        assert!(issued["member_id"].is_string());

        // The same signature again, over HTTP: refused, and in the shape a
        // caller parses.
        let (status, refused) = post_json(
            router(state.clone(), limit),
            "/account/v0/enrollment-tickets",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused["error_code"], "NOT_AUTHENTICATED");
        assert_eq!(
            refused["protocol_version"],
            crate::protocol::PROTOCOL_VERSION
        );
        assert!(refused["server_time"].is_string());

        // A body this route does not define is incompatible input (§14), not
        // something to coerce — and the refusal has the same shape.
        for bad in [
            "{}",
            r#"{"nonce":"abc"}"#,
            r#"{"nonce":"abc","expires_at":"x","signature":"y","extra":1}"#,
            "not json",
        ] {
            let (status, body) = post_json(
                router(state.clone(), limit),
                "/account/v0/enrollment-tickets",
                bad,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(body["error_code"], "MALFORMED_REQUEST", "{bad}");
        }

        // And the member routes are still there, on the same process.
        let (status, _) = post_json(router(state.clone(), limit), "/member/v0/nope", "{}").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The two failures the *framework* answers, which only the shaping
        // layer turns into the protocol's shape. These are what say the
        // account group is merged before that layer rather than after it.
        {
            use tower::ServiceExt as _;

            let wrong_method = axum::http::Request::get("/account/v0/enrollment-tickets")
                .body(axum::body::Body::empty())
                .expect("a well-formed request");
            let response = router(state.clone(), limit)
                .oneshot(wrong_method)
                .await
                .expect("the router answers");
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            let bytes = http_body_util::BodyExt::collect(response.into_body())
                .await
                .expect("a readable body")
                .to_bytes();
            let shaped: serde_json::Value =
                serde_json::from_slice(&bytes).expect("a protocol error body");
            assert_eq!(shaped["error_code"], "METHOD_NOT_ALLOWED");
        }

        // And the body limit reaches this group too: a member's browser is
        // no more entitled to send an unbounded body than their agent is.
        {
            use tower::ServiceExt as _;

            let small = 64;
            let oversized = axum::http::Request::post("/account/v0/enrollment-tickets")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .header(axum::http::header::CONTENT_LENGTH, "4096")
                .body(axum::body::Body::from(vec![b'x'; 4096]))
                .expect("a well-formed request");
            let response = router(state, small)
                .oneshot(oversized)
                .await
                .expect("the router answers");
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let bytes = http_body_util::BodyExt::collect(response.into_body())
                .await
                .expect("a readable body")
                .to_bytes();
            let shaped: serde_json::Value =
                serde_json::from_slice(&bytes).expect("a protocol error body");
            assert_eq!(shaped["error_code"], "BODY_TOO_LARGE");
        }
    }

    #[test]
    fn two_bearers_are_never_the_same() {
        // §3.1: "cryptographically random". A generator that repeated would
        // hand one member another's ticket.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let bearer = mint_bearer().expect("entropy");
            assert_eq!(bearer.len(), 43);
            assert!(seen.insert(bearer), "a bearer value repeated");
        }
    }
}
