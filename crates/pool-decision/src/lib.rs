//! The pure decision rules of `mining_system.md` §6.
//!
//! `architecture.md` §3 fixes what this crate is: a pure function over a
//! block-consistent snapshot and explicitly supplied inputs. It performs no
//! I/O, reads no clock, and — the rule with teeth — derives no randomness. The
//! §6.3 tie draw arrives as a supplied rank map, derived by the controller
//! from the decision's anchor block (`pool_domain::challenge_tie`), because a
//! draw the engine could generate is a draw the pool could re-roll.
//!
//! The stages are separate functions rather than one call because
//! `mining_system.md` §6 is written as stages and the fixture set
//! (`fixtures/decision-engine/v1`) tests them that way — each family fixes one
//! stage's inputs and expected outputs. Slice 1 consumes these rules; it does
//! not extend them (`docs/plans/slice-1-gateway.md` §2).
//!
//! Arithmetic is exact. §6.3 and §6.5 are ratio comparisons whose outcome
//! decides whether a tie exists at all, and a tie is what triggers the
//! recorded draw — so a rounding-dependent comparison would silently replace a
//! recorded decision with an unrecorded one. See [`ratio`].

pub mod algorithm;
pub mod bundles;
pub mod challenge;
pub mod ratio;
pub mod source;

pub use algorithm::{
    Algorithm, AlgorithmExcluded, AlgorithmSelection, AlgorithmSelectionInput, select_algorithm,
};
pub use bundles::{
    BundleSizing, BundleSizingError, BundleSizingInput, Derivation, TrackSizing, size_bundles,
};
pub use challenge::{
    Challenge, ChallengeError, ChallengeSelection, ChallengeSelectionInput, ComputeType, Excluded,
    InFlightBenchmark, OfferedCompute, ProjectionExcluded, TrackStats, select_challenge,
};
pub use ratio::{Ratio, RatioError};
pub use source::{
    SourceBenchmark, SourceExcluded, SourceSelection, SourceSelectionInput, select_source,
};
