//! The public member-facing service (`architecture.md` §3, §4).
//!
//! This is the one process a member agent talks to, and the one that holds no
//! TIG credential: §2.2 puts the key in the gateway alone, and §6's grants stop
//! this role creating a write intent or changing a workflow. What it owns is
//! termination — authentication, replay checks, idempotent recording of what a
//! member said — and nothing about what the pool then does, which §5.1 states
//! outright: it "does not decide whether the member may receive work".
//!
//! `member_protocol.md` is the contract. The route set is §5's table, the
//! signed-request rules are §3.2, and the wire shapes are pinned in
//! `schemas/member_protocol/v0.1.0`.

pub mod auth;
pub mod error;
pub mod protocol;
pub mod service;
