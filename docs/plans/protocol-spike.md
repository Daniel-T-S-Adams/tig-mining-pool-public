---
title: End-to-end protocol spike
status: active
created: 2026-08-03
source: docs/pre_build_checklist.md §7, issues #9–#15
last_verified: 2026-08-03
---

# Protocol spike plan

Plans describe intended work and may go stale; `docs/pre_build_checklist.md`
§7 owns the authoritative checklist, and code/tests outrank this document once
they exist. Status meanings: `active` (being executed), `implemented`
(findings folded back, superseded by the spike report), `abandoned`.

## 1. Goal

Prove, against real TIG testnet, the full pool-mediated path:

```text
testnet precommit -> confirmed assignment -> member computes every nonce
  -> complete artifact upload -> durable pool acceptance
  -> benchmark commitment -> TIG publishes sampled nonces
  -> pool constructs proofs -> proof submission -> confirmed ACTIVE benchmark
```

with the smallest implementation capable of it, while measuring everything
`pre_build_checklist.md` §7 lists. The deliverable is
`docs/protocol_spike_report.md` plus design-doc corrections — **not** the
spike code.

## 2. Non-goals

- Production-quality code, schema, or service topology (architecture §4 is
  implemented later, in vertical slices).
- Accounting, payouts, deposits, tiers, or any funds handling.
- Public membership, member authentication beyond a stub, or the full member
  protocol (only what the path above needs).
- Performance beyond the §7 measurement list.
- Mainnet anything. Failure to reach testnet must never redirect to mainnet
  (`tig_integration.md` §2.1).

## 3. Where spike code lives, and its disposability

- All spike code goes in **`crates/spike/`** — one crate, multiple binaries
  (`spike-gateway`, `spike-member`, `spike-pool`), plus whatever scratch
  modules it needs. It may take shortcuts (SQLite or flat files instead of
  PostgreSQL, in-process queues) **except** where the checklist explicitly
  tests a guarantee (durable acceptance, restart reconciliation, idempotent
  writes) — those must be honestly durable.
- **Spike code is disposable.** It is deleted or archived after the spike
  report merges; production slices re-implement per `architecture.md`. Do not
  polish it, do not grow `pool-domain` types for it beyond what it truly
  needs, and do not let review of it block on style.
- `crates/fake-tig` and `fixtures/` are **not** spike code — they are
  permanent test infrastructure and stay to production.

## 4. Prerequisite: testnet Benchmarker identity (human-only, issue #9)

The one step no agent performs. Runbook (`tig_integration.md` §4,
`architecture.md` §2.2):

1. **Offline**: create or select the testnet account key pair. The signing
   key never enters this repository, any runtime process, TOML, env vars, or
   an agent session.
2. **Offline**: sign exactly this message with the account key
   (lowercase address):
   `I am signing this message to prove that I control address <address>`
3. Obtain the API key via `POST /request-api-key` on
   `https://testnet-api.tig.foundation` with the address and signature.
4. Store the API key at **`secrets/tig-testnet-api-key`** (path is
   `.gitignore`d via `secrets/`), permissions `0600`. This file is the only
   place the key exists; the spike gateway reads only this path.
5. Fund the account's fee balance for testnet precommits (testnet top-up;
   consult TIG operators/docs for the current faucet or top-up route — not
   pinned in this repository).
6. Verify readiness: `get-player-data` for the address shows a positive
   `available_fee_balance`. Record the address (public, safe to commit) in
   this plan when done: **pool testnet address: _TBD_**.

## 5. Environment and sequencing

Every phase develops **against `fake-tig` first** (deterministic, free), then
runs **against pinned testnet** (`tig_integration.md` §2). The fake server and
live testnet must never share a config profile; the API-key file is used only
by the testnet profile. Client limits, retry rules, and the 60/110/120
guardrails come from `config/tig_integration.json` — never hard-coded.

Phases map 1:1 to issues; each lands as its own PR(s) with its issue's
acceptance criteria; `pre_build_checklist.md` §7 boxes get ticked with links
as evidence arrives.

| Phase | Issue | Depends on | Summary |
|---|---|---|---|
| S1 | #10 | identity (§4) | Gateway prototype: persisted write intents → serialized precommit lane → attempt/response ledger → confirmed assignment from block-anchored reads. Verifies the fixture READMEs' envelope/shape assumptions against live testnet; captures live fee/penalty inputs and max collateral across tracks. |
| S2 | #11 | S1 (or fixture assignment) | Prototype member agent: execute every nonce, quality vector, Merkle material, package per `member_protocol.md` §10 — must pass the golden-fixture verifier (`fixtures/benchmark-artifact/v1`). |
| S3 | #12 | S2 | Resumable chunked upload → quarantine → verified durable acceptance (ordered saga) → immutable receipt → slot re-offer while the benchmark continues. |
| S4 | #13 | S3, S1 | Benchmark commitment (only after durable acceptance) → sampled nonces from confirmed state → proofs solely from the retained package → idempotent proof submission → **ACTIVE on testnet** → retention-conditioned deletion. |
| S5 | #14 | S4, fake-tig | Failure paths: stopped/failed without fraud misclassification (testnet); solution-invalid and method-non-reproducible + circuit breaker (**fake-tig only** — never submit deliberately bad work to shared testnet without TIG operator approval); controller restart mid-transition → reconciliation without duplicate writes; method-report penalty config block determined and recorded in `tig_integration.md`. |
| S6 | #15 | S1–S5 | Measurements consolidated into `docs/protocol_spike_report.md`; second clean run proves repeatability; every assumption-changing finding lands as a design-doc diff; viability verdict on artifact storage and pool-side proofs. |

## 6. Acceptance criteria

The authoritative per-item list is `pre_build_checklist.md` §7 (checkboxes)
combined with each issue's acceptance criteria — this plan deliberately does
not duplicate them. Spike-wide exit criteria (§7 completion criteria):

- The full path is repeatable from documented commands (evidenced by a second
  clean run).
- No member machine is needed after the pool acknowledges durable acceptance.
- All assumption-changing findings are reflected in the mining, integration,
  member-protocol, architecture, or security documents.
- The artifact-storage and pool-side proof design is declared viable at the
  intended initial scale, or a replacement design is proposed.

## 7. Measurements to capture (checklist §7)

Recorded incrementally per phase, consolidated in S6:

- total artifact bytes and bytes per nonce (S2/S3)
- package creation and upload time (S2/S3)
- artifact-ingestion time and peak temporary disk use (S3)
- Merkle-proof construction time and memory use (S4)
- number and timing of blocks between lifecycle stages (S1–S4)
- TIG API call count, observed rate-limit and retry behavior (S1–S4)
- member CPU/GPU time spent outside actual nonce execution (S2)
- cost/time of full local solution verification and hidden method
  re-execution samples, including the maximum safe sample under deadlines (S5)
- throughput lost to each invalid-work path and whether failure charge `X`
  makes repeated abuse uneconomic (S5)
- storage and bandwidth projections at the intended initial pool size (S6)

## 8. Known questions the spike must answer

Carried from fixture-work findings (see the fixture READMEs for detail):

1. **Fee basis discrepancy**: `mining_system.md` §6.8 (per-bundle) vs
   `tig_integration.md`/upstream (per-nonce) — resolve against pinned
   `rewards.rs`/fee code and live testnet in S1, then correct the losing
   document.
2. **Response envelopes, `get-algorithms` shape, enum casing, activation
   timing** — the four assumptions in `fixtures/tig/v1/README.md`; verify in
   S1 and mint fixture `v2` if any is wrong.
3. **§6.3 block-derived tie value** — derivation unspecified; propose and
   document in S1 findings.
4. **zstd frame bytes / package SHA-256** — pin real compressed bytes in S2
   and upgrade `fixtures/benchmark-artifact` placeholders.
5. **Method-report penalty configuration block** — determine in S5
   (checklist item), record in `tig_integration.md`.

## 9. Secrets and safety rules (restate; violations stop the spike)

- API key only in `secrets/tig-testnet-api-key`, read only by the gateway
  binary, never logged/traced; redact on error paths.
- No mainnet URLs in any spike config; `mainnet_enabled` stays `false`.
- No deliberately fraudulent submissions to shared testnet (fake-tig only)
  without recorded TIG operator approval.
- Spike state (SQLite/files) lives under an untracked `data/` directory.
