//! Workflow-database facts.
//!
//! Slice 1 ships the TIG write intent of `docs/architecture.md` §7.3: the
//! record that a write was decided on, and the thing that makes a duplicate
//! write impossible rather than unlikely.

pub mod intent;

pub use intent::{
    IntentError, IntentState, NewIntent, PostgresIntentRepository, TigWriteIntentRepository,
    WriteIntent, WriteKind,
};
