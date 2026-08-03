# TIG Mining Pool — Repository Operating Contract

A mining pool for the TIG (The Innovation Game) protocol. Members run
benchmarks on their own machines via a member agent; the pool owns TIG
credentials, orchestrates precommit/benchmark/proof submission, and settles
rewards through an append-only accounting ledger. Implementation: one Rust
(2024 edition) Cargo workspace — Axum services, PostgreSQL 18 as the durable
workflow bus, S3/filesystem artifact store.

## Project phase

**Pre-build.** This repository currently contains design documents, schemas,
and config — no code. The rules below that reference code, tests, or CI take
effect as those artifacts are created. Until then, changes are edits to design
docs and follow the same PR workflow.

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
3. **Never merge PRs. Only a human merges.**
4. Once `make check` exists, it must pass before pushing. Never mask a failing
   check (`|| true` is forbidden on anything described as required).
5. A new durable architectural decision requires an ADR in `docs/adr/`
   (next sequential number, following the existing format).
6. If a change alters protocol meaning (mining rules, member wire contract,
   accounting semantics, TIG integration), update the owning design doc in the
   same PR.

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

- Approving or merging a PR.
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
- A human reviewed and merged the PR.
