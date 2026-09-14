# Protocol spike report

Status: final (S6, issue #15)  
Date: 2026-08-04  
Sources: `docs/pre_build_checklist.md` §7, `docs/plans/protocol-spike.md`
(now `implemented`), phases S1–S5 (PRs #26–#30, issues #10–#14)

This report closes the end-to-end protocol spike: it consolidates every §7
measurement, proves repeatability with a second clean run from a fresh data
directory, records the viability verdict, and indexes every
assumption-changing finding against the design-document diff that absorbed
it. Raw evidence (ledgers, run reports, packages, classification documents)
is preserved untracked under `data/spike-*/` on the spike host; public
cross-checks use `GET /get-benchmark-data?benchmark_id=…` on
`https://testnet-api.tig.foundation`.

## 1. Summary and verdict

The full pool-mediated path — testnet precommit → confirmed assignment →
member computes every nonce in the pinned runtime container → complete
artifact upload → durable pool acceptance → benchmark commitment → TIG's
sampled nonces → pool-constructed proofs from the retained package alone →
idempotent proof submission → confirmed **ACTIVE** benchmark →
retention-conditioned deletion — ran **twice end-to-end on live testnet**
from documented commands (§2), the second time from a completely fresh data
directory (§3). The stopped path also ran live without fraud
misclassification, and the invalid-work paths plus the member circuit
breaker ran deterministically against fake-tig (S5).

**Verdict: the artifact-storage and pool-side proof design is viable at the
intended initial pool size, with at least two orders of magnitude of headroom
on every measured axis.** No replacement design is needed. Justification, at
50 members × 24 benchmarks/day (§6): ingest bandwidth ~16 MB/day, peak
temporary disk ~0.3 MB, proof construction hundreds of microseconds per
benchmark at ~8 MiB peak RSS, and a protocol-write load 4.8× inside the
serialized POST lane's capacity. The measured quantities scale linearly in
nonces per benchmark (~1.1–1.4 KB/nonce compressed for c008), so even
1,000-nonce benchmarks stay trivial (~1.7 GB/day ingest, ~25 MB resident).
The known risks that survive the spike are catalogued in §8 — none blocks
beginning the production slices.

Key spike-wide facts:

- **No member machine is needed after durable acceptance** — demonstrated in
  both live runs: commitment, sampled-nonce proofs, ACTIVE observation, and
  deletion consumed only the pool's retained accepted object (re-verified
  before every use); the member's directory was never touched after the
  receipt, and S3 additionally proved the slot re-offer at that boundary.
- **HTTP 200 is never confirmation**: every lifecycle advance came from
  block-anchored confirmed reads, and both live runs produced exactly one
  attempt per write with no retries needed and no rate-limiting observed.
- **Fee basis settled from pinned source** (§7 row 1): the precommit fee
  multiplies `per_nonce_fee` by **bundles**, not nonces.

## 2. Canonical runbook

The documented command sequence, exactly as executed by both clean runs (the
second run's driver is preserved verbatim at
`data/spike-report/run-live.sh`). Binaries are the spike crate's
(`cargo build -p spike`); the API key file is read only by `spike-active`
(the gateway role); `spike-member` and `spike-pool` never see it.

```bash
PLAYER=<the spike testnet address; held outside this repository>
KEY=secrets/tig-testnet-api-key           # never printed, copied, or committed
GW=data/<run>/gateway MEMBER=data/<run>/member POOL=data/<run>/pool
COMMON=(--player "$PLAYER" --api-key-file "$KEY" --data-dir "$GW")

# 1. Precommit intent -> serialized POST lane -> testnet
spike-active precommit "${COMMON[@]}" --challenge c008 --algorithm c008_a001
# 2. Confirmed assignment from block-anchored reads (never the 200 response)
spike-active await-assignment "${COMMON[@]}" --poll-secs 15 --max-secs 900
BENCH=<id from $GW/assignment-<id>.json>
# 3. Member: every nonce in the pinned runtime container (pulled by digest),
#    quality vector, Merkle material, schema-conformant package
spike-member run --assignment "$GW/assignment-$BENCH.json" \
  --config config/tig_integration.json --data-dir "$MEMBER"
PKG=<package_id from $MEMBER/package-$BENCH/upload-declaration.json>
# 4. Pool: resumable chunked upload -> quarantine -> ordered durable-acceptance
#    saga -> immutable receipt -> slot re-offer
spike-pool run --package-dir "$MEMBER/package-$BENCH" --pool-root "$POOL"
# 5-10. Pool-owned tail; every step resumable from durable ledgers
spike-active commit       "${COMMON[@]}" --package-id "$PKG" --pool-root "$POOL"
spike-active await-sampled "${COMMON[@]}" --benchmark-id "$BENCH"
spike-active prove        "${COMMON[@]}" --package-id "$PKG" --pool-root "$POOL"
spike-active submit-proof "${COMMON[@]}" --benchmark-id "$BENCH"
spike-active await-active "${COMMON[@]}" --benchmark-id "$BENCH"
spike-active retire       "${COMMON[@]}" --package-id "$PKG" --pool-root "$POOL"
```

The S5 stopped path replaces steps 3–10 with
`spike-active stop --benchmark-id "$BENCH"` followed by `await-stopped`.
Guardrails (poll intervals, ≥5 s POST spacing persisted in `post-lane.json`,
block-age limits) come from `config/tig_integration.json`, never hard-coded.

## 3. Repeatability: the second clean run

Run on 2026-08-04 from a **fresh, empty data directory**
(`data/spike-report/`) using only the commands in §2, with no state carried
from any earlier phase: benchmark **`818b03c19fc7c28d71c59c9c09791ef8`**,
the spike player identity, c008/`c008_a001`, track
`s=baseline` (TIG-selected), 1 bundle × 10 nonces, `aws_t4g`. One precommit,
0.001 TIG (`available_fee_balance` 9.996 → 9.995 TIG, recorded before/after
from public reads). The run completed precommit → ACTIVE → deletion with no
retries, no manual intervention, and no code changes. Public cross-check:
`GET /get-benchmark-data?benchmark_id=818b03c19fc7c28d71c59c9c09791ef8`.

The member directory received no reads or writes after the durable-acceptance
receipt (19:28:04Z); everything from commitment onward used only the pool's
retained object — restating §7's completion criterion: **no member machine is
needed after the pool acknowledges durable acceptance.**

## 4. Live-run timelines

### 4.1 First full run (S4, PR #29) — benchmark `894e4d4f2b865ee33eb85e334931bc6d`

| Stage | Height | Wall (UTC, 2026-08-04) | Δblocks |
|---|---|---|---|
| Precommit submitted (anchor 1270237) | — | 13:49:05 | — |
| `block_started` (TIG) | 1270237 | — | — |
| Precommit confirmed | 1270238 | 13:50:56 observed | +1 |
| Member execution + package (10 nonces) | — | 13:50:57–13:51:01 (3.3 s) | 0 |
| Upload + durable acceptance | — | 13:51:18 | 0 |
| Commitment submitted | 1270239 | 13:51:24 | +1 after confirm |
| Benchmark confirmed, sampled `[0, 5, 2]` | 1270240 | 13:51:59 observed | +1 |
| Proof payload built (54 µs) | — | 13:52:08 | 0 |
| Proof submitted | 1270240 | 13:52:13 | 0 |
| Proof confirmed (`submission_delay` 4) | 1270241 | 13:53:56 observed | +1 |
| **ACTIVE** (`active_ids.benchmark`) | **1270245** | 13:57:08 | +4 |
| Retention deletion | 1270245 | 13:57:23 | 0 |

`block_started` → ACTIVE: **8 blocks, ~8 minutes**.

### 4.2 Second clean run (S6, this report) — benchmark `818b03c19fc7c28d71c59c9c09791ef8`

| Stage | Height | Wall (UTC, 2026-08-04) | Δblocks |
|---|---|---|---|
| Precommit submitted (anchor 1270574) | — | 19:26:11 | — |
| `block_started` (TIG) | 1270574 | — | — |
| Precommit confirmed | 1270575 | 19:27:57 observed | +1 |
| Member execution + package (10 nonces) | — | 19:27:57–19:28:04 (7.5 s total, 2.7 s execution) | 0 |
| Upload + durable acceptance | — | 19:28:04 | 0 |
| Commitment submitted | 1270576 | 19:28:05 | +1 after confirm |
| Benchmark confirmed, sampled `[6, 1, 8]` | 1270577 | 19:29:05 observed | +1 |
| Proof payload built (352 µs) | — | 19:29:05 | 0 |
| Proof submitted | 1270577 | 19:29:05 | 0 |
| Proof confirmed (`submission_delay` 4, `block_active` 1270582) | 1270578 | 19:30:06 observed | +1 |
| **ACTIVE** (`active_ids.benchmark`) | **1270582** | 19:34:06 | +4 |
| Retention deletion | 1270582 | 19:34:06 | 0 |

`block_started` → ACTIVE: **8 blocks, ~8 minutes** — the same block profile
as the first run (every stage-to-stage Δ identical), well inside the
`block_started + 110` package guardrail and the 120-block expiry. Both runs
observed `block_active = proof_confirmed + submission_delay` (4).

### 4.3 Stopped path (S5, PR #30) — benchmark `e74b60fbfee18c3fb034992589da2a93`

Precommit anchor 1270371 (16:02:59Z) → confirmed 1270372 → explicit
`{stopped: true}` submission at 1270373 → confirmed stopped
(`details.stopped = true`, `num_active_bundles = 0`) at 1270374: **3 blocks,
182 s**, classified terminal STOPPED / chargeable / **not fraud** (0 proof
intents, 0 `frauds` entries at observation).

## 5. Measurements (checklist §7)

All figures are from live-testnet runs on the spike host (linux/arm64, 2
CPUs and 2 GiB memory capped for the runtime container); fake-tig figures
are labeled as such. "First/second run" refer to §4.1/§4.2.

### 5.1 Total artifact bytes and bytes per nonce

| Package (10 nonces, c008, zstd-19 single frame) | Compressed | Uncompressed tar | Bytes/nonce (compressed) |
|---|---|---|---|
| S2 (`511ca6ea…`) | 13,810 B | 23,040 B | 1,381 |
| First run (`894e4d4f…`) | 13,168 B | 22,016 B | 1,316 |
| Second run (`818b03c1…`) | 11,338 B | 19,968 B | 1,133 |

Member breakdown (second run): `outputs.ndjson` 13,076 B, `manifest.json`
2,158 B, `leaf-hashes.bin` 320 B (32 B/nonce), `qualities.i32le` 40 B
(4 B/nonce). The dominant, variable term is the per-nonce solution string
(challenge-dependent); Merkle material and qualities are fixed at 36 B/nonce.
Working figure: **~1.1–1.4 KB per nonce compressed** for c008.

### 5.2 Package creation and upload time

- Package build (tar + zstd-19 + manifest): 27 ms (S2), 11 ms (first run),
  72 ms (second run).
- Upload (resumable chunked, 4,096 B chunks, every chunk object and range
  ledger fsynced before ack): 19 ms / 4 chunks (S2 package), 18 ms / 4 chunks
  (first run), 25 ms / 3 chunks (second run) — throughput 453–732 KB/s,
  **fsync-bound** (2 fsyncs per chunk). Production chunk sizes
  (`member_protocol.md` §10.3, ≥1 MiB) amortize this ~256×; tracked in
  issue #35.

### 5.3 Artifact-ingestion time and peak temporary disk

Ordered acceptance saga (stream-verify SHA-256 + bounded structural touch →
publish temp→verify→fsync→rename→fsync-parent → atomic state commit):

| Run | Verify | Publish | State commit | Peak temp bytes under pool root |
|---|---|---|---|---|
| S3 | 656 µs | 15.7 ms | 5.2 ms | 29,528 (2.14× package) |
| First run | 828 µs | 4.8 ms | 4.1 ms | 28,243 (2.14×) |
| Second run | 2.7 ms | 24.2 ms | 5.0 ms | 24,477 (2.16×) |

Peak temporary disk is **~2.15× package size** (quarantine copy + accepted
temp copy coexist until the rename). Receipt round-trip (idempotent finalize
retry returning the stored byte-identical receipt): 49–220 µs.

### 5.4 Merkle-proof construction time and memory

From the retained accepted package only, including full re-verification
(recompute whole-package SHA-256, decompress, reproduce every leaf and the
root) before use: 3 sampled proofs in **54 µs** (first run) / **352 µs**
(second run); process peak RSS (`VmHWM`) **4,224 KiB** / **7,808 KiB**, with
no measurable growth across construction. Proof construction is effectively
free at this scale and O(sample × log nonces) in principle.

### 5.5 Blocks between lifecycle stages

Identical profile in both full runs (§4.1, §4.2): precommit submit→confirm
+1; acceptance→commitment submit +1 (same-block work); commitment
submit→confirmed(sampled) +1; sampled→proof submit 0; proof submit→confirm
+1; proof confirm→ACTIVE +4 (= published `submission_delay`);
**`block_started`→ACTIVE = 8 blocks (~8–9 min at ~60 s blocks)**. Stopped
path: 3 blocks anchor→stopped-confirmed. Testnet `blocks_per_round` = 10080.

### 5.6 TIG API call count, rate-limit and retry behavior

Per full run, by endpoint:

| Endpoint | First run | Second run |
|---|---|---|
| `get-block` | 32 | 35 |
| `get-benchmarks` | 14 | 17 |
| `get-challenges` | 1 | 1 |
| `submit-precommit` | 1 | 1 |
| `submit-benchmark` | 1 | 1 |
| `submit-proof` | 1 | 1 |
| **Total** | **50** | **56** |

Plus one `get-binary-blob` per member cold start (2,103,210 B in 1.3–1.5 s).
Polling at the configured 15 s (20 s for ACTIVE) intervals; writes through
the serialized POST lane with ≥5 s spacing persisted across invocations.
**No HTTP 429 and no `Retry-After` was observed on any call in any live
run.** No live write ever needed a retry; ambiguous-outcome and
unsent-intent retry discipline was exercised deterministically against
fake-tig with server-side write-count assertions (S5: exactly one applied
write in every crash scenario).

### 5.7 Member CPU time outside nonce execution

Process CPU (self + docker-CLI children) of the member agent, excluding
container-side nonce computation: **4,580 ms** (S2 cold start: binary
download 1,314 ms wall, container checks), **610 ms** (first run, warm),
**1,520 ms** (second run, cold: fresh binary download 1,533 ms wall).
Against 2.2–2.7 s of actual 10-nonce execution, overhead is 0.6–4.6 s and
dominated by one-time algorithm-binary download and container/manifest
verification — it amortizes toward the warm figure (~0.6 s) under repeated
assignments. GPU: not exercised (spike is CPU-only, `gpu_enabled = false`);
left open in the checklist and tracked in issue #35.

### 5.8 Verification cost and maximum safe sample under deadlines

Measured on the spike host (c008 `s=baseline`):

- Full local solution verification (`tig-verifier` pass over recorded
  output): mean ~118 ms/nonce (99–224 ms observed across runs) → a full
  10-nonce check costs ~1.2 s CPU.
- Hidden method re-execution (runtime + verifier per nonce): **~240
  ms/nonce**; a 3-of-10 sample ≈ 0.72 s CPU. Signature comparison of a
  recorded reproduction sample: 87 µs (S5).
- Deadline structure: durable acceptance landed at block age ≤2 in both
  runs; the screening window sits between acceptance and commitment, bounded
  by the 60-block assignment-age guardrail (and ultimately
  `block_started + 110` for the package). **Maximum safe sample ≈
  `budget_blocks × 60 s / 0.24 s` ≈ 250 nonces per core per block of
  budget.** A conservative 10-block screening budget supports ~2,500
  nonces/core — full re-execution of every nonce of a 10-nonce benchmark
  (2.4 s) or a 3% sample of a 1,000-nonce benchmark (7.2 s) is nowhere near
  the deadline at spike scale.

### 5.9 Invalid-work throughput loss and `X` economics

From S5 (fixture policy stand-ins `k = 2`, `X = 2 TIG`,
`min_verification_quality = 40`; `X` and `J[k]` remain open policy values):

- **solution-invalid**: wastes full member execution+package+upload (~3.3 s
  wall / ~2.4 s CPU per 10-nonce benchmark) + pool ingestion (~6 ms) +
  screening (129 µs) + 0.001 TIG fee + one unverified position for ~3 blocks.
- **method-non-reproducible**: all of the above **plus** the full
  commitment→proof→ACTIVE lifecycle (~8 blocks) and the bundle-scaled
  penalty exposure (`penalty_amount × num_bundles`, reserved per
  `accounting.md` §11.4); deterred by the method reserve, not by `X`.
- **stopped (live)**: 0.001 TIG + 3 blocks/182 s of one unverified position
  + 2 writes; zero proof work.
- **Economics**: pool direct cash loss ≈ 0.001 TIG/failure vs `X = 2 TIG`
  charged — a ≥2000× margin; the circuit breaker caps damage at `k+1`
  failures per membership (0.003 TIG pool loss vs 6 TIG charged + a
  non-refundable `J[k]` re-entry). Abuse is uneconomic whenever `X` exceeds
  ~0.001–0.01 TIG at spike scale, which any plausible `X` clears. The
  breaker refused post-trip offers with zero new intents and zero
  server-side writes, and its state survives restart (S5, fake-tig).

## 6. Storage and bandwidth projections at the intended initial pool size

**Stated initial size: 50 members, each averaging 24 benchmarks/day
(one/hour) = 1,200 benchmarks/day.** Arithmetic from measured per-benchmark
figures (upper-bound package: 13.8 KB; peak-disk factor 2.16; residence
precommit→deletion ~10 min):

| Quantity | Model | At 10 nonces | At 1,000 nonces (linear in 1.4 KB/nonce) |
|---|---|---|---|
| Ingest bandwidth | 1,200/day × package | 1,200 × 13.8 KB ≈ **16.6 MB/day** | 1,200 × 1.4 MB ≈ **1.7 GB/day** |
| Concurrent in-flight | 1,200/day ÷ 1,440 min × 10 min | ≈ 8.3 benchmarks | ≈ 8.3 |
| Peak temporary disk | in-flight × 2.16 × package | 8.3 × 29.8 KB ≈ **0.25 MB** | 8.3 × 3.0 MB ≈ **25 MB** |
| Post-retention residue | receipts/ledgers ~20 KB/benchmark | **~24 MB/day** (prunable) | ~24 MB/day |
| Protocol writes | 3/benchmark | 3,600/day vs lane capacity 17,280/day (5 s spacing) → **4.8× headroom** | same |
| Reads | polling amortizes across all in-flight work | ~11.5 k/day at 15 s cadence, **independent of pool size**, + 1 `get-challenges`/precommit + metadata-cache fills | same |
| Egress to TIG | commitment + proofs | ~1 KB + ~3 branch proofs ≈ few KB/benchmark → **< 10 MB/day** | grows with sample size only |

Every axis is ≥2 orders of magnitude inside a single modest host's capacity;
the S3 filesystem adapter and the `architecture.md` §8.1 S3 backend are both
comfortably viable. Sensitivities: (a) bytes/nonce is challenge-dependent
(solution strings dominate — re-measure per enabled challenge); (b) member
upload throughput at production chunk sizes and streaming structural
acceptance are production-slice work (issue #35); (c) the write-lane
headroom assumes ≤~5,700 benchmarks/day pool-wide — an eventual ceiling to
revisit if the pool outgrows the initial size by ~5×.

## 7. Findings → design-document diffs

Every assumption-changing finding and where it landed (list of diffs
required by checklist §7):

| # | Finding (phase) | Document corrected / evidence |
|---|---|---|
| 1 | **Fee basis is per-bundle**: pinned `tig-protocol/src/contracts/benchmarks.rs:98-99` (@ `ad08d1ea`) computes `base_fee + per_nonce_fee × num_bundles`; `num_nonces` (line 113) never enters the fee (S6) | `mining_system.md` §6.8 (already per-bundle; now carries the pinned citation); `tig_integration.md` §14 (misleading-name discrepancy entry); `fixtures/tig/v1/README.md` + `fixtures/collateral-tier/v1/README.md` annotated — `fixtures/tig/v1/expected.json` is the refuted side, corrected in v2 (issue #31) |
| 2 | `get-algorithms` envelope is `{advances, binarys, codes, player_details}`; `get-player-data` adds top-level `deposits`/`round_earnings` (S1, refuting fixture v1) | `tig_integration.md` §14; `fixtures/tig/v1/README.md`; v2 fixture tracked in issue #31 |
| 3 | Proof-leaf wire form: bare u64 JSON integers; **solution signature preimage is the JSON-quoted string** (`jsonify(solution)`, quotes included); `MerkleBranch` = concat `{depth:02x}{hash:064x}` — all accepted end-to-end by live TIG (S4/S6) | `tig_integration.md` §6.3 (new wire-encoding notes) |
| 4 | Non-power-of-two Merkle promotion rule confirmed against live TIG verification (both runs: 10-leaf trees, proofs confirmed, benchmarks ACTIVE) (S4/S6) | `fixtures/benchmark-artifact/v1/README.md` assumption 4 |
| 5 | Compressed frame bytes are run-scoped; no canonical compression recipe pinned (S3 decision) | `fixtures/benchmark-artifact/v1/README.md` assumption 1 (settled; PR #28 records the reasoning) |
| 6 | Enum key casing lowercase on the wire; activation timing `block_active = proof_confirmed + submission_delay` observed (both runs) — `active_ids` remains the only authority (S4/S6) | `fixtures/tig/v1/README.md` assumptions 3–4 |
| 7 | Method-report penalty governed by configuration live at arbitration/charge time; can apply retroactively → risk buffer required (S5) | `tig_integration.md` §14.1 (landed in PR #30); `accounting.md` §11.5 confirmed as-is; live confirmation tracked in issue #33 |
| 8 | Stopped benchmark = chargeable capacity outcome, not fraud; confirmed live with zero `frauds` entries (S5) | No diff needed — `mining_system.md` §8 + invariant 18 already state this; live evidence recorded here |
| 9 | Block ids are opaque 32-hex strings; testnet `blocks_per_round` = 10080 (S1) | `fixtures/tig/v1` v2 (issue #31); no design-doc rule depended on the old assumption |

`member_protocol.md`, `architecture.md`, and `security.md` required **no
corrections**: every guarantee the spike exercised against them (package
format §10, upload/acceptance §11–§12, artifact reference §8.3, signing
boundary, untrusted-package handling) behaved as written.

## 8. Open questions that survive the spike

Tracked as follow-up issues; none blocks starting checklist §10 step 1:

1. **fixtures/tig v2** with live-verified shapes and corrected fee examples —
   issue #31.
2. **Real member-protocol identity issuance** (every spike run used
   deterministic stand-in identities derived from the benchmark id) before
   the member API / ingestion production slices — issue #32.
3. **Penalty-application block live confirmation** (pinned source cannot
   distinguish arbitration-confirmation from a later charge block) — issue
   #33; reserve conservatively until settled.
4. **`mining_system.md` §6.3 block-derived challenge-tie value derivation**
   (never exercised: the spike chose challenge/algorithm explicitly) — issue
   #34.
5. **Ingestion production hardening + GPU measurements**: streaming
   structural acceptance, ≥1 MiB chunks, fsync amortization, S3-backend peak
   bound, GPU member overhead/package sizes — issue #35.
6. Residuals recorded in fixture READMEs: `jsonify` vs RFC 8785 for
   non-ASCII solution strings; the published chargeable-reason-code list;
   live `min_verification_quality` values; numerical `J[k]`/`X` policy
   values (checklist §5.2, explicitly open).

## 9. Spike-code disposal recommendation

Per plan §3, spike code is disposable and this report — not the code — is
the deliverable. **Recommendation: archive, then delete from the workspace
in a follow-up PR after this report merges.** Concretely: keep
`crates/fake-tig` and `fixtures/` (permanent test infrastructure); delete
`crates/spike` from the Cargo workspace once the first production slice
(checklist §10 step 1, gateway + protocol state machine) has its own
restart-safe ledger tests — the spike's deterministic tests
(`active_fake_tig.rs`, `failures_fake_tig.rs`, `pool_upload.rs`,
`member_package.rs`) encode the acceptance behavior those slices must
re-implement and should be ported, not preserved in place. The git history
and PRs #26–#30 remain the archive; no separate archive branch is needed.

## 10. Evidence index

| Evidence | Location |
|---|---|
| S1 gateway ledgers, first assignment | `data/spike-gateway/` (spike host, untracked), PR #26 |
| S2 package + validation log | `data/spike-member/package-511ca6ea…/`, PR #27 |
| S3 acceptance run report | `data/spike-pool/run-report.json`, PR #28 |
| S4 full-run ledgers + report | `data/spike-active/`, PR #29 |
| S5 stopped-path + fake-tig reports | `data/spike-failures/`, PR #30 |
| S6 second clean run (driver, log, ledgers, balances) | `data/spike-report/` |
| Public benchmark records | `get-benchmark-data?benchmark_id=894e4d4f…`, `…=818b03c1…`, `…=e74b60fb…` |
| Deterministic twins of every live behavior | `crates/spike/tests/` (CI, fake-tig, no Docker) |
