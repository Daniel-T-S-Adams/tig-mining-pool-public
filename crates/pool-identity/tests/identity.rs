//! Issue #32 acceptance, identity half: real worker enrollment (ticket,
//! proof of key possession, idempotency), signed-request authentication with
//! replay protection, credential rotation with a bounded grace period,
//! revocation, account recovery, and pool-issued slot/assignment identities
//! that are NOT derived from benchmark ids.
//!
//! Contracts under test: `docs/member_protocol.md` §2, §3.1–§3.3, §5;
//! `docs/architecture.md` §6 (idempotency guards). Deterministic and
//! offline: the caller supplies every timestamp, and all key material is
//! TEST-ONLY fixed bytes generated in this file — never real credentials.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use pool_identity::keys::{
    encode_public_key, enrollment_signing_string, recovery_signing_string, request_signing_string,
    rotation_signing_string, sha256_hex, sign_b64url,
};
use pool_identity::{
    AssignmentFacts, EnrollRequest, EnrollResponse, IdentityErrorCode, IdentityService,
    RecoverWorkerRequest, RevocationReason, RotateCredentialRequest, SignedRequest, VerifiedWorker,
};

const NOW: u64 = 1_754_000_000;

fn fresh_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pool-identity-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// TEST-ONLY deterministic key bytes; never a real credential.
fn test_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// TEST-ONLY issuance facts with a fixed digest/network.
fn facts<'a>(
    worker_id: &'a str,
    slot_id: &'a str,
    benchmark_id: &'a str,
    assignment_digest: &'a str,
) -> AssignmentFacts<'a> {
    AssignmentFacts {
        worker_id,
        slot_id,
        benchmark_id,
        assignment_digest,
        network: "testnet",
    }
}

fn enroll_request(request_id: &str, ticket: &str, key: &SigningKey) -> EnrollRequest {
    let public_key = encode_public_key(&key.verifying_key());
    let proof = sign_b64url(
        key,
        &enrollment_signing_string(request_id, ticket, &public_key),
    );
    EnrollRequest {
        enrollment_request_id: request_id.to_owned(),
        enrollment_ticket: ticket.to_owned(),
        worker_name: "bench-box".to_owned(),
        ed25519_public_key: public_key,
        ed25519_key_proof: proof,
        supported_protocol_versions: vec!["0.1.0".to_owned()],
        supported_package_formats: vec!["proof-material-v1".to_owned()],
        member_agent_version: "spike-test-0".to_owned(),
    }
}

/// Enroll a fresh worker: member account, one-time ticket, signed proof.
fn enroll_worker(
    svc: &IdentityService,
    member_ref: &str,
    seed: u8,
    n: u64,
) -> (EnrollResponse, SigningKey) {
    let key = test_key(seed);
    let member_id = svc.create_member(member_ref, NOW).expect("member");
    let ticket = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    let response = svc
        .enroll(&enroll_request(&uuid(n), &ticket.secret, &key), NOW)
        .expect("enroll");
    (response, key)
}

#[allow(clippy::too_many_arguments)]
fn signed(
    key: &SigningKey,
    method: &str,
    path: &str,
    worker_id: &str,
    credential_id: &str,
    request_id: &str,
    timestamp: u64,
    body: &[u8],
) -> SignedRequest {
    let body_sha256 = sha256_hex(body);
    let message = request_signing_string(
        method,
        path,
        "0.1.0",
        worker_id,
        credential_id,
        request_id,
        timestamp,
        &body_sha256,
    );
    SignedRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        protocol_version: "0.1.0".to_owned(),
        worker_id: worker_id.to_owned(),
        credential_id: credential_id.to_owned(),
        request_id: request_id.to_owned(),
        request_timestamp: timestamp,
        body_sha256,
        signature: sign_b64url(key, &message),
    }
}

fn heartbeat(key: &SigningKey, e: &EnrollResponse, request_id: &str, ts: u64) -> SignedRequest {
    signed(
        key,
        "POST",
        "/member/v0/heartbeats",
        &e.worker_id,
        &e.credential_id,
        request_id,
        ts,
        b"{}",
    )
}

// ---------------------------------------------------------------------------
// Scope 1: enrollment issues durable identities, idempotently
// ---------------------------------------------------------------------------

#[test]
fn enrollment_issues_durable_pool_identities() {
    let root = fresh_root("enroll");
    let svc = IdentityService::open(&root).expect("open");
    let (e, key) = enroll_worker(&svc, "alice", 1, 1);

    assert_eq!(e.protocol_version, "0.1.0");
    assert_eq!(e.package_format, "proof-material-v1");
    assert_eq!(e.worker_status, "ACTIVE");
    for id in [&e.member_id, &e.worker_id, &e.credential_id] {
        assert_eq!(id.len(), 36, "pool IDs are UUIDs: {id}");
        assert_eq!(&id[14..15], "4");
    }
    // Distinct identities per §2 table (member != worker != credential).
    assert_ne!(e.member_id, e.worker_id);
    assert_ne!(e.worker_id, e.credential_id);

    // Durability: a fresh service over the same root authenticates the
    // enrolled credential.
    drop(svc);
    let svc = IdentityService::open(&root).expect("reopen");
    let verified = svc
        .verify_request(&heartbeat(&key, &e, &uuid(100), NOW), b"{}", NOW)
        .expect("verify after restart");
    assert_eq!(
        verified,
        VerifiedWorker {
            member_id: e.member_id.clone(),
            worker_id: e.worker_id.clone(),
            credential_id: e.credential_id.clone(),
        }
    );
}

#[test]
fn enrollment_is_idempotent_by_request_id_plus_hash() {
    let root = fresh_root("enroll-idem");
    let svc = IdentityService::open(&root).expect("open");
    let key = test_key(1);
    let member_id = svc.create_member("alice", NOW).expect("member");
    let ticket = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    let req = enroll_request(&uuid(1), &ticket.secret, &key);

    let first = svc.enroll(&req, NOW).expect("enroll");
    // Identical retry — even later, even though the ticket is consumed —
    // returns the identical recorded response (§3.1).
    let second = svc.enroll(&req, NOW + 60).expect("retry");
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );

    // Changing any field for that enrollment ID is a conflict.
    let mut changed = req.clone();
    changed.worker_name = "other-name".to_owned();
    let e = svc.enroll(&changed, NOW).expect_err("conflict");
    assert_eq!(e.code, IdentityErrorCode::IdempotencyConflict);
}

#[test]
fn enrollment_ticket_is_single_use_hashed_and_expiring() {
    let root = fresh_root("enroll-ticket");
    let svc = IdentityService::open(&root).expect("open");
    let key = test_key(1);
    let member_id = svc.create_member("alice", NOW).expect("member");
    let ticket = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    assert!(ticket.secret.len() >= 43, "≥256 bits of entropy (§3.1)");
    assert_eq!(ticket.expires_at, NOW + 900);

    // The plaintext ticket appears nowhere in the durable store (§3.1:
    // stored only as a keyed hash).
    fn walk(dir: &std::path::Path, needle: &str) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, needle);
            } else {
                let contents = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(
                    !contents.contains(needle),
                    "plaintext ticket found in {}",
                    path.display()
                );
            }
        }
    }
    walk(&root, &ticket.secret);

    // Schema bounds are enforced at the service boundary (never delegated
    // to a future HTTP layer): oversized fields are rejected with no state
    // change — the ticket survives.
    let mut oversized = enroll_request(&uuid(1), &ticket.secret, &key);
    oversized.worker_name = "x".repeat(129);
    let e = svc.enroll(&oversized, NOW).expect_err("oversized name");
    assert_eq!(e.code, IdentityErrorCode::UnknownResource);
    let mut too_many = enroll_request(&uuid(1), &ticket.secret, &key);
    too_many.supported_protocol_versions = (0..17).map(|i| format!("0.1.{i}")).collect();
    let e = svc.enroll(&too_many, NOW).expect_err("oversized array");
    assert_eq!(e.code, IdentityErrorCode::UnknownResource);

    // Version mismatch is INCOMPATIBLE_PROTOCOL with no state change (§4):
    // the ticket survives.
    let mut incompatible = enroll_request(&uuid(1), &ticket.secret, &key);
    incompatible.supported_protocol_versions = vec!["9.9.9".to_owned()];
    let e = svc.enroll(&incompatible, NOW).expect_err("incompatible");
    assert_eq!(e.code, IdentityErrorCode::IncompatibleProtocol);

    // A bad key proof consumes nothing either.
    let mut bad_proof = enroll_request(&uuid(1), &ticket.secret, &key);
    bad_proof.ed25519_key_proof = sign_b64url(&key, "wrong message")
        .chars()
        .collect::<String>();
    let e = svc.enroll(&bad_proof, NOW).expect_err("bad proof");
    assert_eq!(e.code, IdentityErrorCode::NotAuthenticated);

    // The ticket still enrolls exactly once...
    svc.enroll(&enroll_request(&uuid(1), &ticket.secret, &key), NOW)
        .expect("enroll");
    // ...and is then consumed for any other enrollment request.
    let e = svc
        .enroll(&enroll_request(&uuid(2), &ticket.secret, &test_key(2)), NOW)
        .expect_err("single use");
    assert_eq!(e.code, IdentityErrorCode::TicketRejected);

    // A fresh ticket expires after 15 minutes.
    let expired = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    let e = svc
        .enroll(
            &enroll_request(&uuid(3), &expired.secret, &test_key(3)),
            NOW + 901,
        )
        .expect_err("expired");
    assert_eq!(e.code, IdentityErrorCode::TicketRejected);
}

// ---------------------------------------------------------------------------
// Scope 2: signed requests — authentication, freshness, replay protection
// ---------------------------------------------------------------------------

#[test]
fn signed_requests_verify_freshness_and_replay_rules() {
    let root = fresh_root("signed");
    let svc = IdentityService::open(&root).expect("open");
    let (a, key_a) = enroll_worker(&svc, "alice", 1, 1);
    let (b, _key_b) = enroll_worker(&svc, "bob", 2, 2);

    // Valid request: member identity comes from the stored worker binding.
    let ok = svc
        .verify_request(&heartbeat(&key_a, &a, &uuid(100), NOW), b"{}", NOW)
        .expect("valid");
    assert_eq!(ok.member_id, a.member_id);

    // Tampered body: X-Body-SHA256 no longer matches.
    let e = svc
        .verify_request(&heartbeat(&key_a, &a, &uuid(101), NOW), b"{ }", NOW)
        .expect_err("tampered body");
    assert_eq!(e.code, IdentityErrorCode::NotAuthenticated);

    // Signature by the wrong key.
    let e = svc
        .verify_request(&heartbeat(&test_key(9), &a, &uuid(102), NOW), b"{}", NOW)
        .expect_err("wrong key");
    assert_eq!(e.code, IdentityErrorCode::NotAuthenticated);

    // Unknown credential.
    let mut unknown = a.clone();
    unknown.credential_id = uuid(999);
    let e = svc
        .verify_request(&heartbeat(&key_a, &unknown, &uuid(103), NOW), b"{}", NOW)
        .expect_err("unknown credential");
    assert_eq!(e.code, IdentityErrorCode::NotAuthenticated);

    // A's credential cannot claim B's worker id, even correctly signed
    // (§3.2: the credential authorizes only its exact worker).
    let cross = signed(
        &key_a,
        "POST",
        "/member/v0/heartbeats",
        &b.worker_id,
        &a.credential_id,
        &uuid(104),
        NOW,
        b"{}",
    );
    let e = svc
        .verify_request(&cross, b"{}", NOW)
        .expect_err("cross-worker header");
    assert_eq!(e.code, IdentityErrorCode::NotAuthenticated);

    // Freshness: exactly ±300 s is accepted, beyond is stale (§3.2).
    svc.verify_request(&heartbeat(&key_a, &a, &uuid(105), NOW - 300), b"{}", NOW)
        .expect("lower bound");
    svc.verify_request(&heartbeat(&key_a, &a, &uuid(106), NOW + 300), b"{}", NOW)
        .expect("upper bound");
    let e = svc
        .verify_request(&heartbeat(&key_a, &a, &uuid(107), NOW - 301), b"{}", NOW)
        .expect_err("stale");
    assert_eq!(e.code, IdentityErrorCode::StaleTimestamp);

    // Replay: an identical retry of the same HTTP attempt is accepted...
    let retry = heartbeat(&key_a, &a, &uuid(108), NOW);
    svc.verify_request(&retry, b"{}", NOW).expect("first");
    svc.verify_request(&retry, b"{}", NOW + 5)
        .expect("identical retry");
    // ...but the same request ID with different signed bytes is rejected
    // and audited (§3.2).
    let reused = signed(
        &key_a,
        "POST",
        "/member/v0/capacity-offers",
        &a.worker_id,
        &a.credential_id,
        &uuid(108),
        NOW,
        b"{}",
    );
    let e = svc
        .verify_request(&reused, b"{}", NOW)
        .expect_err("request id reuse");
    assert_eq!(e.code, IdentityErrorCode::ReplayRejected);
    let audit = svc.audit_entries().expect("audit");
    assert!(
        audit
            .iter()
            .any(|a| a["event"] == "request_id_reuse_rejected"),
        "reuse must be audited"
    );
}

// ---------------------------------------------------------------------------
// Scope 3: rotation, revocation, recovery
// ---------------------------------------------------------------------------

fn rotate_request(
    worker_id: &str,
    rotation_id: &str,
    new_key: &SigningKey,
) -> RotateCredentialRequest {
    let new_public_key = encode_public_key(&new_key.verifying_key());
    let proof = sign_b64url(
        new_key,
        &rotation_signing_string(worker_id, rotation_id, &new_public_key),
    );
    RotateCredentialRequest {
        protocol_version: "0.1.0".to_owned(),
        rotation_id: rotation_id.to_owned(),
        new_ed25519_public_key: new_public_key,
        new_key_proof: proof,
    }
}

#[test]
fn rotation_swaps_keys_with_bounded_grace_and_is_idempotent() {
    let root = fresh_root("rotate");
    let svc = IdentityService::open(&root).expect("open");
    let (e, old_key) = enroll_worker(&svc, "alice", 1, 1);
    let new_key = test_key(2);

    // The rotation request is itself a signed request by the current key.
    let caller = svc
        .verify_request(&heartbeat(&old_key, &e, &uuid(100), NOW), b"{}", NOW)
        .expect("caller");
    let req = rotate_request(&e.worker_id, &uuid(50), &new_key);
    let resp = svc
        .rotate(&caller, &e.worker_id, &req, NOW)
        .expect("rotate");
    assert_eq!(resp.worker_id, e.worker_id);
    assert_ne!(resp.new_credential_id, e.credential_id);

    // Repeating the same rotation ID and key returns the same result;
    // a different key for the same ID conflicts (§3.3).
    let retry = svc
        .rotate(&caller, &e.worker_id, &req, NOW + 30)
        .expect("retry");
    assert_eq!(
        serde_json::to_string(&resp).unwrap(),
        serde_json::to_string(&retry).unwrap()
    );
    let conflict = rotate_request(&e.worker_id, &uuid(50), &test_key(3));
    let err = svc
        .rotate(&caller, &e.worker_id, &conflict, NOW)
        .expect_err("conflict");
    assert_eq!(err.code, IdentityErrorCode::IdempotencyConflict);

    // A proof not made by the new key is rejected.
    let mut bad = rotate_request(&e.worker_id, &uuid(51), &test_key(4));
    bad.new_key_proof = rotate_request(&e.worker_id, &uuid(51), &test_key(5)).new_key_proof;
    let err = svc
        .rotate(&caller, &e.worker_id, &bad, NOW)
        .expect_err("bad proof");
    assert_eq!(err.code, IdentityErrorCode::NotAuthenticated);

    // The `{worker_id}` path component must equal the signed header (§3.2);
    // a mismatched path is rejected with the uniform non-disclosing code.
    let other_path = rotate_request(&uuid(999), &uuid(52), &test_key(4));
    let err = svc
        .rotate(&caller, &uuid(999), &other_path, NOW)
        .expect_err("path worker mismatch");
    assert_eq!(err.code, IdentityErrorCode::UnknownResource);

    // The new credential signs requests.
    let mut rotated = e.clone();
    rotated.credential_id = resp.new_credential_id.clone();
    svc.verify_request(&heartbeat(&new_key, &rotated, &uuid(101), NOW), b"{}", NOW)
        .expect("new credential");

    // The old credential stays valid through the ≤10-minute grace window,
    // then is revoked (§3.3).
    let in_grace = NOW + 599;
    svc.verify_request(
        &heartbeat(&old_key, &e, &uuid(102), in_grace),
        b"{}",
        in_grace,
    )
    .expect("old key within grace");
    let after_grace = NOW + 600;
    let err = svc
        .verify_request(
            &heartbeat(&old_key, &e, &uuid(103), after_grace),
            b"{}",
            after_grace,
        )
        .expect_err("old key after grace");
    assert_eq!(err.code, IdentityErrorCode::NotAuthenticated);
}

#[test]
fn revocation_takes_effect_on_the_next_request() {
    let root = fresh_root("revoke");
    let svc = IdentityService::open(&root).expect("open");
    let (e, key) = enroll_worker(&svc, "alice", 1, 1);

    svc.verify_request(&heartbeat(&key, &e, &uuid(100), NOW), b"{}", NOW)
        .expect("before revocation");

    // The architecture §6 guard: revocation names the worker binding, the
    // acting principal, and a command ID; a credential outside the named
    // worker's binding is rejected with the uniform non-disclosing code.
    let err = svc
        .revoke_credential(
            &uuid(999),
            &e.credential_id,
            "member:alice",
            &uuid(200),
            NOW,
        )
        .expect_err("wrong worker binding");
    assert_eq!(err.code, IdentityErrorCode::UnknownResource);
    svc.verify_request(&heartbeat(&key, &e, &uuid(103), NOW), b"{}", NOW)
        .expect("unrevoked by mismatched command");

    svc.revoke_credential(
        &e.worker_id,
        &e.credential_id,
        "member:alice",
        &uuid(201),
        NOW,
    )
    .expect("revoke");
    let err = svc
        .verify_request(&heartbeat(&key, &e, &uuid(101), NOW), b"{}", NOW)
        .expect_err("after credential revocation");
    assert_eq!(err.code, IdentityErrorCode::NotAuthenticated);

    // Whole-worker revocation blocks every credential, and blocks new
    // assignment issuance (§3.3: prevents new offers/uploads).
    let (e2, key2) = enroll_worker(&svc, "bob", 2, 2);
    svc.revoke_worker(
        &e2.worker_id,
        RevocationReason::Accidental,
        "member:bob",
        &uuid(202),
        NOW,
    )
    .expect("revoke worker");
    // Idempotent by command ID: an identical retry is a recorded no-op; a
    // different payload under the same command ID conflicts.
    svc.revoke_worker(
        &e2.worker_id,
        RevocationReason::Accidental,
        "member:bob",
        &uuid(202),
        NOW,
    )
    .expect("identical command retry");
    let err = svc
        .revoke_worker(
            &e2.worker_id,
            RevocationReason::Security,
            "member:bob",
            &uuid(202),
            NOW,
        )
        .expect_err("changed command payload");
    assert_eq!(err.code, IdentityErrorCode::IdempotencyConflict);
    let err = svc
        .verify_request(&heartbeat(&key2, &e2, &uuid(102), NOW), b"{}", NOW)
        .expect_err("worker revoked");
    assert_eq!(err.code, IdentityErrorCode::NotAuthenticated);
    let audit = svc.audit_entries().expect("audit");
    assert!(audit.iter().any(|a| a["event"] == "credential_revoked"));
    assert!(audit.iter().any(|a| a["event"] == "worker_revoked"));

    // The audit ledger is durable: a fresh service over the same root
    // still returns every recorded security event (the ledger file's
    // directory entry survives like any fsynced state document).
    drop(svc);
    let svc = IdentityService::open(&root).expect("reopen");
    let audit = svc.audit_entries().expect("audit after restart");
    assert!(audit.iter().any(|a| a["event"] == "credential_revoked"));
    assert!(audit.iter().any(|a| a["event"] == "worker_revoked"));
}

#[test]
fn security_revocation_blocks_recovery_until_operator_reinstates() {
    let root = fresh_root("security-revoke");
    let svc = IdentityService::open(&root).expect("open");
    let (e, key) = enroll_worker(&svc, "alice", 1, 1);

    // A ticket issued BEFORE the security action must not survive it.
    let stale_ticket = svc
        .create_recovery_ticket(&e.member_id, &e.worker_id, NOW)
        .expect("pre-revocation ticket");

    // Security revocation (§3.3): recorded with reason and actor.
    svc.revoke_worker(
        &e.worker_id,
        RevocationReason::Security,
        "operator:audit-7",
        &uuid(210),
        NOW,
    )
    .expect("security revoke");

    // The reason is MONOTONIC: a later member-initiated ACCIDENTAL
    // revocation cannot launder the pool's security action into a
    // recoverable one (§3.3). The attempt is refused and audited.
    let err = svc
        .revoke_worker(
            &e.worker_id,
            RevocationReason::Accidental,
            "member:alice",
            &uuid(213),
            NOW,
        )
        .expect_err("downgrade refused");
    assert_eq!(err.code, IdentityErrorCode::CommandRefused);

    // No new recovery ticket for a security-revoked worker...
    let err = svc
        .create_recovery_ticket(&e.member_id, &e.worker_id, NOW)
        .expect_err("ticket refused");
    assert_eq!(err.code, IdentityErrorCode::TicketRejected);

    // ...and the pre-revocation ticket cannot be consumed either.
    let new_key = test_key(2);
    let new_public_key = encode_public_key(&new_key.verifying_key());
    let req = RecoverWorkerRequest {
        recovery_request_id: uuid(60),
        worker_id: e.worker_id.clone(),
        recovery_ticket: stale_ticket.secret.clone(),
        new_ed25519_public_key: new_public_key.clone(),
        new_key_proof: sign_b64url(
            &new_key,
            &recovery_signing_string(&uuid(60), &stale_ticket.secret, &new_public_key),
        ),
    };
    let err = svc.recover_worker(&req, NOW).expect_err("recovery refused");
    assert_eq!(err.code, IdentityErrorCode::TicketRejected);

    // Only the explicit, audited pool decision clears a security
    // revocation (§3.3: "the pool decides explicitly").
    svc.reinstate_worker(&e.worker_id, "operator:audit-7", &uuid(211), NOW)
        .expect("reinstate");
    svc.verify_request(&heartbeat(&key, &e, &uuid(101), NOW), b"{}", NOW)
        .expect("active again after reinstatement");

    // An ACCIDENTAL revocation, by contrast, is recoverable by the member.
    svc.revoke_worker(
        &e.worker_id,
        RevocationReason::Accidental,
        "member:alice",
        &uuid(212),
        NOW,
    )
    .expect("accidental revoke");
    let ticket = svc
        .create_recovery_ticket(&e.member_id, &e.worker_id, NOW)
        .expect("accidental ticket allowed");
    let req = RecoverWorkerRequest {
        recovery_request_id: uuid(61),
        worker_id: e.worker_id.clone(),
        recovery_ticket: ticket.secret.clone(),
        new_ed25519_public_key: new_public_key.clone(),
        new_key_proof: sign_b64url(
            &new_key,
            &recovery_signing_string(&uuid(61), &ticket.secret, &new_public_key),
        ),
    };
    let resp = svc.recover_worker(&req, NOW).expect("accidental recovery");
    let mut recovered = e.clone();
    recovered.credential_id = resp.new_credential_id.clone();
    svc.verify_request(
        &heartbeat(&new_key, &recovered, &uuid(102), NOW),
        b"{}",
        NOW,
    )
    .expect("recovered after accidental revocation");
    let audit = svc.audit_entries().expect("audit");
    assert!(
        audit
            .iter()
            .any(|a| a["event"] == "recovery_ticket_refused")
    );
    assert!(audit.iter().any(|a| a["event"] == "recovery_refused"));
    assert!(audit.iter().any(|a| a["event"] == "worker_reinstated"));
    assert!(
        audit
            .iter()
            .any(|a| a["event"] == "revocation_downgrade_refused")
    );
}

#[test]
fn worker_chosen_idempotency_keys_are_scoped_per_worker() {
    let root = fresh_root("key-scope");
    let svc = IdentityService::open(&root).expect("open");
    let (a, key_a) = enroll_worker(&svc, "alice", 1, 1);
    let (b, key_b) = enroll_worker(&svc, "bob", 2, 2);
    let caller_a = svc
        .verify_request(&heartbeat(&key_a, &a, &uuid(100), NOW), b"{}", NOW)
        .expect("caller a");
    let caller_b = svc
        .verify_request(&heartbeat(&key_b, &b, &uuid(101), NOW), b"{}", NOW)
        .expect("caller b");

    // A rotates with rotation_id X; B reusing the SAME X rotates its own
    // credential normally — no conflict, no disclosure that A used it
    // (§3.2: worker-chosen keys never share a namespace).
    let rot_a = rotate_request(&a.worker_id, &uuid(50), &test_key(3));
    svc.rotate(&caller_a, &a.worker_id, &rot_a, NOW)
        .expect("a rotates");
    let rot_b = rotate_request(&b.worker_id, &uuid(50), &test_key(4));
    let resp_b = svc
        .rotate(&caller_b, &b.worker_id, &rot_b, NOW)
        .expect("b rotates with the same rotation_id");
    assert_eq!(resp_b.worker_id, b.worker_id);

    // Same for slot_registration_id: B reusing A's ID registers B's own
    // slot cleanly.
    let slot_a = svc
        .register_slot(&caller_a, &uuid(70), "cpu-0", NOW)
        .expect("a's slot");
    let slot_b = svc
        .register_slot(&caller_b, &uuid(70), "cpu-0", NOW)
        .expect("b's slot with the same slot_registration_id");
    assert_ne!(slot_a.slot_id, slot_b.slot_id);
    assert_eq!(slot_b.worker_id, b.worker_id);
}

#[test]
fn offer_recording_is_scoped_qualified_and_idempotent() {
    let root = fresh_root("offer-scope");
    let svc = IdentityService::open(&root).expect("open");
    let (a, key_a) = enroll_worker(&svc, "alice", 1, 1);
    let (b, key_b) = enroll_worker(&svc, "bob", 2, 2);
    let caller_a = svc
        .verify_request(&heartbeat(&key_a, &a, &uuid(100), NOW), b"{}", NOW)
        .expect("caller a");
    let caller_b = svc
        .verify_request(&heartbeat(&key_b, &b, &uuid(101), NOW), b"{}", NOW)
        .expect("caller b");
    let slot_a = svc
        .register_slot(&caller_a, &uuid(70), "cpu-0", NOW)
        .expect("slot a");

    // An unqualified slot cannot record an offer (§16.2, fail-closed §14).
    let err = svc
        .record_capacity_offer(&caller_a, &uuid(50), &slot_a.slot_id, NOW)
        .expect_err("unqualified");
    assert_eq!(err.code, IdentityErrorCode::SlotNotQualified);
    svc.mark_slot_qualified(
        &slot_a.slot_id,
        "operator:test",
        &uuid(75),
        &sha256_hex(b"qualification-spec"),
        NOW,
    )
    .expect("qualify");

    // Worker B naming worker A's slot is rejected uniformly, identically
    // to a nonexistent slot (§3.2) — the check does not depend on the
    // caller repeating it.
    let cross = svc
        .record_capacity_offer(&caller_b, &uuid(51), &slot_a.slot_id, NOW)
        .expect_err("cross-worker slot");
    assert_eq!(cross.code, IdentityErrorCode::UnknownResource);
    let missing = svc
        .record_capacity_offer(&caller_b, &uuid(52), &uuid(999), NOW)
        .expect_err("missing slot");
    assert_eq!(cross.code, missing.code);

    // Recording is idempotent by (worker, offer_id); the same offer ID for
    // a different slot conflicts (§2, §5).
    let record = svc
        .record_capacity_offer(&caller_a, &uuid(50), &slot_a.slot_id, NOW)
        .expect("record");
    let retry = svc
        .record_capacity_offer(&caller_a, &uuid(50), &slot_a.slot_id, NOW + 5)
        .expect("retry");
    assert_eq!(record, retry);
    let slot_a2 = svc
        .register_slot(&caller_a, &uuid(71), "cpu-1", NOW)
        .expect("slot a2");
    svc.mark_slot_qualified(
        &slot_a2.slot_id,
        "operator:test",
        &uuid(76),
        &sha256_hex(b"qualification-spec-2"),
        NOW,
    )
    .expect("qualify a2");
    let err = svc
        .record_capacity_offer(&caller_a, &uuid(50), &slot_a2.slot_id, NOW)
        .expect_err("offer_id reused for a different slot");
    assert_eq!(err.code, IdentityErrorCode::IdempotencyConflict);
}

// ---------------------------------------------------------------------------
// Concurrency: single-use and uniqueness invariants under parallel requests
// ---------------------------------------------------------------------------

#[test]
fn concurrent_enrollments_consume_a_ticket_exactly_once() {
    let root = fresh_root("race-ticket");
    let svc = IdentityService::open(&root).expect("open");
    let member_id = svc.create_member("alice", NOW).expect("member");
    let ticket = svc
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");

    // Two racing enrollments present the same one-time ticket under
    // different enrollment request IDs: exactly one may win (§3.1).
    let outcomes: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = [(1u64, 11u8), (2u64, 12u8)]
            .into_iter()
            .map(|(n, seed)| {
                let svc = &svc;
                let secret = ticket.secret.clone();
                scope.spawn(move || {
                    svc.enroll(&enroll_request(&uuid(n), &secret, &test_key(seed)), NOW)
                        .is_ok()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "exactly one racing enrollment may consume the ticket: {outcomes:?}"
    );
}

#[test]
fn concurrent_issuances_map_a_benchmark_exactly_once() {
    let root = fresh_root("race-benchmark");
    let svc = IdentityService::open(&root).expect("open");
    let (e, key) = enroll_worker(&svc, "alice", 1, 1);
    let caller = svc
        .verify_request(&heartbeat(&key, &e, &uuid(100), NOW), b"{}", NOW)
        .expect("caller");
    let slot = svc
        .register_slot(&caller, &uuid(70), "cpu-0", NOW)
        .expect("slot");
    svc.mark_slot_qualified(
        &slot.slot_id,
        "operator:test",
        &uuid(75),
        &sha256_hex(b"qualification-spec"),
        NOW,
    )
    .expect("qualify");

    // Two racing issuances for the same benchmark under different issuance
    // IDs: exactly one may map it (§2, §16.1).
    let outcomes: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = [80u64, 81u64]
            .into_iter()
            .map(|n| {
                let (svc, e, slot) = (&svc, &e, &slot);
                scope.spawn(move || {
                    svc.issue_assignment(
                        &uuid(n),
                        &facts(
                            &e.worker_id,
                            &slot.slot_id,
                            "raced-benchmark-id",
                            &sha256_hex(b"raced-digest"),
                        ),
                        NOW,
                    )
                    .is_ok()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "exactly one racing issuance may map the benchmark: {outcomes:?}"
    );
}

#[test]
fn recovery_attaches_new_key_and_revokes_every_old_credential() {
    let root = fresh_root("recover");
    let svc = IdentityService::open(&root).expect("open");
    let (e, old_key) = enroll_worker(&svc, "alice", 1, 1);

    // Lost key: the authenticated member account creates a WORKER_RECOVERY
    // ticket bound to the existing worker (§3.3).
    let ticket = svc
        .create_recovery_ticket(&e.member_id, &e.worker_id, NOW)
        .expect("recovery ticket");
    let new_key = test_key(2);
    let new_public_key = encode_public_key(&new_key.verifying_key());
    let proof = sign_b64url(
        &new_key,
        &recovery_signing_string(&uuid(60), &ticket.secret, &new_public_key),
    );
    let req = RecoverWorkerRequest {
        recovery_request_id: uuid(60),
        worker_id: e.worker_id.clone(),
        recovery_ticket: ticket.secret.clone(),
        new_ed25519_public_key: new_public_key,
        new_key_proof: proof,
    };
    let resp = svc.recover_worker(&req, NOW).expect("recover");
    assert_eq!(resp.worker_id, e.worker_id);
    assert!(resp.revoked_credential_ids.contains(&e.credential_id));

    // Identical retry returns the recorded response.
    let retry = svc.recover_worker(&req, NOW + 30).expect("retry");
    assert_eq!(
        serde_json::to_string(&resp).unwrap(),
        serde_json::to_string(&retry).unwrap()
    );

    // Every old credential is revoked; the new one works; the worker and
    // its ownership are unchanged.
    let err = svc
        .verify_request(&heartbeat(&old_key, &e, &uuid(103), NOW), b"{}", NOW)
        .expect_err("old key revoked by recovery");
    assert_eq!(err.code, IdentityErrorCode::NotAuthenticated);
    let mut recovered = e.clone();
    recovered.credential_id = resp.new_credential_id.clone();
    let verified = svc
        .verify_request(
            &heartbeat(&new_key, &recovered, &uuid(104), NOW),
            b"{}",
            NOW,
        )
        .expect("recovered key");
    assert_eq!(verified.member_id, e.member_id);
    assert_eq!(verified.worker_id, e.worker_id);

    // A recovery ticket is bound to its exact worker: consuming it for a
    // different worker is rejected.
    let (other, _) = enroll_worker(&svc, "bob", 3, 3);
    let ticket2 = svc
        .create_recovery_ticket(&e.member_id, &e.worker_id, NOW)
        .expect("ticket2");
    let wrong_worker = RecoverWorkerRequest {
        recovery_request_id: uuid(61),
        worker_id: other.worker_id.clone(),
        recovery_ticket: ticket2.secret.clone(),
        new_ed25519_public_key: encode_public_key(&test_key(4).verifying_key()),
        new_key_proof: sign_b64url(
            &test_key(4),
            &recovery_signing_string(
                &uuid(61),
                &ticket2.secret,
                &encode_public_key(&test_key(4).verifying_key()),
            ),
        ),
    };
    let err = svc
        .recover_worker(&wrong_worker, NOW)
        .expect_err("wrong worker");
    assert_eq!(err.code, IdentityErrorCode::TicketRejected);
}

// ---------------------------------------------------------------------------
// Scope 5 (identity half): pool-issued slot and assignment identities
// ---------------------------------------------------------------------------

#[test]
fn slot_registration_is_idempotent_and_scoped() {
    let root = fresh_root("slots");
    let svc = IdentityService::open(&root).expect("open");
    let (e, key) = enroll_worker(&svc, "alice", 1, 1);
    let caller = svc
        .verify_request(&heartbeat(&key, &e, &uuid(100), NOW), b"{}", NOW)
        .expect("caller");

    let grant = svc
        .register_slot(&caller, &uuid(70), "cpu-0", NOW)
        .expect("register");
    assert_eq!(grant.worker_id, e.worker_id);
    assert_eq!(grant.generation, 1);
    // Every slot is issued UNQUALIFIED until the §6 flow (or its audited
    // pool stand-in) qualifies its exact generation (§16.2).
    assert_eq!(grant.qualification, "UNQUALIFIED");

    // Same registration ID retried: same grant. Different content for the
    // same ID: conflict (§3.1: `slot_registration_id` is the idempotency
    // key).
    let retry = svc
        .register_slot(&caller, &uuid(70), "cpu-0", NOW + 5)
        .expect("retry");
    assert_eq!(grant, retry);
    let err = svc
        .register_slot(&caller, &uuid(70), "cpu-1", NOW)
        .expect_err("conflict");
    assert_eq!(err.code, IdentityErrorCode::IdempotencyConflict);

    // A new registration ID for the same logical slot with unchanged facts
    // is a no-op returning the existing generation (§2) — no extra slot
    // generation is manufactured (§16.3).
    let noop = svc
        .register_slot(&caller, &uuid(71), "cpu-0", NOW)
        .expect("no-op");
    assert_eq!(noop, grant);
}

#[test]
fn assignment_identity_is_pool_issued_bound_and_not_derived() {
    let root = fresh_root("assign");
    let svc = IdentityService::open(&root).expect("open");
    let (a, key_a) = enroll_worker(&svc, "alice", 1, 1);
    let (b, _key_b) = enroll_worker(&svc, "bob", 2, 2);
    let caller_a = svc
        .verify_request(&heartbeat(&key_a, &a, &uuid(100), NOW), b"{}", NOW)
        .expect("caller a");
    let slot_a = svc
        .register_slot(&caller_a, &uuid(70), "cpu-0", NOW)
        .expect("slot a");

    const BENCHMARK_ID: &str = "511ca6ea841e0c82fd8ee83959b31e99";
    // Fail closed (§16.2, §14): an unqualified slot cannot receive work.
    let digest = sha256_hex(b"assignment-digest-1");
    let err = svc
        .issue_assignment(
            &uuid(80),
            &facts(&a.worker_id, &slot_a.slot_id, BENCHMARK_ID, &digest),
            NOW,
        )
        .expect_err("unqualified slot");
    assert_eq!(err.code, IdentityErrorCode::SlotNotQualified);
    svc.mark_slot_qualified(
        &slot_a.slot_id,
        "operator:test",
        &uuid(75),
        &sha256_hex(b"qualification-spec"),
        NOW,
    )
    .expect("qualify");
    let grant = svc
        .issue_assignment(
            &uuid(80),
            &facts(&a.worker_id, &slot_a.slot_id, BENCHMARK_ID, &digest),
            NOW,
        )
        .expect("issue");
    // Bound to the enrolled member through the §2 ownership chain.
    assert_eq!(grant.member_id, a.member_id);
    assert_eq!(grant.worker_id, a.worker_id);
    assert_eq!(grant.slot_id, slot_a.slot_id);
    assert_eq!(grant.benchmark_id, BENCHMARK_ID);

    // Idempotent by issuance id; different content for the same id
    // conflicts (architecture §6).
    let retry = svc
        .issue_assignment(
            &uuid(80),
            &facts(&a.worker_id, &slot_a.slot_id, BENCHMARK_ID, &digest),
            NOW,
        )
        .expect("retry");
    assert_eq!(grant, retry);
    let err = svc
        .issue_assignment(
            &uuid(80),
            &facts(&a.worker_id, &slot_a.slot_id, "otherbenchmark", &digest),
            NOW,
        )
        .expect_err("conflict");
    assert_eq!(err.code, IdentityErrorCode::IdempotencyConflict);

    // One TIG benchmark maps permanently to one assignment (§2, §16.1).
    let err = svc
        .issue_assignment(
            &uuid(81),
            &facts(
                &a.worker_id,
                &slot_a.slot_id,
                BENCHMARK_ID,
                &sha256_hex(b"assignment-digest-2"),
            ),
            NOW,
        )
        .expect_err("benchmark uniqueness");
    assert_eq!(err.code, IdentityErrorCode::IdempotencyConflict);

    // A worker cannot be issued an assignment on another worker's slot.
    let err = svc
        .issue_assignment(
            &uuid(82),
            &facts(
                &b.worker_id,
                &slot_a.slot_id,
                "bench2",
                &sha256_hex(b"assignment-digest-3"),
            ),
            NOW,
        )
        .expect_err("cross-worker slot");
    assert_eq!(err.code, IdentityErrorCode::UnknownResource);

    // NOT derived from the benchmark id: an independent pool issuing an
    // assignment for the same benchmark produces a different identity
    // (the spike's stand-ins were pure functions of the benchmark id).
    let other_root = fresh_root("assign-other");
    let other = IdentityService::open(&other_root).expect("open other");
    let (a2, key_a2) = enroll_worker(&other, "alice", 1, 1);
    let caller_a2 = other
        .verify_request(&heartbeat(&key_a2, &a2, &uuid(100), NOW), b"{}", NOW)
        .expect("caller");
    let slot2 = other
        .register_slot(&caller_a2, &uuid(70), "cpu-0", NOW)
        .expect("slot");
    other
        .mark_slot_qualified(
            &slot2.slot_id,
            "operator:test",
            &uuid(75),
            &sha256_hex(b"qualification-spec"),
            NOW,
        )
        .expect("qualify");
    let grant2 = other
        .issue_assignment(
            &uuid(80),
            &facts(&a2.worker_id, &slot2.slot_id, BENCHMARK_ID, &digest),
            NOW,
        )
        .expect("issue other");
    assert_ne!(
        grant.assignment_id, grant2.assignment_id,
        "assignment identity must not be a function of the benchmark id"
    );
}
