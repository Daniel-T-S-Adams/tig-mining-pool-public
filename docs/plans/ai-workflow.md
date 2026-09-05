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

Three AI review passes on every PR — one general-correctness pass and two
domain-invariants passes — built to avoid the gaps the source guide documented
in its reference repo:

| Decision | Choice | Rationale |
|---|---|---|
| Reviewer count | **3** (general-correctness + two domain-invariants passes) | Primary-review duty warrants a broad lens plus two deep, independent model passes over settled invariants |
| Models | general: `claude-sonnet-5`; invariants: `claude-opus-5` and `claude-fable-5-1` | Broad/fast plus two deep models; all model IDs are explicit |
| CLI | `@anthropic-ai/claude-code@2.1.261`, pinned | Pin the Fable-5.1-capable runner; never install latest in release-critical CI |
| Verdicts | Machine-readable JSON artifacts, schema-checked, **bound to the head SHA** | Never grep prose for "MUST FIX" |
| Re-review | Runs on **every push** (superseded runs cancelled) | Skip-after-first-review leaves later code unreviewed |
| Permissions | `contents: read` + PR-comment write only; no fork PRs; reviewers read, never write code | Review jobs are untrusted-input processors |
| Consistency rule | any `must_fix` finding ⇒ `changes_required`; violations = invalid verdict = gate failure | No self-contradictory verdicts |
| Prompts | Versioned files in `.github/review/` | Reviewed like code |
| Auth | **Owner's Claude subscription** via long-lived OAuth token (`claude setup-token` → repo secret `CLAUDE_CODE_OAUTH_TOKEN`); `ANTHROPIC_API_KEY` supported as fallback | Decided 2026-08-05: no separate API billing; accepted trade-off is that reviewer runs share the subscription's rate limits with interactive use |

**Bootstrap exception: ENDED 2026-08-05** (evidence: issue #38 and PR #43,
whose own checks were the acceptance test). The `CLAUDE_CODE_OAUTH_TOKEN`
secret is set, every configured reviewer runs for real on every PR push, and
`AI review: verdict gate` is a required status check on `main` (recorded in
the #38 closing comment). Review is **fail-closed**: every configured reviewer
must return a valid approval bound to the current head SHA. A missing
credential, model or CLI error, missing verdict, invalid or stale output, or
`changes_required` verdict fails the gate — the primary review layer can never
silently not-happen.

Provenance note: the original reviewers' first genuine run reviewed the PR
ending this exception and returned `changes_required` — the draft wording
claimed fail-closed behavior the workflow didn't yet have. The fail-closed gate
and this paragraph are the result of resolving those findings.

Also W1: PR template with the definition-of-done; `CLAUDE.md` gains the AI
review rules.

## W2 — during early slices, evidence-driven (issue #39)

Trigger: first slices merged and reviewer findings accumulating.

Activated for review expansion on 2026-09-05 through issue #67. Review history
through PR #66 showed repeated, load-bearing domain-invariant findings. The
owner therefore selected Fable 5.1 as a second mandatory invariants pass beside
Opus 5. This deliberately overlaps the invariant remit while varying the model;
all three reviewers remain required and fail closed. This owner decision
supersedes the earlier ordering that required completed quantitative metrics
before adding reviewer three. Metrics remain required before adding a fourth
reviewer or changing the required set again.

- Further specialist reviewers **where findings cluster** (expected:
  accounting/ledger invariants; upload/artifact handling; add security when
  member-facing code lands). Beyond the deliberate Opus/Fable invariant
  redundancy, add a reviewer only for a distinct, non-overlapping focus.
- Single-writer branch ownership rules written into `CLAUDE.md` (local agent
  vs CI agents), then a **bounded fix agent**: one attempt, feature branch
  only, distinct identity, may not touch workflows/tests-to-pass/scope.
- Reviewer metrics per the guide §14: findings accepted / rejected / already
  covered by tests; escapes found after merge. Review them before adding a
  fourth reviewer or changing the required set again.

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
