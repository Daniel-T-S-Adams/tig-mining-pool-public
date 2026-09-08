//! Workflow-database facts.
//!
//! Slice 1 ships the TIG write intent of `docs/architecture.md` §7.3 — the
//! record that a write was decided on, and the thing that makes a duplicate
//! write impossible rather than unlikely — and the gateway's write-attempt
//! ledger, which records that a request was sent before it is sent.

pub mod attempt;
pub mod decision;
pub mod intent;
pub mod lease;
pub mod workflow;

pub use attempt::{
    AttemptError, AttemptOutcome, PostgresAttemptLedger, WriteAttempt, WriteAttemptLedger,
};
pub use decision::{
    AdmissionError, Admitted, AnchorSnapshot, NewDecision, PRECOMMIT_ADMISSION_LOCK, RecordedDraw,
    RecordedTie, admit_precommit,
};
pub use intent::{
    IntentError, IntentState, NewIntent, PostgresIntentRepository, TigWriteIntentRepository,
    WriteIntent, WriteKind,
};
pub use lease::{Lease, LeaseError, LeaseKind};
pub use workflow::{
    ConfirmedBenchmark, ConfirmedFraud, ConfirmedPrecommit, ConfirmedProof, Owner,
    POOL_BOOTSTRAP_OWNER, Submitted, Workflow, WorkflowError, WorkflowState,
};
