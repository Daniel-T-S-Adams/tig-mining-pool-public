//! The TIG gateway.
//!
//! Slice 1 ships one part of it: the compatibility gate of
//! `docs/tig_integration.md` §13, which every write is behind, and the
//! runtime gate that holds that decision and takes it away when TIG stops
//! accepting the credential or the schema stops matching. The gateway
//! is also the sole holder of the TIG API key (`architecture.md` §2.2,
//! invariant 2). [`credential`] is where that key is loaded, and it is
//! loadable only from inside this crate — §13 check 8 still takes the
//! *observation* that the key is correctly placed, never the key.

pub mod claim;
pub mod credential;
pub mod drive;
pub mod lane;
pub mod readiness;
pub mod reconcile;
pub mod transmit;
pub mod write_gate;
pub mod write_policy;

pub use claim::{
    ClaimDecision, ConfirmedBenchmarks, OwningWorkflow, SiblingGenerations, SkipReason, StopReason,
    decide, decide_benchmark,
};
pub use credential::{CredentialError, TigApiKey};
pub use drive::{
    Acted, DriveError, Driver, IntentOutcome, RunReport, run_once, run_once_benchmarks,
};
pub use lane::PostLane;
pub use readiness::{Check, Evidence, Failure, Pins, WriteReady, evaluate};
pub use reconcile::{PrecommitSubmission, Reconciliation, TrackSettings, reconcile_precommit};
pub use transmit::{PrecommitTransmitter, TransmitError, Transmitted, precommit_body};
pub use write_gate::{Blocked, Revocation, WriteGate, WritePermit};
pub use write_policy::{WritePolicy, WritePolicyError};
