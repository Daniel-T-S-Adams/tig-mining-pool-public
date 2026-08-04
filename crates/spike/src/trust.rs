//! Spike S5 member-trust module: local screening classification and the
//! per-member chargeable-failure circuit breaker.
//!
//! Contracts: `docs/mining_system.md` §8 (member failures and trust; the
//! chargeable-failure charge `X`; removal means "no new precommits"),
//! `docs/member_protocol.md` §15 (fault attribution),
//! `fixtures/benchmark-artifact/v1/expected.json` (expected classifications
//! for the solution-invalid and method-non-reproducible cases). Plan:
//! `docs/plans/protocol-spike.md` phase S5 (issue #14).
//!
//! POLICY VALUES ARE FIXTURE STAND-INS. `mining_system.md` §11 leaves the
//! numerical values of `X`, `J[k]`, and the tier sizes open. Everything in
//! [`PolicyStandIn`] is taken from the fixture sets (`collateral-tier/v1`,
//! `benchmark-artifact/v1`, `queue-lifecycle/v1`) and must not leak into
//! production as a constant.
//!
//! The rules under test, enforced here in code:
//!
//! - A structurally perfect package whose solutions are semantically
//!   worthless (zero bundles meet the minimum verification quality) is a
//!   **chargeable tier failure but NOT fraud** (`mining_system.md` §8,
//!   invariant 18).
//! - A method-non-reproducible package is attributed `MEMBER` via the
//!   **separate method-loss rule** and does **not** increment the chargeable
//!   failure count `f` (`mining_system.md` §8).
//! - Once `f > k` for tier `k`, the breaker is OPEN and the member gets **no
//!   new precommit intents**: the guard refuses before any intent is
//!   persisted and before any TIG write can happen (`mining_system.md` §8
//!   "Removal means tier = NONE, concurrency zero, and no new precommits").

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{Gateway, Ledger, PrecommitPlan};

/// Fixture policy stand-ins (NOT settled protocol constants —
/// `mining_system.md` §11 leaves the real values open).
#[derive(Debug, Clone)]
pub struct PolicyStandIn {
    /// Tier size `k`: breaker opens when chargeable failures `f > k`
    /// (`mining_system.md` §8). Fixture value 2 echoes the tier of
    /// `member_alpha` in `fixtures/queue-lifecycle/v1`.
    pub tier_k: u64,
    /// Minimum verification quality; fixture invention 40 from
    /// `fixtures/benchmark-artifact/v1/expected.json` `context`.
    pub min_verification_quality: i32,
    /// Per-failure charge `X` in attoTIG; fixture value 2 TIG from
    /// `fixtures/collateral-tier/v1` `fixture_policy.failure_charge_X`.
    pub failure_charge_x_attotig: u128,
}

impl PolicyStandIn {
    pub fn fixture_v1() -> PolicyStandIn {
        PolicyStandIn {
            tier_k: 2,
            min_verification_quality: 40,
            failure_charge_x_attotig: 2_000_000_000_000_000_000,
        }
    }
}

/// Local screening classification of one member package, per the
/// `member_protocol.md` §15 / `mining_system.md` §8 outcome taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screening {
    /// `PASS` | `SOLUTION_INVALID` | `METHOD_NON_REPRODUCIBLE`
    pub outcome: String,
    /// §15 fault attribution: `NONE` | `MEMBER`.
    pub attribution: String,
    /// Never true for these outcomes (invariant 18: valid low-quality work is
    /// not mislabeled as fraud; non-reproducibility is handled by the
    /// method-loss rule, and TIG — not local screening — declares fraud).
    pub fraud: bool,
    /// Increments the tier-policy failure count `f` and charges `X`.
    pub chargeable_tier_failure: bool,
    /// Uses the separate bundle-scaled method-loss rule instead of `f`.
    pub method_loss: bool,
    pub detail: Value,
}

/// Screen a quality vector: does any bundle meet the minimum verification
/// quality? Bundles are consecutive `num_nonces / num_bundles` chunks (the
/// pinned upstream bundles nonces before quality ranking; consecutive
/// chunking here is a spike stand-in — the golden fixture does not pin
/// bundle membership). A bundle "meets" the minimum when its best quality
/// reaches the threshold — also a stand-in rule, labeled as such.
pub fn screen_solution_quality(
    qualities: &[i32],
    num_bundles: u64,
    policy: &PolicyStandIn,
) -> Result<Screening> {
    if num_bundles == 0 || qualities.is_empty() {
        bail!("screening requires at least one bundle and one quality");
    }
    let bundles = usize::try_from(num_bundles)?;
    let per_bundle = qualities.len() / bundles;
    if per_bundle == 0 || !qualities.len().is_multiple_of(bundles) {
        bail!(
            "quality vector length {} does not divide into {num_bundles} bundles",
            qualities.len()
        );
    }
    let bundles_meeting = qualities
        .chunks(per_bundle)
        .filter(|chunk| chunk.iter().any(|q| *q >= policy.min_verification_quality))
        .count();
    if bundles_meeting == 0 {
        Ok(Screening {
            outcome: "SOLUTION_INVALID".to_owned(),
            attribution: "NONE".to_owned(),
            fraud: false,
            chargeable_tier_failure: true,
            method_loss: false,
            detail: json!({
                "bundles_meeting_min_quality": 0,
                "num_bundles": num_bundles,
                "min_verification_quality_stand_in": policy.min_verification_quality,
                "rule": "mining_system.md §8: zero bundles meeting TIG's minimum verification \
                         quality is a chargeable tier failure even though it is not fraud; \
                         invariant 18 forbids the fraud label",
            }),
        })
    } else {
        Ok(Screening {
            outcome: "PASS".to_owned(),
            attribution: "NONE".to_owned(),
            fraud: false,
            chargeable_tier_failure: false,
            method_loss: false,
            detail: json!({
                "bundles_meeting_min_quality": bundles_meeting,
                "num_bundles": num_bundles,
            }),
        })
    }
}

/// Screen a method re-execution sample: packaged runtime signatures versus
/// the re-executed (reproduced) ones, per nonce. Any mismatch classifies the
/// package method-non-reproducible: `MEMBER` attribution through the
/// method-loss rule, NOT a chargeable `f` failure (`mining_system.md` §8).
pub fn screen_reproduction(
    packaged: &[(u64, u64)],
    reproduced: &[(u64, u64)],
) -> Result<Screening> {
    if packaged.is_empty() || packaged.len() != reproduced.len() {
        bail!("reproduction screening needs matching non-empty sample sets");
    }
    let mut mismatched = Vec::new();
    for ((nonce_a, sig_a), (nonce_b, sig_b)) in packaged.iter().zip(reproduced) {
        if nonce_a != nonce_b {
            bail!("reproduction sample nonce order mismatch: {nonce_a} vs {nonce_b}");
        }
        if sig_a != sig_b {
            mismatched.push(*nonce_a);
        }
    }
    if mismatched.is_empty() {
        Ok(Screening {
            outcome: "PASS".to_owned(),
            attribution: "NONE".to_owned(),
            fraud: false,
            chargeable_tier_failure: false,
            method_loss: false,
            detail: json!({ "mismatched_nonces": [] }),
        })
    } else {
        Ok(Screening {
            outcome: "METHOD_NON_REPRODUCIBLE".to_owned(),
            attribution: "MEMBER".to_owned(),
            fraud: false,
            chargeable_tier_failure: false,
            method_loss: true,
            detail: json!({
                "mismatched_nonces": mismatched,
                "rule": "member_protocol.md §15 row 2 -> MEMBER via the method-loss rule; \
                         mining_system.md §8: does not also consume X unless an independently \
                         evidenced tier failure occurred; the FRAUDULENT terminal state is \
                         declared by TIG confirmation, never by local screening",
            }),
        })
    }
}

/// Decision returned by the guarded precommit path.
#[derive(Debug)]
pub enum GuardDecision {
    /// Breaker closed: the intent was created (and the write attempted).
    Submitted { intent_id: String },
    /// Breaker OPEN: no intent was created, nothing was sent.
    Refused {
        member_id: String,
        chargeable_failures: u64,
        tier_k: u64,
    },
}

/// Per-member trust state over a durable append-only ledger
/// (`member-trust.jsonl`). Counts fold from the ledger on every read, so the
/// breaker survives restarts.
pub struct MemberTrust {
    pub ledger: Ledger,
}

impl MemberTrust {
    pub fn open(dir: impl Into<std::path::PathBuf>) -> Result<MemberTrust> {
        Ok(MemberTrust {
            ledger: Ledger::open(dir)?,
        })
    }

    /// Durably record one screening outcome for a member-owned package.
    pub fn record_screening(
        &self,
        member_id: &str,
        benchmark_ref: &str,
        screening: &Screening,
        policy: &PolicyStandIn,
    ) -> Result<()> {
        self.ledger.append(
            "member-trust",
            &json!({
                "member_id": member_id,
                "benchmark_ref": benchmark_ref,
                "outcome": screening.outcome,
                "attribution": screening.attribution,
                "fraud": screening.fraud,
                "chargeable_tier_failure": screening.chargeable_tier_failure,
                "method_loss": screening.method_loss,
                "charge_x_attotig_stand_in": screening
                    .chargeable_tier_failure
                    .then(|| policy.failure_charge_x_attotig.to_string()),
                "detail": screening.detail,
            }),
        )
    }

    /// Chargeable failure count `f` for a member, folded from the ledger.
    pub fn chargeable_failures(&self, member_id: &str) -> Result<u64> {
        Ok(self
            .ledger
            .read_all("member-trust")?
            .iter()
            .filter(|r| {
                r.get("member_id").and_then(Value::as_str) == Some(member_id)
                    && r.get("chargeable_tier_failure").and_then(Value::as_bool) == Some(true)
            })
            .count() as u64)
    }

    /// Method-loss event count for a member (separate from `f`).
    pub fn method_loss_events(&self, member_id: &str) -> Result<u64> {
        Ok(self
            .ledger
            .read_all("member-trust")?
            .iter()
            .filter(|r| {
                r.get("member_id").and_then(Value::as_str) == Some(member_id)
                    && r.get("method_loss").and_then(Value::as_bool) == Some(true)
            })
            .count() as u64)
    }

    /// Breaker state: OPEN once `f > k` (`mining_system.md` §8 removal rule).
    pub fn breaker_open(&self, member_id: &str, policy: &PolicyStandIn) -> Result<bool> {
        Ok(self.chargeable_failures(member_id)? > policy.tier_k)
    }

    /// The only precommit path for member-owned work in this spike: consult
    /// the breaker BEFORE any durable intent exists. When the breaker is
    /// open, no intent record is appended and no network send can occur —
    /// the refusal is returned to the caller and durably journaled.
    pub fn guard_precommit(
        &self,
        gw: &Gateway,
        member_id: &str,
        policy: &PolicyStandIn,
        plan: &PrecommitPlan,
    ) -> Result<GuardDecision> {
        let failures = self.chargeable_failures(member_id)?;
        if failures > policy.tier_k {
            self.ledger.append(
                "member-trust",
                &json!({
                    "member_id": member_id,
                    "event": "PRECOMMIT_REFUSED_BREAKER_OPEN",
                    "chargeable_failures": failures,
                    "tier_k_stand_in": policy.tier_k,
                    "rule": "mining_system.md §8: f > k removes tier membership — no new \
                             precommits; the refusal precedes any intent or TIG write",
                }),
            )?;
            return Ok(GuardDecision::Refused {
                member_id: member_id.to_owned(),
                chargeable_failures: failures,
                tier_k: policy.tier_k,
            });
        }
        let intent_id = gw
            .submit_precommit(plan)
            .map_err(|e| anyhow!("guarded precommit failed after breaker check: {e}"))?;
        Ok(GuardDecision::Submitted { intent_id })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn solution_invalid_fixture_vector_is_chargeable_not_fraud() {
        // Exactly the fixture quality vector of
        // fixtures/benchmark-artifact/v1/cases/solution-invalid.
        let qualities = [-1, 0, 3, -5, 2, 0, -2, 1];
        let s = screen_solution_quality(&qualities, 2, &PolicyStandIn::fixture_v1()).unwrap();
        assert_eq!(s.outcome, "SOLUTION_INVALID");
        assert!(s.chargeable_tier_failure);
        assert!(!s.fraud, "invariant 18: never labeled fraud");
        assert!(!s.method_loss);
        assert_eq!(s.attribution, "NONE");
    }

    #[test]
    fn one_qualifying_bundle_passes() {
        let qualities = [-1, 0, 3, -5, 41, 0, -2, 1];
        let s = screen_solution_quality(&qualities, 2, &PolicyStandIn::fixture_v1()).unwrap();
        assert_eq!(s.outcome, "PASS");
        assert!(!s.chargeable_tier_failure);
    }

    #[test]
    fn reproduction_mismatch_is_method_loss_not_f() {
        let packaged = [(2u64, 305_419_897u64), (5, 17_446_744_073_709_551_616)];
        let reproduced = [(2u64, 305_419_896u64), (5, 17_446_744_073_709_551_615)];
        let s = screen_reproduction(&packaged, &reproduced).unwrap();
        assert_eq!(s.outcome, "METHOD_NON_REPRODUCIBLE");
        assert!(s.method_loss);
        assert!(!s.chargeable_tier_failure, "method loss does not consume f");
        assert_eq!(s.attribution, "MEMBER");
        assert!(!s.fraud, "fraud is TIG-confirmed, never local");
    }
}
