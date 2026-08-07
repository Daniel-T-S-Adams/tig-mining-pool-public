//! Issue #32 acceptance, upload half: the S3 upload/acceptance suite's core
//! scenarios (resumable upload, durable-acceptance saga, byte-identical
//! immutable receipt, slot re-offer) re-validated under REAL enrolled and
//! authenticated identities — pool-issued member/worker/slot/assignment IDs,
//! not the spike's benchmark-derived stand-ins — plus the negative
//! authentication and authorization-scoping paths.
//!
//! Contracts under test: `docs/member_protocol.md` §3.2 (signed requests;
//! cross-worker resource IDs rejected without disclosing existence), §5
//! (routes and idempotency keys), §11–§12 (upload and acceptance);
//! `docs/pre_build_checklist.md` §5.1 (a worker restricted to its own
//! slots/assignments/uploads).
//!
//! Deterministic and offline: fixed timestamps, TEST-ONLY fixed key bytes
//! (never real credentials), and the golden fixture package from
//! `fixtures/benchmark-artifact/v1/cases/golden`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::SigningKey;
use pool_identity::keys::{
    encode_public_key, enrollment_signing_string, request_signing_string, sha256_hex, sign_b64url,
};
use pool_identity::{
    AssignmentGrant, EnrollRequest, EnrollResponse, IdentityErrorCode, SignedRequest,
};
use serde_json::{Value, json};
use spike::authed::{AResult, AssignmentIssuance, AuthedPool};
use spike::member::{build_tar, compress_zstd};
use spike::pool::{CrashPoint, ErrorCode, FinalizeOutcome, UploadSession};

const NOW: u64 = 1_754_000_000;
const BENCHMARK_ID: &str = "511ca6ea841e0c82fd8ee83959b31e99";
const BENCHMARK_ID_2: &str = "818b03c19fc7c28d71c59c9c09791ef8";
const CHUNK: usize = 512;

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// The assignment-scoped storage key the authed path derives for a
/// worker-chosen package_id (mirrors `spike::authed::scoped_package_id`).
fn scoped_pool_package_id(assignment_id: &str, package_id: &str) -> String {
    sha256_hex(format!("{assignment_id}:{package_id}").as_bytes())[..32].to_owned()
}

/// TEST-ONLY deterministic key bytes; never a real credential.
fn test_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

// -- golden package (identical to tests/pool_upload.rs) ---------------------

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/benchmark-artifact/v1/cases/golden")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(golden_dir().join(name)).expect(name)
}

fn unhex_lines(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8(data.to_vec()).expect("utf8");
    let joined: String = text.split_whitespace().collect();
    (0..joined.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&joined[i..i + 2], 16).expect("hex"))
        .collect()
}

struct Package {
    bytes: Vec<u8>,
    manifest_sha256: String,
    uncompressed_size: u64,
}

fn build_package() -> Package {
    let manifest = read("manifest.json");
    let qualities = unhex_lines(&read("qualities.i32le.hex"));
    let leaf_hashes = unhex_lines(&read("leaf-hashes.bin.hex"));
    let outputs = read("outputs.ndjson");
    let tar = build_tar(&manifest, &qualities, &leaf_hashes, &outputs).expect("tar");
    let bytes = compress_zstd(&tar, 3).expect("zstd");
    Package {
        manifest_sha256: sha256_hex(&manifest),
        uncompressed_size: tar.len() as u64,
        bytes,
    }
}

fn declaration(p: &Package, package_id: &str, digest: &str) -> Value {
    json!({
        "package_id": package_id,
        "assignment_digest": digest,
        "media_type": "application/vnd.tig-pool.proof-material-v1.tar+zstd",
        "compressed_size_bytes": p.bytes.len() as u64,
        "uncompressed_size_bytes": p.uncompressed_size,
        "manifest_sha256": p.manifest_sha256,
        "package_sha256": sha256_hex(&p.bytes),
    })
}

// -- enrolled test environment ----------------------------------------------

struct Worker {
    enrolled: EnrollResponse,
    key: SigningKey,
}

struct Env {
    authed: AuthedPool,
    root: PathBuf,
    a: Worker,
    b: Worker,
    slot_a: String,
    grant: AssignmentGrant,
    digest: String,
    next_rid: AtomicU64,
}

fn enroll(authed: &AuthedPool, member_ref: &str, seed: u8, n: u64) -> Worker {
    let key = test_key(seed);
    let identity = authed.identity();
    let member_id = identity.create_member(member_ref, NOW).expect("member");
    let ticket = identity
        .create_enrollment_ticket(&member_id, NOW)
        .expect("ticket");
    let public_key = encode_public_key(&key.verifying_key());
    let proof = sign_b64url(
        &key,
        &enrollment_signing_string(&uuid(n), &ticket.secret, &public_key),
    );
    let enrolled = identity
        .enroll(
            &EnrollRequest {
                enrollment_request_id: uuid(n),
                enrollment_ticket: ticket.secret,
                worker_name: format!("{member_ref}-box"),
                ed25519_public_key: public_key,
                ed25519_key_proof: proof,
                supported_protocol_versions: vec!["0.1.0".to_owned()],
                supported_package_formats: vec!["proof-material-v1".to_owned()],
                member_agent_version: "spike-test-0".to_owned(),
            },
            NOW,
        )
        .expect("enroll");
    Worker { enrolled, key }
}

fn setup(name: &str) -> Env {
    let root = std::env::temp_dir().join(format!("spike-authed-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let authed = AuthedPool::open(&root).expect("open");
    let a = enroll(&authed, "alice", 1, 1);
    let b = enroll(&authed, "bob", 2, 2);
    let env = Env {
        authed,
        root,
        a,
        b,
        slot_a: String::new(),
        grant: AssignmentGrant {
            assignment_id: String::new(),
            member_id: String::new(),
            worker_id: String::new(),
            slot_id: String::new(),
            benchmark_id: String::new(),
        },
        digest: sha256_hex(b"authed-s3-assignment"),
        next_rid: AtomicU64::new(1000),
    };
    // Register worker A's slot through a signed request, then issue and
    // register the assignment (pool-internal, controller-owned).
    let hb = env.signed(&env.a, "POST", "/member/v0/heartbeats", b"{}");
    let caller = env
        .authed
        .identity()
        .verify_request(&hb, b"{}", NOW)
        .expect("caller");
    let slot = env
        .authed
        .identity()
        .register_slot(&caller, &uuid(70), "cpu-0", NOW)
        .expect("slot");
    // Stand-in for the deferred §6 qualification flow: the audited pool
    // decision qualifies the slot generation so it may receive work.
    env.authed
        .identity()
        .mark_slot_qualified(
            &slot.slot_id,
            "operator:spike-test",
            &uuid(75),
            &sha256_hex(b"qualification-spec"),
            NOW,
        )
        .expect("qualify slot");
    let grant = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(80),
                worker_id: &env.a.enrolled.worker_id,
                slot_id: &slot.slot_id,
                benchmark_id: BENCHMARK_ID,
                assignment_digest: &env.digest,
                network: "testnet",
            },
            NOW,
        )
        .expect("issue assignment");
    Env {
        slot_a: slot.slot_id,
        grant,
        ..env
    }
}

impl Env {
    fn rid(&self) -> String {
        uuid(self.next_rid.fetch_add(1, Ordering::Relaxed))
    }

    fn signed(&self, w: &Worker, method: &str, path: &str, body: &[u8]) -> SignedRequest {
        self.signed_at(w, method, path, body, NOW)
    }

    fn signed_at(
        &self,
        w: &Worker,
        method: &str,
        path: &str,
        body: &[u8],
        timestamp: u64,
    ) -> SignedRequest {
        let request_id = self.rid();
        let body_sha256 = sha256_hex(body);
        let message = request_signing_string(
            method,
            path,
            "0.1.0",
            &w.enrolled.worker_id,
            &w.enrolled.credential_id,
            &request_id,
            timestamp,
            &body_sha256,
        );
        SignedRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            protocol_version: "0.1.0".to_owned(),
            worker_id: w.enrolled.worker_id.clone(),
            credential_id: w.enrolled.credential_id.clone(),
            request_id,
            request_timestamp: timestamp,
            body_sha256,
            signature: String::new(),
        }
        .with_signature(&w.key, &message)
    }

    fn create_upload(
        &self,
        w: &Worker,
        assignment_id: &str,
        decl: &Value,
    ) -> AResult<UploadSession> {
        let body = serde_json::to_vec(decl).expect("decl bytes");
        let path = format!("/member/v0/assignments/{assignment_id}/uploads");
        let sr = self.signed(w, "POST", &path, &body);
        self.authed.create_upload(&sr, &body, NOW)
    }

    fn put_chunk(&self, w: &Worker, upload_id: &str, offset: u64, bytes: &[u8]) -> AResult<u64> {
        let path = format!("/member/v0/uploads/{upload_id}");
        let sr = self.signed(w, "PUT", &path, bytes);
        self.authed
            .put_chunk(&sr, offset, &sha256_hex(bytes), bytes, NOW)
    }

    fn upload_all(&self, w: &Worker, upload_id: &str, bytes: &[u8]) -> u64 {
        let mut offset = 0u64;
        while (offset as usize) < bytes.len() {
            let end = (offset as usize + CHUNK).min(bytes.len());
            offset = self
                .put_chunk(w, upload_id, offset, &bytes[offset as usize..end])
                .expect("chunk");
        }
        offset
    }

    fn finalize(&self, w: &Worker, upload_id: &str, decl: &Value) -> AResult<FinalizeOutcome> {
        let body = serde_json::to_vec(&json!({
            "protocol_version": "0.1.0",
            "finalize_id": self.rid(),
            "package_sha256": decl["package_sha256"],
            "compressed_size_bytes": decl["compressed_size_bytes"],
        }))
        .expect("finalize bytes");
        let path = format!("/member/v0/uploads/{upload_id}/finalize");
        let sr = self.signed(w, "POST", &path, &body);
        self.authed.finalize(&sr, &body, NOW)
    }

    fn status(&self, w: &Worker, upload_id: &str) -> AResult<Value> {
        let path = format!("/member/v0/uploads/{upload_id}/status");
        let sr = self.signed(w, "POST", &path, b"{}");
        self.authed.upload_status(&sr, b"{}", NOW)
    }

    fn offer(&self, w: &Worker, slot_id: &str, n: u64) -> AResult<Value> {
        let body = serde_json::to_vec(&json!({ "offer_id": uuid(n), "slot_id": slot_id }))
            .expect("offer bytes");
        let sr = self.signed(w, "POST", "/member/v0/capacity-offers", &body);
        self.authed.offer_capacity(&sr, &body, NOW)
    }

    fn reopen(self) -> Env {
        let root = self.root.clone();
        let Env {
            a,
            b,
            slot_a,
            grant,
            digest,
            next_rid,
            ..
        } = self;
        Env {
            authed: AuthedPool::open(&root).expect("reopen"),
            root,
            a,
            b,
            slot_a,
            grant,
            digest,
            next_rid,
        }
    }
}

trait WithSignature {
    fn with_signature(self, key: &SigningKey, message: &str) -> SignedRequest;
}

impl WithSignature for SignedRequest {
    fn with_signature(mut self, key: &SigningKey, message: &str) -> SignedRequest {
        self.signature = sign_b64url(key, message);
        self
    }
}

// ---------------------------------------------------------------------------
// Scope 6: S3 core scenarios re-run under enrolled, authenticated identities
// ---------------------------------------------------------------------------

#[test]
fn authed_interrupted_upload_resumes_from_last_durable_offset() {
    let env = setup("resume");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    assert!(!session.resumed);

    let c0 = &pkg.bytes[0..CHUNK];
    assert_eq!(
        env.put_chunk(&env.a, &session.upload_id, 0, c0)
            .expect("chunk 0"),
        CHUNK as u64
    );

    // Kill the writer between the chunk object write and the range commit
    // (storage-level crash injection, exactly as the S3 suite does).
    let c1 = &pkg.bytes[CHUNK..2 * CHUNK];
    let e = env
        .authed
        .pool()
        .put_chunk_crash(
            &session.upload_id,
            CHUNK as u64,
            &sha256_hex(c1),
            c1,
            Some(CrashPoint::AfterChunkObjectWrite),
        )
        .expect_err("simulated crash");
    assert_eq!(e.code, ErrorCode::SimulatedCrash);

    // Restart: a fresh authed pool over the same root sees only durable
    // state, and the worker resumes through signed requests.
    let env = env.reopen();
    let status = env.status(&env.a, &session.upload_id).expect("status");
    assert_eq!(status["committed_offset"].as_u64(), Some(CHUNK as u64));

    let resumed = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("resume");
    assert!(resumed.resumed);
    assert_eq!(resumed.upload_id, session.upload_id);
    assert_eq!(resumed.committed_offset, CHUNK as u64);

    // An identical retry adopts the orphaned chunk; a conflicting retry of
    // a committed range fails.
    assert_eq!(
        env.put_chunk(&env.a, &session.upload_id, CHUNK as u64, c1)
            .expect("identical retry adopts"),
        2 * CHUNK as u64
    );
    let mut wrong = c0.to_vec();
    wrong[0] ^= 0xff;
    let e = env
        .put_chunk(&env.a, &session.upload_id, 0, &wrong)
        .expect_err("conflicting retry");
    assert_eq!(e.pool_code(), Some(ErrorCode::ChunkConflict));
}

#[test]
fn authed_saga_commits_receipt_bound_to_issued_identities() {
    let env = setup("saga");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let sha = decl["package_sha256"].as_str().unwrap().to_owned();
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    env.upload_all(&env.a, &session.upload_id, &pkg.bytes);

    let outcome = env
        .finalize(&env.a, &session.upload_id, &decl)
        .expect("finalize");
    assert!(outcome.fresh);

    // The receipt binds the §2 ownership chain of ISSUED identities:
    // benchmark -> assignment -> slot -> worker -> member.
    let receipt: Value = serde_json::from_str(&outcome.receipt_json).unwrap();
    assert_eq!(receipt["benchmark_id"].as_str(), Some(BENCHMARK_ID));
    assert_eq!(
        receipt["assignment_id"].as_str(),
        Some(env.grant.assignment_id.as_str())
    );
    assert_eq!(receipt["slot_id"].as_str(), Some(env.slot_a.as_str()));
    assert_eq!(
        receipt["worker_id"].as_str(),
        Some(env.a.enrolled.worker_id.as_str())
    );
    assert_eq!(
        receipt["member_id"].as_str(),
        Some(env.a.enrolled.member_id.as_str())
    );
    assert_eq!(receipt["package_sha256"].as_str(), Some(sha.as_str()));
    assert_eq!(receipt["member_may_delete_package"], true);
    assert_eq!(receipt["slot_released"], true);
    // The member-visible receipt echoes the worker's own package_id (§3.2:
    // responses carry the application idempotency key)...
    assert_eq!(receipt["package_id"].as_str(), Some(uuid(90).as_str()));

    // ...while storage keys use the assignment-scoped package identity so
    // worker-chosen IDs never share a global namespace (§3.2).
    let scoped = scoped_pool_package_id(&env.grant.assignment_id, &uuid(90));
    let object_key = format!("pool/accepted/testnet/{BENCHMARK_ID}/{scoped}/{sha}.tar.zst");
    let object = std::fs::read(env.root.join(object_key)).expect("accepted object");
    assert_eq!(object, pkg.bytes);

    // A lost response is recovered through signed status: the same
    // immutable receipt (§3.2, §9).
    let status = env.status(&env.a, &session.upload_id).expect("status");
    assert_eq!(status["durably_accepted"], true);
    assert_eq!(
        serde_json::to_string(&status["acceptance_receipt"]).unwrap(),
        outcome.receipt_json
    );
}

#[test]
fn authed_retried_finalization_returns_byte_identical_receipt() {
    let env = setup("receipt");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    env.upload_all(&env.a, &session.upload_id, &pkg.bytes);

    let first = env
        .finalize(&env.a, &session.upload_id, &decl)
        .expect("finalize");
    assert!(first.fresh);

    // Crash after commit, before the response: the retry — a fresh signed
    // request against a fresh process — returns the SAME immutable receipt.
    let env = env.reopen();
    let second = env
        .finalize(&env.a, &session.upload_id, &decl)
        .expect("retry");
    assert!(!second.fresh);
    assert_eq!(first.receipt_json, second.receipt_json);
}

#[test]
fn authed_slot_reoffer_only_after_durable_acceptance() {
    let env = setup("reoffer");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    env.upload_all(&env.a, &session.upload_id, &pkg.bytes);

    // Before durable acceptance the slot is occupied: no new offer and no
    // new assignment issuance (§9: RECEIVED releases nothing).
    let e = env.offer(&env.a, &env.slot_a, 300).expect_err("occupied");
    assert_eq!(e.pool_code(), Some(ErrorCode::SlotOccupied));
    let digest2 = sha256_hex(b"authed-s3-assignment-2");
    let e = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(81),
                worker_id: &env.a.enrolled.worker_id,
                slot_id: &env.slot_a,
                benchmark_id: BENCHMARK_ID_2,
                assignment_digest: &digest2,
                network: "testnet",
            },
            NOW,
        )
        .expect_err("slot occupied");
    assert_eq!(e.pool_code(), Some(ErrorCode::SlotOccupied));

    // Worker B cannot offer A's slot — rejected without disclosing whether
    // the slot exists (§3.2), same as a nonexistent slot.
    let cross = env
        .offer(&env.b, &env.slot_a, 301)
        .expect_err("cross-worker");
    assert_eq!(
        cross.identity_code(),
        Some(IdentityErrorCode::UnknownResource)
    );
    let missing = env
        .offer(&env.b, &uuid(999), 302)
        .expect_err("missing slot");
    assert_eq!(
        format!("{:?}", cross.identity_code()),
        format!("{:?}", missing.identity_code()),
        "cross-worker and nonexistent must be indistinguishable"
    );

    // Durable acceptance releases the slot exactly once; the worker's next
    // signed offer is admissible and a full second cycle completes at the
    // bumped generation.
    env.finalize(&env.a, &session.upload_id, &decl)
        .expect("finalize");
    assert_eq!(
        env.authed
            .pool()
            .release_count(&env.slot_a, 1)
            .expect("count"),
        1
    );
    // The released slot's offer command is recorded; without a Controller
    // admission policy the answer is NO_ACTION, never silent admission
    // (§6: absence of an applicable policy produces NO_ACTION).
    let offer = env.offer(&env.a, &env.slot_a, 303).expect("re-offer");
    assert_eq!(offer["offer_state"], "NO_ACTION");

    let grant2 = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(82),
                worker_id: &env.a.enrolled.worker_id,
                slot_id: &env.slot_a,
                benchmark_id: BENCHMARK_ID_2,
                assignment_digest: &digest2,
                network: "testnet",
            },
            NOW,
        )
        .expect("second issuance");
    assert_ne!(grant2.assignment_id, env.grant.assignment_id);

    // A replayed issuance is a PURE READ (§16.3: a retry cannot create an
    // extra slot generation or reservation): replaying the FIRST issuance
    // while the slot is reserved for the second returns the recorded grant
    // and leaves the slot untouched.
    let replayed = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(80),
                worker_id: &env.a.enrolled.worker_id,
                slot_id: &env.slot_a,
                benchmark_id: BENCHMARK_ID,
                assignment_digest: &env.digest,
                network: "testnet",
            },
            NOW,
        )
        .expect("replay of first issuance");
    assert_eq!(replayed.assignment_id, env.grant.assignment_id);
    let view = env.authed.pool().slot_view(&env.slot_a).expect("view");
    assert_eq!(view.generation, 2, "replay must not bump the generation");
    assert_eq!(
        view.assignment_id, grant2.assignment_id,
        "replay must not steal the reservation"
    );

    let decl2 = declaration(&pkg, &uuid(91), &digest2);
    let session2 = env
        .create_upload(&env.a, &grant2.assignment_id, &decl2)
        .expect("second upload");
    env.upload_all(&env.a, &session2.upload_id, &pkg.bytes);
    let outcome2 = env
        .finalize(&env.a, &session2.upload_id, &decl2)
        .expect("second finalize");
    let receipt2: Value = serde_json::from_str(&outcome2.receipt_json).unwrap();
    assert_eq!(receipt2["slot_generation"].as_u64(), Some(2));
    assert_eq!(
        env.authed
            .pool()
            .release_count(&env.slot_a, 2)
            .expect("count"),
        1
    );
}

// ---------------------------------------------------------------------------
// Scope 2/4 negatives: unsigned, bad signature, revoked, stale, cross-worker
// ---------------------------------------------------------------------------

#[test]
fn authed_rejects_bad_signatures_stale_timestamps_and_revoked_keys() {
    let env = setup("negative");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    let c0 = &pkg.bytes[0..CHUNK];
    let path = format!("/member/v0/uploads/{}", session.upload_id);

    // Unsigned (garbage signature).
    let mut sr = env.signed(&env.a, "PUT", &path, c0);
    sr.signature = "A".repeat(86);
    let e = env
        .authed
        .put_chunk(&sr, 0, &sha256_hex(c0), c0, NOW)
        .expect_err("garbage signature");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::NotAuthenticated));

    // Signed by a key that is not the enrolled credential.
    let msg = request_signing_string(
        "PUT",
        &path,
        "0.1.0",
        &env.a.enrolled.worker_id,
        &env.a.enrolled.credential_id,
        &uuid(400),
        NOW,
        &sha256_hex(c0),
    );
    let mut sr = env.signed(&env.a, "PUT", &path, c0);
    sr.request_id = uuid(400);
    sr.signature = sign_b64url(&test_key(9), &msg);
    let e = env
        .authed
        .put_chunk(&sr, 0, &sha256_hex(c0), c0, NOW)
        .expect_err("wrong key");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::NotAuthenticated));

    // Stale timestamp (§3.2: outside ±300 s).
    let sr = env.signed_at(&env.a, "PUT", &path, c0, NOW - 301);
    let e = env
        .authed
        .put_chunk(&sr, 0, &sha256_hex(c0), c0, NOW)
        .expect_err("stale");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::StaleTimestamp));

    // Nothing was committed by any rejected request.
    assert_eq!(
        env.status(&env.a, &session.upload_id).expect("status")["committed_offset"].as_u64(),
        Some(0)
    );

    // Revoked credential: takes effect on the very next request (§3.3).
    env.authed
        .identity()
        .revoke_credential(
            &env.a.enrolled.worker_id,
            &env.a.enrolled.credential_id,
            "member:alice",
            &uuid(500),
            NOW,
        )
        .expect("revoke");
    let e = env
        .put_chunk(&env.a, &session.upload_id, 0, c0)
        .expect_err("revoked key");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::NotAuthenticated));
}

#[test]
fn worker_b_cannot_touch_worker_a_resources() {
    let env = setup("scoping");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    let c0 = &pkg.bytes[0..CHUNK];
    env.put_chunk(&env.a, &session.upload_id, 0, c0)
        .expect("a's chunk");

    // B cannot open an upload session under A's assignment...
    let e = env
        .create_upload(&env.b, &env.grant.assignment_id, &decl)
        .expect_err("b on a's assignment");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));

    // ...cannot upload to A's session...
    let c1 = &pkg.bytes[CHUNK..2 * CHUNK];
    let e = env
        .put_chunk(&env.b, &session.upload_id, CHUNK as u64, c1)
        .expect_err("b upload to a's session");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));

    // ...cannot read A's upload status...
    let e = env
        .status(&env.b, &session.upload_id)
        .expect_err("b status");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));

    // ...and cannot finalize A's package.
    env.upload_all(&env.a, &session.upload_id, &pkg.bytes);
    let e = env
        .finalize(&env.b, &session.upload_id, &decl)
        .expect_err("b finalize");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));

    // Cross-worker and nonexistent are indistinguishable (§3.2).
    let e_missing = env
        .status(&env.b, "00000000000000000000000000000000")
        .expect_err("missing");
    assert_eq!(
        format!("{:?}", e.identity_code()),
        format!("{:?}", e_missing.identity_code())
    );

    // The rejections are audited; A's upload then completes untouched.
    assert!(
        env.authed
            .identity()
            .audit_entries()
            .expect("audit")
            .iter()
            .any(|a| a["event"] == "resource_access_rejected")
    );
    let outcome = env
        .finalize(&env.a, &session.upload_id, &decl)
        .expect("a finalizes");
    assert!(outcome.fresh);
}

#[test]
fn worker_chosen_package_ids_are_scoped_per_assignment() {
    let env = setup("pkg-scope");
    let pkg = build_package();

    // Give worker B its own slot and issued assignment.
    let hb = env.signed(&env.b, "POST", "/member/v0/heartbeats", b"{}");
    let caller_b = env
        .authed
        .identity()
        .verify_request(&hb, b"{}", NOW)
        .expect("caller b");
    let slot_b = env
        .authed
        .identity()
        .register_slot(&caller_b, &uuid(71), "cpu-0", NOW)
        .expect("slot b");
    env.authed
        .identity()
        .mark_slot_qualified(
            &slot_b.slot_id,
            "operator:spike-test",
            &uuid(76),
            &sha256_hex(b"qualification-spec-b"),
            NOW,
        )
        .expect("qualify slot b");
    let digest_b = sha256_hex(b"authed-s3-assignment-b");
    let grant_b = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(85),
                worker_id: &env.b.enrolled.worker_id,
                slot_id: &slot_b.slot_id,
                benchmark_id: BENCHMARK_ID_2,
                assignment_digest: &digest_b,
                network: "testnet",
            },
            NOW,
        )
        .expect("issue b");

    // A and B choose the SAME worker-chosen package_id (§2) for their own
    // assignments: neither collides with nor discloses the other (§3.2),
    // because storage keys are scoped by the issued assignment.
    let decl_a = declaration(&pkg, &uuid(90), &env.digest);
    let decl_b = declaration(&pkg, &uuid(90), &digest_b);
    let session_a = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl_a)
        .expect("a's upload");
    let session_b = env
        .create_upload(&env.b, &grant_b.assignment_id, &decl_b)
        .expect("b's upload with the same package_id");
    assert_ne!(session_a.upload_id, session_b.upload_id);

    // Both complete independently, each receipt echoing its own worker's
    // package identity and ownership chain.
    env.upload_all(&env.a, &session_a.upload_id, &pkg.bytes);
    env.upload_all(&env.b, &session_b.upload_id, &pkg.bytes);
    let receipt_a: Value = serde_json::from_str(
        &env.finalize(&env.a, &session_a.upload_id, &decl_a)
            .expect("a finalizes")
            .receipt_json,
    )
    .unwrap();
    let receipt_b: Value = serde_json::from_str(
        &env.finalize(&env.b, &session_b.upload_id, &decl_b)
            .expect("b finalizes")
            .receipt_json,
    )
    .unwrap();
    assert_eq!(receipt_a["package_id"].as_str(), Some(uuid(90).as_str()));
    assert_eq!(receipt_b["package_id"].as_str(), Some(uuid(90).as_str()));
    assert_ne!(receipt_a["receipt_id"], receipt_b["receipt_id"]);
    assert_eq!(
        receipt_a["worker_id"].as_str(),
        Some(env.a.enrolled.worker_id.as_str())
    );
    assert_eq!(
        receipt_b["worker_id"].as_str(),
        Some(env.b.enrolled.worker_id.as_str())
    );
}

#[test]
fn issuance_id_reuse_with_different_facts_is_a_conflict() {
    let env = setup("issuance-idem");

    // An identical retry of the recorded issuance returns the same grant.
    let retry = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(80),
                worker_id: &env.a.enrolled.worker_id,
                slot_id: &env.slot_a,
                benchmark_id: BENCHMARK_ID,
                assignment_digest: &env.digest,
                network: "testnet",
            },
            NOW,
        )
        .expect("identical retry");
    assert_eq!(retry.assignment_id, env.grant.assignment_id);

    // Reusing the issuance ID with different facts is an idempotency
    // conflict (§2: benchmark -> assignment is permanent and one-to-one;
    // architecture §6) — never a silently returned prior grant, even while
    // the slot is occupied. EVERY fact is inside the idempotency boundary
    // (§14, §16.5): a changed benchmark, a changed digest ALONE, and a
    // changed network ALONE all conflict.
    let variations: [AssignmentIssuance<'_>; 3] = [
        AssignmentIssuance {
            issuance_id: &uuid(80),
            worker_id: &env.a.enrolled.worker_id,
            slot_id: &env.slot_a,
            benchmark_id: BENCHMARK_ID_2,
            assignment_digest: &sha256_hex(b"some-other-digest"),
            network: "testnet",
        },
        // Only the digest differs.
        AssignmentIssuance {
            issuance_id: &uuid(80),
            worker_id: &env.a.enrolled.worker_id,
            slot_id: &env.slot_a,
            benchmark_id: BENCHMARK_ID,
            assignment_digest: &sha256_hex(b"some-other-digest"),
            network: "testnet",
        },
        // Only the network differs.
        AssignmentIssuance {
            issuance_id: &uuid(80),
            worker_id: &env.a.enrolled.worker_id,
            slot_id: &env.slot_a,
            benchmark_id: BENCHMARK_ID,
            assignment_digest: &env.digest,
            network: "mainnet",
        },
    ];
    for (i, changed) in variations.iter().enumerate() {
        let e = env
            .authed
            .issue_and_register_assignment(changed, NOW)
            .expect_err("changed facts");
        assert_eq!(
            e.identity_code(),
            Some(IdentityErrorCode::IdempotencyConflict),
            "variation {i} must conflict"
        );
    }

    // The pool registration layer independently refuses a second digest for
    // an already-registered assignment_id (§14, §16.5).
    let e = env
        .authed
        .pool()
        .register_assignment(&serde_json::json!({
            "assignment_digest": sha256_hex(b"smuggled-second-digest"),
            "assignment_id": env.grant.assignment_id,
            "benchmark_id": BENCHMARK_ID,
            "slot_id": env.grant.slot_id,
            "slot_generation": 1u64,
            "network": "testnet",
        }))
        .expect_err("second digest for a registered assignment");
    assert_eq!(e.code, ErrorCode::DeclarationConflict);
}

#[test]
fn invalid_member_package_id_is_rejected_before_any_pool_state() {
    let env = setup("bad-package-id");
    let pkg = build_package();

    // A package_id outside the bounded member-token charset is rejected
    // before the pool creates any durable upload state.
    let bad = declaration(&pkg, "NOT-a-Valid-Token!", &env.digest);
    let e = env
        .create_upload(&env.a, &env.grant.assignment_id, &bad)
        .expect_err("invalid package_id");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));
    let upload_index = env.root.join("pool/state/uploads-by-package");
    assert!(
        !upload_index.exists() || std::fs::read_dir(&upload_index).expect("dir").count() == 0,
        "no orphaned pool upload state may exist for a rejected declaration"
    );

    // The same worker then declares a valid package normally.
    let good = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &good)
        .expect("valid declaration");
    assert!(!session.resumed);
}

#[test]
fn concurrent_issuances_for_one_slot_admit_exactly_one() {
    let env = setup("race-slot");

    // The setup assignment occupies slot A; complete it so the slot is
    // released and two NEW issuances can race for it.
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    env.upload_all(&env.a, &session.upload_id, &pkg.bytes);
    env.finalize(&env.a, &session.upload_id, &decl)
        .expect("finalize");

    // Two racing issuances for the released slot under different issuance
    // IDs and benchmarks: exactly one may reserve it, and the loser's
    // benchmark mapping is never consumed (§2, §16.1).
    let digests = [
        sha256_hex(b"raced-assignment-1"),
        sha256_hex(b"raced-assignment-2"),
    ];
    let outcomes: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = [(200u64, "raced-benchmark-a"), (201u64, "raced-benchmark-b")]
            .into_iter()
            .zip(&digests)
            .map(|((n, benchmark), digest)| {
                let env = &env;
                scope.spawn(move || {
                    env.authed
                        .issue_and_register_assignment(
                            &AssignmentIssuance {
                                issuance_id: &uuid(n),
                                worker_id: &env.a.enrolled.worker_id,
                                slot_id: &env.slot_a,
                                benchmark_id: benchmark,
                                assignment_digest: digest,
                                network: "testnet",
                            },
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
        "exactly one racing issuance may reserve the slot: {outcomes:?}"
    );
    // The losing issuance consumed nothing: its issuance record was never
    // written, so its benchmark remains issuable elsewhere.
    let loser = if outcomes[0] { uuid(201) } else { uuid(200) };
    assert!(
        env.authed
            .identity()
            .assignment_issuance(&loser)
            .expect("read")
            .is_none(),
        "rejected issuance must not consume the benchmark mapping"
    );
}

#[test]
fn one_assignment_durably_accepts_exactly_one_package() {
    let env = setup("one-accepted");
    let pkg = build_package();
    let decl = declaration(&pkg, &uuid(90), &env.digest);
    let session = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("create");
    env.upload_all(&env.a, &session.upload_id, &pkg.bytes);
    let first = env
        .finalize(&env.a, &session.upload_id, &decl)
        .expect("first acceptance");
    assert!(first.fresh);
    assert_eq!(
        env.authed
            .pool()
            .release_count(&env.slot_a, 1)
            .expect("count"),
        1
    );

    // A second worker-chosen package_id under the already-accepted
    // assignment is refused at admission (§11: only one package can become
    // durably accepted for an assignment; §16.8).
    let second = declaration(&pkg, &uuid(95), &env.digest);
    let e = env
        .create_upload(&env.a, &env.grant.assignment_id, &second)
        .expect_err("second package refused");
    assert_eq!(e.pool_code(), Some(ErrorCode::AssignmentAlreadyAccepted));

    // Even driven against pool storage directly (bypassing the authed
    // admission), the acceptance saga itself refuses a second package for
    // the assignment before publishing anything.
    let scoped_second = scoped_pool_package_id(&env.grant.assignment_id, &uuid(95));
    let mut bypass = second.clone();
    bypass["package_id"] = json!(scoped_second);
    let pool_session = env
        .authed
        .pool()
        .create_upload(&bypass)
        .expect("pool-level session");
    let mut offset = 0u64;
    while (offset as usize) < pkg.bytes.len() {
        let end = (offset as usize + CHUNK).min(pkg.bytes.len());
        let chunk = &pkg.bytes[offset as usize..end];
        offset = env
            .authed
            .pool()
            .put_chunk(&pool_session.upload_id, offset, &sha256_hex(chunk), chunk)
            .expect("pool-level chunk");
    }
    let e = env
        .authed
        .pool()
        .finalize(&scoped_second)
        .expect_err("saga refuses second package");
    assert_eq!(e.code, ErrorCode::AssignmentAlreadyAccepted);

    // The slot was released exactly once, and a retried finalize of the
    // ACCEPTED package still returns the identical immutable receipt.
    assert_eq!(
        env.authed
            .pool()
            .release_count(&env.slot_a, 1)
            .expect("count"),
        1
    );
    let retry = env
        .finalize(&env.a, &session.upload_id, &decl)
        .expect("retry of accepted package");
    assert!(!retry.fresh);
    assert_eq!(first.receipt_json, retry.receipt_json);

    // The accepted package itself may still resume to recover its receipt.
    let resumed = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect("accepted package resumes");
    assert!(resumed.resumed);
}

#[test]
fn concurrent_finalizes_accept_exactly_one_package() {
    let env = setup("race-accept");
    let pkg = build_package();

    // Two fully uploaded packages for the same assignment — legal before
    // acceptance (§11: a package generation may be replaced with a new
    // package_id before the deadline).
    let decl1 = declaration(&pkg, &uuid(90), &env.digest);
    let decl2 = declaration(&pkg, &uuid(91), &env.digest);
    let session1 = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl1)
        .expect("upload 1");
    let session2 = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl2)
        .expect("upload 2");
    env.upload_all(&env.a, &session1.upload_id, &pkg.bytes);
    env.upload_all(&env.a, &session2.upload_id, &pkg.bytes);

    // Racing finalizes of DIFFERENT packages for one assignment: the
    // acceptance saga is serialized, so exactly one may commit (§11,
    // §16.8) and the slot is released exactly once (§16.10); the loser
    // gets the typed single-acceptance refusal.
    let outcomes: Vec<Result<(), Option<ErrorCode>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = [(&session1, &decl1), (&session2, &decl2)]
            .into_iter()
            .map(|(session, decl)| {
                let env = &env;
                scope.spawn(move || {
                    env.finalize(&env.a, &session.upload_id, decl)
                        .map(|_| ())
                        .map_err(|e| e.pool_code())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    let accepted = outcomes.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        accepted, 1,
        "exactly one racing finalize may commit: {outcomes:?}"
    );
    let loser = outcomes
        .iter()
        .find_map(|r| r.as_ref().err())
        .expect("one loser");
    assert_eq!(*loser, Some(ErrorCode::AssignmentAlreadyAccepted));
    assert_eq!(
        env.authed
            .pool()
            .release_count(&env.slot_a, 1)
            .expect("count"),
        1,
        "the slot must be released exactly once"
    );
}

#[test]
fn unqualified_slot_cannot_offer_or_receive_work() {
    let env = setup("unqualified");

    // Register a second slot for worker A and do NOT qualify it.
    let hb = env.signed(&env.a, "POST", "/member/v0/heartbeats", b"{}");
    let caller = env
        .authed
        .identity()
        .verify_request(&hb, b"{}", NOW)
        .expect("caller");
    let slot = env
        .authed
        .identity()
        .register_slot(&caller, &uuid(72), "cpu-1", NOW)
        .expect("unqualified slot");

    // No qualification, no offer (§16.2; §14 fail-closed)...
    let e = env
        .offer(&env.a, &slot.slot_id, 310)
        .expect_err("unqualified offer");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::SlotNotQualified));

    // ...and no assignment issuance either.
    let e = env
        .authed
        .issue_and_register_assignment(
            &AssignmentIssuance {
                issuance_id: &uuid(86),
                worker_id: &env.a.enrolled.worker_id,
                slot_id: &slot.slot_id,
                benchmark_id: BENCHMARK_ID_2,
                assignment_digest: &sha256_hex(b"unqualified-digest"),
                network: "testnet",
            },
            NOW,
        )
        .expect_err("unqualified issuance");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::SlotNotQualified));

    // The audited pool decision qualifies it, and the offer is recorded.
    env.authed
        .identity()
        .mark_slot_qualified(
            &slot.slot_id,
            "operator:spike-test",
            &uuid(77),
            &sha256_hex(b"qualification-spec-2"),
            NOW,
        )
        .expect("qualify");
    let offer = env.offer(&env.a, &slot.slot_id, 311).expect("offer");
    // Recording only — no admission policy exists, so NO_ACTION with its
    // machine reason and no lease or reservation (§6, architecture §6:
    // admission/reservation are Controller scope).
    assert_eq!(offer["offer_state"], "NO_ACTION");
    assert_eq!(offer["precommit_state"], "NOT_STARTED");
    assert_eq!(offer["reason_code"], "NO_APPLICABLE_ADMISSION_POLICY");
    assert!(offer.get("lease_expires_at").is_none());

    // The recorded offer is idempotent by offer_id (§5). (Reuse of an
    // offer_id for a different slot conflicting per §2 is covered at the
    // service level in the identity suite.)
    let retry = env.offer(&env.a, &slot.slot_id, 311).expect("offer retry");
    assert_eq!(retry["offer_state"], "NO_ACTION");
}

// ---------------------------------------------------------------------------
// Scope 5: the S3 path requires pool-ISSUED assignment identities
// ---------------------------------------------------------------------------

#[test]
fn upload_requires_a_pool_issued_assignment_identity() {
    let env = setup("issued-only");
    let pkg = build_package();

    // Register an assignment context directly in pool storage with
    // stand-in identities, exactly as the spike suite did — bypassing
    // issuance.
    let standin_digest = sha256_hex(b"spike-standin-assignment");
    let standin_assignment = spike::member::derived_uuid("spike-member:v0:standin:assignment");
    env.authed
        .pool()
        .register_assignment(&json!({
            "assignment_digest": standin_digest,
            "assignment_id": standin_assignment,
            "benchmark_id": BENCHMARK_ID_2,
            "slot_id": spike::member::derived_uuid("spike-member:v0:standin:slot"),
            "slot_generation": 1u64,
            "network": "testnet",
        }))
        .expect("stand-in registration");

    // The authed path refuses it: no issued identity, no upload session —
    // even for a correctly signed request from an enrolled worker.
    let decl = declaration(&pkg, &uuid(90), &standin_digest);
    let e = env
        .create_upload(&env.a, &standin_assignment, &decl)
        .expect_err("stand-in assignment");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));

    // A declaration whose digest belongs to a DIFFERENT assignment than the
    // one named in the signed path is refused too.
    let decl = declaration(&pkg, &uuid(90), &standin_digest);
    let e = env
        .create_upload(&env.a, &env.grant.assignment_id, &decl)
        .expect_err("digest not bound to issued assignment");
    assert_eq!(e.identity_code(), Some(IdentityErrorCode::UnknownResource));
}
