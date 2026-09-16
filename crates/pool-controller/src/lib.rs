//! The pool's protocol state machine (`architecture.md` §3).
//!
//! §6 makes the controller the sole owner of choosing work, recording a
//! decision, and creating or cancelling a TIG write intent. It never holds the
//! TIG API key and never sends a write — §2.2 keeps the credential and the
//! transmission inside `tig-gateway`.
//!
//! Slice 1 ships the part that decides. What it deliberately does not ship is
//! any way to *manufacture* the facts a write depends on, except behind the
//! gate in [`stub`].

pub mod bind;
pub mod commit;
pub mod decide;
pub mod ingest;
pub mod propose;
pub mod reconciler;
pub mod service;
pub mod window;

#[cfg(feature = "stub-acceptance")]
pub mod stub;
