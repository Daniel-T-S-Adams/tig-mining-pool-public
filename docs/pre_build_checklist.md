# TIG mining pool: pre-build checklist

Status: active checklist  
Last updated: 2026-07-31

This checklist defines the work required before committing to the full pool
implementation. Its purpose is to turn the mining design into testable
contracts, verify the risky TIG integration path, and prevent account,
website, and community work from getting ahead of the mining system.

The mining rules in [mining_system.md](mining_system.md) remain the source of
truth. Supporting documents may explain interfaces, implementation, security,
or operations, but must not silently redefine those rules.

## How to use this checklist

- Leave an item unchecked until its stated output exists and has been reviewed.
- Link the resulting document, test, decision record, or report beside the
  completed item.
- If implementation reveals that a settled mining rule is wrong, update the
  design explicitly before changing the implementation.
- Add database fields only when an implemented behavior requires them.
- Use TIG testnet for the first end-to-end integration. Real funds and public
  membership have additional launch gates later in this document.

## Milestones

- **Ready for protocol spike:** sections 1-6 are complete to the extent marked
  as required for the spike.
- **Ready for full product implementation:** the protocol spike in section 7
  succeeds and its findings have been incorporated into the designs.
- **Ready for public funds:** the accounting, custody, security, operational,
  and legal launch gates in section 9 are complete.

## 1. Finish the mining-system source of truth

Output: an internally consistent revision of
[mining_system.md](mining_system.md).

- [x] Record that the pool constructs sampled-nonce proofs in v0.
- [x] State that the member worker executes every nonce, collects every output,
  calculates the ordered quality vector, and produces a complete proof-material
  package.
- [x] Define the proof-material package conceptually as everything the pool
  needs to answer a sample of any nonce in the benchmark.
- [x] Require the pool to durably receive the complete package before it
  submits the benchmark commitment to TIG.
- [x] State that the pool submits the quality vector and Merkle root, waits for
  TIG's nonce sample, constructs the requested proofs, and submits them.
- [x] Keep semantic correctness separate from ingestion: the pool may check
  package identity, completeness, hashes, Merkle consistency, and structure,
  but TIG determines protocol validity.
- [x] Distinguish member compute availability from benchmark protocol state. A
  member may offer compute again after its package has been accepted even while
  the earlier benchmark awaits sampling or verification.
- [x] Decide the exact point at which a member may delete its local package.
  The v0 default should be after the pool acknowledges durable receipt.
- [x] Update the lifecycle states to cover artifact upload, durable acceptance,
  waiting for a sample, proof construction, and proof submission.
- [x] Update member-fault and pool-fault attribution for incomplete packages,
  corrupted uploads, proof-construction errors, missed submissions, and TIG
  rejection.
- [x] State that heavy benchmark artifacts use temporary file or object storage,
  not ordinary relational database fields.
- [x] Update retention rules so the pool keeps proof material until the
  benchmark is active or otherwise terminal and no retry can require it.
- [x] Review every required invariant after making these changes.
- [x] Classify every remaining open decision as one of:
  `required for protocol spike`, `required before product implementation`,
  `required before public funds`, or `deferred beyond v0`.
- [x] Perform a final contradiction and terminology review.

Section completion criteria:

- [x] A reader can identify exactly which party owns every action and artifact
  from capacity offer through proof confirmation.
- [x] No flow requires the member to reconnect after durable artifact
  acceptance.
- [x] No benchmark commitment can occur before complete proof material is
  durably accepted, and no proof submission can occur before the requested
  proofs are ready.

## 2. Pin the TIG integration

Output: [tig_integration.md](tig_integration.md) plus
[`config/tig_integration.json`](../config/tig_integration.json).

Primary references:

- [TIG monorepo](https://github.com/tig-foundation/tig-monorepo)
- [TIG API specification](https://swagger.tig.foundation/)
- the locally pinned TIG source revision used during development

- [x] Select TIG testnet as the first integration target and record how network
  selection is configured.
- [x] Pin an exact TIG monorepo revision rather than depending on a moving
  branch.
- [x] Pin compatible benchmarker, runtime, verifier, challenge-image, and worker
  versions.
- [x] Record the supported member CPU architectures and GPU/runtime
  combinations for v0.
- [x] Catalogue every API read needed for snapshots, lifecycle reconciliation,
  qualifier attribution, and rewards.
- [x] Catalogue every protocol write: precommit, benchmark commitment, and
  proof submission.
- [x] Record request and response schemas, authentication requirements, and
  confirmed-state indicators for those calls.
- [x] Determine the protocol deadlines and block-age limits for precommits,
  benchmark commitments, and proofs from pinned source and live configuration.
- [x] Document API rate limits, caching rules, retry policy, timeouts, and
  backoff.
- [x] Identify every value that must come from live network configuration and
  must not be compiled into the pool.
- [x] Document how a block-consistent snapshot is obtained and reconciled after
  a process restart.
- [x] Record how API/schema incompatibility will stop submissions safely.

Section completion criteria:

- [x] Every external TIG dependency used by the spike has a pinned version or
  an explicit live-configuration lookup.
- [x] The expected protocol lifecycle can be followed using only this document
  and the pinned schemas.

## 3. Specify the member-pool protocol

Output: [member_protocol.md](member_protocol.md) and
[`schemas/member_protocol/v0.1.0`](../schemas/member_protocol/v0.1.0/README.md).

- [x] Define member, worker, and compute-slot identities.
- [x] Define worker enrollment, authentication, credential rotation, and
  revocation.
- [x] Define the capacity-offer request, including compute type, CPU core count
  where applicable, runtime version, and current slot state.
- [x] Define the benchmark-assignment response using the confirmed TIG
  precommit as its authority.
- [x] Include enough assignment identity to prevent work from being submitted
  against the wrong benchmark, network, track, settings, binary, or runtime.
- [x] Define local progress, completion, cancellation, and error messages.
- [x] Specify the proof-material package contents without committing prematurely
  to a storage implementation.
- [x] Require the package to cover the expected nonce range exactly and to
  include the ordered quality vector, per-nonce proof data, Merkle material,
  version metadata, and integrity checks.
- [x] Define package serialization, compression, maximum sizes, and safe
  extraction rules.
- [x] Define resumable, idempotent upload behavior and content checksums.
- [x] Define when an upload is `received`, `structurally accepted`, and
  `durably accepted`.
- [x] Define the acknowledgement that releases the member's artifact-retention
  obligation and compute-slot obligation.
- [x] Decide whether the member may offer its compute while an upload continues
  in the background and document any exposure limit.
- [x] Define heartbeats, timeouts, retry limits, and clock assumptions.
- [x] Define protocol-version negotiation and the behavior for incompatible
  clients.
- [x] Define which errors count against member trust and which are pool or TIG
  failures.

Section completion criteria:

- [x] A member agent and pool server could be implemented independently from
  the schemas and would agree on every lifecycle transition.
- [x] Retrying any member request cannot create a duplicate assignment,
  package, or state transition.

## 4. Define the minimal system architecture

Output: [architecture.md](architecture.md) and
[architecture decision records](adr/README.md) for choices that are expensive
to reverse.

The initial architecture must assign responsibilities for at least:

```text
Member Agent
    -> Pool API
    -> Orchestrator / Decision Engine
    -> TIG Gateway
    -> Artifact Store / Proof Builder
    -> Workflow Database
    -> Accounting Ledger
    -> Monitoring and Operator Tools
```

- [x] Define each component's responsibilities and explicitly excluded
  responsibilities.
- [x] Draw the benchmark data flow, control flow, and trust boundaries.
- [x] Define the signing boundary so TIG credentials never enter member-facing
  services or member machines.
- [x] Keep the orchestrator logically responsible for proof completion while
  allowing bulk artifact and proof work to run in an isolated module or worker.
- [x] Select the implementation language and framework for the backend and
  member agent.
- [x] Select the workflow database and migration mechanism.
- [x] Select temporary artifact storage for local development, testnet, and
  production.
- [x] Define how database state refers to artifacts using identifiers,
  locations, checksums, sizes, and lifecycle state rather than storing large
  outputs inline.
- [x] Define transaction boundaries and idempotency keys for TIG writes and
  accounting operations.
- [x] Define concurrency ownership so two controllers cannot advance the same
  benchmark simultaneously.
- [x] Define configuration and secret-loading boundaries.
- [x] Define the minimum logs, metrics, traces, and operator alerts needed for
  the spike.
- [x] Define local-development topology and the smallest deployable testnet
  topology.
- [x] Record which components may initially share a process and the interface
  that allows them to be split later.

Section completion criteria:

- [x] Every state-changing operation has one owning component.
- [x] Every large artifact has one authoritative temporary location and a
  documented deletion owner.
- [x] Component failures can be restarted without silently losing or duplicating
  protocol work.

## 5. Establish security and accounting rules

Outputs: [security.md](security.md), [accounting.md](accounting.md), and
[member_attack_model.md](member_attack_model.md).

### 5.1 Security baseline required for the protocol spike

- [x] Decide how the testnet TIG API key and signing material are stored and
  accessed.
- [x] Authenticate every worker and authorize access only to its own
  assignments and uploads.
- [x] Treat every member package as untrusted input.
- [x] Define protection against path traversal, decompression bombs, malformed
  records, oversized uploads, disk exhaustion, and resource-exhaustion attacks.
- [x] Validate content length, nonce coverage, checksums, and package identity
  before durable acceptance.
- [x] Define safe temporary-file handling and atomic publication of accepted
  packages.
- [x] Prevent replay of capacity offers, uploads, acknowledgements, and TIG
  submissions.
- [x] Ensure secrets and raw solutions cannot appear in ordinary logs.
- [x] Define audit events for assignment, upload acceptance, protocol writes,
  terminal outcomes, and operator overrides.

### 5.2 Accounting decisions required before accounting implementation

- [x] Confirm the post-delegator-sharing pool coinbase field used as the payout
  base.
- [x] Decide the pool fee and how configuration changes are versioned by block.
- [x] Set the TIG protocol delegator reward share and define changes as a
  separate versioned policy.
- [x] Choose the exact integer unit used by the internal ledger.
- [x] Define proportional-allocation rounding and payout-dust handling.
- [x] Define the zero-qualifier suspense procedure.
- [x] Define the block confirmation/finality rule before credits are posted.
- [x] Define how corrections are represented without rewriting historical
  ledger entries.
- [x] Define delegated TIG and slashable security-deposit recognition and
  custody separately from earned balances.
  — **Superseded by ADR 0008.** Delegated TIG stays separate and non-custodial,
  but deposits and earned balances are now one member balance in one member
  custody address; `accounting.md` §11.7 is the current rule.
- [x] Settle the dynamic bundle-scaled method reserve and separate per-failure
  reserve shape, fault-attribution/appeal boundary, and collateral return
  rule.
- [x] Define flat tier `k` as exactly `k` concurrent unverified benchmarks,
  with end-of-round removal for `U > V` or chargeable failures `f > k`.
- [x] Define non-refundable paid tier entry/re-entry, immediate rejoining with
  no cooldown/admission queue, dormancy behavior, and persistence of all open
  liabilities across re-entry.
- [x] Define the renewable FIFO compute-availability queue used only when
  `internal_pool_unverified_limit` is full, including stale-offer expiry and
  fresh checks before precommit.
- [ ] Choose the versioned numerical tier fee schedule `J[k]`, per-failure
  charge `X`, and internal-limit recovery headroom.
- [x] Define when credits are automatically paid, whether a minimum applies,
  and the payout cadence.
  — **Superseded by ADR 0008.** Settlement into a member's balance stays
  automatic; paying *out* is member-initiated, with no minimum and no cadence
  (`accounting.md` §12.1).
- [x] Confirm payout-address-change protection and who pays Base gas; payout
  authorization, replay protection, limits, and operator
  recovery.
- [x] Enumerate member attacks separately for method fraud, hostile packages,
  slow-but-correct work, missing/invalid work, capacity abuse, and Sybil
  variants.
- [ ] Verify TIG's pending-benchmark limit and stopped-benchmark release
  behavior, then settle the corresponding member/global capacity controls.

Section completion criteria:

- [x] The spike can run without exposing production credentials or accepting
  unsafe uploads.
- [ ] Accounting implementation cannot begin until its numerical and ledger
  invariants are settled in writing.

## 6. Create deterministic test fixtures

Output: a versioned fixture set and a written list of expected results. Fixtures
may be captured TIG responses, generated cases, or both, but must not contain
credentials.

- [x] Capture or construct a complete block-consistent TIG snapshot fixture.
  — PR #16 (`fixtures/tig/v1`); coverage audited in PR #23, corrected in
  PR #22. Live-verified reshoot tracked in issue #31.
- [x] Create challenge-selection cases, including zero counts and random ties.
  — PR #19 (`challenge-selection.json`): zero counts, all-zero tie, and
  supplied-draw tie resolution
- [x] Create algorithm-selection and track-performance cases.
  — PR #19 (`algorithm-selection.json`): adoption, bans, missing binaries,
  track-best rate, and id tiebreaks
- [x] Create hyperparameter-source cases, including quality ties and missing
  sources.
  — PR #19 (`hyperparameter-source.json`)
- [x] Create CPU and GPU bundle-sizing cases.
  — PR #19 (`bundle-sizing.json`): GPU minimum plus the CPU
  core/nonce-multiple rounding cases
- [x] Create projected-qualifier cases with multiple confirmed in-flight
  benchmarks.
  — PR #19 (`projected-qualifiers.json`)
- [x] Create a complete small benchmark artifact with a known quality vector,
  Merkle root, sampled nonces, proofs, and expected verification result.
  — PR #20 (`benchmark-artifact/v1` golden case; all five recorded in
  `expected.json`)
- [x] Create corrupt and incomplete artifact cases.
  — PR #20: bad checksum, truncated upload, missing manifest entries, wrong
  declared size, missing/duplicate nonce, leaf-hash and Merkle-root mismatch,
  identity mismatch
- [x] Create structurally valid but solution-invalid and method-
  non-reproducible package cases for local screening and fault attribution.
  — PR #20 (`solution-invalid`, `method-non-reproducible`)
- [x] Create collateral cases covering different per-track bundle counts,
  simultaneous reservations, insufficient balance, and a live penalty change.
  — PR #21 (`collateral.json`)
- [x] Create tier cases covering paid join/rejoin, `k`-unverified enforcement,
  outstanding exposure across re-entry, dormancy, round `U > V`, per-failure
  `X`, and the `f > k` removal boundary.
  — PR #21 (`tier.json`)
- [x] Create availability-queue cases covering FIFO ties, more than one member
  and slot, stale/cancelled offers, ready-check replay/expiry, tier removal,
  newly opened global capacity, and a fresh atomic limit failure at promotion.
  — PR #17 (`availability-queue.json`)
- [x] Create lifecycle cases for duplicate requests, delayed confirmation,
  timeout, stopped, expired, active, fraudulent, and restart recovery.
  — PR #17 (`lifecycle.json`)
- [x] Create qualifier-attribution cases, including an equal-quality group that
  crosses the qualifier boundary.
  — PR #18 (`qualifier-attribution.json`: `equal_quality_boundary_draw`)
- [x] Create payout cases covering several members, zero qualifiers, fee
  subtraction, rounding, dust, delayed TIG settlement, and automatic round
  transfers.
  — PR #18 (`payouts.json`). ADR 0008 replaced automatic round transfers with
  member-initiated withdrawal from one balance, so cases 9 and 10 need a `v2`;
  the residual is recorded in the fixture's own README and on issue #31.
- [x] Record the expected output for every fixture independently of the future
  implementation.
  — every set carries expected values and a README recording derivation and
  any refuted/pending annotation

Section completion criteria:

- [x] The decision engine, protocol state machine, proof builder, qualifier
  attribution, and accounting can each be tested without depending on live TIG
  state.
  — `fixtures/` plus `crates/fake-tig` (PR #16, `make smoke`); the spike's
  deterministic suites ran entirely offline against them (PR #30)
- [x] The expected values are reviewed before generated implementation tests
  are accepted as evidence.
  — owner sign-off recorded 2026-08-23 in review of the fixture PR. That
  review thread predates this repository and is not reproduced here;
  the reviewed values themselves are the fixtures under `fixtures/`.
  Fixture residuals stay recorded in each set's README.

## 7. Complete one end-to-end protocol spike

Output: `docs/protocol_spike_report.md`, the spike code, repeatable commands,
and captured non-secret evidence.

The spike should use the smallest implementation capable of proving this path:

```text
testnet precommit
    -> confirmed assignment
    -> member computes every nonce
    -> complete artifact upload
    -> durable pool acceptance
    -> benchmark commitment
    -> TIG publishes sampled nonces
    -> pool constructs proofs
    -> proof submission
    -> confirmed active benchmark
```

- [x] Create and fund the testnet Benchmarker identity required for the spike.
  — PR #25 (plan §4; the spike identity is recorded outside this
    repository, see the note in `plans/protocol-spike.md` §4)
- [x] Submit a valid precommit through the pool's TIG gateway.
  — PR #26 (S1); repeated in every live run
  ([spike report §4](protocol_spike_report.md))
- [x] Reconcile the confirmed selected track and settings.
  — PR #26 (S1): block-anchored reads, never the write response
- [x] Execute the assignment through a prototype member agent.
  — PR #27 (S2): all nonces in the pinned runtime container
- [x] Produce and upload a complete proof-material package.
  — PRs #27 (produce, schema-validated) and #28 (resumable chunked upload)
- [x] Persist the package outside the relational database and record its
  checksum and lifecycle state.
  — PR #28 (S3): filesystem artifact store, §8.3 artifact reference
- [x] Demonstrate that the member compute slot can be offered again after
  durable acceptance.
  — PR #28 (S3): slot released exactly once, re-offer admissible immediately
- [x] Submit the benchmark commitment.
  — PR #29 (S4): only after durable acceptance, live testnet
- [x] Observe TIG's sampled nonces from confirmed state.
  — PR #29 (S4): from `get-benchmarks` confirmed entries only
- [x] Construct every requested proof solely from the pool's retained package.
  — PR #29 (S4): package re-verified before use; tamper refused
- [x] Submit the proof idempotently and observe confirmed protocol state.
  — PR #29 (S4): live repeat refused with no second attempt
- [x] Reach `ACTIVE` with at least one valid test benchmark.
  — PR #29 (`894e4d4f…`, block 1270245) and the second clean run
  (`818b03c1…`; [spike report §4](protocol_spike_report.md))
- [x] Exercise at least one stopped or failed path without misclassifying it as
  member fraud.
  — PR #30 (S5): live stopped benchmark `e74b60fb…`, classified not-fraud
- [x] Against the local fake TIG/verifier, exercise invalid-solution and
  method-non-reproducible packages and prove the member circuit breaker stops
  further commitments. Do not deliberately submit fraudulent work to shared
  TIG testnet without TIG operator approval.
  — PR #30 (S5): fake-tig only, as this item requires; nothing invalid
  touched testnet
- [x] Capture the live penalty/fee inputs and calculate the maximum collateral
  reservation across every proposed track before precommit.
  — PR #26 (S1): both candidate fee bases recorded per intent
- [x] Determine from pinned behavior or TIG confirmation which configuration
  block controls a later method-report penalty.
  — PR #30 (S5): `tig_integration.md` §14.1; live confirmation tracked in
  issue #33
- [x] Restart the controller during at least one pending protocol transition
  and demonstrate reconciliation without a duplicate write.
  — PR #30 (S5): two crash points against fake-tig with server-side
  write-count assertions (deterministic; not a shared-testnet exercise)
- [x] Delete the retained package only after the documented retention condition
  is satisfied.
  — PR #29 (S4) and the second clean run: deletion gated on
  `active_ids.benchmark` ([spike report §4](protocol_spike_report.md))

Measure and record (all consolidated in
[`protocol_spike_report.md`](protocol_spike_report.md) §5):

- [x] total artifact bytes and bytes per nonce;
  — [spike report §5.1](protocol_spike_report.md)
- [x] package creation and upload time;
  — [spike report §5.2](protocol_spike_report.md)
- [x] artifact-ingestion time and peak temporary disk use;
  — [spike report §5.3](protocol_spike_report.md)
- [x] Merkle-proof construction time and memory use;
  — [spike report §5.4](protocol_spike_report.md)
- [x] number and timing of blocks between lifecycle stages;
  — [spike report §5.5](protocol_spike_report.md)
- [x] TIG API call count, observed rate-limit behavior, and retry behavior;
  — [spike report §5.6](protocol_spike_report.md)
- [ ] member CPU/GPU time spent outside actual nonce execution; and
  — CPU measured ([spike report §5.7](protocol_spike_report.md)); left
  unticked because the GPU half was not exercised (the spike ran CPU/arm64
  only, `gpu_enabled = false`); GPU measurement tracked in issue #35
- [x] cost/time of full local solution verification and hidden method
  re-execution samples, including the maximum safe sample under deadlines;
  — PR #30 and [spike report §5.8](protocol_spike_report.md)
- [x] throughput lost to each invalid-work path and whether the proposed
  failure charge `X` makes repeated abuse uneconomic; and
  — PR #30 and [spike report §5.9](protocol_spike_report.md) (parameterized
  on the still-open policy value `X`)
- [x] storage and bandwidth projections at the intended initial pool size.
  — [spike report §6](protocol_spike_report.md)

Section completion criteria:

- [x] The full path is repeatable from documented commands.
  — second clean run from a fresh data directory
  ([spike report §3–§4](protocol_spike_report.md))
- [x] No member machine is needed after the pool acknowledges durable artifact
  acceptance.
  — demonstrated in both live runs; commitment→ACTIVE→deletion used only the
  pool's retained object ([spike report §4](protocol_spike_report.md))
- [x] All spike findings that change an assumption have been reflected in the
  mining, integration, member-protocol, architecture, or security documents.
  — findings→diff table in [spike report §7](protocol_spike_report.md)
- [x] The artifact-storage and pool-side proof design is viable at the intended
  initial scale, or a replacement design has been approved.
  — verdict in [spike report §1](protocol_spike_report.md)

## 8. Prepare the repository for full implementation

These tasks can be performed alongside the protocol spike, but must be complete
before multiple product slices are developed in parallel.

- [x] Initialize and document the repository structure.
  — PR #1: pinned toolchain, Cargo workspace, layout documented in `README.md`
- [x] Add a root README describing scope, local setup, and document authority.
  — PR #1 (`README.md`)
- [ ] Choose and record the project license.
  — **open; human decision.** No `LICENSE` file and no `license` field in any
  manifest. Required before any public repository or public membership.
- [x] Add formatting, linting, type checking, and unit-test commands.
  — PR #1 (`make check`: `cargo fmt --check`, `clippy -D warnings`,
  `cargo test --workspace`), extended with the feature gate in PR #44
- [x] Add CI that runs those commands from a clean checkout.
  — PR #1 (`.github/workflows/pr-checks.yml`); AI review added in PR #41
- [ ] Define migration, fixture, and generated-code conventions.
  — fixture conventions exist (per-set READMEs, `fixtures/<set>/v1`, expected
  values recorded independently). Migration conventions landed with slice 1's
  foundations PR: forward-only numbered SQL files in the single ordered
  `migrations/` directory, applied only by `pool-admin migrate` under the
  SQLx version/checksum lock, with no service auto-migrating
  (`architecture.md` §6, §7.1). **Outstanding: the generated-code
  convention**, which no slice has needed yet.
- [x] Define environment configuration without committing secrets.
  — `config/tig_integration.json` pinned and non-secret; untracked
  `secrets/`; policy in `CLAUDE.md` and `architecture.md` §9
- [x] Add contribution rules for changing settled mining behavior.
  — `CLAUDE.md` mandatory workflow (owning-doc update in the same PR, ADR for
  durable decisions, AI review, auto-merge on green)
- [x] Add a short implementation plan with independently testable vertical
  slices and acceptance criteria.
  — [`plans/slice-1-gateway.md`](plans/slice-1-gateway.md) §4 for slice 1;
  §10 below owns the slice order, and each later slice gets its own plan
  before it starts

## 9. Gates before public funds or public membership

These items do not block a local or testnet protocol spike. They do block a
public pool handling deposits, earned balances, or payouts.

- [ ] Complete a threat model covering member abuse, account takeover,
  malicious uploads, credential theft, payout manipulation, and operator error.
- [ ] Move production TIG signing material into the approved production custody
  boundary.
- [ ] Complete backup and disaster-recovery procedures for workflow and ledger
  data.
- [ ] Complete withdrawal and custody-sweep controls, reconciliation,
  alerting, and an emergency pause procedure.
- [ ] Test restoration and replay from a database backup plus confirmed TIG
  state.
- [ ] Load-test member APIs and artifact storage at or above the intended launch
  capacity.
- [ ] Conduct security review of authentication, uploads, signing, ledger, and
  member withdrawals.
- [ ] Obtain jurisdiction-specific legal and tax guidance for operating the
  pool, accepting deposits, charging fees, and paying members.
- [ ] Publish member terms covering rewards, failures, deposits, fees,
  suspension, payouts, and operator powers.
- [ ] Complete user-facing explanations of qualifier attribution and payout
  calculations.
- [ ] Establish support, incident-response, status communication, and responsible
  disclosure processes.

## 10. Full implementation order after the spike

Once sections 1-7 are complete, build the product in this order:

1. TIG gateway and restart-safe protocol state machine.
2. Member agent, member API, and artifact ingestion.
3. Pool-side proof builder and artifact retention.
4. Orchestration decision process.
5. Persistent reconciliation, operator tooling, and monitoring.
6. Per-block qualifier attribution and accounting ledger.
7. Member registration, aliases, authentication, and dashboard.
8. Deposits, the single member balance, withdrawals, and reconciliation.
9. Public documentation and website.
10. Discord community and launch operations.

**Step 1 is complete (2026-09-16).** The TIG gateway and the restart-safe
protocol state machine shipped as slice 1:
[`plans/slice-1-gateway.md`](plans/slice-1-gateway.md), status `implemented`.
Every one of its 61 acceptance criteria has a passing test, recorded evidence,
or a written waiver — the record is
[`evidence/slice-1-criteria.md`](evidence/slice-1-criteria.md), and the live
testnet run behind criteria K3 and K4 is
[`evidence/slice-1-live-run.md`](evidence/slice-1-live-run.md). Four criteria
were waived, each re-homed to the step that owns it: the metrics exporter and
alert tests to step 5, three member-dependent lifecycle cases to step 2, and
J4's transaction-duration measurement to step 5.

The product must advance in vertical slices. Each slice should include its
minimum schema, migration, APIs, implementation, tests, observability, and
documentation rather than creating a speculative full database or all service
scaffolding in advance.

## Final readiness checks

### Ready for protocol spike

- [x] Sections 1-4 are complete for the spike path.
- [x] The security baseline in section 5.1 is complete.
- [x] The proof and lifecycle fixtures required by the spike exist.
  — section 6
- [x] Testnet credentials, compute, and temporary artifact storage are ready.
  — PR #25 (funded testnet identity, key at `secrets/tig-testnet-api-key`);
  pinned runtime container and local artifact directories exercised in
  PRs #27–#29

### Ready for full product implementation

- [x] The end-to-end protocol spike succeeds and is repeatable.
  — two live runs, the second from a fresh data directory
  ([spike report §3–§4](protocol_spike_report.md))
- [x] The spike report contains measured capacity data rather than estimates
  alone.
  — [spike report §5–§6](protocol_spike_report.md). One measurement gap
  remains: GPU member overhead was never exercised (CPU/arm64 only), tracked
  in issue #35; it does not change the section 7 verdict.
- [x] Spike findings have been incorporated into the source-of-truth documents.
  — findings→diff table in [spike report §7](protocol_spike_report.md);
  surviving open questions are issues #31, #33 and #35
- [x] Repository checks run successfully from a clean checkout.
  — `.github/workflows/pr-checks.yml` on every PR and push to `main`
- [x] The first vertical implementation slice has written acceptance criteria.
  — [`plans/slice-1-gateway.md`](plans/slice-1-gateway.md) §4

### Ready for public launch

- [ ] Accounting, deposit, and payout rules are complete and tested.
- [ ] Every section 9 launch gate is complete.
- [ ] A staged private pilot has completed successfully before open
  registration.
