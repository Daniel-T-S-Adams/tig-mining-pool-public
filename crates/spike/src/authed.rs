//! Authenticated S3 upload/acceptance path (issue #32): the spike pool's
//! resumable upload, durable-acceptance saga, receipt, and slot re-offer,
//! gated behind real pool-issued identities.
//!
//! Contracts: `docs/member_protocol.md` §3.2 (every member request signed by
//! an enrolled worker credential; cross-worker resource IDs rejected without
//! disclosing existence), §5 (routes and idempotency keys);
//! `docs/pre_build_checklist.md` §5.1 (a worker restricted to its own
//! slots/assignments/uploads); `docs/architecture.md` §2.1 (the signature
//! establishes worker identity only) and §6 (the Pool API owns request
//! authentication; the controller owns assignment creation).
//!
//! This is the bridge between the disposable S3 spike storage
//! (`crate::pool`) and the production identity crate (`pool-identity`): the
//! storage semantics stay exactly as validated by `tests/pool_upload.rs`,
//! while every member-facing operation now requires a verified signature and
//! an issued (non-derived) identity chain.

use pool_identity::{
    AssignmentGrant, IdentityError, IdentityErrorCode, IdentityService, SignedRequest,
    UploadBinding, VerifiedWorker,
};
use serde_json::{Value, json};

use crate::pool::{ErrorCode, FinalizeOutcome, Pool, PoolError, UploadSession};

/// Error from the authenticated path: either an identity/authorization
/// rejection or a storage-path error from the underlying pool.
#[derive(Debug)]
pub enum AuthedError {
    Identity(IdentityError),
    Pool(PoolError),
}

impl AuthedError {
    pub fn identity_code(&self) -> Option<IdentityErrorCode> {
        match self {
            AuthedError::Identity(e) => Some(e.code),
            AuthedError::Pool(_) => None,
        }
    }

    pub fn pool_code(&self) -> Option<ErrorCode> {
        match self {
            AuthedError::Pool(e) => Some(e.code),
            AuthedError::Identity(_) => None,
        }
    }
}

impl std::fmt::Display for AuthedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthedError::Identity(e) => write!(f, "{e}"),
            AuthedError::Pool(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuthedError {}

impl From<IdentityError> for AuthedError {
    fn from(e: IdentityError) -> AuthedError {
        AuthedError::Identity(e)
    }
}

impl From<PoolError> for AuthedError {
    fn from(e: PoolError) -> AuthedError {
        AuthedError::Pool(e)
    }
}

pub type AResult<T> = Result<T, AuthedError>;

fn unknown_resource() -> AuthedError {
    AuthedError::Identity(IdentityError::new(
        IdentityErrorCode::UnknownResource,
        "unknown resource",
    ))
}

/// Member-chosen `package_id` must be an exact lowercase UUID
/// (member_protocol §2; `api.schema.json#/$defs/BeginUploadRequest` pins it
/// to `common.schema.json#/$defs/Uuid`) — validated before it participates
/// in any storage operation.
fn valid_member_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
}

/// Assignment-scoped storage key for a worker-chosen `package_id` (§2):
/// SHA-256 of `assignment_id:package_id`, truncated to 32 hex characters —
/// the same shape the pool already uses for upload IDs. Worker-chosen
/// identifiers never reach a global storage namespace (§3.2).
fn scoped_package_id(assignment_id: &str, package_id: &str) -> String {
    crate::member::sha256_hex(format!("{assignment_id}:{package_id}").as_bytes())[..32].to_owned()
}

/// Extract `{id}` from `prefix{id}suffix`, rejecting empty or nested-path
/// values. The path is part of the signed byte string, so a mismatch here is
/// a caller routing error, not a forgery vector.
fn path_id(path: &str, prefix: &str, suffix: &str) -> Option<String> {
    let rest = path.strip_prefix(prefix)?;
    let id = rest.strip_suffix(suffix)?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    Some(id.to_owned())
}

fn body_str<'v>(body: &'v Value, key: &str) -> AResult<&'v str> {
    body.get(key).and_then(Value::as_str).ok_or_else(|| {
        AuthedError::Identity(IdentityError::new(
            IdentityErrorCode::UnknownResource,
            format!("request body missing '{key}'"),
        ))
    })
}

/// The confirmed-benchmark facts a controller supplies when issuing and
/// registering an assignment (pool-internal; architecture §6).
#[derive(Debug, Clone, Copy)]
pub struct AssignmentIssuance<'a> {
    pub issuance_id: &'a str,
    pub worker_id: &'a str,
    pub slot_id: &'a str,
    pub benchmark_id: &'a str,
    pub assignment_digest: &'a str,
    pub network: &'a str,
}

pub struct AuthedPool {
    pool: Pool,
    identity: IdentityService,
    /// Serializes assignment issuance across BOTH stores: the slot-occupancy
    /// admission read (pool state) and the permanent benchmark mapping write
    /// (identity state) must be one critical section, or two concurrent
    /// issuances for the same slot could both pass admission.
    issuance: std::sync::Mutex<()>,
}

impl AuthedPool {
    /// Open the authed pool at `root`: spike-pool storage under
    /// `root/pool`, identity state under `root/identity`.
    pub fn open(root: impl AsRef<std::path::Path>) -> AResult<AuthedPool> {
        let root = root.as_ref();
        Ok(AuthedPool {
            pool: Pool::open(root.join("pool"))?,
            identity: IdentityService::open(root.join("identity"))?,
            issuance: std::sync::Mutex::new(()),
        })
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    pub fn identity(&self) -> &IdentityService {
        &self.identity
    }

    // -- pool-internal issuance (controller-owned, architecture §6) ---------

    /// Issue the assignment identity for a confirmed benchmark and register
    /// the assignment context the upload will bind to. The identity is
    /// pool-issued and bound to the enrolled member through
    /// `slot -> worker -> member` (member_protocol §2) — never derived from
    /// the benchmark id. Idempotent by `issuance_id`.
    ///
    /// For a slot that already completed an accepted upload, this is the
    /// re-offer admission: the slot must have been released by a durable
    /// acceptance, and its generation is bumped (member_protocol §9).
    pub fn issue_and_register_assignment(
        &self,
        req: &AssignmentIssuance<'_>,
        now: u64,
    ) -> AResult<AssignmentGrant> {
        let AssignmentIssuance {
            issuance_id,
            worker_id,
            slot_id,
            benchmark_id,
            assignment_digest,
            network,
        } = *req;
        // One critical section over pool admission + identity issuance.
        let _issuing = self
            .issuance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let view = match self.pool.slot_view(slot_id) {
            // First assignment for this slot.
            Err(e) if e.code == ErrorCode::SlotUnknown => None,
            Err(e) => return Err(e.into()),
            Ok(view) => Some(view),
        };
        // Admission before issuance: a FRESH issuance must not consume the
        // permanent benchmark -> assignment mapping (§2) while the slot
        // still has open work. The prior record is used ONLY for that
        // admission decision and for replay purity — `issue_assignment`
        // itself always runs, so a reused `issuance_id` with ANY changed
        // fact (worker, slot, benchmark, digest, or network — all inside
        // the idempotency boundary) is an `IdempotencyConflict`, never a
        // silent grant (§2, §14, §16.4, §16.5, §16.12).
        let replay = self.identity.assignment_issuance(issuance_id)?.is_some();
        if !replay
            && let Some(v) = &view
            && v.occupied
        {
            return Err(AuthedError::Pool(PoolError {
                code: ErrorCode::SlotOccupied,
                detail: format!(
                    "slot {slot_id} generation {} not released by a durable acceptance",
                    v.generation
                ),
                committed_offset: None,
            }));
        }
        let facts = pool_identity::AssignmentFacts {
            worker_id,
            slot_id,
            benchmark_id,
            assignment_digest,
            network,
        };
        let grant = self.identity.issue_assignment(issuance_id, &facts, now)?;
        // A replayed issuance is a pure read of the recorded grant: it never
        // reserves the slot or bumps a generation (§16.3 — a retry cannot
        // create an extra slot generation or reservation). The context is
        // (re-)registered only when the live slot view already belongs to
        // this grant; a crash window that left no matching view is an
        // operator-reconciliation case (§14), never an automatic mutation.
        let generation = match (&view, replay) {
            (Some(v), _) if v.assignment_id == grant.assignment_id => v.generation,
            (_, true) => return Ok(grant),
            (None, false) => 1,
            // Released by a durable acceptance: reserve at the next
            // generation (release check re-run inside `offer_slot`).
            (Some(v), false) if !v.occupied => {
                self.pool.offer_slot(slot_id, &grant.assignment_id)?
            }
            (Some(v), false) => {
                return Err(AuthedError::Pool(PoolError {
                    code: ErrorCode::SlotOccupied,
                    detail: format!("slot {slot_id} generation {} has open work", v.generation),
                    committed_offset: None,
                }));
            }
        };
        self.pool.register_assignment(&json!({
            "assignment_digest": assignment_digest,
            "assignment_id": grant.assignment_id,
            "benchmark_id": grant.benchmark_id,
            "member_id": grant.member_id,
            "worker_id": grant.worker_id,
            "slot_id": grant.slot_id,
            "slot_generation": generation,
            "network": network,
        }))?;
        Ok(grant)
    }

    // -- member-facing signed operations (member_protocol §5 routes) --------

    /// `POST /member/v0/capacity-offers` (§5, idempotency `offer_id`):
    /// verify the signature and durably record the member's offer command —
    /// the ONLY offer mutation the Pool API owns (architecture §6).
    ///
    /// FAIL-CLOSED: admission is Controller scope — account status, tier,
    /// per-member queue allowance, collateral, unresolved-exposure limits,
    /// slot reservation, leases, queueing, and precommit intent (§6). None
    /// of that policy layer exists in this bridge, and §6 is explicit that
    /// "absence of an applicable policy produces `NO_ACTION`, never
    /// unlimited work" — so every recorded offer returns
    /// `offer_state: "NO_ACTION"` with no lease and no reservation until
    /// the production Controller admission slice (checklist §10 step 2)
    /// exists. The slot must still exist, belong to the signing worker, and
    /// be qualified (§3.2, §16.2, §14) — enforced inside
    /// `record_capacity_offer` — and a slot with open work is rejected
    /// (§6 step 1).
    pub fn offer_capacity(&self, sr: &SignedRequest, body: &[u8], now: u64) -> AResult<Value> {
        let caller = self.identity.verify_request(sr, body, now)?;
        if sr.method != "POST" || sr.path != "/member/v0/capacity-offers" {
            return Err(unknown_resource());
        }
        let parsed: Value = serde_json::from_slice(body).map_err(|_| unknown_resource())?;
        let offer_id = body_str(&parsed, "offer_id")?.to_owned();
        let slot_id = body_str(&parsed, "slot_id")?.to_owned();
        // Ownership and qualification are enforced by the identity service
        // before anything is recorded; check open work first so a busy slot
        // is rejected rather than recorded (§6 step 1).
        match self.identity.slot_grant(&slot_id) {
            Ok(Some(grant)) if grant.worker_id == caller.worker_id => {}
            _ => {
                return Err(self
                    .identity
                    .reject_unknown_resource(&caller, "slot", now)
                    .into());
            }
        }
        match self.pool.slot_view(&slot_id) {
            // Never used: offerable.
            Err(e) if e.code == ErrorCode::SlotUnknown => {}
            Err(e) => return Err(e.into()),
            Ok(view) if view.occupied => {
                return Err(AuthedError::Pool(PoolError {
                    code: ErrorCode::SlotOccupied,
                    detail: format!("slot {slot_id} has open work"),
                    committed_offset: None,
                }));
            }
            Ok(_) => {}
        }
        let record = self
            .identity
            .record_capacity_offer(&caller, &offer_id, &slot_id, now)?;
        // Shaped as `api.schema.json#/$defs/CapacityOfferStatusResponse`:
        // NO_ACTION carries its machine reason and creates no reservation.
        Ok(json!({
            "protocol_version": "0.1.0",
            "offer_id": record.offer_id,
            "worker_id": caller.worker_id,
            "slot_id": record.slot_id,
            "offer_state": "NO_ACTION",
            "precommit_state": "NOT_STARTED",
            "reason_code": "NO_APPLICABLE_ADMISSION_POLICY",
            "server_time": pool_identity::rfc3339_utc(now),
        }))
    }

    /// `POST /member/v0/assignments/{assignment_id}/uploads` (§5,
    /// idempotency `package_id`): create or resume the upload session for
    /// the caller's own issued assignment.
    pub fn create_upload(
        &self,
        sr: &SignedRequest,
        body: &[u8],
        now: u64,
    ) -> AResult<UploadSession> {
        let caller = self.identity.verify_request(sr, body, now)?;
        let assignment_id = (sr.method == "POST")
            .then(|| path_id(&sr.path, "/member/v0/assignments/", "/uploads"))
            .flatten()
            .ok_or_else(unknown_resource)?;
        let grant = self.authorize_assignment(&caller, &assignment_id, now)?;

        let declaration: Value = serde_json::from_slice(body).map_err(|_| unknown_resource())?;
        // The declaration's digest must belong to this issued assignment:
        // the registered assignment doc for the digest carries the issued
        // assignment_id.
        let digest = body_str(&declaration, "assignment_digest")?;
        let registered = self
            .pool
            .assignment_by_digest(digest)?
            .ok_or_else(|| self.reject(&caller, "assignment", now))?;
        if registered.get("assignment_id").and_then(Value::as_str)
            != Some(grant.assignment_id.as_str())
        {
            return Err(self.reject(&caller, "assignment", now));
        }
        let package_id = body_str(&declaration, "package_id")?.to_owned();
        // The worker-chosen identifier is validated BEFORE any durable pool
        // state is created (CLAUDE.md: member-provided names are never
        // trusted; a later rejection must not leave an orphaned, unbindable
        // upload session).
        if !valid_member_uuid(&package_id) {
            return Err(self.reject(&caller, "package", now));
        }
        // `package_id` is worker-chosen (§2). The pool's storage keys are
        // global, so the worker-chosen ID is namespaced by the issued
        // assignment before it reaches storage: one member's chosen ID can
        // never collide with — or disclose the existence of — another
        // member's (§3.2). The member-visible identity stays the worker's
        // own `package_id` (rewritten back on responses).
        let pool_package_id = scoped_package_id(&grant.assignment_id, &package_id);
        // Only one package can become durably accepted for an assignment
        // (§11, §16.8): once one is accepted, a NEW package for the same
        // assignment is refused at admission; the accepted package itself
        // may still resume to recover its receipt.
        if let Some(accepted) = self
            .pool
            .accepted_package_for_assignment(&grant.assignment_id)?
            && accepted != pool_package_id
        {
            return Err(AuthedError::Pool(PoolError {
                code: ErrorCode::AssignmentAlreadyAccepted,
                detail: format!(
                    "assignment {} already durably accepted a package",
                    grant.assignment_id
                ),
                committed_offset: None,
            }));
        }
        let mut pool_declaration = declaration.clone();
        pool_declaration["package_id"] = json!(pool_package_id);
        let session = self.pool.create_upload(&pool_declaration)?;
        self.identity.bind_upload(&UploadBinding {
            upload_id: session.upload_id.clone(),
            package_id,
            pool_package_id,
            assignment_id: grant.assignment_id.clone(),
            worker_id: caller.worker_id.clone(),
        })?;
        Ok(session)
    }

    /// `PUT /member/v0/uploads/{upload_id}` (§5, idempotency
    /// `(upload_id, offset, chunk SHA-256)`). `offset` and `chunk_sha256`
    /// arrive as `Upload-Offset` / `Upload-Chunk-SHA256` headers; the raw
    /// chunk is the signed body.
    pub fn put_chunk(
        &self,
        sr: &SignedRequest,
        offset: u64,
        chunk_sha256: &str,
        body: &[u8],
        now: u64,
    ) -> AResult<u64> {
        let caller = self.identity.verify_request(sr, body, now)?;
        let upload_id = (sr.method == "PUT")
            .then(|| path_id(&sr.path, "/member/v0/uploads/", ""))
            .flatten()
            .ok_or_else(unknown_resource)?;
        self.authorize_upload(&caller, &upload_id, now)?;
        Ok(self
            .pool
            .put_chunk(&upload_id, offset, chunk_sha256, body)?)
    }

    /// `POST /member/v0/uploads/{upload_id}/finalize` (§5, idempotency
    /// `finalize_id`; the pool saga itself is idempotent per package).
    pub fn finalize(&self, sr: &SignedRequest, body: &[u8], now: u64) -> AResult<FinalizeOutcome> {
        let caller = self.identity.verify_request(sr, body, now)?;
        let upload_id = (sr.method == "POST")
            .then(|| path_id(&sr.path, "/member/v0/uploads/", "/finalize"))
            .flatten()
            .ok_or_else(unknown_resource)?;
        let binding = self.authorize_upload(&caller, &upload_id, now)?;

        // FinalizeUploadRequest echoes the declaration; a mismatch is a
        // client error, not a new declaration (§11).
        let parsed: Value = serde_json::from_slice(body).map_err(|_| unknown_resource())?;
        let declaration = self.pool.declaration(&upload_id)?;
        let echoed_sha = body_str(&parsed, "package_sha256")?;
        let echoed_size = parsed
            .get("compressed_size_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(unknown_resource)?;
        if Some(echoed_sha) != declaration.get("package_sha256").and_then(Value::as_str)
            || Some(echoed_size)
                != declaration
                    .get("compressed_size_bytes")
                    .and_then(Value::as_u64)
        {
            return Err(AuthedError::Pool(PoolError {
                code: ErrorCode::DeclarationConflict,
                detail: "finalize does not match the upload declaration".to_owned(),
                committed_offset: None,
            }));
        }
        let outcome = self.pool.finalize(&binding.pool_package_id)?;
        Ok(FinalizeOutcome {
            receipt_json: self.member_receipt(&binding, &outcome.receipt_json)?,
            ..outcome
        })
    }

    /// `POST /member/v0/uploads/{upload_id}/status` (§5, read only): the
    /// resume authority and the durable-receipt recovery path.
    pub fn upload_status(&self, sr: &SignedRequest, body: &[u8], now: u64) -> AResult<Value> {
        let caller = self.identity.verify_request(sr, body, now)?;
        let upload_id = (sr.method == "POST")
            .then(|| path_id(&sr.path, "/member/v0/uploads/", "/status"))
            .flatten()
            .ok_or_else(unknown_resource)?;
        let binding = self.authorize_upload(&caller, &upload_id, now)?;
        let mut status = self.pool.upload_status(&upload_id)?;
        // Responses carry the worker's own package identity, not the
        // storage-scoped key.
        status["package_id"] = json!(binding.package_id);
        // A lost durable-acceptance response is recovered through status,
        // which returns the same immutable receipt (§3.2, §9).
        if let Some(acceptance) = self.pool.acceptance(&binding.pool_package_id)?
            && let Some(receipt) = acceptance.get("receipt")
        {
            let receipt_json = serde_json::to_string(receipt).map_err(|e| {
                AuthedError::Pool(PoolError {
                    code: ErrorCode::Storage,
                    detail: format!("receipt serialization: {e}"),
                    committed_offset: None,
                })
            })?;
            let rewritten = self.member_receipt(&binding, &receipt_json)?;
            status["acceptance_receipt"] = serde_json::from_str(&rewritten).map_err(|e| {
                AuthedError::Pool(PoolError {
                    code: ErrorCode::Storage,
                    detail: format!("receipt parse: {e}"),
                    committed_offset: None,
                })
            })?;
        }
        Ok(status)
    }

    /// The member-visible receipt: byte-identical across retries, with the
    /// worker's own `package_id` in place of the storage-scoped key. The
    /// scoped key remains in `upload_id`-addressed storage for audit.
    fn member_receipt(&self, binding: &UploadBinding, receipt_json: &str) -> AResult<String> {
        let mut receipt: Value = serde_json::from_str(receipt_json).map_err(|e| {
            AuthedError::Pool(PoolError {
                code: ErrorCode::Storage,
                detail: format!("receipt parse: {e}"),
                committed_offset: None,
            })
        })?;
        receipt["package_id"] = json!(binding.package_id);
        serde_json::to_string(&receipt).map_err(|e| {
            AuthedError::Pool(PoolError {
                code: ErrorCode::Storage,
                detail: format!("receipt serialization: {e}"),
                committed_offset: None,
            })
        })
    }

    // -- authorization scoping (checklist §5.1) -----------------------------

    fn reject(&self, caller: &VerifiedWorker, kind: &str, now: u64) -> AuthedError {
        self.identity
            .reject_unknown_resource(caller, kind, now)
            .into()
    }

    fn authorize_assignment(
        &self,
        caller: &VerifiedWorker,
        assignment_id: &str,
        now: u64,
    ) -> AResult<AssignmentGrant> {
        match self.identity.assignment_grant(assignment_id) {
            Ok(Some(grant)) if grant.worker_id == caller.worker_id => Ok(grant),
            // Missing and cross-worker are indistinguishable to the caller
            // (§3.2).
            _ => Err(self.reject(caller, "assignment", now)),
        }
    }

    fn authorize_upload(
        &self,
        caller: &VerifiedWorker,
        upload_id: &str,
        now: u64,
    ) -> AResult<UploadBinding> {
        match self.identity.upload_binding(upload_id) {
            Ok(Some(binding)) if binding.worker_id == caller.worker_id => Ok(binding),
            _ => Err(self.reject(caller, "upload", now)),
        }
    }
}
