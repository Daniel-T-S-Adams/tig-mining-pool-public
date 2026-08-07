//! Pool-issued member/worker identity (issue #32).
//!
//! Implements the member protocol's identity contract with real, pool-issued
//! identities instead of the spike's deterministic benchmark-derived
//! stand-ins:
//!
//! - `docs/member_protocol.md` §2 (identities and ownership), §3.1
//!   (enrollment tickets and proof of key possession), §3.2 (signed requests,
//!   freshness, and replay protection), §3.3 (rotation, recovery, and
//!   revocation), §5 (idempotency keys);
//! - `docs/architecture.md` §3 (the Pool API owns enrollment/rotation/
//!   revocation and request authentication) and §6 (state-change ownership:
//!   enrollment/rotation idempotent by ID plus request hash);
//! - `docs/pre_build_checklist.md` §5.1 (authenticate every worker and
//!   authorize access only to its own assignments and uploads).
//!
//! Storage is the S3 spike pool's durable fsynced-document pattern (see
//! `store`), standing in for the production PostgreSQL rows behind the same
//! storage-neutral contract (member_protocol §17).
//!
//! Nothing in this crate reads a wall clock: callers supply `now` (Unix
//! seconds) so behavior is deterministic under test. Nothing in this crate
//! generates key material except through the caller; private keys never
//! reach the pool side (§3.1).

pub mod keys;
pub mod service;
mod store;
pub mod wire;

use std::fmt;

pub use service::{
    AssignmentFacts, AssignmentGrant, IdentityService, IssuedTicket, OfferRecord,
    RecoverWorkerRequest, RecoverWorkerResponse, RevocationReason, SlotGrant, UploadBinding,
};
pub use wire::{
    EnrollRequest, EnrollResponse, RotateCredentialRequest, RotateCredentialResponse,
    SignedRequest, VerifiedWorker,
};

/// Stable typed reasons (member_protocol §12 note: status and errors return
/// stable typed reasons; §15: security failures are audited, not trust
/// penalties).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityErrorCode {
    /// Enrollment or recovery ticket is unknown, expired, wrong-purpose, or
    /// already consumed. Deliberately one code: a bearer-ticket probe learns
    /// nothing about which condition failed.
    TicketRejected,
    /// Same idempotency key, different request content (§5: `409
    /// IDEMPOTENCY_CONFLICT`).
    IdempotencyConflict,
    /// No common protocol version or package format (§4: HTTP 426, no state
    /// change).
    IncompatibleProtocol,
    /// Credential/worker unknown or revoked, body hash mismatch, or signature
    /// verification failure. Deliberately one code: credential probing learns
    /// nothing (§15); the detail stays server-side in the audit ledger.
    NotAuthenticated,
    /// Request timestamp outside the ±300 s window (§3.2); the client should
    /// correct its clock and retry (§13).
    StaleTimestamp,
    /// `(credential_id, request_id)` reused with different signed bytes
    /// (§3.2: rejected and audited).
    ReplayRejected,
    /// The resource does not exist or does not belong to the signing worker.
    /// Deliberately one code: cross-worker resource IDs are rejected without
    /// disclosing whether the resource exists (§3.2).
    UnknownResource,
    /// The slot's current generation has no successful qualification, so it
    /// may not offer capacity or receive work (§6, invariant §16.2 —
    /// fail-closed per §14).
    SlotNotQualified,
    /// An account/operator command was refused by the current state — e.g.
    /// a SECURITY revocation cannot be downgraded to ACCIDENTAL by a later
    /// command; only the explicit `reinstate_worker` decision clears it
    /// (§3.3). The refusal is audited.
    CommandRefused,
    Storage,
}

#[derive(Debug)]
pub struct IdentityError {
    pub code: IdentityErrorCode,
    pub detail: String,
}

impl IdentityError {
    pub fn new(code: IdentityErrorCode, detail: impl Into<String>) -> IdentityError {
        IdentityError {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.detail)
    }
}

impl std::error::Error for IdentityError {}

pub type IdentityResult<T> = Result<T, IdentityError>;

/// Format Unix seconds as RFC 3339 `YYYY-MM-DDTHH:MM:SSZ` (same
/// days-from-civil inverse as the spike member library; duplicated here so
/// the production-seed crate does not depend on disposable spike code).
pub fn rfc3339_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs = unix_secs % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_767_225_599), "2025-12-31T23:59:59Z");
    }
}
