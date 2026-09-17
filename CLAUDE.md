# TIG Mining Pool — Repository Operating Contract

A mining pool for the TIG (The Innovation Game) protocol. Members run
benchmarks on their own machines via a member agent; the pool owns TIG
credentials, orchestrates precommit/benchmark/proof submission, and settles
rewards through an append-only accounting ledger. Implementation: one Rust
(2024 edition) Cargo workspace — Axum services, PostgreSQL 18 as the durable
workflow bus, S3/filesystem artifact store.

## Project phase

**Slice 1 shipped; slice 2 starting.** The pool talks to TIG: it takes in
block-consistent snapshots, decides what to submit, transmits precommits
through the credential-holding gateway, advances a workflow only on confirmed
reads, and survives a crash at each of the four `architecture.md` §12 points on
the TIG write path without paying twice. That ran live on testnet, and where each of its 61 acceptance
criteria is satisfied is recorded in `docs/evidence/slice-1-criteria.md`.

The active milestone is **slice 2 — member agent, member API, and artifact
ingestion** (`docs/pre_build_checklist.md` §10 step 2), whose acceptance
criteria and PR sequencing are in `docs/plans/slice-2-member-agent.md`. It
replaces the two stand-ins slice 1 shipped with: the pool-owned placeholder
where a member should be, and the feature-gated stub where durable package
acceptance should be.

The product advances in vertical slices; each carries its own minimum schema,
migration, tests, observability, and documentation. Doc-only changes follow
the same PR workflow as code.

## Source of truth

1. During pre-build: `docs/` is the authoritative specification.
   Protocol meaning is owned by `docs/mining_system.md`,
   `docs/tig_integration.md`, `docs/member_protocol.md`, and
   `docs/accounting.md`. Component/process/storage boundaries are owned by
   `docs/architecture.md`. They are deliberately non-overlapping — never
   duplicate a rule from one into another.
2. Once code exists: tests and current code outrank prose. Design docs become
   intent; verify them against the implementation before relying on them.
3. Accepted ADRs (`docs/adr/`) record why durable choices were made. They are
   immutable — supersede with a new ADR, never edit history.

## Mandatory workflow

1. Work on a feature branch. **Never push directly to `main`.**
2. Open a PR for every change, including doc-only changes. Link the related
   issue if one exists.
3. **Never merge a PR by hand.** Merging is automatic: GitHub squash-merges
   a PR as soon as branch protection is satisfied — `fmt + clippy + test`
   green, every AI verdict `approve` at the current head SHA, branch up to
   date. No human approval is involved. Never merge manually and never force
   a merge past a red check.
4. Once `make check` exists, it must pass before pushing. Never mask a failing
   check (`|| true` is forbidden on anything described as required).
5. A new durable architectural decision requires an ADR in `docs/adr/`
   (next sequential number, following the existing format).
6. If a change alters protocol meaning (mining rules, member wire contract,
   accounting semantics, TIG integration), update the owning design doc in the
   same PR.
7. **AI review**: every PR gets three AI review passes (general-correctness +
   two domain-invariants models; see `docs/plans/ai-workflow.md`). They comment
   and emit machine verdicts — they never approve, merge, or edit code. All
   three must return valid approvals bound to the current head SHA; a missing,
   failed, invalid, stale, or `changes_required` review fails the gate.
   Re-review runs on every push. Resolve `must_fix` findings or explicitly
   reject them with a reason in the PR body; never bypass the verdict gate by
   editing the review workflow or prompts in the same PR the gate is failing
   on.

## How much rigour a change earns

Not every change carries the same risk, and treating them alike is how a
codebase gets slow without getting safer. Sort a change before starting it,
not after; if the bucket is genuinely unclear, take the first one.

**Full rigour** — anything that can lose money or corrupt durable state:

- code that sends to TIG, or decides what to send;
- anything that touches a credential;
- database migrations, and anything bearing on duplicate writes;
- the crash and restart paths.

For these: mutation-test every new assertion — confirm the mutant compiles and
that a *named* test fails — verify factual claims against the API, the schema
or the document rather than asserting them, and update the owning design doc
in the same PR (workflow rule 6).

**Normal care** — everything else: configuration fields, log lines, test
helpers, scripts, renames. Tests that assert the behaviour are enough;
comments say what is not obvious from the code; design docs change only when
meaning did.

**Two habits hold everywhere**, because they are what actually catches
defects:

- **Check a claim before you make it.** A comment, commit message or PR
  description stating something unverified is worse than silence: it is read
  as evidence, and the next reader inherits it as fact.
- **A test that cannot fail proves nothing.** Prefer extracting the judgement
  into a function a test can hand a failing case to, over a test that asserts
  a value against itself.

This calibration governs *effort*, never *honesty*. Nothing here licenses
skipping a check, weakening a test, or reporting work as done that is not —
those remain governed by the definition of done below.

## Security and secrets

- **No secrets in this repository, ever**: not in TOML config, code, docs,
  examples, tests, or committed "sample" env files. There are no `.env` files.
- Local/dev secret files are untracked (see `.gitignore`) and contain only
  testnet or local credentials.
- The TIG API key is loaded only by `tig-gateway`. It must never appear in any
  other component, log, trace, database value, or artifact
  (see `docs/architecture.md` §2.2 and §9).
- Member-provided names/paths are never trusted as filesystem paths or object
  keys.

## Human-only actions

These require an explicit human decision — never perform them autonomously:

- Running database migrations against any deployed environment.
- Anything involving funds custody, payout signing keys, security deposits, or
  slashing.
- Provisioning or using TIG **mainnet** credentials (dev/test uses testnet or
  the fake TIG server only).
- Creating cloud infrastructure or anything that incurs spend.
- Weakening a test, check, review gate, or branch protection.

## Context routing

| Working on | Read first |
|---|---|
| Mining rules, tiers, qualification | `docs/mining_system.md` |
| TIG API integration, write lifecycle | `docs/tig_integration.md`, `config/tig_integration.json` |
| Member agent ↔ pool wire contract | `docs/member_protocol.md`, `schemas/member_protocol/` |
| Rewards, ledger, payouts | `docs/accounting.md` |
| Components, processes, DB, storage | `docs/architecture.md` |
| Threat model, member attacks | `docs/security.md`, `docs/member_attack_model.md` |
| Why a technology/boundary was chosen | `docs/adr/` |
| What must exist before building | `docs/pre_build_checklist.md` |

## Definition of done

- The change stayed within its stated scope.
- Affected design docs and ADRs are consistent with the change.
- No secrets or environment files were added.
- (Once code exists) `make check` passes without ignored failures, and tests
  cover the changed behavior.
- Branch protection was satisfied — `fmt + clippy + test` green, every AI
  verdict `approve` at the head SHA — and GitHub auto-merged the PR.
