---
title: "Slice 1: TIG gateway and restart-safe protocol state machine"
status: active
created: 2026-08-15
source: docs/pre_build_checklist.md §8 (last item) and §10 step 1
last_verified: 2026-08-15
---

# Slice 1 plan: TIG gateway and restart-safe protocol state machine

Plans describe intended work and may go stale. `docs/pre_build_checklist.md`
§10 owns the slice order; `docs/architecture.md` owns component and storage
boundaries; `docs/tig_integration.md` owns protocol read/write meaning. This
plan **references** those rules and does not restate them — where a criterion
below cites a section, that section is authoritative and this document is
not. Once slice-1 code exists, its tests outrank this plan.

Status meanings: `active` (being executed), `implemented` (superseded by the
code and its tests), `abandoned`.

## 1. Goal

Build the first production vertical slice: a `tig-gateway` that is the only
process holding the TIG API key and the only transmitter of protocol writes,
plus the `pool-controller` protocol state machine that decides writes,
consumes confirmed TIG evidence, and reconciles correctly across restarts.

The slice is complete when the pool can drive one workflow from precommit
intent to **confirmed precommit and a created assignment against live
testnet**, on production foundations (PostgreSQL 18, SQLx migrations,
least-privilege roles, fenced leases, structured telemetry) rather than the
spike's shortcuts — and can be killed at any point in that path without
duplicating a TIG write, losing a confirmed transition, or requiring manual
repair.

The live path stops at the confirmed assignment **by construction, not by
choice**. Reaching a benchmark commitment requires durable package
acceptance, and reaching a proof requires a canonical payload built from a
retained package (`architecture.md` invariants 4 and 5; `mining_system.md`
§10 invariants 5, 6 and 8). Both depend on the member and artifact path that
§2 defers to checklist §10 steps 2–3. A slice-1 live run that reached
`ACTIVE` would therefore have to violate those invariants, so it is not an
acceptance criterion here; the first live run to confirmed `ACTIVE` on
production binaries belongs to the slice that completes durable acceptance
and proof construction.

`mining_system.md` §10 invariant 1 ("one TIG benchmark has exactly one member
owner") is deferred for the same by-construction reason: slice 1 has no
members to own a benchmark. It is **deferred, not ignored** — criterion F6
creates the permanent owner mapping and unverified interval now, filled with
a clearly-marked pool-owned placeholder, so the record the tier gate and
attribution will read exists from the first benchmark rather than being
retrofitted.

The state machine itself is still built and tested across the **whole**
lifecycle in this slice — commitment, sampled nonces, proof, stopped, fraud
and active transitions all run deterministically against `fake-tig`
(criterion F4), where no real package is required. Only the *live* run is
scoped to the confirmed assignment.

The spike already proved the protocol path is viable
([`protocol_spike_report.md`](../protocol_spike_report.md) §1). This slice
proves the *production shape* of its first third.

## 2. Non-goals

Deferred to later slices in `pre_build_checklist.md` §10:

- Member API, member agent, enrollment, or artifact upload (step 2).
- Artifact ingestion, proof construction from packages, retention (steps 2–3).
- The full decision engine's mining policy (step 4). Slice 1 consumes
  `decide(...)` as a pure function against the existing
  `fixtures/decision-engine/v1` cases; it does not extend mining rules.
- Qualifier attribution, ledger, payouts, tiers, deposits (steps 6, 8).
- Availability queue and offer disposition (step 4). The **global** capacity
  gate is not deferred with them: because slice 1 creates precommit intents,
  it builds the `internal_pool_unverified_limit` recount and reservation into
  the admission transaction now (criteria D2, D2a, D2b).
- Any funds handling, mainnet credential, or deployed environment.

Explicitly out of scope for slice 1 even though adjacent:

- **Multi-instance failover.** Fencing must be correct so standby controllers
  are possible (`architecture.md` §7.5), but only one controller runs.
- **GPU compute paths** (issue #35).
- Deleting `crates/spike` — see §7.

## 3. What ships

| Area | Deliverable |
|---|---|
| Binaries | `tig-gateway`, `pool-controller`, `pool-admin migrate` (`architecture.md` §4) |
| Schema | The minimum tables for workflows, write intents, attempts, snapshots, leases, and audit — added only as a query, invariant, or recovery action needs them (`architecture.md` §7.1) |
| Roles | Separate controller, gateway, and migration login roles with the grants that enforce `architecture.md` §6 |
| Config | One non-secret TOML per binary, typed, unknown fields rejected (`architecture.md` §9); the pinned `config/tig_integration.json` loaded and validated |
| Tests | Fixture-driven unit tests, `fake-tig` integration tests, crash/restart tests, one live testnet run |
| Telemetry | The snapshot, workflow, database, and TIG subsets of `architecture.md` §10.2 |
| Docs | Migration and generated-code conventions (`pre_build_checklist.md` §8); any design-doc diff this slice's findings require |

## 4. Acceptance criteria

Each criterion is independently testable. "Fixture" means an existing set
under `fixtures/`; "fake-tig" means `crates/fake-tig`; "live" means pinned
TIG testnet with the identity recorded in
[`protocol-spike.md`](protocol-spike.md) §4.

### A. Configuration and fail-closed startup

- A1. Each binary starts from exactly one explicit TOML path, parses into
  typed structures, and **exits non-zero before serving or claiming work** on
  an unknown field, a missing required field, or a failed cross-field
  invariant (`architecture.md` §9).
- A2. There is no production default for `network`. A config naming a
  non-`testnet` network is rejected by slice-1 builds
  (`tig_integration.md` §13 check 1).
- A3. A decision-affecting configuration digest is computed and stored with
  each decision (`architecture.md` §9).
- A4. No secret is readable from any config file, command-line argument,
  database value, log, or trace. Test: a scripted scan of a full local run's
  logs and database dump for the testnet key finds nothing. The scan reads
  the key **only from the untracked secret file at runtime** — never from the
  repository, a fixture, or CI configuration — and on a match reports the
  file and byte offset only, never the matched bytes. A leak detector must
  not itself become the leak (`architecture.md` §2.2, §9).

### B. Compatibility gate before writes

- B1. The gateway enters `WRITE_READY` only after **all nine** checks in
  `tig_integration.md` §13 pass, and reports which check failed otherwise.
- B2. Each of the nine checks has a test that fails it in isolation and
  asserts the gateway stays out of `WRITE_READY` — including a mutated
  OpenAPI checksum and a pool player ID that disagrees with configured
  identity.
- B3. Losing `WRITE_READY` at runtime (authentication failure, schema
  incompatibility) blocks **new** writes and raises the
  `architecture.md` §10.3 alert without corrupting in-flight intents.

### C. Block-consistent snapshot

- C1. The seven-step algorithm in `tig_integration.md` §9 is implemented, and
  a snapshot whose closing `get-block` returns a different block ID is
  **discarded, not patched** (fake-tig injects the mid-assembly block
  advance).
- C2. An accepted snapshot is persisted atomically with its completeness
  status before any decision derived from it (`tig_integration.md` §9 step 7).
- C3. No component refetches and substitutes a single field into an accepted
  snapshot; within one block each endpoint response is cached by its complete
  request key (`tig_integration.md` §9).
- C4. Every value listed in `tig_integration.md` §12 is read from the
  snapshot. Test: a grep-based check that the observed testnet constants
  recorded in the spike report do not appear as literals in slice-1 source.
- C5. The orchestrator performs no work while the snapshot is incomplete or
  the required active cache is unavailable.

### D. Write intents and idempotency

- D1. The unique constraint `(network, workflow_id, write_kind, generation)`
  exists in the schema and a concurrent duplicate-intent insertion test
  proves exactly one row survives (`architecture.md` §7.3).
- D1a. Benchmark and proof generations are additionally bound to the TIG
  `benchmark_id` in the schema (`architecture.md` §7.3), with a test that a
  generation cannot be reused across a different `benchmark_id`. Slice 1
  fixes the write-intent schema and creates both intent kinds under fake-tig
  (F4, F4a), so the binding is built here even though those writes only
  reach live TIG in a later slice.
- D2. The decision and its `PRECOMMIT` intent commit in **one** transaction
  that also, under the serialized precommit-admission lease, recounts
  authoritative unverified workflows and checks
  `pool_unverified < internal_pool_unverified_limit`
  (`architecture.md` §5.1 step 4, §7.6). A crash between any of them cannot
  leave a decision without its intent, nor an intent without its recount.
- D2d. Inside the same transaction the controller derives the block-specific
  tie input and **persists it with the decision record**
  (`architecture.md` §5.1 step 4; `mining_system.md` §6.3 and §10
  invariant 25). Concretely: derive `challenge_tie_seed` from the domain
  string, network and the anchor snapshot's `block_id`, then `draw_rank[c]`
  for **every** compute-compatible eligible challenge — not only the tied
  ones, since the full map is the audit evidence — supply the ranks to
  `decide(...)` as explicit input, and store the domain string, network,
  anchor `block_id`, the rank map, and on a tie the tied set and the winner,
  exactly as ADR-0005's consequences require. The decision engine derives no
  randomness of its own. This cannot be retrofitted: a decision record is
  written once, and which challenges were compute-compatible and eligible at
  that instant is not recoverable afterwards. Tested against
  `two_way_tie_resolved_by_supplied_draw_ranks`,
  `all_zero_counts_tie_at_factor_zero` and
  `projected_tie_resolved_by_supplied_draw_ranks`, plus the existing §6.3
  vector in `crates/pool-domain/tests/challenge_tie_vector.rs`.
- D2e. The decision transaction uses the **newest complete persisted
  snapshot available when the transaction begins**, and persists that
  snapshot's identity with the decision (`mining_system.md` §6.3). This is
  what closes the pool's remaining re-roll freedom: the draw is fixed by the
  anchor block, so free choice of anchor would restore the re-roll that
  §6.3's seed design removes. Test: with a newer complete snapshot present, a
  stale-but-complete one is not used. Like D2d this fixes transaction shape
  and cannot be retrofitted.
- D2c. The financial half of that transaction splits the way D2b splits the
  capacity gate. Slice 1 **computes and durably records the
  `precommit_reserve` inputs** per intent — `P[s]`, `B[t]`, `F[s,t]`, the
  `X` policy version, and the resulting maximum across proposed tracks
  (`accounting.md` §11.4) — and posts **no accounting batch**. The
  `eligible_collateral[m] - reserved_exposure[m] >= precommit_reserve`
  admission check is member-scoped against a finalized security-deposit
  liability, which slice 1 has no members, tiers, or deposits to supply; it
  lands with the deposits slice (checklist §10 step 8) and must slot into
  this transaction without reshaping it. Recording inputs without a posting
  keeps slice 1 clear of the balanced-batch rules in `accounting.md` §8–§10.
- D2a. `mining_system.md` §10 invariant 23 — the pool never creates a
  precommit at or above `internal_pool_unverified_limit` — holds in slice 1,
  asserted by a test that drives the pool to the limit and shows the next
  precommit intent is refused. A cached metric must never authorize work
  (`architecture.md` §7.6).
- D2b. The `member_unverified < tier_number` half of the §7.6 gate has no
  meaning before members and tiers exist. Tier membership requires a
  finalized joining-fee batch (`architecture.md` §7.6), so it lands with the
  registration and accounting slices (checklist §10 steps 6–8), not with the
  orchestration slice.
  It must slot into this transaction **without reshaping it** — slice 1 fixes
  the transaction's shape, so the global gate is built now rather than
  retrofitted around a live-exercised unguarded path. This holds only because
  F6 creates the per-benchmark owner record and unverified interval that the
  deferred half will read.
- D3. Changing a canonical payload requires a new generation and is refused
  once an earlier attempt may have reached TIG, unless reconciliation proves
  it safe (`architecture.md` §7.3).
- D4. The gateway cannot create an intent or change a workflow. Enforced by
  database grants and tested by attempting both under the real gateway role
  credential (`architecture.md` §6).
- D5. The controller cannot transmit a protocol write. This rests on the
  credential boundary, not on grants: the controller never holds the API key
  (`architecture.md` §2.2, invariant 2), enforced and tested by criterion H2.

### E. Gateway transmission and the attempt ledger

- E1. Every attempt is recorded **before** the request is sent and its
  response recorded separately (`architecture.md` §7.3).
- E2. Precommits use the single unresolved lane; a second precommit cannot
  enter the lane while one is unresolved (`tig_integration.md` §10).
- E3. An ambiguous outcome leaves the intent `OUTCOME_UNKNOWN`, and the next
  action is reconciliation against confirmed TIG state — never a blind
  resend. Test: fake-tig drops the response after processing the write, and
  a server-side write counter asserts exactly one write reached it.
- E4. Precommit reconciliation matches on the full tuple in
  `tig_integration.md` §10 and **stops for operator resolution** when more
  than one candidate matches — with a test that constructs two candidates.
- E5. The client limits and retry rules in `tig_integration.md` §11 are
  implemented as configuration, honor `Retry-After`, and never retry a schema
  or authentication failure.
- E6. Two concurrent writes for the same benchmark are impossible
  (`tig_integration.md` §11).

### F. Confirmation-driven state machine

- F1. Local state advances **only** from the authoritative evidence in the
  `tig_integration.md` §7 mapping. A recorded HTTP 200 never advances a
  workflow. Test: fake-tig returns success for a write that never confirms;
  the workflow stays unconfirmed and the deadline path fires.
- F2. Confirmed precommit settings and details **replace** proposed values
  and become authoritative (`tig_integration.md` §7).
- F3. Workflow revisions are monotonic; a transition attempt at a stale
  revision is rejected (`architecture.md` §7.5).
- F4. The state machine handles every lifecycle mapped in
  `tig_integration.md` §7, including `stopped` (no proof is sent) and fraud,
  driven by the `fixtures/queue-lifecycle/v1/lifecycle.json` cases. **Only
  the protocol-transition and terminal-state assertions of each case are in
  slice-1 scope.** Those cases also assert fault attribution, charges, and
  slot release — `member_package_timeout_failed` expects a `MEMBER`
  attribution and a chargeable `X`, `fraud_confirmed_after_proof` expects the
  member to own the outcome — none of which slice 1 can legitimately
  evaluate, because it has no members. Porting them unscoped would record
  member fault for a benchmark that has no member.
- F4b. No slice-1 terminal reason, fault record, or charge may assert member
  fault or a chargeable member failure — `mining_system.md` §8 allows a
  chargeable tier failure only after fault attribution classifies it as
  `MEMBER`. Asserted negatively. Attributing fault to a member needs
  members (checklist §10 step 2); recording a chargeable failure needs the
  tier counters and the ledger (steps 6–8).
- F4a. The acceptance preconditions hold **on the fake-tig path too**, as
  negative assertions: a benchmark-commitment intent cannot be created
  without a recorded durable-acceptance fact, and a proof intent cannot be
  created without a ready canonical payload for the confirmed sample
  (`architecture.md` invariants 4 and 5; `mining_system.md` §10 invariants 5
  and 8). Slice 1 satisfies the precondition with a stub acceptance record —
  simulated bytes are fine, a missing precondition is not. Without this,
  driving the full lifecycle under fake-tig (F4) would let slice 1 enshrine
  an unguarded commitment path in a passing test, contradicting the same
  invariants §1 uses to bound the live run.
- F4c. The stub of F4a is **compiled out of default builds**, not merely
  refused at runtime. The stub acceptance path sits behind a test-only cargo
  feature on the controller crate, and `scripts/feature-gate.sh` is extended
  to prove a default `pool-controller` build cannot reach it — the same
  mechanism this repository already uses for the spike qualification
  stand-in and that H2 uses for the API key. K3 runs the production binaries
  against live testnet, so the stub must not exist in that binary at all.
- F4d. As defense in depth behind F4c, creating or accepting a stub
  acceptance record is also refused at runtime unless the TIG API endpoint
  resolves to the local fake-tig target. This guard is a **separate config
  surface from `network`** — A2 constrains `network` to exactly `testnet`
  and slice 1 adds no third accepted value, so the check reads the
  endpoint/target field, not the network field. Test: under a live-endpoint
  config, a benchmark-commitment intent cannot be created from a stub
  acceptance.
- F5. Deadlines and remaining block reserve are monitored, and the terminal
  reason is recorded on expiry (`architecture.md` §10.2).
- F6. Every workflow and confirmed benchmark carries an explicit owner
  reference and its unverified interval **from creation**, so the permanent
  `benchmark_id` → owner mapping required by `mining_system.md` §2
  ("Member-owned benchmark"), `architecture.md` §6 (confirmed-assignment
  ownership constraint) and §7.6 exists before members do. Slice 1 has no
  members, so it writes one clearly-marked **pool-owned placeholder** owner
  that cannot be mistaken for a member id, and a test asserts no slice-1
  workflow is attributable to a member. `mining_system.md` §10 invariant 1
  ("exactly one member owner") is therefore not yet satisfiable by
  construction — the same reasoning §1 applies to invariants 4 and 5. The
  mapping is **immutable once written**: checklist §10 step 2 supplies real
  member owners for benchmarks created from then on, and never rewrites the
  placeholder on an existing row. A slice-1 benchmark stays pool-owned for
  life, so it can never later be attributed to a member or have its faults
  charged to one (`mining_system.md` §8). The negative test asserting that no
  slice-1 benchmark becomes member-attributable is permanent, not
  slice-scoped.
- F6a. Because F6's placeholder rows are permanent, `mining_system.md` §10
  invariant 1 ("one TIG benchmark has exactly one member owner") becomes
  permanently false for them rather than merely deferred. The PR that
  implements F6 must therefore amend that invariant in the same change
  (`CLAUDE.md` mandatory workflow §6), carving out pool-owned bootstrap
  benchmarks created before members exist and bounding the carve-out to
  `testnet` via A2. This is a protocol-meaning change to a source-of-truth
  document and needs the owner's explicit approval when it lands — it is
  flagged here, not made here.

### G. Restart safety

- G1. The seven-step restart reconciliation in `tig_integration.md` §10 runs
  before the controller claims work.
- G2. Crash tests kill the process at each of these points and assert
  recovery matches the `architecture.md` §12 row: after decision commit;
  after the attempt row but before the HTTP response; after the response but
  before the outcome commit; and after TIG state changed but before the local
  transition. Each asserts a fake-tig server-side write count of exactly one.
- G3. A lease claimant that lost its fence cannot commit a late result
  (`architecture.md` §7.5 step 4, invariant 8). Test: reclaim with a higher
  fence, then attempt the stale commit.
- G4. A block gap greater than one height is recorded as a data gap and
  alerts; mining may resume but no per-block attribution is invented
  (`tig_integration.md` §10).
- G5. Restart never turns an attempt into a confirmation
  (`architecture.md` invariant 14).

### H. Credential boundary

- H1. The TIG API key is loaded **only** by `tig-gateway`, from a file
  readable only by the gateway identity (`architecture.md` §2.2).
- H2. A `make check` gate asserts the key-loading code path is absent from
  every non-gateway binary — the same shape as the existing
  `scripts/feature-gate.sh`.
- H3. The key never appears in an intent, an attempt row, a log, a trace, or
  an error message (covered by A4's scan).

### I. Observability

- I1. Structured JSON logs carry the correlation IDs relevant to this slice
  (`request_id`, `assignment_id`, `benchmark_id`, `intent_id`, `block_id`,
  `trace_id`) with the allow-listed field policy in `architecture.md` §10.1.
- I2. The snapshot, workflow, PostgreSQL, and TIG metric groups of
  `architecture.md` §10.2 are exported. Metric labels carry no unbounded IDs.
- I3. Durable intents store the originating trace ID so work resumed after a
  restart stays correlated (`architecture.md` §10.1).
- I4. Every `architecture.md` §10.3 alert that slice 1 can reach fires in a
  test:
  - TIG writes disabled by compatibility or authentication failure;
  - the latest accepted snapshot more than two target blocks old, or any
    recorded block gap;
  - **a TIG write outcome ambiguous for more than two target blocks** — this
    one is load-bearing here, because E3 leaves an intent `OUTCOME_UNKNOWN`
    and E4 stops for operator resolution on a multi-candidate precommit
    match; without the page, "stops for operator resolution" stalls
    indefinitely with nobody told;
  - the oldest ready control-plane job exceeding two target block intervals;
  - database capacity above 80%, or repeated transaction/lease failures; and
  - any required process crash-looping or not ready.

  The remaining §10.3 alerts are out of slice-1 scope because their subjects
  do not exist yet: package-acceptance and proof-job ages, confirmed samples
  awaiting a ready proof, accepted-artifact loss or checksum failure, and
  attribution/accounting reconciliation. Each lands with the slice that
  introduces its subject.

### J. Database and migrations

- J1. Migrations are forward-only SQL files in one ordered directory, applied
  by `pool-admin migrate` (`architecture.md` §7.1) under the SQLx
  version/checksum migration lock (`architecture.md` §6); no service
  auto-migrates at startup.
- J2. Each migration is tested against an empty database **and** a copy of the
  preceding schema (`architecture.md` §7.1).
- J3. Separate least-privilege roles exist for controller, gateway, and
  migration, and the D4 grant tests run under them.
- J4. No transaction stays open across TIG network I/O
  (`architecture.md` §7.2, invariant 7). Test: assert transaction duration
  bounds under an injected slow TIG response.

### K. Evidence required to call the slice done

- K1. `make check` passes from a clean checkout, including the new gates.
- K2. The full lifecycle scenario — through commitment, sampled nonces,
  proof, stopped, fraud and active — runs deterministically against fake-tig
  in CI with no network.
- K3. **One live testnet run** reaches a confirmed precommit and a created
  assignment, driven by the production binaries, with the run's block
  heights and intent/attempt rows recorded in the PR. Reaching `ACTIVE` live
  is out of scope for the reason given in §1 and is an acceptance criterion
  of the slice that completes durable acceptance and proof construction.
- K4. At least one crash test from G2 is repeated against live testnet on
  the precommit path, not only fake-tig — **with a live equivalent of G2's
  load-bearing assertion**. G2 asserts a fake-tig server-side write count of
  exactly one, which live TIG cannot report, so the live evidence is: after
  the crash and reconciliation, a `get-benchmarks` scan of the pool's
  precommits for the run's decision block shows **exactly one** entry
  matching the `tig_integration.md` §10 reconciliation tuple (player,
  decision block, challenge, algorithm, compute type, selected-track
  settings). That scan and the resulting intent/attempt rows are recorded in
  the PR alongside K3's evidence. A duplicate found there **fails the
  criterion** — it is not absorbed as an operator-resolution case. Without
  this, K4 could be recorded as passing while a duplicate precommit was
  silently confirmed on testnet, which is exactly what
  `tig_integration.md` §10 and `architecture.md` invariant 14 exist to
  prevent.
- K5. Every assertion in `gateway_fake_tig.rs` and `active_fake_tig.rs`, and
  the **failure-path, restart-reconciliation and write-count** assertions of
  `failures_fake_tig.rs`, are ported into slice-1 tests or explicitly
  re-homed with a reason. `failures_fake_tig.rs` also carries member-trust
  assertions — `chargeable_failures`, `MEMBER` attribution and the `f > k`
  circuit breaker (`circuit_breaker_stops_new_commitments`) — which F4b
  forbids slice 1 from asserting; those re-home to checklist §10 steps 2
  and 6–8. The upload, authorization, durable-acceptance and package-format
  assertions in `pool_upload.rs`, `pool_upload_authed.rs` and
  `member_package.rs` are **not** in slice-1 scope — see the §7 table.

## 5. Sequencing

Intended PR order. Each is independently reviewable and leaves `main` green:

1. **Foundations** — config loading, `pool-admin migrate`, first migration,
   roles, telemetry skeleton. Criteria A, J.
2. **Reads and snapshot** — TIG read client, rate limiting, block-consistent
   snapshot, persistence, compatibility gate. Criteria B, C.
3. **Intents and transmission** — write-intent schema, gateway attempt
   ledger, serialized precommit lane, ambiguity handling, against fake-tig.
   Criteria D, E, H.
4. **State machine and reconciliation** — confirmation-driven transitions,
   deadlines, restart reconciliation, crash tests. Criteria F, G.
5. **Live run and close-out** — observability completion, the live testnet
   run to confirmed precommit and assignment, ported spike assertions, plan
   status flip. Criteria I, K.

## 6. Risks and carried questions

- **Fee-basis and collateral reservation** depend on the still-open policy
  values `J[k]` and `X` (`pre_build_checklist.md` §5.2). Slice 1 records the
  reservation inputs per intent as the spike did (PR #26) but does not settle
  the numbers.
- **TIG's pending-benchmark limit and stopped-benchmark release behavior**
  (`pre_build_checklist.md` §5.2, last item) is unsettled and touches gateway
  capacity controls. Slice 1 must not hard-code a limit; if the live run
  reveals the behavior, record it in `tig_integration.md` and tick the
  checklist item in the same PR.
- **`fixtures/tig` v2** (issue #31) has live-verified shapes this slice's
  model validation wants. If slice 1 needs v2 to write honest tests, mint it
  first rather than encoding v1's refuted shapes.
- **Penalty-application block** (issue #33) stays conservative until settled.

## 7. Spike code disposal

`crates/spike` stays in the workspace through this slice, and **slice 1 alone
does not clear it for deletion**.

[`protocol_spike_report.md`](../protocol_spike_report.md) §9 requires the
spike's deterministic test files to be "ported, not preserved in place."
Slice 1 can satisfy only part of that, because it re-implements only the
gateway and state machine.

The report named four files. `crates/spike/tests/` currently holds **six** —
the report's list predates `gateway_fake_tig.rs` being counted and
`pool_upload_authed.rs`, which PR #44 added after the report merged. The
authoritative list is the directory, not the report:

| Spike test file | Behavior it encodes | Ported by |
|---|---|---|
| `gateway_fake_tig.rs` | Attempt recorded before send, HTTP 200 ≠ confirmation, serialized precommit lane, ambiguous outcome | Slice 1 (K5) |
| `active_fake_tig.rs` | Confirmed lifecycle, sampled nonces, idempotent writes | Slice 1 (K5) |
| `failures_fake_tig.rs` | Failure paths, restart reconciliation, write counts | Slice 1 (K5) |
| `failures_fake_tig.rs` | Member trust: `chargeable_failures`, `MEMBER` attribution, `f > k` circuit breaker | Members: step 2; charges and tier counters: steps 6–8 (F4b forbids both here) |
| `pool_upload.rs` | Resumable upload, durable acceptance, receipt, slot re-offer | Checklist §10 step 2 |
| `pool_upload_authed.rs` | Authenticated upload and cross-worker authorization scoping (`member_protocol.md` §3.2, §16) | Checklist §10 step 2 |
| `member_package.rs` | Package format and member-side proof material | Checklist §10 step 2 |

`crates/spike` may be deleted only once every row above is ported or
explicitly re-homed — that is, after the step 2 slice, not this one. Until
then the spike tests stay as the only executable record of the upload,
authorization, and acceptance behavior. A slice that adds a new spike test
file adds a row here in the same PR.

## 8. Definition of done

- Every criterion in §4 has a passing test or recorded evidence, or an
  explicit written waiver in the PR that closes the slice.
- Design-doc diffs required by slice-1 findings are merged in the PR that
  found them (`CLAUDE.md` mandatory workflow §6).
- `pre_build_checklist.md` §10 step 1 is recorded as complete and this plan's
  status is flipped to `implemented`.
