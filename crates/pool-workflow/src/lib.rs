//! Workflow-database facts.
//!
//! Slice 1 ships the TIG write intent of `docs/architecture.md` §7.3 — the
//! record that a write was decided on, and the thing that makes a duplicate
//! write impossible rather than unlikely — and the gateway's write-attempt
//! ledger, which records that a request was sent before it is sent.

pub mod acceptance;
pub mod attempt;
pub mod deadlines;
pub mod decision;
pub mod intent;
pub mod lease;
pub mod payload;
pub mod reconcile;
pub mod restart;
pub mod workflow;

pub use acceptance::{
    AcceptanceError, CanonicalPayload, PackageAcceptance, record_acceptance,
    record_canonical_payload,
};
pub use attempt::{
    AttemptError, AttemptOutcome, PostgresAttemptLedger, WriteAttempt, WriteAttemptLedger,
    has_transmitted_precommit_write,
};
pub use deadlines::{GuardrailError, Guardrails, Remaining, Standing};
pub use decision::{
    AdmissionError, Admitted, AnchorSnapshot, NewDecision, PRECOMMIT_ADMISSION_LOCK, RecordedDraw,
    RecordedTie, admit_precommit, payload_inputs,
};
pub use intent::{
    IntentError, IntentState, NewIntent, PostgresIntentRepository, PrecommitSiblings,
    SettledOutcome, TigWriteIntentRepository, WriteIntent, WriteKind, precommit_siblings,
};
pub use lease::{Lease, LeaseError, LeaseKind};
pub use payload::{
    DecisionPayloadInputs, PayloadError, PrecommitSubmission, TrackSettings, precommit_body,
    precommit_digest,
};
pub use reconcile::{ReconcileError, Reconciliation, reconcile_precommit};
pub use restart::{
    ConfirmedWindow, NeedsAttention, Reconciled, RestartReport, open_block_gaps,
    reconcile_after_restart, record_block_gap,
};
pub use workflow::{
    ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof, Owner,
    POOL_BOOTSTRAP_OWNER, Submitted, Workflow, WorkflowError, WorkflowState,
};
