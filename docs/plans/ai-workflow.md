---
title: AI-first development workflow build-out
status: active
created: 2026-08-05
source: adapted from the C3 guide "Building an AI-First Development Workflow"; issues #37–#40
---

# AI workflow plan

All development in this repository is performed by AI agents with minimal
human input. This plan phases in the workflow machinery that makes that safe.
Governing principle (from the source guide): *the repository supplies context,
executable feedback, durable memory, and explicit authority boundaries — a
longer prompt is not a substitute.*

Because the human does not read most diffs, **AI review is the primary review
layer here, not additional coverage**. The human's remaining roles are the
approve/merge click (informed by an evidence bundle), spend/custody/secret
decisions, and the launch gates in `pre_build_checklist.md` §9. Those are
never delegated, and are enforced by branch protection — not prompts.

## Already in place (pre-plan)

Operating contract (`CLAUDE.md`/`AGENTS.md`), protected `main` (required CI
check, human-only merge, up-to-date branches), `make check` with no masked
failures, pinned-action CI, deterministic feedback layer (`fixtures/`,
`crates/fake-tig`), issues-with-acceptance-criteria as the unit of agent work,
durable memory (ADRs, plans, spike report), secrets discipline (Doppler-style
file secrets, `.gitignore`d).

## W1 — before slice 1 (issue #37; enablement #38)

Two specialized AI reviewers on every PR, built to avoid the gaps the source
guide documented in its reference repo:

| Decision | Choice | Rationale |
|---|---|---|
| Reviewer count | **2** (general-correctness + domain-invariants) | Primary-review duty warrants two independent lenses; more only when finding-data justifies (W2) |
| Models | general: `claude-sonnet-5`; invariants: `claude-opus-5` | Broad/fast + deep/careful; both pinned |
| CLI | `@anthropic-ai/claude-code@2.1.221`, pinned | Never install latest in release-critical CI |
| Verdicts | Machine-readable JSON artifacts, schema-checked, **bound to the head SHA** | Never grep prose for "MUST FIX" |
| Re-review | Runs on **every push** (superseded runs cancelled) | Skip-after-first-review leaves later code unreviewed |
| Permissions | `contents: read` + PR-comment write only; no fork PRs; reviewers read, never write code | Review jobs are untrusted-input processors |
| Consistency rule | any `must_fix` finding ⇒ `changes_required`; violations = invalid verdict = gate failure | No self-contradictory verdicts |
| Prompts | Versioned files in `.github/review/` | Reviewed like code |

**Bootstrap exception (time-boxed):** until `ANTHROPIC_API_KEY` exists as a
repo secret, reviewer jobs skip with a loud annotation and the gate passes
with a warning; the gate is not yet a required status check. Issue #38 tracks
the two enablement steps (human sets the secret; then the gate becomes a
required check). This exception ends there — after that, a missing key fails
the gate.

Also W1: PR template with the definition-of-done; `CLAUDE.md` gains the AI
review rules.

## W2 — during early slices, evidence-driven (issue #39)

Trigger: first slices merged and reviewer findings accumulating.

- Specialist reviewers **where findings cluster** (expected: accounting/ledger
  invariants; upload/artifact handling; add security when member-facing code
  lands). Cap: only add a reviewer with distinct, non-overlapping focus.
- Single-writer branch ownership rules written into `CLAUDE.md` (local agent
  vs CI agents), then a **bounded fix agent**: one attempt, feature branch
  only, distinct identity, may not touch workflows/tests-to-pass/scope.
- Reviewer metrics per the guide §14: findings accepted / rejected / already
  covered by tests; escapes found after merge. Reviewed before adding any
  third reviewer.

## W3 — at first deployed environment (~slice 5) (issue #40)

Trigger: the smallest deployed testnet exists (`architecture.md` §11.2).

- Staging deploy workflow (exact SHA), spike runbook recast as deployed-env
  E2E, promotion gating: E2E green before anything advances.
- UAT evidence in the merge bundle (command, environment, build, result).
- Environment protection + separate deploy credentials.

## Never delegated (any phase)

PR approval and merge; production/mainnet anything; migrations against
deployed environments; funds custody, payout keys, slashing; raising spend;
weakening tests/gates/branch protection; secret rotation. See `CLAUDE.md`
"Human-only actions".
