//! The TIG gateway.
//!
//! Slice 1 ships one part of it: the compatibility gate of
//! `docs/tig_integration.md` §13, which every write is behind, and the
//! runtime gate that holds that decision and takes it away when TIG stops
//! accepting the credential or the schema stops matching. The gateway
//! is also the sole holder of the TIG API key (`architecture.md` §2.2,
//! invariant 2); nothing here reads or stores one — check 8 takes the
//! *observation* that the key is correctly placed, not the key.

pub mod readiness;
pub mod write_gate;

pub use readiness::{Check, Evidence, Failure, Pins, WriteReady, evaluate};
pub use write_gate::{Blocked, Revocation, WriteGate, WritePermit};
