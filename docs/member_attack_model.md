# TIG mining pool: member attack and failure model

Status: attack inventory and v0 tier controls settled; numerical policy values pending  
Last updated: 2026-07-31

This document lists the ways a member can harm the pool after being offered or
assigned work. It exists so that deposit, admission, timeout, suspension, and
slashing rules are chosen against specific exposures instead of being hidden
inside one generic "bad benchmark" score.

It complements [security.md](security.md), which controls hostile input and
credentials, and [accounting.md](accounting.md), which defines custody and the
financial reservation. The mining and payout rules remain authoritative in
[mining_system.md](mining_system.md).

## 1. Vocabulary and the three resources at risk

In this document:

- a **member compute slot** is the member's CPU or GPU availability;
- a **protocol-pending slot** is capacity consumed from precommit until TIG
  treats the benchmark as confirmed for the purpose of its pending-benchmark
  limit;
- a **method-risk reserve** is slashable member collateral retained while TIG
  can still impose a later method-verification penalty; and
- a **benchmark commitment** is the pool's later submission of the Merkle root
  and quality vector. It is distinct from the earlier precommit.
- an **unverified benchmark** is a member-owned benchmark from the pool's
  precommit intent until TIG verifies it or records a terminal stopped,
  expired, or failed outcome; and
- **tier `k`** is a paid membership permitting at most `k` concurrent
  unverified benchmarks.

These resources release at different times:

| Resource | Starts | Normal release |
|---|---|---|
| Member compute slot | Member accepts confirmed assignment | Complete package is durably accepted by the pool |
| Protocol-pending slot | Pool precommits | TIG's exact pending-limit state says the benchmark no longer counts |
| Method-risk reserve | Before pool creates the precommit intent | Method-report/arbitration exposure is terminal |

Freeing the member's machine therefore does not free pool protocol capacity or
financial collateral.

The owner-provided current TIG pending limit is:

```text
pending_limit = max(40, 15% * number_of_confirmed_benchmarks)
```

Here "confirmed" means solution verified for this limit, not merely that a
precommit or benchmark transaction has acquired a TIG `state`. Before this rule
is implemented, the protocol spike must identify its authoritative API/source,
scope, rounding rule, refresh timing, and treatment of stopped benchmarks.
The formula is not present in the pinned public protocol configuration or
contract source and must not be guessed in code.

## 2. Primary benchmark attacks

### M1. Method non-reproducibility

The member returns internally consistent outputs but did not run the algorithm,
runtime, hyperparameters, fuel budget, or other method it claimed. The data may
pass ordinary solution verification and only fail if the member self-reports or
TIG performs method verification later.

Impact:

- TIG can charge the pool a report penalty for each affected bundle;
- the pool can lose the affected reward and reputation; and
- exposure remains after solution verification and after the member compute
  slot is released.

The owner-provided current maximum is `penalty_amount * num_bundles`, on a
**per bundle** basis: a benchmark's exposure is bounded by its bundle count
however many of its nonces are reported. That basis is what
`accounting.md` §11.4's `method_reserve = P[s] * B[t]` assumes, and a
per-nonce basis would not adjust the formula but invalidate it — by a factor
of `num_nonces_per_bundle`, which is a per-track configuration value and not a
small one.

**It is now confirmed, and the answer is the bounded one.**
`tig_integration.md` §14.1 records the charge as
`penalty_amount * min(R, B)`: one penalty per distinct nonce successfully
arbitrated against the benchmark, pooled across every report rather than
applied per report, and capped at the bundle count. The unbounded per-nonce
reading above — the one that would have invalidated the formula by a factor of
`num_nonces_per_bundle` — is not what TIG does. `accounting.md` §11.4's
The bundle count therefore bounds how many times the price is charged, which
is what `accounting.md` §11.4's `method_reserve = P[s] * B[t]` assumed. Two
things it still does not bound: the **price**, which §14.1 determines is read
live at the charge block and can rise afterwards, and what the member actually
holds, which is the multiplier-scaled reserve and below `10_000` bps is
deliberately less (ADR 0010).

The standing of that confirmation is owner statement, not pinned source, and
the distinction is kept because it is real: the code applying a penalty sits
behind `Context` hooks absent from the pinned tree, so nothing in the
repository can check it. A live observation spanning a real report and
arbitration would upgrade that standing; what it would not change is the
formula's shape, which is what the reserve depends on.

The live mainnet configuration currently expresses `reports.penalty_amount` as
`10 TIG`. The pool still reads the live value every accepted block rather than
compiling it in, because §14.1 determined the penalty is governed by the
configuration live when the arbitration is applied and can therefore change
retroactively.

Admission reserves the live maximum bundle-scaled exposure; a final slash is
the exact evidenced loss, not the maximum reservation automatically.

Local hidden re-execution may reduce the probability of submitting this work,
but it is only a candidate control. It cannot prove all work was honestly
executed and does not replace collateral.

### M2. Hostile, malformed, or excessive package data

Instead of the exact assigned proof-material package, the member can send:

- an absent, malformed, or false Merkle root;
- too many roots, qualities, outputs, files, or records;
- an oversized or never-ending upload;
- a decompression bomb or pathological archive;
- invalid lengths, nonce coverage, checksums, identities, encodings, or
  integers;
- conflicting chunks or package generations; or
- data crafted to consume CPU, memory, disk, file descriptors, parser time, or
  network connections.

Impact:

- the precommit fee has already been spent;
- the benchmark can occupy pending capacity until recovered;
- ingestion capacity and storage can be denied to honest members; and
- a parser vulnerability could threaten a pool service.

This class is primarily controlled mechanically: exact schemas and file counts,
streamed byte and time bounds, per-member/global quotas, isolated parsers,
checksums, one package generation, and no general-purpose archive extraction.
A mechanically rejected package never becomes a normal benchmark commitment.
The pool should submit the corresponding precommit as stopped when safe and
supported so its pending exposure is bounded.

A structurally valid Merkle root is not proof that the underlying solutions are
valid. That case belongs to M4.

### M3. Slow but correct completion

The member eventually supplies a correct benchmark, but takes much longer than
the pool intended. Until it is resolved, it consumes one protocol-pending slot
and distorts the orchestrator's estimate of in-flight qualifiers.

This is an availability failure, not automatically fraud. V0 does not estimate
a bespoke expected finish time or charge a slow-but-correct benchmark. Instead,
tier `k` caps the member at `k` concurrent unverified benchmarks. At each
accepted block, the pool adds the member's current unverified count `U` and
current TIG-verified/active count `V` to round aggregates. At round close,
`sum(U) > sum(V)` removes the member's tier.

The technical package cutoff remains only to preserve enough TIG lifespan for
stopping, package acceptance, sampling, and proof submission. It is not the
ordinary tier-performance score. A separate pool-wide limit below TIG's full
limit retains recovery headroom. Capacity offers wait in a renewable FIFO queue
when that internal limit is full.

### M4. Missing or solution-invalid completion

The member submits nothing, abandons the assignment, or returns outputs that do
not pass TIG solution verification. A well-formed but invalid package is
particularly important because structural checks alone cannot identify it.

Impact:

- loss of the already-paid precommit fee;
- pending-capacity blockage and balancing distortion;
- pool verification, proof-building, bandwidth, and operator cost; and
- repeated denial of service even when no method-report penalty is charged.

On no delivery or a mechanically rejected package, the pool can attempt the
stopped-benchmark recovery path without making a normal commitment. If invalid
work is discovered only after commitment and TIG sampling, stopping may no
longer be available; the workflow must follow the confirmed TIG terminal state.

Every abandoned, unusable, or solution-invalid benchmark charges its owner
that benchmark's own amount under `accounting.md` §11.6 — the precommit fee,
since it earned no active bundles — from its reserved collateral, and
increments
the round failure count `f`. A benchmark with zero bundles meeting TIG's
minimum verification quality is treated the same way for capacity economics,
without being called fraud. For tier `k`, `f > k` removes the tier at round
close. Collateral cannot restore lost capacity, so the flat concurrency limit,
round liveness rule, global headroom, and joining fee remain necessary.

## 3. Additional member attack vectors

The four primary attacks cover most direct damage, but the admission and
security design must also account for the following variants.

| ID | Vector | Pool impact | Principal control |
|---|---|---|---|
| M5 | Repeatedly finish slowly or alternate slow and fast work | Keeps capacity occupied while avoiding a simple failure counter | Round-average `U > V` tier removal; do not call correct work fraud |
| M6 | Offer false compute type, core count, throughput, runtime, or several logical slots backed by the same hardware | Assignments finish late and multiply pending exposure | Exact-slot qualification, tier concurrency, and no trust in self-reported performance |
| M7 | Create many members, workers, wallets, or credentials to bypass per-member limits | Sybil occupation of pending, upload, and qualification capacity | Non-refundable tier fee per identity, global internal limit, FIFO offer leases, and collateral cannot be reused |
| M8 | Create/cancel/churn capacity offers or disconnect before the pool precommits | Scheduler and API denial without consuming TIG pending capacity | Short offer leases, rate/churn limits, and no automatic financial slash |
| M9 | After pool precommit, never acknowledge the confirmed assignment, abandon it, or suppress heartbeats | Fee loss and pending-slot occupation | Technical cutoff, stopped recovery, the reserved precommit fee charged back (`accounting.md` §11.6), tier concurrency and round failure count |
| M10 | Replay a package, submit it for another assignment, mix benchmark identities, or send conflicting retries | Wrong ownership, duplicate effects, or another member being blamed | Signed assignment binding, exact identity digest, permanent idempotency keys, relational authorization |
| M11 | Return valid but deliberately weak work or throttle hardware | Lower qualifier yield without necessarily violating TIG | No fraud slash; compare observed performance and reduce/stop future assignments |
| M12 | Manipulate reported speed, progress, qualities, or benchmark summaries to influence orchestration or payout | Poor scheduling or false member credit | Treat self-reports as telemetry only; use accepted artifacts and confirmed TIG facts for decisions and payout |
| M13 | Withdraw, reuse, transfer, or race security collateral while exposure remains | Leaves pool unable to recover a later loss | Atomically reserve collateral; a pending withdrawal grants no capacity (`accounting.md` §11.4, §11.6); keep method reserve through report closure |
| M14 | Exploit a TIG fee, penalty, deadline, schema, or verifier change | Previously safe work becomes under-collateralized or fails in a correlated way | Read live config, version every decision, compatibility breaker, pause rather than blame members |
| M15 | Use stolen worker credentials or compromise a worker to cancel, upload junk, or claim capacity | Same workflow damage as the account, plus a charge the owning member cannot contest in-system | Worker keys cannot change funds; revocation/recovery; preserve evidence. There is no appeal before a charge (`accounting.md` §11.6), so a member whose worker is compromised pays and then asks the pool out of band — the reason worker revocation matters more under this model than under the previous one |
| M16 | Craft parser data to escape the ingestion sandbox, reach credentials, logs, paths, or internal services | Service compromise and possible protocol-key theft | Isolated no-secret parser, generated paths, no egress, resource limits, redacted telemetry |
| M17 | Claim durable receipt or pool corruption after sending different/incomplete bytes | Disputed fault and attempted avoidance of consequences | Chunk/package hashes, immutable receipt, accepted-object hash, append-only event history |
| M18 | Coordinate accounts so failures are staggered below individual thresholds | Sustained pool-wide degradation | Joining fees make cycling costly; global internal limit/outcome breaker bounds pool exposure |
| M19 | Trigger or exploit a pool/runtime-wide bug that makes many honest packages fail | Mass charging and demotion of honest members | Correlation analysis; classify common-version clusters as compatibility incidents. The classification no longer prevents the charge — `accounting.md` §11.6 charges regardless of cause — so it exists to detect the incident and decide whether to reverse the round under §10 |
| M20 | Try to obtain another member's payout attribution by replaying outputs or exploiting identity/tie handling | Misallocated member rewards | Permanent benchmark ownership, artifact identity binding, deterministic stored qualifier attribution |
| M21 | Mix valid and invalid nonces, falsify some quality values, or otherwise gamble that TIG samples only valid entries | Invalid work may consume verification capacity or escape a small probabilistic sample | Pool retains the whole package; measure full local solution checking; method collateral and immediate circuit breaker remain necessary |
| M22 | Exploit nondeterminism, hardware-specific behavior, or an unpinned runtime to make reproduction ambiguous | False method failures or an avoidable pool/member attribution dispute | Pin the exact runtime/binary/compute class, qualify it, and preserve the assignment and environment evidence |

M11 is deliberately listed even though it is not protocol fraud. The
orchestrator chooses the algorithm and settings, and the payout rule rewards
qualifying bundles, so the correct response to valid but unproductive work is
reduced future trust/capacity, not confiscation.

One attack is intentionally removed by the v0 design: after the complete
package is durably accepted, the member cannot withhold TIG's later sampled
nonce proofs. The pool owns the retained package and constructs those proofs.
Member withholding is therefore confined to the period before durable package
acceptance; a pool proof-builder or submission failure after that point is not
assigned to the member.

## 4. Recovery path for pending-capacity attacks

The pinned TIG contract accepts:

```text
submit_benchmark(
    stopped = true,
    merkle_root = none,
    solution_quality = none
)
```

and records a stopped benchmark with zero active bundles. This is the intended
recovery candidate when a member does not produce an acceptable package.
However, the public source does not establish that this immediately frees the
server-enforced pending allowance described in section 1.

The protocol spike must prove all of the following on the target network:

1. which state starts and ends one unit of pending usage;
2. whether a confirmed stopped submission ends that usage;
3. how quickly it does so and whether write-rate limits can delay recovery;
4. whether a stopped precommit can still incur later verification/report risk;
5. the latest block age at which stopping is accepted; and
6. how the pool reconciles an ambiguous stopped submission after restart.

Until those results exist, the orchestrator must assume a stopped submission
may take time to release capacity and retain explicit headroom.

## 5. Admission controls implied by the inventory

The attack inventory implies four independent gates before a precommit:

```text
financial gate:
    sufficient finalized, unreserved method and fee exposure

member gate:
    active paid tier, qualification, member unverified count, and outstanding
    liabilities permit work

pool gate:
    internal unverified budget permits work, otherwise queue the live offer

compatibility gate:
    TIG config, schemas, runtime, verifier, and pool health are current
```

Passing one gate never bypasses another. In particular:

- a large deposit does not buy unlimited pending slots;
- a clean history does not replace method-risk collateral;
- unused TIG capacity does not justify work during a compatibility incident;
  and
- a free member machine does not mean its previous financial exposure is
  closed.

The internal pending budget should be derived as:

```text
protocol_limit      = authoritative live TIG pending limit
recovery_headroom   = policy amount retained for stop/retry uncertainty
internal_limit      = protocol_limit - recovery_headroom
available_for_work  = internal_limit - current_protocol_pending
```

Exact recovery headroom remains a numerical policy decision and must not be
hard-coded as an unreviewed percentage. The member cap itself is settled as the
tier number: tier `k` allows `k` concurrent unverified benchmarks.

Joining tier `k` costs the non-refundable, versioned fee `J[k]`. Removal means
no tier and no new work, but the member may immediately pay the fee to join
again. Rejoining never clears unresolved benchmarks, reservations, fines, or
method exposure. There is no admission queue or cooldown. The only queue is a
renewable compute-availability queue used when the pool-wide limit is full; v0
orders it FIFO and rechecks every gate before precommit.

## 6. Evidence and consequence classes

Different failures require different evidence and consequences.

| Outcome | Minimum evidence | Immediate consequence | Financial consequence status |
|---|---|---|---|
| Method-verification penalty against a member-owned benchmark | TIG report/penalty, immutable assignment/package, applicable config | Suspend and freeze the evidenced amount | The evidenced protocol loss is charged on the arbitration itself; no attribution step and no appeal (`accounting.md` §11.6) |
| Hostile or mechanically invalid package | Accepted byte hash or rejected upload evidence and deterministic parser reason | Stop/recover benchmark; count the failure | Charge the benchmark's fee (`accounting.md` §11.6); round `f > k` removes tier |
| Technical package cutoff missed, no acceptable package | Confirmed assignment, server receipt history, cutoff/config, absence of pool outage | Stop/recover benchmark; count the failure | Charge the benchmark's fee; round `f > k` removes tier |
| Correct completion within published deadline | Durable package and timestamps | Normal processing | No slash |
| TIG solution-verification failure | TIG terminal evidence tied to immutable member package and pool runtime/config | Review correlated failures; count the failure | Charge the benchmark's fee; round `f > k` removes tier |
| Zero bundles meet TIG minimum verification quality | Accepted/confirmed benchmark outcome and policy | Count as tier capacity failure, not fraud | Charge the benchmark's fee (`accounting.md` §11.6); round `f > k` removes tier |
| TIG-verified work earns no qualifiers | Accepted/confirmed benchmark outcome | Normal tier accounting | No charge |
| Pool, TIG, correlated compatibility, or unresolved fault | Incident evidence | Pause affected path and investigate | **Charged like any other failure** — `accounting.md` §11.6 charges the owning member regardless of cause. The investigation is for the pool's own purposes and, where it concludes the pool was at fault, for deciding whether to reverse under §10 |

Intent need not be proven to stop further exposure. Intent or strong evidence
of deliberate abuse may matter to permanent exclusion, but a slash must still
be tied to the published consequence rule and documented loss. A timeout alone
must not erase evidence of a pool outage, protocol outage, or bad pool-issued
settings.

## 7. Settled v0 controls and remaining values

The v0 control shape is settled:

1. non-refundable `J[k]` to join or immediately rejoin tier `k`;
2. exactly `k` concurrent unverified benchmarks at tier `k`;
3. tier removal when round-average unverified exceeds current
   verified/active, equivalently `sum(U) > sum(V)`;
4. charge every chargeable failure its own amount under `accounting.md` §11.6,
   whatever caused it, and remove tier `k`
   when round failures `f > k`;
5. dormancy with `U = V = 0` has no consequence;
6. a pool-wide internal unverified limit below the TIG limit; and
7. a renewable FIFO compute-availability queue when that internal limit is
   full, with fresh admission checks before precommit.

The remaining implementation values and measurements are:

1. the authoritative pending-limit formula, exact rounding/scope, and stopped
   benchmark behavior;
2. the pool's recovery headroom below that limit;
3. the numerical fee schedule `J[k]`. The failure charge is no longer a
   number to set: `accounting.md` §11.6 derives it from the benchmark's own
   precommit fee and penalty;
4. the technical package cutoff and when the pool submits `stopped`;
5. false-positive handling for chargeable failures — out of band under `accounting.md` §11.6, reversed through §10 where the pool agrees, since there is no in-system appeal; and
6. whether full local solution checking or hidden method re-execution is
   mandatory after its cost and detection value are measured.

None of these open values changes the already settled principle that the member
owns all work in one protocol benchmark or that the pool constructs the sampled
nonce proofs from the durably accepted package.

## 8. Protocol evidence

- Pinned TIG
  [`submit_benchmark`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-protocol/src/contracts/benchmarks.rs)
  accepts the explicit stopped path and defines the Merkle-root and quality
  requirements for normal submissions.
- TIG's public Swagger describes precommit, benchmark, sample, and proof
  submissions at [swagger.tig.foundation](https://swagger.tig.foundation/).
- The implementation must read the target network's live
  `reports.penalty_amount` and challenge fee configuration; observed values are
  evidence, not constants.
