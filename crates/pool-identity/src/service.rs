//! The pool-side identity service: enrollment tickets, worker enrollment,
//! signed-request verification, rotation, revocation, recovery, and issuance
//! of slot and assignment identities.
//!
//! Contract: `docs/member_protocol.md` §2–§3 and §5; ownership:
//! `docs/architecture.md` §3 (Pool API) and §6 (idempotency guards). The
//! caller supplies `now` (Unix seconds); the service never reads a clock.
//!
//! Pool-issued IDs are opaque UUIDs (§2). They are derived with a keyed
//! BLAKE3 hash of the issuing event's idempotency key under the store's
//! random server key: unpredictable to clients, but a crash-interrupted
//! issuance retried with the same idempotency key converges on the same
//! identity instead of orphaning half-written state (architecture §6:
//! "Enrollment or rotation ID plus request hash").

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::keys::{
    decode_public_key, enrollment_signing_string, recovery_signing_string, request_signing_string,
    rotation_signing_string, sha256_hex, verify_b64url,
};
use crate::store::Store;
use crate::wire::{
    EnrollRequest, EnrollResponse, RotateCredentialRequest, RotateCredentialResponse,
    SignedRequest, VerifiedWorker,
};
use crate::{IdentityError, IdentityErrorCode, IdentityResult, rfc3339_utc};

pub const PROTOCOL_VERSION: &str = "0.1.0";
pub const PACKAGE_FORMAT: &str = "proof-material-v1";

/// Enrollment and recovery tickets expire after 15 minutes (§3.1, §3.3).
pub const TICKET_TTL_SECS: u64 = 15 * 60;
/// Signed-request timestamps are accepted within 300 seconds of server time
/// (§3.2).
pub const REQUEST_FRESHNESS_SECS: u64 = 300;
/// Server-declared rotation grace period; §3.3 caps it at 10 minutes.
pub const ROTATION_GRACE_SECS: u64 = 10 * 60;

const PURPOSE_ENROLLMENT: &str = "WORKER_ENROLLMENT";
const PURPOSE_RECOVERY: &str = "WORKER_RECOVERY";

fn err(code: IdentityErrorCode, detail: impl Into<String>) -> IdentityError {
    IdentityError::new(code, detail)
}

fn storage(detail: impl std::fmt::Display) -> IdentityError {
    err(IdentityErrorCode::Storage, detail.to_string())
}

/// Member-supplied identifiers never become paths unvalidated (CLAUDE.md,
/// architecture §8.2). Pool IDs and idempotency keys must be exact lowercase
/// UUIDs (§2).
fn validate_uuid(kind: &str, id: &str) -> IdentityResult<()> {
    let bytes = id.as_bytes();
    let ok = bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        });
    if ok {
        Ok(())
    } else {
        Err(err(
            IdentityErrorCode::UnknownResource,
            format!("invalid {kind} identifier"),
        ))
    }
}

/// Spike upload IDs are 32 lowercase hex characters; accept the same bounded
/// charset the spike pool accepts for path-safe identifiers.
fn validate_token(kind: &str, id: &str) -> IdentityResult<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(err(
            IdentityErrorCode::UnknownResource,
            format!("invalid {kind} identifier"),
        ))
    }
}

/// Schema length bounds (`api.schema.json`), enforced at the service
/// boundary so the durable store never holds unbounded member-controlled
/// strings regardless of what a future HTTP layer forgets to check.
fn validate_bounded_str(kind: &str, value: &str, min: usize, max: usize) -> IdentityResult<()> {
    if value.len() < min || value.len() > max {
        return Err(err(
            IdentityErrorCode::UnknownResource,
            format!("{kind} length outside schema bounds"),
        ));
    }
    Ok(())
}

/// Schema array bounds: 1–16 items, each a non-empty string no longer than
/// `max_item` bytes.
fn validate_bounded_list(kind: &str, values: &[String], max_item: usize) -> IdentityResult<()> {
    if values.is_empty() || values.len() > 16 {
        return Err(err(
            IdentityErrorCode::UnknownResource,
            format!("{kind} item count outside schema bounds"),
        ));
    }
    for value in values {
        validate_bounded_str(kind, value, 1, max_item)?;
    }
    Ok(())
}

fn field_str<'v>(v: &'v Value, key: &str) -> IdentityResult<&'v str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| storage(format!("missing string field '{key}'")))
}

fn field_u64(v: &Value, key: &str) -> IdentityResult<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| storage(format!("missing u64 field '{key}'")))
}

/// A one-time bearer ticket. `secret` is returned exactly once to the
/// authenticated member account; the pool retains only a keyed hash (§3.1).
#[derive(Debug, Clone)]
pub struct IssuedTicket {
    pub secret: String,
    pub expires_at: u64,
}

/// A pool-issued slot identity bound to one worker (§2: `slot_id` issuer is
/// the pool; `client_slot_key` is the worker-local identity).
///
/// `generation` is the §2 slot REGISTRATION generation — it changes only on
/// reconfiguration, which is out of scope for this bridge, so it stays 1.
/// (The spike pool's `slot_generation` counter is a different thing: an
/// occupancy epoch used for release accounting. Unifying the two under the
/// §2 model belongs to the production slot slice, checklist §10 step 2.)
///
/// `qualification` is `UNQUALIFIED` until qualification succeeds for the
/// exact generation recorded in `qualified_generation` (§6); a slot whose
/// current generation is not the qualified one may not offer capacity or
/// receive work (§16.2, fail-closed per §14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotGrant {
    pub slot_id: String,
    pub worker_id: String,
    pub client_slot_key: String,
    pub generation: u64,
    pub qualification: String,
    pub qualified_generation: Option<u64>,
}

impl SlotGrant {
    /// Qualified for the slot's exact current registration generation
    /// (§16.2).
    pub fn is_qualified(&self) -> bool {
        self.qualification == "QUALIFIED" && self.qualified_generation == Some(self.generation)
    }
}

/// A pool-issued assignment identity bound to the enrolled member through
/// the §2 ownership chain `assignment_id -> slot_id -> worker_id ->
/// member_id`, never derived from the benchmark id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentGrant {
    pub assignment_id: String,
    pub member_id: String,
    pub worker_id: String,
    pub slot_id: String,
    pub benchmark_id: String,
}

/// Ownership record for one upload session: `upload -> package ->
/// assignment -> worker` (§2), used to scope chunk/finalize/status requests.
///
/// `package_id` is the worker-chosen §2 identity; `pool_package_id` is the
/// assignment-scoped key the storage layer uses, so one worker's chosen ID
/// can never reach (or be disclosed by) another worker's namespace (§3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadBinding {
    pub upload_id: String,
    pub package_id: String,
    pub pool_package_id: String,
    pub assignment_id: String,
    pub worker_id: String,
}

/// Every fact of one assignment issuance. ALL fields sit inside the
/// issuance idempotency boundary (architecture §6, §13 invariant 6):
/// reusing an issuance ID with any changed field is a conflict, never a
/// silently returned prior grant (§14, §16.5).
#[derive(Debug, Clone, Copy)]
pub struct AssignmentFacts<'a> {
    pub worker_id: &'a str,
    pub slot_id: &'a str,
    pub benchmark_id: &'a str,
    pub assignment_digest: &'a str,
    pub network: &'a str,
}

/// One durably recorded capacity-offer command, idempotent by
/// `(worker_id, offer_id)` (§5). Recording is the only offer mutation the
/// Pool API owns; admission, queueing, slot reservation, and leases are
/// Controller scope (architecture §6) deferred to the production offer
/// slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferRecord {
    pub offer_id: String,
    pub worker_id: String,
    pub slot_id: String,
    pub recorded_at: u64,
}

/// Why a worker was revoked (§3.3): accidental revocation may be undone by
/// account recovery; a security action requires an explicit, audited pool
/// decision (`reinstate_worker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationReason {
    Accidental,
    Security,
}

impl RevocationReason {
    fn as_str(self) -> &'static str {
        match self {
            RevocationReason::Accidental => "ACCIDENTAL",
            RevocationReason::Security => "SECURITY",
        }
    }
}

/// Account-recovery consumption per §3.3: the proof of new-key possession
/// signs the `TIG-POOL-RECOVERY-V1` string, idempotent by
/// `recovery_request_id`, the ticket bound to its exact worker. The HTTP
/// route and schema remain outside protocol 0.1.0 (§17), so this struct is
/// a pool-internal service boundary, not a wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverWorkerRequest {
    pub recovery_request_id: String,
    pub worker_id: String,
    pub recovery_ticket: String,
    pub new_ed25519_public_key: String,
    pub new_key_proof: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverWorkerResponse {
    pub recovery_request_id: String,
    pub worker_id: String,
    pub new_credential_id: String,
    pub revoked_credential_ids: Vec<String>,
    pub server_time: String,
}

pub struct IdentityService {
    store: Store,
    server_key: [u8; 32],
    /// Serializes every check-then-write sequence (ticket claims, benchmark
    /// uniqueness, idempotency records) so single-use and uniqueness
    /// invariants hold under concurrent requests. The production Pool API
    /// gets this from database transactions (architecture §6); the doc store
    /// gets it from one writer lock.
    mutate: std::sync::Mutex<()>,
}

impl IdentityService {
    /// Open (or initialize) the identity store at `root`. A random 32-byte
    /// server key is created on first open and persisted as local runtime
    /// state; it keys ticket hashes and pool-ID derivation and must never be
    /// committed or logged.
    pub fn open(root: impl Into<std::path::PathBuf>) -> IdentityResult<IdentityService> {
        let store = Store::open(root)?;
        let server_key = match store.read_doc("server-key.json")? {
            Some(doc) => {
                let hex = field_str(&doc, "server_key_hex")?;
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or(""), 16))
                    .collect::<Result<Vec<u8>, _>>()
                    .map_err(|e| storage(format!("corrupt server key: {e}")))?;
                bytes
                    .try_into()
                    .map_err(|_| storage("server key is not 32 bytes"))?
            }
            None => {
                let mut key = [0u8; 32];
                getrandom::fill(&mut key).map_err(|e| storage(format!("getrandom: {e}")))?;
                let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
                store.write_doc("server-key.json", &json!({ "server_key_hex": hex }))?;
                key
            }
        };
        Ok(IdentityService {
            store,
            server_key,
            mutate: std::sync::Mutex::new(()),
        })
    }

    /// Take the writer lock; a poisoned lock is recovered because the store
    /// itself is crash-safe (every commit is one atomic rename).
    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A pool-issued RFC 4122-shaped UUID: keyed BLAKE3 of the issuing
    /// event's label under the server key (opaque per §2, crash-idempotent
    /// per architecture §6).
    fn issue_uuid(&self, label: &str) -> String {
        let digest = blake3::keyed_hash(&self.server_key, label.as_bytes());
        let mut b = [0u8; 16];
        b.copy_from_slice(&digest.as_bytes()[..16]);
        b[6] = 0x40 | (b[6] & 0x0f);
        b[8] = 0x80 | (b[8] & 0x3f);
        let s: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &s[0..8],
            &s[8..12],
            &s[12..16],
            &s[16..20],
            &s[20..32]
        )
    }

    fn ticket_hash(&self, secret: &str) -> String {
        blake3::keyed_hash(&self.server_key, secret.as_bytes())
            .to_hex()
            .to_string()
    }

    fn audit(&self, event: &str, at: u64, details: Value) -> IdentityResult<()> {
        self.store.append_jsonl(
            "audit",
            &json!({ "event": event, "at": at, "details": details }),
        )
    }

    /// The append-only audit ledger (§5.1 checklist: audit events for
    /// enrollment, rotation, revocation, and rejected security events).
    pub fn audit_entries(&self) -> IdentityResult<Vec<Value>> {
        self.store.read_jsonl("audit")
    }

    // -- member accounts and tickets (outside the wire protocol, §3.1) ------

    /// Register a member account. Account login and the interface that
    /// invokes this are outside the member protocol (§3.1); `member_ref` is
    /// the pool-side account handle. Idempotent per reference.
    pub fn create_member(&self, member_ref: &str, now: u64) -> IdentityResult<String> {
        let member_id = self.issue_uuid(&format!("member:{member_ref}"));
        let doc = json!({ "member_id": member_id, "created_at": now });
        match self.store.read_doc(&format!("members/{member_id}.json"))? {
            Some(_) => Ok(member_id),
            None => {
                self.store
                    .write_doc(&format!("members/{member_id}.json"), &doc)?;
                Ok(member_id)
            }
        }
    }

    fn create_ticket(
        &self,
        purpose: &str,
        member_id: &str,
        worker_id: Option<&str>,
        now: u64,
    ) -> IdentityResult<IssuedTicket> {
        validate_uuid("member_id", member_id)?;
        if self
            .store
            .read_doc(&format!("members/{member_id}.json"))?
            .is_none()
        {
            return Err(err(IdentityErrorCode::UnknownResource, "unknown member"));
        }
        // ≥256 bits of entropy, single-use bearer value (§3.1).
        let mut secret_bytes = [0u8; 32];
        getrandom::fill(&mut secret_bytes).map_err(|e| storage(format!("getrandom: {e}")))?;
        let secret = crate::keys::b64url(&secret_bytes);
        let expires_at = now + TICKET_TTL_SECS;
        let doc = json!({
            "purpose": purpose,
            "member_id": member_id,
            "worker_id": worker_id,
            "expires_at": expires_at,
        });
        // Stored only under the keyed hash of the secret (§3.1).
        self.store
            .write_doc(&format!("tickets/{}.json", self.ticket_hash(&secret)), &doc)?;
        Ok(IssuedTicket { secret, expires_at })
    }

    /// One-time `WORKER_ENROLLMENT` ticket bound to one member (§3.1).
    pub fn create_enrollment_ticket(
        &self,
        member_id: &str,
        now: u64,
    ) -> IdentityResult<IssuedTicket> {
        let _write = self.guard();
        self.create_ticket(PURPOSE_ENROLLMENT, member_id, None, now)
    }

    /// One-time `WORKER_RECOVERY` ticket bound to one existing worker (§3.3).
    ///
    /// Refused for a security-revoked worker: §3.3 lets account recovery
    /// undo an *accidental* revocation only; a security action requires an
    /// explicit pool decision (`reinstate_worker`) first.
    pub fn create_recovery_ticket(
        &self,
        member_id: &str,
        worker_id: &str,
        now: u64,
    ) -> IdentityResult<IssuedTicket> {
        let _write = self.guard();
        validate_uuid("worker_id", worker_id)?;
        let worker = self
            .store
            .read_doc(&format!("workers/{worker_id}.json"))?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown worker"))?;
        if field_str(&worker, "member_id")? != member_id {
            return Err(err(
                IdentityErrorCode::UnknownResource,
                "worker does not belong to member",
            ));
        }
        if worker.get("revoked_reason").and_then(Value::as_str)
            == Some(RevocationReason::Security.as_str())
        {
            self.audit(
                "recovery_ticket_refused",
                now,
                json!({ "worker_id": worker_id, "reason": "security_revocation" }),
            )?;
            return Err(err(
                IdentityErrorCode::TicketRejected,
                "security-revoked worker: recovery requires an explicit operator decision",
            ));
        }
        self.create_ticket(PURPOSE_RECOVERY, member_id, Some(worker_id), now)
    }

    /// Validate and claim a ticket for one consuming request ID. The claim is
    /// the atomic commit point of consumption: a crash between claim and
    /// result record leaves the ticket burned for every other request but
    /// lets an identical retry (same request ID) complete.
    fn claim_ticket(
        &self,
        purpose: &str,
        secret: &str,
        bound_worker: Option<&str>,
        claimed_by: &str,
        now: u64,
    ) -> IdentityResult<Value> {
        let rejected = || err(IdentityErrorCode::TicketRejected, "ticket rejected");
        let hash = self.ticket_hash(secret);
        let ticket = self
            .store
            .read_doc(&format!("tickets/{hash}.json"))?
            .ok_or_else(rejected)?;
        if field_str(&ticket, "purpose")? != purpose {
            return Err(rejected());
        }
        if now >= field_u64(&ticket, "expires_at")? {
            return Err(rejected());
        }
        if let Some(worker_id) = bound_worker
            && ticket.get("worker_id").and_then(Value::as_str) != Some(worker_id)
        {
            return Err(rejected());
        }
        let claim_rel = format!("ticket-claims/{hash}.json");
        match self.store.read_doc(&claim_rel)? {
            Some(claim) if field_str(&claim, "claimed_by")? == claimed_by => Ok(ticket),
            Some(_) => Err(rejected()),
            None => {
                self.store
                    .write_doc(&claim_rel, &json!({ "claimed_by": claimed_by, "at": now }))?;
                Ok(ticket)
            }
        }
    }

    // -- enrollment (§3.1, route `POST /member/v0/enroll`) ------------------

    pub fn enroll(&self, req: &EnrollRequest, now: u64) -> IdentityResult<EnrollResponse> {
        let _write = self.guard();
        validate_uuid("enrollment_request_id", &req.enrollment_request_id)?;
        // The schema bounds (`api.schema.json#/$defs/EnrollRequest`) are
        // enforced here, not delegated to a future HTTP layer: the durable
        // store never holds unbounded attacker-controlled values.
        validate_bounded_str("enrollment_ticket", &req.enrollment_ticket, 43, 512)?;
        validate_bounded_str("worker_name", &req.worker_name, 1, 128)?;
        validate_bounded_str("member_agent_version", &req.member_agent_version, 1, 64)?;
        validate_bounded_list(
            "supported_protocol_versions",
            &req.supported_protocol_versions,
            32,
        )?;
        validate_bounded_list(
            "supported_package_formats",
            &req.supported_package_formats,
            64,
        )?;
        let request_hash = sha256_hex(serde_json::to_string(req).map_err(storage)?.as_bytes());
        let record_rel = format!("enrollments/{}.json", req.enrollment_request_id);

        // Idempotent by enrollment ID + request hash (§3.1, architecture §6):
        // an identical retry returns the recorded response; any changed field
        // for that ID is a conflict.
        if let Some(record) = self.store.read_doc(&record_rel)? {
            if field_str(&record, "request_sha256")? != request_hash {
                return Err(err(
                    IdentityErrorCode::IdempotencyConflict,
                    "enrollment_request_id reused with different content",
                ));
            }
            let response = record
                .get("response")
                .cloned()
                .ok_or_else(|| storage("enrollment record missing response"))?;
            return serde_json::from_value(response).map_err(storage);
        }

        // Version negotiation (§4): an exact common version is required; no
        // common version is `INCOMPATIBLE_PROTOCOL` with no state change.
        if !req
            .supported_protocol_versions
            .iter()
            .any(|v| v == PROTOCOL_VERSION)
            || !req
                .supported_package_formats
                .iter()
                .any(|f| f == PACKAGE_FORMAT)
        {
            return Err(err(
                IdentityErrorCode::IncompatibleProtocol,
                "no common protocol version or package format",
            ));
        }

        // Proof of key possession (§3.1) before the ticket is consumed.
        let public_key = decode_public_key(&req.ed25519_public_key)?;
        let proof_msg = enrollment_signing_string(
            &req.enrollment_request_id,
            &req.enrollment_ticket,
            &req.ed25519_public_key,
        );
        if let Err(e) = verify_b64url(&public_key, &proof_msg, &req.ed25519_key_proof) {
            self.audit(
                "enroll_rejected",
                now,
                json!({ "enrollment_request_id": req.enrollment_request_id, "reason": "bad_key_proof" }),
            )?;
            return Err(e);
        }

        // Consume the single-use ticket; consuming it and creating the
        // worker are one logical transaction (§3.1).
        let ticket = match self.claim_ticket(
            PURPOSE_ENROLLMENT,
            &req.enrollment_ticket,
            None,
            &req.enrollment_request_id,
            now,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.audit(
                    "enroll_rejected",
                    now,
                    json!({ "enrollment_request_id": req.enrollment_request_id, "reason": "ticket" }),
                )?;
                return Err(e);
            }
        };
        let member_id = field_str(&ticket, "member_id")?.to_owned();

        let worker_id = self.issue_uuid(&format!("worker:{}", req.enrollment_request_id));
        let credential_id = self.issue_uuid(&format!("credential:{}", req.enrollment_request_id));
        self.write_credential(&credential_id, &worker_id, &req.ed25519_public_key, now)?;
        self.store.write_doc(
            &format!("workers/{worker_id}.json"),
            &json!({
                "worker_id": worker_id,
                "member_id": member_id,
                "worker_name": req.worker_name,
                "status": "ACTIVE",
                "protocol_version": PROTOCOL_VERSION,
                "package_format": PACKAGE_FORMAT,
                "created_at": now,
            }),
        )?;

        let response = EnrollResponse {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            enrollment_request_id: req.enrollment_request_id.clone(),
            package_format: PACKAGE_FORMAT.to_owned(),
            member_id: member_id.clone(),
            worker_id: worker_id.clone(),
            credential_id,
            worker_status: "ACTIVE".to_owned(),
            server_time: rfc3339_utc(now),
        };
        // Commit point: the durable enrollment record makes the retry path
        // return this exact response.
        self.store.write_doc(
            &record_rel,
            &json!({
                "request_sha256": request_hash,
                "response": serde_json::to_value(&response).map_err(storage)?,
            }),
        )?;
        self.audit(
            "worker_enrolled",
            now,
            json!({ "member_id": member_id, "worker_id": worker_id }),
        )?;
        Ok(response)
    }

    fn write_credential(
        &self,
        credential_id: &str,
        worker_id: &str,
        public_key: &str,
        now: u64,
    ) -> IdentityResult<()> {
        self.store.write_doc(
            &format!("credentials/{credential_id}.json"),
            &json!({
                "credential_id": credential_id,
                "worker_id": worker_id,
                "public_key": public_key,
                "revoked": false,
                "revokes_at": Value::Null,
                "created_at": now,
            }),
        )?;
        // Enumeration marker for recovery and worker-wide revocation.
        self.store.write_doc(
            &format!("worker-credentials/{worker_id}/{credential_id}.json"),
            &json!({ "credential_id": credential_id }),
        )
    }

    // -- signed request verification (§3.2) ---------------------------------

    pub fn verify_request(
        &self,
        sr: &SignedRequest,
        body: &[u8],
        now: u64,
    ) -> IdentityResult<VerifiedWorker> {
        // Serialized for the replay-record check-then-write below.
        let _write = self.guard();
        let denied = |detail: &str| err(IdentityErrorCode::NotAuthenticated, detail);
        if sr.protocol_version != PROTOCOL_VERSION {
            return Err(err(
                IdentityErrorCode::IncompatibleProtocol,
                "unsupported protocol version",
            ));
        }
        validate_uuid("worker_id", &sr.worker_id)
            .and(validate_uuid("credential_id", &sr.credential_id))
            .and(validate_uuid("request_id", &sr.request_id))
            .map_err(|_| denied("malformed identity header"))?;

        // Body hash and signature are verified before anything else consumes
        // the request (§3.2).
        if sha256_hex(body) != sr.body_sha256 {
            return Err(denied("X-Body-SHA256 does not match request body"));
        }
        let credential = self
            .store
            .read_doc(&format!("credentials/{}.json", sr.credential_id))?
            .ok_or_else(|| denied("unknown credential"))?;
        if field_str(&credential, "worker_id")? != sr.worker_id {
            self.audit(
                "auth_rejected",
                now,
                json!({ "credential_id": sr.credential_id, "reason": "worker_mismatch" }),
            )?;
            return Err(denied("credential does not authorize this worker"));
        }
        let worker = self
            .store
            .read_doc(&format!("workers/{}.json", sr.worker_id))?
            .ok_or_else(|| denied("unknown worker"))?;

        let signing_string = request_signing_string(
            &sr.method,
            &sr.path,
            &sr.protocol_version,
            &sr.worker_id,
            &sr.credential_id,
            &sr.request_id,
            sr.request_timestamp,
            &sr.body_sha256,
        );
        let public_key = decode_public_key(field_str(&credential, "public_key")?)?;
        verify_b64url(&public_key, &signing_string, &sr.signature)?;

        // Credential and worker state take effect on the next request
        // (§3.3): checked on every verification.
        let revoked = credential
            .get("revoked")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let grace_expired = credential
            .get("revokes_at")
            .and_then(Value::as_u64)
            .is_some_and(|t| now >= t);
        if revoked || grace_expired {
            self.audit(
                "auth_rejected",
                now,
                json!({ "credential_id": sr.credential_id, "reason": "credential_revoked" }),
            )?;
            return Err(denied("credential revoked"));
        }
        if field_str(&worker, "status")? != "ACTIVE" {
            self.audit(
                "auth_rejected",
                now,
                json!({ "worker_id": sr.worker_id, "reason": "worker_revoked" }),
            )?;
            return Err(denied("worker revoked"));
        }

        // Freshness: within 300 seconds of server time (§3.2).
        if now.abs_diff(sr.request_timestamp) > REQUEST_FRESHNESS_SECS {
            return Err(err(
                IdentityErrorCode::StaleTimestamp,
                "request timestamp outside the accepted window",
            ));
        }

        // Replay protection: each `(credential_id, request_id)` is
        // remembered; reuse with different signed bytes is rejected and
        // audited (§3.2). An identical retry of the same HTTP attempt is
        // accepted; application idempotency keys make it harmless.
        let signed_hash = sha256_hex(signing_string.as_bytes());
        let replay_rel = format!("replay/{}/{}.json", sr.credential_id, sr.request_id);
        match self.store.read_doc(&replay_rel)? {
            Some(seen) if field_str(&seen, "signed_sha256")? != signed_hash => {
                self.audit(
                    "request_id_reuse_rejected",
                    now,
                    json!({ "credential_id": sr.credential_id, "request_id": sr.request_id }),
                )?;
                return Err(err(
                    IdentityErrorCode::ReplayRejected,
                    "request_id reused with different signed bytes",
                ));
            }
            Some(_) => {}
            None => self.store.write_doc(
                &replay_rel,
                &json!({ "signed_sha256": signed_hash, "seen_at": now }),
            )?,
        }

        // `member_id` comes from the stored worker binding, never a client
        // claim (§3.2).
        Ok(VerifiedWorker {
            member_id: field_str(&worker, "member_id")?.to_owned(),
            worker_id: sr.worker_id.clone(),
            credential_id: sr.credential_id.clone(),
        })
    }

    // -- rotation (§3.3, route `POST .../credentials/rotate`) ---------------

    /// Ordinary rotation: the request itself was signed by the current key
    /// (`caller` is the verified signer); the new key proves possession via
    /// the §3.3 rotation string.
    pub fn rotate(
        &self,
        caller: &VerifiedWorker,
        path_worker_id: &str,
        req: &RotateCredentialRequest,
        now: u64,
    ) -> IdentityResult<RotateCredentialResponse> {
        let _write = self.guard();
        // Any worker ID in the path must equal the signed header (§3.2);
        // the route is `POST /member/v0/workers/{worker_id}/credentials/
        // rotate`. Rejected with the uniform non-disclosing code.
        if path_worker_id != caller.worker_id {
            self.audit(
                "resource_access_rejected",
                now,
                json!({ "worker_id": caller.worker_id, "kind": "rotate_path_worker" }),
            )?;
            return Err(err(IdentityErrorCode::UnknownResource, "unknown resource"));
        }
        if req.protocol_version != PROTOCOL_VERSION {
            return Err(err(
                IdentityErrorCode::IncompatibleProtocol,
                "unsupported protocol version",
            ));
        }
        validate_uuid("rotation_id", &req.rotation_id)?;
        // Worker-chosen idempotency keys are scoped by the verified caller
        // (§3.2): another worker's use of the same rotation ID is simply
        // absent for this caller — no cross-worker disclosure or squatting.
        let record_rel = format!("rotations/{}/{}.json", caller.worker_id, req.rotation_id);
        if let Some(record) = self.store.read_doc(&record_rel)? {
            // Repeating the same rotation ID and public key returns the same
            // result; a different key for the same ID is a conflict (§3.3).
            if field_str(&record, "worker_id")? == caller.worker_id
                && field_str(&record, "new_public_key")? == req.new_ed25519_public_key
            {
                let response = record
                    .get("response")
                    .cloned()
                    .ok_or_else(|| storage("rotation record missing response"))?;
                return serde_json::from_value(response).map_err(storage);
            }
            return Err(err(
                IdentityErrorCode::IdempotencyConflict,
                "rotation_id reused with a different key",
            ));
        }

        let new_key = decode_public_key(&req.new_ed25519_public_key)?;
        let proof_msg = rotation_signing_string(
            &caller.worker_id,
            &req.rotation_id,
            &req.new_ed25519_public_key,
        );
        verify_b64url(&new_key, &proof_msg, &req.new_key_proof)?;

        // Derivation labels include the worker so worker-chosen keys can
        // never collide across workers (§3.2 scoping).
        let new_credential_id = self.issue_uuid(&format!(
            "rotation:{}:{}",
            caller.worker_id, req.rotation_id
        ));
        self.write_credential(
            &new_credential_id,
            &caller.worker_id,
            &req.new_ed25519_public_key,
            now,
        )?;

        // The old credential stays valid for the declared grace period, then
        // becomes REVOKED (§3.3, no longer than 10 minutes).
        let revokes_at = now + ROTATION_GRACE_SECS;
        let old_rel = format!("credentials/{}.json", caller.credential_id);
        let mut old = self
            .store
            .read_doc(&old_rel)?
            .ok_or_else(|| storage("rotating credential disappeared"))?;
        old["revokes_at"] = json!(revokes_at);
        self.store.write_doc(&old_rel, &old)?;

        let response = RotateCredentialResponse {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            rotation_id: req.rotation_id.clone(),
            worker_id: caller.worker_id.clone(),
            new_credential_id: new_credential_id.clone(),
            old_credential_revokes_at: rfc3339_utc(revokes_at),
            server_time: rfc3339_utc(now),
        };
        self.store.write_doc(
            &record_rel,
            &json!({
                "worker_id": caller.worker_id,
                "new_public_key": req.new_ed25519_public_key,
                "response": serde_json::to_value(&response).map_err(storage)?,
            }),
        )?;
        self.audit(
            "credential_rotated",
            now,
            json!({
                "worker_id": caller.worker_id,
                "old_credential_id": caller.credential_id,
                "new_credential_id": new_credential_id,
            }),
        )?;
        Ok(response)
    }

    // -- revocation and recovery (§3.3, member/operator actions) ------------

    /// Revoke one credential. Takes effect on the next request; erases no
    /// state and reassigns no benchmark (§3.3).
    /// Guard (architecture §6 "Recover/revoke worker credential": command
    /// ID, worker binding, and current credential state): the credential
    /// must belong to the named worker, and the acting principal plus its
    /// command ID are recorded in the audit event. A credential outside the
    /// named worker's binding is rejected with the uniform non-disclosing
    /// code (§3.2).
    pub fn revoke_credential(
        &self,
        worker_id: &str,
        credential_id: &str,
        actor: &str,
        command_id: &str,
        now: u64,
    ) -> IdentityResult<()> {
        let _write = self.guard();
        validate_uuid("worker_id", worker_id)?;
        validate_uuid("credential_id", credential_id)?;
        validate_uuid("command_id", command_id)?;
        let command = format!("revoke_credential\n{worker_id}\n{credential_id}");
        if self.check_command(command_id, &command)? {
            return Ok(());
        }
        let rel = format!("credentials/{credential_id}.json");
        let mut doc = self
            .store
            .read_doc(&rel)?
            .filter(|doc| doc.get("worker_id").and_then(Value::as_str) == Some(worker_id))
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown resource"))?;
        doc["revoked"] = json!(true);
        self.store.write_doc(&rel, &doc)?;
        self.record_command(command_id, &command)?;
        self.audit(
            "credential_revoked",
            now,
            json!({
                "worker_id": worker_id,
                "credential_id": credential_id,
                "actor": actor,
                "command_id": command_id,
            }),
        )
    }

    /// Idempotency check for an operator/account command (architecture §6:
    /// command-ID guard). `true` means this exact command already executed
    /// (recorded no-op); a different payload under the same ID conflicts.
    fn check_command(&self, command_id: &str, content: &str) -> IdentityResult<bool> {
        validate_uuid("command_id", command_id)?;
        match self
            .store
            .read_doc(&format!("worker-commands/{command_id}.json"))?
        {
            Some(rec) if field_str(&rec, "request_sha256")? == sha256_hex(content.as_bytes()) => {
                Ok(true)
            }
            Some(_) => Err(err(
                IdentityErrorCode::IdempotencyConflict,
                "command_id reused with different content",
            )),
            None => Ok(false),
        }
    }

    /// Record the executed command AFTER its idempotent state change, so a
    /// crash between the two re-applies the change on retry instead of
    /// silently skipping it.
    fn record_command(&self, command_id: &str, content: &str) -> IdentityResult<()> {
        self.store.write_doc(
            &format!("worker-commands/{command_id}.json"),
            &json!({ "request_sha256": sha256_hex(content.as_bytes()) }),
        )
    }

    /// Revoke the whole worker: prevents new offers, heartbeats, events, and
    /// uploads; historical ownership is unchanged (§2, §3.3). The reason and
    /// actor are recorded: only an ACCIDENTAL revocation may later be undone
    /// by account recovery; a SECURITY revocation requires an explicit
    /// `reinstate_worker` decision first (§3.3).
    pub fn revoke_worker(
        &self,
        worker_id: &str,
        reason: RevocationReason,
        actor: &str,
        command_id: &str,
        now: u64,
    ) -> IdentityResult<()> {
        let _write = self.guard();
        validate_uuid("worker_id", worker_id)?;
        let command = format!("revoke_worker\n{worker_id}\n{}", reason.as_str());
        if self.check_command(command_id, &command)? {
            return Ok(());
        }
        let rel = format!("workers/{worker_id}.json");
        let mut doc = self
            .store
            .read_doc(&rel)?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown worker"))?;
        // The revocation reason is MONOTONIC: a stored SECURITY reason can
        // never be laundered into a member-recoverable ACCIDENTAL one by a
        // later command — only the explicit, audited `reinstate_worker`
        // decision clears it (§3.3: "if it was a security action, the pool
        // decides explicitly").
        if doc.get("revoked_reason").and_then(Value::as_str)
            == Some(RevocationReason::Security.as_str())
            && reason == RevocationReason::Accidental
        {
            self.audit(
                "revocation_downgrade_refused",
                now,
                json!({ "worker_id": worker_id, "actor": actor, "command_id": command_id }),
            )?;
            return Err(err(
                IdentityErrorCode::CommandRefused,
                "security revocation cannot be downgraded; reinstate_worker is required",
            ));
        }
        doc["status"] = json!("REVOKED");
        doc["revoked_reason"] = json!(reason.as_str());
        doc["revoked_by"] = json!(actor);
        self.store.write_doc(&rel, &doc)?;
        self.record_command(command_id, &command)?;
        self.audit(
            "worker_revoked",
            now,
            json!({
                "worker_id": worker_id,
                "reason": reason.as_str(),
                "actor": actor,
                "command_id": command_id,
            }),
        )
    }

    /// The explicit, audited pool decision that clears a SECURITY
    /// revocation (§3.3: "if it was a security action, the pool decides
    /// explicitly"). Restores the worker to ACTIVE; credentials revoked
    /// separately stay revoked. Idempotent by `command_id` (architecture
    /// §6 guard), so exactly one approved command backs the state change.
    pub fn reinstate_worker(
        &self,
        worker_id: &str,
        actor: &str,
        command_id: &str,
        now: u64,
    ) -> IdentityResult<()> {
        let _write = self.guard();
        validate_uuid("worker_id", worker_id)?;
        let command = format!("reinstate_worker\n{worker_id}");
        if self.check_command(command_id, &command)? {
            return Ok(());
        }
        let rel = format!("workers/{worker_id}.json");
        let mut doc = self
            .store
            .read_doc(&rel)?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown worker"))?;
        doc["status"] = json!("ACTIVE");
        doc["revoked_reason"] = Value::Null;
        doc["revoked_by"] = Value::Null;
        self.store.write_doc(&rel, &doc)?;
        self.record_command(command_id, &command)?;
        self.audit(
            "worker_reinstated",
            now,
            json!({ "worker_id": worker_id, "actor": actor, "command_id": command_id }),
        )
    }

    /// Consume a `WORKER_RECOVERY` ticket: attach a new key to the existing
    /// worker and revoke every old credential, preserving the worker's
    /// assignments and ownership (§3.3). Restores an ACCIDENTALLY revoked
    /// worker to ACTIVE; a SECURITY revocation is never undone here — it
    /// requires the explicit `reinstate_worker` decision first (§3.3).
    pub fn recover_worker(
        &self,
        req: &RecoverWorkerRequest,
        now: u64,
    ) -> IdentityResult<RecoverWorkerResponse> {
        let _write = self.guard();
        validate_uuid("recovery_request_id", &req.recovery_request_id)?;
        validate_uuid("worker_id", &req.worker_id)?;
        let request_hash = sha256_hex(
            format!(
                "{}\n{}\n{}",
                req.worker_id,
                self.ticket_hash(&req.recovery_ticket),
                req.new_ed25519_public_key
            )
            .as_bytes(),
        );
        let record_rel = format!("recoveries/{}.json", req.recovery_request_id);
        if let Some(record) = self.store.read_doc(&record_rel)? {
            if field_str(&record, "request_sha256")? != request_hash {
                return Err(err(
                    IdentityErrorCode::IdempotencyConflict,
                    "recovery_request_id reused with different content",
                ));
            }
            let response = record
                .get("response")
                .cloned()
                .ok_or_else(|| storage("recovery record missing response"))?;
            return serde_json::from_value(response).map_err(storage);
        }

        let new_key = decode_public_key(&req.new_ed25519_public_key)?;
        // Distinct TIG-POOL-RECOVERY-V1 signing domain: recovery and
        // enrollment never share a signed-message namespace.
        let proof_msg = recovery_signing_string(
            &req.recovery_request_id,
            &req.recovery_ticket,
            &req.new_ed25519_public_key,
        );
        verify_b64url(&new_key, &proof_msg, &req.new_key_proof)?;

        // A security revocation cannot be undone by member-initiated
        // recovery, even if a ticket predates the revocation (§3.3). Checked
        // before the ticket is consumed.
        let worker_rel = format!("workers/{}.json", req.worker_id);
        let worker_before = self
            .store
            .read_doc(&worker_rel)?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown worker"))?;
        if worker_before.get("revoked_reason").and_then(Value::as_str)
            == Some(RevocationReason::Security.as_str())
        {
            self.audit(
                "recovery_refused",
                now,
                json!({ "worker_id": req.worker_id, "reason": "security_revocation" }),
            )?;
            return Err(err(
                IdentityErrorCode::TicketRejected,
                "security-revoked worker: recovery requires an explicit operator decision",
            ));
        }

        self.claim_ticket(
            PURPOSE_RECOVERY,
            &req.recovery_ticket,
            Some(&req.worker_id),
            &req.recovery_request_id,
            now,
        )?;

        let new_credential_id = self.issue_uuid(&format!(
            "recovery:{}:{}",
            req.worker_id, req.recovery_request_id
        ));
        self.write_credential(
            &new_credential_id,
            &req.worker_id,
            &req.new_ed25519_public_key,
            now,
        )?;

        // Revoke every old credential of this worker.
        let mut revoked = Vec::new();
        for name in self
            .store
            .list(&format!("worker-credentials/{}", req.worker_id))?
        {
            let Some(credential_id) = name.strip_suffix(".json") else {
                continue;
            };
            if credential_id == new_credential_id {
                continue;
            }
            let rel = format!("credentials/{credential_id}.json");
            if let Some(mut doc) = self.store.read_doc(&rel)? {
                if doc.get("revoked") != Some(&json!(true)) {
                    doc["revoked"] = json!(true);
                    self.store.write_doc(&rel, &doc)?;
                }
                revoked.push(credential_id.to_owned());
            }
        }

        // Restore the worker only when the revocation was accidental
        // (§3.3); the security case was rejected above.
        let mut worker = self
            .store
            .read_doc(&worker_rel)?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown worker"))?;
        if field_str(&worker, "status")? != "ACTIVE" {
            worker["status"] = json!("ACTIVE");
            worker["revoked_reason"] = Value::Null;
            worker["revoked_by"] = Value::Null;
            self.store.write_doc(&worker_rel, &worker)?;
        }

        let response = RecoverWorkerResponse {
            recovery_request_id: req.recovery_request_id.clone(),
            worker_id: req.worker_id.clone(),
            new_credential_id: new_credential_id.clone(),
            revoked_credential_ids: revoked,
            server_time: rfc3339_utc(now),
        };
        self.store.write_doc(
            &record_rel,
            &json!({
                "request_sha256": request_hash,
                "response": serde_json::to_value(&response).map_err(storage)?,
            }),
        )?;
        self.audit(
            "worker_recovered",
            now,
            json!({ "worker_id": req.worker_id, "new_credential_id": new_credential_id }),
        )?;
        Ok(response)
    }

    // -- slot identity issuance (§2, §3.1: separate authenticated call) -----

    /// Minimal initial slot registration: issues a pool `slot_id` bound to
    /// the worker, idempotent by `slot_registration_id`; a new registration
    /// ID for an existing `(worker_id, client_slot_key)` is a no-op that
    /// returns the existing generation (§2). Compute-fact recording and the
    /// §6 qualification flow are outside identity issuance and remain open
    /// for the production slot slice.
    pub fn register_slot(
        &self,
        caller: &VerifiedWorker,
        slot_registration_id: &str,
        client_slot_key: &str,
        now: u64,
    ) -> IdentityResult<SlotGrant> {
        let _write = self.guard();
        validate_uuid("slot_registration_id", slot_registration_id)?;
        if client_slot_key.is_empty() || client_slot_key.len() > 128 {
            return Err(err(
                IdentityErrorCode::UnknownResource,
                "invalid client_slot_key",
            ));
        }
        let request_hash =
            sha256_hex(format!("{}\n{client_slot_key}", caller.worker_id).as_bytes());
        // Scoped by the verified caller like every worker-chosen
        // idempotency key (§3.2): reuse of another worker's registration ID
        // neither conflicts nor discloses it.
        let record_rel = format!(
            "slot-registrations/{}/{slot_registration_id}.json",
            caller.worker_id
        );
        if let Some(record) = self.store.read_doc(&record_rel)? {
            if field_str(&record, "request_sha256")? != request_hash {
                return Err(err(
                    IdentityErrorCode::IdempotencyConflict,
                    "slot_registration_id reused with different content",
                ));
            }
            let grant = record
                .get("grant")
                .cloned()
                .ok_or_else(|| storage("slot registration record missing grant"))?;
            return serde_json::from_value(grant).map_err(storage);
        }

        // The worker-chosen key never becomes a path (CLAUDE.md); index by
        // its hash.
        let index_rel = format!(
            "slot-index/{}/{}.json",
            caller.worker_id,
            sha256_hex(client_slot_key.as_bytes())
        );
        let grant = match self.store.read_doc(&index_rel)? {
            Some(index) => {
                // Existing logical slot, unchanged facts: no-op returning the
                // existing generation (§2).
                let slot_id = field_str(&index, "slot_id")?;
                let slot = self
                    .store
                    .read_doc(&format!("slots/{slot_id}.json"))?
                    .ok_or_else(|| storage("slot index points at missing slot"))?;
                serde_json::from_value::<SlotGrant>(slot).map_err(storage)?
            }
            None => {
                let slot_id =
                    self.issue_uuid(&format!("slot:{}:{slot_registration_id}", caller.worker_id));
                let grant = SlotGrant {
                    slot_id: slot_id.clone(),
                    worker_id: caller.worker_id.clone(),
                    client_slot_key: client_slot_key.to_owned(),
                    qualification: "UNQUALIFIED".to_owned(),
                    qualified_generation: None,
                    generation: 1,
                };
                self.store.write_doc(
                    &format!("slots/{slot_id}.json"),
                    &serde_json::to_value(&grant).map_err(storage)?,
                )?;
                self.store
                    .write_doc(&index_rel, &json!({ "slot_id": slot_id }))?;
                grant
            }
        };
        self.store.write_doc(
            &record_rel,
            &json!({
                "request_sha256": request_hash,
                "grant": serde_json::to_value(&grant).map_err(storage)?,
            }),
        )?;
        self.audit(
            "slot_registered",
            now,
            json!({ "worker_id": caller.worker_id, "slot_id": grant.slot_id }),
        )?;
        Ok(grant)
    }

    pub fn slot_grant(&self, slot_id: &str) -> IdentityResult<Option<SlotGrant>> {
        validate_uuid("slot_id", slot_id)?;
        match self.store.read_doc(&format!("slots/{slot_id}.json"))? {
            Some(doc) => serde_json::from_value(doc).map(Some).map_err(storage),
            None => Ok(None),
        }
    }

    /// Durably record a member capacity-offer command — the only offer
    /// mutation the Pool API owns (architecture §6: admitting, rejecting,
    /// queueing, and reserving the slot belong to the Controller). Idempotent
    /// by `(worker_id, offer_id)` (§5); the credential authorizes only its
    /// own worker's slots, so an unknown or cross-worker slot is rejected
    /// uniformly and an unqualified slot fails closed (§3.2, §16.2, §14).
    /// No reservation, lease, or admission happens here.
    pub fn record_capacity_offer(
        &self,
        caller: &VerifiedWorker,
        offer_id: &str,
        slot_id: &str,
        now: u64,
    ) -> IdentityResult<OfferRecord> {
        let _write = self.guard();
        validate_uuid("offer_id", offer_id)?;
        validate_uuid("slot_id", slot_id)?;
        // Authorization does not depend on the caller repeating the check:
        // the slot must exist, belong to the signing worker, and be
        // qualified for its exact current generation.
        match self.slot_grant(slot_id)? {
            Some(grant) if grant.worker_id == caller.worker_id => {
                if !grant.is_qualified() {
                    return Err(err(
                        IdentityErrorCode::SlotNotQualified,
                        "slot generation has no successful qualification",
                    ));
                }
            }
            _ => return Err(self.reject_unknown_resource(caller, "slot", now)),
        }
        // Idempotent by (worker, offer_id): an offer ID is never reused for
        // another availability period (§2).
        let rel = format!("offers/{}/{offer_id}.json", caller.worker_id);
        if let Some(doc) = self.store.read_doc(&rel)? {
            let record: OfferRecord = serde_json::from_value(doc).map_err(storage)?;
            if record.slot_id == slot_id {
                return Ok(record);
            }
            return Err(err(
                IdentityErrorCode::IdempotencyConflict,
                "offer_id reused for a different slot",
            ));
        }
        let record = OfferRecord {
            offer_id: offer_id.to_owned(),
            worker_id: caller.worker_id.clone(),
            slot_id: slot_id.to_owned(),
            recorded_at: now,
        };
        self.store
            .write_doc(&rel, &serde_json::to_value(&record).map_err(storage)?)?;
        self.audit(
            "offer_recorded",
            now,
            json!({ "worker_id": caller.worker_id, "offer_id": offer_id, "slot_id": slot_id }),
        )?;
        Ok(record)
    }

    /// SPIKE/TEST-ONLY stand-in for a successful §6 qualification of the
    /// slot's current generation. The real known-output qualification flow
    /// (task issuance, execution, result verification, spec-digest binding)
    /// is owned by the production slot slice; until it exists, a slot stays
    /// `UNQUALIFIED` — and therefore cannot offer or receive work (§16.2,
    /// fail-closed per §14).
    ///
    /// Compiled only for tests and the disposable spike bridge (`spike`
    /// cargo feature) so this evidence-free path can never become the
    /// production qualification route. Guarded by the architecture §6
    /// qualification-result-ID idempotency rule and records the attested
    /// qualification identity and spec digest it stands in for.
    #[cfg(any(test, feature = "spike"))]
    pub fn mark_slot_qualified(
        &self,
        slot_id: &str,
        actor: &str,
        qualification_result_id: &str,
        qualification_spec_digest: &str,
        now: u64,
    ) -> IdentityResult<()> {
        let _write = self.guard();
        validate_uuid("slot_id", slot_id)?;
        if qualification_spec_digest.len() != 64
            || !qualification_spec_digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(err(
                IdentityErrorCode::UnknownResource,
                "invalid qualification_spec_digest",
            ));
        }
        let command = format!("mark_slot_qualified\n{slot_id}\n{qualification_spec_digest}");
        if self.check_command(qualification_result_id, &command)? {
            return Ok(());
        }
        let rel = format!("slots/{slot_id}.json");
        let mut doc = self
            .store
            .read_doc(&rel)?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown slot"))?;
        // Qualification binds to the exact current generation (§6, §16.2):
        // a later reconfiguration generation would invalidate it.
        let generation = field_u64(&doc, "generation")?;
        let qualification_id = self.issue_uuid(&format!(
            "qualification:{slot_id}:{qualification_result_id}"
        ));
        doc["qualification"] = json!("QUALIFIED");
        doc["qualified_generation"] = json!(generation);
        doc["qualification_id"] = json!(qualification_id);
        doc["qualification_spec_digest"] = json!(qualification_spec_digest);
        self.store.write_doc(&rel, &doc)?;
        self.record_command(qualification_result_id, &command)?;
        self.audit(
            "slot_qualified",
            now,
            json!({
                "slot_id": slot_id,
                "actor": actor,
                "generation": generation,
                "qualification_id": qualification_id,
                "qualification_result_id": qualification_result_id,
                "qualification_spec_digest": qualification_spec_digest,
            }),
        )
    }

    // -- assignment identity issuance (§2: `assignment_id` issuer is the pool)

    /// Issue an assignment identity bound to the enrolled member through the
    /// §2 ownership chain. Pool-internal (the controller owns "create a
    /// confirmed assignment", architecture §6); idempotent by `issuance_id`;
    /// one benchmark maps permanently to one assignment and member (§2,
    /// invariant §16.1).
    pub fn issue_assignment(
        &self,
        issuance_id: &str,
        facts: &AssignmentFacts<'_>,
        now: u64,
    ) -> IdentityResult<AssignmentGrant> {
        // Serialized so two concurrent issuances for the same benchmark
        // cannot both pass the uniqueness read below (§2, §16.1).
        let _write = self.guard();
        let AssignmentFacts {
            worker_id,
            slot_id,
            benchmark_id,
            assignment_digest,
            network,
        } = *facts;
        validate_uuid("issuance_id", issuance_id)?;
        validate_uuid("worker_id", worker_id)?;
        validate_uuid("slot_id", slot_id)?;
        if benchmark_id.is_empty() || benchmark_id.len() > 256 {
            return Err(err(
                IdentityErrorCode::UnknownResource,
                "invalid benchmark_id",
            ));
        }
        if assignment_digest.len() != 64
            || !assignment_digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(err(
                IdentityErrorCode::UnknownResource,
                "invalid assignment_digest",
            ));
        }
        validate_token("network", network)?;
        // EVERY issuance fact sits inside the idempotency boundary
        // (architecture §6, §13 invariant 6): a reused issuance ID with a
        // changed digest or network conflicts like any other changed fact
        // (§14: never a different track/binary/benchmark under the same
        // assignment; §16.5).
        let request_hash = sha256_hex(
            format!("{worker_id}\n{slot_id}\n{benchmark_id}\n{assignment_digest}\n{network}")
                .as_bytes(),
        );
        let record_rel = format!("assignment-issuances/{issuance_id}.json");
        if let Some(record) = self.store.read_doc(&record_rel)? {
            if field_str(&record, "request_sha256")? != request_hash {
                return Err(err(
                    IdentityErrorCode::IdempotencyConflict,
                    "issuance_id reused with different content",
                ));
            }
            let grant = record
                .get("grant")
                .cloned()
                .ok_or_else(|| storage("issuance record missing grant"))?;
            return serde_json::from_value(grant).map_err(storage);
        }

        let worker = self
            .store
            .read_doc(&format!("workers/{worker_id}.json"))?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown worker"))?;
        if field_str(&worker, "status")? != "ACTIVE" {
            return Err(err(
                IdentityErrorCode::NotAuthenticated,
                "worker is revoked; no new assignments",
            ));
        }
        let slot = self
            .slot_grant(slot_id)?
            .ok_or_else(|| err(IdentityErrorCode::UnknownResource, "unknown slot"))?;
        if slot.worker_id != worker_id {
            return Err(err(
                IdentityErrorCode::UnknownResource,
                "slot does not belong to worker",
            ));
        }
        // Fail closed (§14): work is issued only against a slot whose
        // current generation is qualified (§16.2).
        if !slot.is_qualified() {
            return Err(err(
                IdentityErrorCode::SlotNotQualified,
                "slot generation has no successful qualification",
            ));
        }

        // benchmark_id -> assignment_id is permanent and unique (§2). The
        // TIG-issued benchmark id never becomes a path; index by its hash.
        let benchmark_rel = format!(
            "assignments-by-benchmark/{}.json",
            sha256_hex(benchmark_id.as_bytes())
        );
        let assignment_id = self.issue_uuid(&format!("assignment:{issuance_id}"));
        if let Some(existing) = self.store.read_doc(&benchmark_rel)?
            && field_str(&existing, "assignment_id")? != assignment_id
        {
            return Err(err(
                IdentityErrorCode::IdempotencyConflict,
                "benchmark already mapped to another assignment",
            ));
        }

        let grant = AssignmentGrant {
            assignment_id: assignment_id.clone(),
            member_id: field_str(&worker, "member_id")?.to_owned(),
            worker_id: worker_id.to_owned(),
            slot_id: slot_id.to_owned(),
            benchmark_id: benchmark_id.to_owned(),
        };
        self.store.write_doc(
            &format!("assignments/{assignment_id}.json"),
            &serde_json::to_value(&grant).map_err(storage)?,
        )?;
        self.store
            .write_doc(&benchmark_rel, &json!({ "assignment_id": assignment_id }))?;
        self.store.write_doc(
            &record_rel,
            &json!({
                "request_sha256": request_hash,
                "grant": serde_json::to_value(&grant).map_err(storage)?,
            }),
        )?;
        self.audit(
            "assignment_issued",
            now,
            json!({ "assignment_id": assignment_id }),
        )?;
        Ok(grant)
    }

    /// The grant already issued under an issuance ID, if any (read-only;
    /// lets a caller distinguish an idempotent retry from a fresh issuance
    /// before running admission checks).
    pub fn assignment_issuance(
        &self,
        issuance_id: &str,
    ) -> IdentityResult<Option<AssignmentGrant>> {
        validate_uuid("issuance_id", issuance_id)?;
        match self
            .store
            .read_doc(&format!("assignment-issuances/{issuance_id}.json"))?
        {
            Some(record) => {
                let grant = record
                    .get("grant")
                    .cloned()
                    .ok_or_else(|| storage("issuance record missing grant"))?;
                serde_json::from_value(grant).map(Some).map_err(storage)
            }
            None => Ok(None),
        }
    }

    pub fn assignment_grant(&self, assignment_id: &str) -> IdentityResult<Option<AssignmentGrant>> {
        validate_uuid("assignment_id", assignment_id)?;
        match self
            .store
            .read_doc(&format!("assignments/{assignment_id}.json"))?
        {
            Some(doc) => serde_json::from_value(doc).map(Some).map_err(storage),
            None => Ok(None),
        }
    }

    // -- upload ownership binding (checklist §5.1 authorization scoping) ----

    /// Record the `upload -> package -> assignment -> worker` ownership chain
    /// when an upload session is created; idempotent for identical bindings.
    pub fn bind_upload(&self, binding: &UploadBinding) -> IdentityResult<()> {
        let _write = self.guard();
        validate_token("upload_id", &binding.upload_id)?;
        validate_uuid("worker_id", &binding.worker_id)?;
        validate_uuid("assignment_id", &binding.assignment_id)?;
        validate_token("package_id", &binding.package_id)?;
        validate_token("pool_package_id", &binding.pool_package_id)?;
        let rel = format!("upload-bindings/{}.json", binding.upload_id);
        let doc = serde_json::to_value(binding).map_err(storage)?;
        match self.store.read_doc(&rel)? {
            Some(existing) if existing == doc => Ok(()),
            Some(_) => Err(err(
                IdentityErrorCode::IdempotencyConflict,
                "upload already bound differently",
            )),
            None => self.store.write_doc(&rel, &doc),
        }
    }

    pub fn upload_binding(&self, upload_id: &str) -> IdentityResult<Option<UploadBinding>> {
        validate_token("upload_id", upload_id)?;
        match self
            .store
            .read_doc(&format!("upload-bindings/{upload_id}.json"))?
        {
            Some(doc) => serde_json::from_value(doc).map(Some).map_err(storage),
            None => Ok(None),
        }
    }

    /// Audited uniform rejection for a resource that is missing or not owned
    /// by the caller: cross-worker resource IDs are rejected without
    /// disclosing whether the resource exists (§3.2). If the audit ledger
    /// itself cannot be written, the storage failure is surfaced instead of
    /// silently dropping the security record (§15, checklist §5.1).
    pub fn reject_unknown_resource(
        &self,
        caller: &VerifiedWorker,
        kind: &str,
        now: u64,
    ) -> IdentityError {
        match self.audit(
            "resource_access_rejected",
            now,
            json!({ "worker_id": caller.worker_id, "kind": kind }),
        ) {
            Ok(()) => err(IdentityErrorCode::UnknownResource, "unknown resource"),
            Err(audit_failure) => audit_failure,
        }
    }
}
