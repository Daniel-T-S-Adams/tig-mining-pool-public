//! Workflow deadlines and remaining block reserve
//! (`tig_integration.md` §8, criterion F5).
//!
//! §8's four guardrails are **spike safeguards, not permanent constants** —
//! that document says so in as many words, and adds that production timeouts
//! will be replaced after the spike measures real block and upload timing. So
//! they are read from `spike.workflow_guardrails` in
//! `config/tig_integration.json` and never compiled in. A default here would
//! be a protocol constant in the binary reached exactly when the configuration
//! was missing, which is the failure mode §12 and this repository's config
//! rules both exist to prevent.
//!
//! The relationship §8 states — the ten-block interval from package deadline
//! to expiry, reserved for pool-owned commitment, sampling, proof construction
//! and confirmation — is checked at load rather than trusted. A configuration
//! that set `package_due` and `expiry` ten blocks apart by coincidence and a
//! reserve of five would silently give the pool half the runway it thinks it
//! has, and nothing downstream would notice.

use serde::Deserialize;

/// §8's guardrails, as loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guardrails {
    /// Do not assign a confirmed precommit at or beyond this age.
    pub max_assignment_age_blocks: u32,
    /// Require durable member-package acceptance before this age.
    pub package_due_age_blocks: u32,
    /// Mark an unfinished local workflow expired at or beyond this age.
    pub workflow_expiry_age_blocks: u32,
    /// The interval reserved for pool-owned work after the package is due.
    pub proof_reserve_blocks: u32,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    spike: RawSpike,
}

#[derive(Debug, Deserialize)]
struct RawSpike {
    workflow_guardrails: RawGuardrails,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGuardrails {
    max_assignment_age_blocks: u32,
    workflow_expiry_age_blocks: u32,
    proof_reserve_blocks: u32,
    package_due_age_blocks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardrailError {
    #[error("tig_integration.json is not readable as workflow guardrails: {0}")]
    Malformed(String),
    /// §8's `110 = 120 - 10`, checked rather than assumed.
    #[error(
        "guardrails disagree: package_due {package_due} + reserve {reserve} \
         != expiry {expiry}"
    )]
    ReserveDoesNotAddUp {
        package_due: u32,
        reserve: u32,
        expiry: u32,
    },
    #[error("assignment age {assignment} must be below the package deadline {package_due}")]
    AssignmentAfterPackageDue { assignment: u32, package_due: u32 },
    #[error("every guardrail must be a positive number of blocks; {field} is 0")]
    NotPositive { field: &'static str },
}

impl Guardrails {
    /// Read the pinned configuration. No fallback: see the module note.
    pub fn from_config_json(text: &str) -> Result<Self, GuardrailError> {
        let raw: RawConfig =
            serde_json::from_str(text).map_err(|e| GuardrailError::Malformed(e.to_string()))?;
        let g = raw.spike.workflow_guardrails;

        for (field, value) in [
            ("max_assignment_age_blocks", g.max_assignment_age_blocks),
            ("package_due_age_blocks", g.package_due_age_blocks),
            ("workflow_expiry_age_blocks", g.workflow_expiry_age_blocks),
            ("proof_reserve_blocks", g.proof_reserve_blocks),
        ] {
            if value == 0 {
                return Err(GuardrailError::NotPositive { field });
            }
        }

        // §8: "code reads that configuration and checks the 110 = 120 - 10
        // relationship at startup." The reserve is the pool's own runway for
        // commitment, sampling, proof construction and confirmation; a
        // configuration where it does not add up hands the pool less time than
        // it believes it has, silently.
        // Checked, because an unchecked sum can wrap in a release build and
        // land exactly on `workflow_expiry_age_blocks` — silently passing the
        // one check this function exists to perform.
        let reserved = g
            .package_due_age_blocks
            .checked_add(g.proof_reserve_blocks)
            .ok_or(GuardrailError::ReserveDoesNotAddUp {
                package_due: g.package_due_age_blocks,
                reserve: g.proof_reserve_blocks,
                expiry: g.workflow_expiry_age_blocks,
            })?;
        if reserved != g.workflow_expiry_age_blocks {
            return Err(GuardrailError::ReserveDoesNotAddUp {
                package_due: g.package_due_age_blocks,
                reserve: g.proof_reserve_blocks,
                expiry: g.workflow_expiry_age_blocks,
            });
        }

        // Assigning work that is already past its package deadline would
        // create a workflow with no runway at all.
        if g.max_assignment_age_blocks >= g.package_due_age_blocks {
            return Err(GuardrailError::AssignmentAfterPackageDue {
                assignment: g.max_assignment_age_blocks,
                package_due: g.package_due_age_blocks,
            });
        }

        Ok(Guardrails {
            max_assignment_age_blocks: g.max_assignment_age_blocks,
            package_due_age_blocks: g.package_due_age_blocks,
            workflow_expiry_age_blocks: g.workflow_expiry_age_blocks,
            proof_reserve_blocks: g.proof_reserve_blocks,
        })
    }
}

/// Where a workflow stands against §8's guardrails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Young enough to assign to a member.
    Assignable,
    /// Past the assignment age but inside the package deadline. Existing work
    /// continues; §8 only forbids *assigning* at this age.
    RunningOut,
    /// Past the package deadline, inside the reserve. The pool's own
    /// commitment, sampling and proof work is what the remaining blocks are
    /// for.
    InProofReserve,
    /// At or beyond the expiry age. F5: the terminal reason is recorded.
    Expired,
}

/// The remaining block reserve `architecture.md` §10.2 asks to be monitored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remaining {
    pub age_blocks: u32,
    pub standing: Standing,
    /// Blocks left before the package is due; zero once it has passed.
    pub until_package_due: u32,
    /// Blocks left before expiry; zero once expired.
    pub until_expiry: u32,
}

impl Guardrails {
    /// Where a workflow started at `block_started` stands at `current_block`.
    ///
    /// A `current_block` behind `block_started` is age zero, not a negative
    /// age: a snapshot can legitimately be read at a height below one the pool
    /// already recorded, and treating that as a wrapped or negative age would
    /// expire a workflow that had barely begun.
    pub fn standing(&self, block_started: i64, current_block: i64) -> Remaining {
        let age = current_block.saturating_sub(block_started).max(0);
        let age_blocks = u32::try_from(age).unwrap_or(u32::MAX);

        let standing = if age_blocks >= self.workflow_expiry_age_blocks {
            Standing::Expired
        } else if age_blocks >= self.package_due_age_blocks {
            Standing::InProofReserve
        } else if age_blocks >= self.max_assignment_age_blocks {
            Standing::RunningOut
        } else {
            Standing::Assignable
        };

        Remaining {
            age_blocks,
            standing,
            until_package_due: self.package_due_age_blocks.saturating_sub(age_blocks),
            until_expiry: self.workflow_expiry_age_blocks.saturating_sub(age_blocks),
        }
    }

    /// §8: "do not assign a confirmed precommit at age >= 60 blocks."
    pub fn may_assign(&self, block_started: i64, current_block: i64) -> bool {
        self.standing(block_started, current_block).standing == Standing::Assignable
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SHIPPED: &str = include_str!("../../../config/tig_integration.json");

    fn shipped() -> Guardrails {
        Guardrails::from_config_json(SHIPPED).expect("the pinned config parses")
    }

    #[test]
    fn the_shipped_configuration_satisfies_the_reserve_relationship() {
        // §8 states it as `110 = 120 - 10`; this asserts the relationship
        // rather than the numbers, which §8 calls spike safeguards and says
        // will be replaced.
        let g = shipped();
        assert_eq!(
            g.package_due_age_blocks + g.proof_reserve_blocks,
            g.workflow_expiry_age_blocks
        );
        assert!(g.max_assignment_age_blocks < g.package_due_age_blocks);
    }

    #[test]
    fn a_reserve_that_does_not_add_up_is_refused() {
        // The whole point of checking at startup. These numbers look
        // plausible and would give the pool five blocks of runway while every
        // other part of the system believed it had ten.
        let text = SHIPPED.replace(
            "\"proof_reserve_blocks\": 10",
            "\"proof_reserve_blocks\": 5",
        );
        assert!(matches!(
            Guardrails::from_config_json(&text),
            Err(GuardrailError::ReserveDoesNotAddUp { .. })
        ));
    }

    #[test]
    fn a_reserve_that_only_adds_up_by_wrapping_is_refused() {
        // The one check this function exists to perform, defeated by an
        // unchecked `+`. `package_due = u32::MAX` and `reserve = 121` wrap to
        // 120 — exactly `workflow_expiry_age_blocks` — so the relationship
        // appears to hold while the pool believes it has four billion blocks
        // of runway. Every other startup check passes on these values.
        let text = SHIPPED
            .replace(
                "\"package_due_age_blocks\": 110",
                "\"package_due_age_blocks\": 4294967295",
            )
            .replace(
                "\"proof_reserve_blocks\": 10",
                "\"proof_reserve_blocks\": 121",
            );
        assert!(
            matches!(
                Guardrails::from_config_json(&text),
                Err(GuardrailError::ReserveDoesNotAddUp { .. })
            ),
            "a wrapped sum is not a satisfied relationship"
        );
    }

    #[test]
    fn assigning_past_the_package_deadline_is_refused() {
        let text = SHIPPED.replace(
            "\"max_assignment_age_blocks\": 60",
            "\"max_assignment_age_blocks\": 110",
        );
        assert!(matches!(
            Guardrails::from_config_json(&text),
            Err(GuardrailError::AssignmentAfterPackageDue { .. })
        ));
    }

    #[test]
    fn a_zero_guardrail_is_refused() {
        let text = SHIPPED.replace(
            "\"proof_reserve_blocks\": 10",
            "\"proof_reserve_blocks\": 0",
        );
        assert!(matches!(
            Guardrails::from_config_json(&text),
            Err(GuardrailError::NotPositive { .. })
        ));
    }

    #[test]
    fn an_unknown_guardrail_field_is_refused() {
        // A typo that silently kept the old value would be the worst outcome:
        // the pool would run on a guardrail the operator thought they changed.
        let text = SHIPPED.replace(
            "\"proof_reserve_blocks\": 10",
            "\"proof_reserve_blocks\": 10, \"proof_reserve\": 20",
        );
        assert!(matches!(
            Guardrails::from_config_json(&text),
            Err(GuardrailError::Malformed(_))
        ));
    }

    #[test]
    fn standing_moves_through_every_band() {
        let g = shipped();
        let started = 1_000;
        let at = |age: i64| g.standing(started, started + age).standing;

        assert_eq!(at(0), Standing::Assignable);
        assert_eq!(
            at(i64::from(g.max_assignment_age_blocks) - 1),
            Standing::Assignable
        );
        // §8 says "age >= 60", so the boundary block is already too old.
        assert_eq!(
            at(i64::from(g.max_assignment_age_blocks)),
            Standing::RunningOut
        );
        assert_eq!(
            at(i64::from(g.package_due_age_blocks) - 1),
            Standing::RunningOut
        );
        assert_eq!(
            at(i64::from(g.package_due_age_blocks)),
            Standing::InProofReserve
        );
        assert_eq!(
            at(i64::from(g.workflow_expiry_age_blocks) - 1),
            Standing::InProofReserve
        );
        assert_eq!(
            at(i64::from(g.workflow_expiry_age_blocks)),
            Standing::Expired
        );
        assert_eq!(
            at(i64::from(g.workflow_expiry_age_blocks) + 500),
            Standing::Expired
        );
    }

    #[test]
    fn the_remaining_reserve_is_reported_for_monitoring() {
        // `architecture.md` §10.2 lists "remaining block reserve" among the
        // minimum metrics, so it is returned rather than recomputed by each
        // caller that wants it.
        let g = shipped();
        let r = g.standing(1_000, 1_000 + i64::from(g.max_assignment_age_blocks));
        assert_eq!(r.age_blocks, g.max_assignment_age_blocks);
        assert_eq!(
            r.until_package_due,
            g.package_due_age_blocks - g.max_assignment_age_blocks
        );
        assert_eq!(
            r.until_expiry,
            g.workflow_expiry_age_blocks - g.max_assignment_age_blocks
        );

        let expired = g.standing(1_000, 1_000 + i64::from(g.workflow_expiry_age_blocks));
        assert_eq!(expired.until_package_due, 0);
        assert_eq!(expired.until_expiry, 0);
    }

    #[test]
    fn a_current_block_behind_the_start_is_age_zero() {
        // Not a negative or wrapped age. A snapshot can be read at a height
        // below one already recorded, and an underflow here would expire a
        // workflow that had barely begun.
        let g = shipped();
        let r = g.standing(1_000, 900);
        assert_eq!(r.age_blocks, 0);
        assert_eq!(r.standing, Standing::Assignable);
    }

    #[test]
    fn may_assign_is_exactly_the_assignable_band() {
        let g = shipped();
        let started = 1_000;
        assert!(g.may_assign(started, started));
        assert!(g.may_assign(
            started,
            started + i64::from(g.max_assignment_age_blocks) - 1
        ));
        assert!(!g.may_assign(started, started + i64::from(g.max_assignment_age_blocks)));
    }
}
