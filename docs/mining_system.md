# TIG mining pool: mining system v0

Status: draft source of truth for the v0 mining system  
Last updated: 2026-07-31

This document defines how the mining side of the TIG mining pool works. It
covers member-owned benchmarks, orchestration, proof-material handoff, TIG
submissions, raw-factor balancing, qualifier attribution, pool payouts,
participant failure tracking, and artifact retention.

The pool website, account registration, member balances and withdrawals,
deployment topology, and Discord community are separate designs. They must use
the mining and payout facts produced by this system rather than redefining
them.

## 1. Goals

The pool exists to let members with relatively small amounts of compute mine
TIG without choosing challenges, algorithms, tracks, hyperparameters, fuel, or
bundle counts themselves.

The v0 goals are:

- operate one TIG Benchmarker identity for the pool;
- let one member own and execute each complete TIG benchmark;
- make the member collect and package the complete benchmark output, then make
  the pool responsible for constructing any later sampled-nonce proofs;
- make every mining decision centrally in the orchestrator;
- balance the pool across challenges using a deliberately simple raw factor;
- attribute the pool's qualifiers to member-owned benchmarks each block;
- divide the pool's post-delegator-sharing Benchmarker proceeds among members
  in proportion to their attributed qualifiers in that block;
- use member deposit and failure history to limit how much work the pool will
  entrust to a member; and
- persist only the data required by implemented behavior, recovery, and
  accounting.

V0 does not split one protocol benchmark across multiple members. It also does
not attempt to optimize expected profit, model competitor work, use TIG's
legacy multiplier in its balancing rule, or independently establish the
semantic correctness of member solutions before submitting them to TIG.

## 2. Terms

### Pool

The service described by this document. The pool owns the TIG API credentials,
makes precommits and subsequent protocol submissions, durably stores accepted
proof material, constructs TIG's sampled-nonce proofs, and receives the TIG
Benchmarker proceeds.

### TIG Benchmarker

The single TIG player identity through which the pool submits all of its
benchmarks. TIG does not know about the pool's internal members.

### Member

A registered pool participant who offers CPU or GPU compute. A member owns
every protocol benchmark assigned to them and is responsible for computing all
of its nonces, collecting their outputs and qualities, and uploading the
complete proof material. The member has no later proof-serving obligation after
the pool acknowledges durable package acceptance.

### Member-owned benchmark

One TIG benchmark assigned in full to one member. A benchmark never contains
work from multiple members in v0. The database permanently maps its TIG
`benchmark_id` to its `member_id`.

Ownership is an attribution and responsibility rule. It does not mean the
member must hold the artifacts or construct proofs after the pool accepts the
package.

### Member compute slot

One independently assignable CPU or GPU capacity unit offered by a member. Its
availability is separate from the TIG lifecycle of benchmarks it previously
computed. In v0 a slot remains occupied through computation, packaging, and
upload, then becomes available when the pool durably accepts the package.

### Proof-material package

The complete member-produced artifact for one benchmark. It contains the
ordered quality vector, the output data for every expected nonce, the Merkle
root and sufficient Merkle data to construct a proof for any nonce TIG may
sample, plus the manifest, version, and integrity metadata needed to interpret
it safely.

The exact serialization and transport format belongs to the member-pool
protocol specification. Regardless of format, the package must be
self-contained: the pool must not need the member to reconnect after accepting
it.

### Durable package acceptance

The point at which the pool has received the complete proof-material package,
passed its mechanical ingestion checks, committed the artifact to its temporary
artifact store, and atomically recorded its location, checksum, size, and
lifecycle state. An HTTP upload response or receipt of the final byte alone is
not durable acceptance.

### Active bundle

A bundle from a verified, active TIG benchmark whose protocol-calculated
quality passed the track's active-quality threshold. The TIG API exposes these
qualities in a benchmark's `average_quality_by_bundle` field.

### TIG qualifier

An active bundle selected by TIG's frontier and cutoff processing for the
current block. A qualifier is not the same as an active bundle.

### Pool-attributed qualifier

A TIG qualifier count attributed by the pool to a particular active bundle and
therefore to the member who owns that bundle's benchmark. Section 7 defines the
attribution procedure.

### Raw balancing factor

The pool's unweighted share of qualifiers in one challenge:

```text
raw_factor[c] = pool_qualifiers[c] / network_qualifiers[c]
```

This is an internal scheduling metric. It is not TIG's protocol challenge
factor because it deliberately omits the legacy multiplier.

### In-flight benchmark

A pool benchmark whose precommit is confirmed and whose TIG-selected track is
known, but which is not yet active or terminal. Unconfirmed precommits are not
included in projected qualifier counts.

## 3. Separation between TIG rewards and pool payouts

TIG calculates the pool's Benchmarker reward using TIG's own active bundles,
qualifiers, cutoff, challenge factors, deposit factors, imbalance, influence,
delegator sharing, and coinbase rules. The pool does not replace or reproduce
that reward calculation for member payouts.

The pool instead reads the confirmed result for each block and applies its own
transparent payout rule.

For block `b`, let:

```text
pool_proceeds[b] = opow.coinbase[pool_player_id]

distributable[b] = pool_proceeds[b] - configured_pool_fee[b]
```

`pool_proceeds` is the coinbase actually retained by the pool after TIG has
applied delegator reward sharing. The gross `opow.reward` must not be used as
the payout base because part of it may already belong to delegators.

Let `q[m,b]` be member `m`'s number of pool-attributed qualifiers in block `b`.
Then:

```text
total_q[b] = sum(q[m,b] for every member m)

member_payout[m,b] =
    distributable[b] * q[m,b] / total_q[b]
```

Consequences of this policy:

- every raw qualifier has equal internal payout weight;
- the pool, not the member, bears responsibility for challenge balancing and
  work selection;
- a member earns from an owned benchmark in every block in which the pool
  attributes qualifiers to its active bundles;
- a valid active bundle that is not a qualifier earns no payout in that block;
- an expired, stopped, inactive, or fraudulent benchmark earns no further
  qualifier payout; and
- legacy multipliers can affect what TIG pays the pool but do not change how
  the pool divides that block's distributable proceeds.

If TIG reports pool proceeds but `total_q[b]` is zero, the system must not
invent an allocation. It records the proceeds in a suspense state and raises
an operator alert. [accounting.md](accounting.md) specifies exact token
rounding, dust handling, the pool fee, delayed TIG settlement, and the single
member balance a round settles into (ADR 0008).

## 4. End-to-end benchmark flow

The member owns the complete benchmark, but the pool owns every TIG protocol
submission.

```text
Member reports idle compute
    -> pool checks whether the member may receive another benchmark
    -> orchestrator obtains a fresh, block-consistent TIG snapshot
    -> decision process creates settings for every active track
    -> pool submits a TIG precommit
    -> pool waits for confirmed precommit and TIG-selected track
    -> pool sends the confirmed benchmark to its owning member
    -> member computes every nonce
    -> member collects outputs and qualities and builds the proof material
    -> member uploads the complete proof-material package
    -> pool mechanically checks and durably stores the package
    -> pool acknowledges acceptance and releases the member's compute slot
    -> pool submits the benchmark commitment to TIG
    -> TIG publishes sampled nonces
    -> pool constructs and submits the required proofs from retained artifacts
    -> TIG verifies the benchmark
    -> benchmark becomes active, stopped, expired, or fraudulent
    -> an active benchmark participates in per-block qualifier attribution
    -> pool artifacts are deleted when no longer required
```

### 4.1 Capacity offer

An idle notification is an offer to execute one complete benchmark. At a
minimum it identifies:

- the member;
- the offered compute-slot identity;
- whether the offered compute is CPU or GPU;
- the CPU core count when CPU compute is offered;
- the TIG-compatible compute type and qualified worker/runtime version;
- the exact slot generation and qualification-spec digest covering those
  compute and runtime facts; and
- whether that compute slot is already reserved, computing, packaging, or
  uploading another benchmark.

The member is expected to keep the offered compute assigned until the
benchmark computation and package handoff finish or fail. V0 deliberately does
not estimate whether the member can finish the chosen nonce count. The agreed
bundle-sizing policy is treated as sufficient to make the assigned work
comfortably finishable. A member who does not finish or does not deliver a
usable package is handled through the member-failure policy.

V0 does not assign new work to the same compute slot while its previous package
is still uploading or awaiting durable acceptance. Once acceptance is
acknowledged, that slot may immediately offer and receive another benchmark;
the earlier benchmark may still be waiting for TIG sampling, verification, or
activation. This keeps the first implementation simple while removing all
block-waiting time from the member's compute path.

If the pool-wide unverified-benchmark budget is full, an otherwise eligible
offer enters the compute-availability queue instead of causing a TIG
precommit. This is a work-dispatch queue, not a tier-admission queue. Its v0
ordering is FIFO by durable server acceptance time, with `offer_id` as the
deterministic tie-break. The offer holds no TIG capacity and remains valid only
while the worker renews its short lease. Before promotion, the pool rechecks
the member tier, the member's current unverified count, slot qualification,
collateral, compatibility breakers, and the global limit. The member must
confirm that the compute is still available before the pool precommits.

An expired or cancelled offer leaves the queue without a member penalty. A
later policy may give trustworthy members priority, but FIFO is the only v0
priority rule and any change must be versioned.

While a live FIFO queue exists, a newly received offer cannot bypass it merely
because a capacity position becomes visible between scheduler passes. New
offers append to the queue; the Controller offers each open position to the
oldest eligible live entry first.

### 4.2 Precommit

The orchestrator creates one algorithm choice and valid track settings for
every currently active track in the selected challenge. TIG chooses one track
during precommit processing. The pool therefore does not send work to the
member until the precommit is confirmed.

The confirmed precommit is authoritative for:

```text
benchmark_id
selected track
num_bundles
num_nonces
rand_hash
hyperparameters
fuel_budget
compute_type
block_started
fee_paid
```

The pool never gives its TIG API key or signing material to a member.

For the protocol spike, the pool does not assign a confirmed precommit at age
60 blocks or greater. Member proof material must be durably accepted before
`block_started + 110`, preserving ten pool-owned blocks before the local
`block_started + 120` workflow expiry for commitment, sampling, proof work, and
confirmation. These measured-and-reviewable spike guardrails are defined by
the TIG integration and member-pool contracts.

### 4.3 Member computation and package ingestion

The member worker runs every nonce in the confirmed benchmark and collects its
complete output. It uses the pinned TIG verifier to calculate the ordered
per-nonce quality vector and constructs the Merkle root and proof material. It
then uploads one self-contained proof-material package.

The member-side verifier run is part of producing the required benchmark data;
it is not protocol confirmation. V0 does not rerun semantic solution
verification in the pool or independently decide whether the member's
solutions are correct. Final correctness is determined by TIG.

Before accepting the package, the pool performs the mechanical ingestion checks
required to store it safely and later use it:

- it is for the expected `benchmark_id`;
- its manifest matches the confirmed network, challenge, algorithm, track,
  settings, binary, runtime, verifier, nonce count, and nonce range;
- required per-nonce output records are present without missing or duplicate
  nonce indices;
- the ordered solution-quality vector is present;
- the Merkle root and the material required for every possible Merkle branch
  are present and internally consistent;
- package sizes, encoding, paths, and record shapes comply with the member-pool
  protocol; and
- the package checksum and stored-object checksum agree.

These are completeness and structural checks, not solution validation. A
complete but false package may be submitted and later rejected or penalized by
TIG; that outcome is attributable to the owning member.

The pool acknowledges durable acceptance only after the accepted artifact and
its compact database record can survive a controller restart. That
acknowledgement transfers responsibility for artifact availability and proof
construction to the pool. The member may delete its local copy, disconnect,
and offer the compute slot again; the pool must never depend on a later member
response for that benchmark.

### 4.4 Benchmark and proof submission

The pool must not submit a benchmark commitment until durable package
acceptance. It submits the package's ordered quality vector and Merkle root. If
TIG determines that no bundle passed the active-quality threshold, the
benchmark is stopped, requires no proof, and will produce no qualifiers. A
valid stopped benchmark is not automatically a false benchmark.

Otherwise, after the commitment is confirmed, TIG publishes the sampled
nonces. The pool uses its retained proof material to construct complete proofs
for exactly those nonces, mechanically checks that each proof is for a requested
nonce and links to the submitted root, and submits them. This proof construction
is protocol packaging, not semantic solution verification. The pool waits for
confirmed protocol state before advancing the local benchmark state.

### 4.5 Protocol lifecycle

The local state machine must distinguish at least:

```text
PRECOMMIT_SUBMITTED
    -> PRECOMMIT_CONFIRMED
    -> ASSIGNED
    -> COMPUTING
    -> PACKAGING
    -> PACKAGE_UPLOADING
    -> PACKAGE_RECEIVED
    -> PACKAGE_STRUCTURALLY_ACCEPTED
    -> PACKAGE_DURABLY_ACCEPTED
    -> BENCHMARK_SUBMITTED
    -> BENCHMARK_CONFIRMED
    -> PROOF_BUILDING
    -> PROOF_READY
    -> PROOF_SUBMITTED
    -> PROOF_CONFIRMED
    -> VERIFYING
    -> VERIFIED
    -> ACTIVE
```

Terminal branches include:

```text
STOPPED
EXPIRED
FAILED
FRAUDULENT
```

`VERIFYING` is the wait after a confirmed proof, and `ACTIVE` is membership of
TIG's active benchmark set — two different observations, each with its own row
in `tig_integration.md` §7. Between them sits a third fact the ladder above
does not name: the moment TIG *records the benchmark as verified*, which §6.1
uses to end the unverified interval and release the tier's concurrency. The
pool records that as **`VERIFIED`**.

It is a distinct state and not a synonym for either neighbour. `VERIFYING` is
true while the pool is still waiting and nothing has been published; `VERIFIED`
is true once the verification event has been read; `ACTIVE` is a later and
separate fact that TIG's active set is authoritative for. A pool that recorded
only `VERIFYING` and `ACTIVE` would have no state to enter on the event §6.1
keys the capacity release to, and would either hold the concurrency slot until
activation or release it against an event it had not observed.

`VERIFIED` is not terminal. §9 and §10 invariant 17 treat *active* as distinct
from terminal, and the ladder continues past it.

State transitions must be restart-safe and reconciled against confirmed TIG
state. TIG HTTP acceptance alone is not protocol confirmation.

`PACKAGE_RECEIVED` means all advertised bytes arrived but are not yet trusted
or durable. `PACKAGE_STRUCTURALLY_ACCEPTED` means the bounded mechanical checks
succeeded but storage and workflow state may not yet be committed together.
`PACKAGE_DURABLY_ACCEPTED` is durable package acceptance and is the only state
that permits benchmark submission and release of the member's local retention
obligation.

The compute-slot state is related but separate:

```text
AVAILABLE
    -> RESERVED
    -> COMPUTING
    -> PACKAGING
    -> UPLOADING
    -> AVAILABLE
```

The slot returns to `AVAILABLE` at `PACKAGE_DURABLY_ACCEPTED`; it does not wait
for `BENCHMARK_CONFIRMED`, proof construction, verification, or `ACTIVE`.

## 5. TIG state ingestion

The database supplies two primary product capabilities:

1. the current and projected state needed to decide what work to issue; and
2. the participant history needed to decide how much work to entrust to each
   member.

It also persists the minimum workflow and payout facts required for safe
restart and auditable accounting.

### 5.1 Block-consistent refresh

The controller reacts to each newly confirmed TIG block. It first fetches the
latest block and then anchors all compatible API reads to that same
`block_id`. It must not combine responses from different block snapshots into
one decision.

Relevant TIG data includes:

- block identity, height, round, active IDs, and live configuration;
- active challenges, tracks, bundle sizes, and network qualifier counts;
- active algorithms, adoption, binaries, and per-track qualifier counts;
- the pool's qualifier counts by challenge and track;
- the pool's post-sharing coinbase and gross reward data;
- active pool benchmarks and their bundle-quality arrays;
- recent benchmark lifecycle records needed to find source hyperparameters;
  and
- confirmed fraud and terminal states.

The implementation should respect TIG API rate limits, avoid fetching the same
object repeatedly within one snapshot, and cache benchmark lifecycle data that
remains valid.

### 5.2 Incremental database policy

The physical schema is intentionally not designed in full before the
corresponding behavior exists. Each implementation slice adds only the fields,
indexes, constraints, and migrations it requires.

Early versions may store compact snapshot and decision facts as JSON rather
than normalizing every TIG response. Bulky protocol responses are not retained
merely because they are available.

The first working slice needs to persist only facts in these logical groups:

- members, their recognized deposits, status, and failure history;
- the permanent `benchmark_id -> member_id` ownership mapping;
- benchmark decisions, confirmed settings, lifecycle, and terminal reason;
- the current TIG snapshot fields used by the decision;
- active bundle qualities needed for qualifier attribution;
- per-block pool proceeds, qualifier attribution, and member credits; and
- artifact-upload state plus the accepted package's temporary location,
  checksum, size, and retention state while a benchmark needs its proof
  material.

Whether these groups use separate tables or JSON fields is an implementation
decision made when their first queries and invariants are implemented.

## 6. Orchestration decision process

The decision function has the conceptual signature:

```text
decide(
    offered_compute,
    current_tig_snapshot,
    confirmed_in_flight_benchmarks,
    member_status
) -> benchmark_plan | no_action
```

A benchmark plan contains:

```text
decision block ID
member ID
challenge ID
algorithm ID
compute type
track settings for every active track:
    hyperparameters
    fuel budget
    num_bundles
decision inputs and tie-break results
```

Every decision is made from one confirmed block snapshot and recorded so it
can be explained later.

### 6.1 Member admission

Before choosing work, the pool applies the flat-tier admission policy:

- joining tier `k` costs the versioned, non-refundable fee `J[k]`;
- tier `k` permits at most `k` concurrent unverified benchmarks;
- a benchmark is unverified from creation of its pool precommit intent until
  TIG records it as verified or it reaches a terminal stopped, expired, or
  failed state;
- tier removal does not cancel outstanding benchmarks, reservations, fines,
  or method-verification exposure; and
- a removed member may buy a tier again immediately by paying its joining fee
  again. There is no tier-admission queue or cooldown.

An immediate repurchase does not reset protocol state. Every still-unverified
benchmark continues to count against the newly purchased tier, and unpaid or
reserved liabilities must still pass the financial gate.

The complete set of member financial, liveness, and capacity attacks is
maintained in [member_attack_model.md](member_attack_model.md). After the
decision engine proposes bundle counts for all tracks and before any
precommit, the Controller must reserve at least the maximum **policy-scaled**
assignment reserve across those tracks:

```text
max(
    ceil( live reports.penalty_amount * proposed num_bundles[track]
          * member collateral multiplier bps / 10_000 )
    + exact proposed TIG fee[track]
    for every proposed track
)
```

There is no separate per-benchmark failure charge in this reserve. ADR 0013
derives that charge from the benchmark's own precommit fee, so the fee term
above covers it; carrying both would reserve the same money twice. The slice-1
implementation still has a third term set to zero, which computes the same
amount; issue #61 removes it.

The multiplier is the pool-set per-member value in integer basis points,
default `10_000` and never above it, and it scales the method-penalty term
only — the TIG fee is an outlay the pool certainly makes, so trust does not
discount it. It is fixed into the reservation when the reservation is made and
a later change never reaches an open one. [accounting.md](accounting.md) §11.4 owns the term and ADR 0010
the decision.

**This is a floor against the scaled reserve, not against the full exposure.**
Below `10_000` bps the reservation is deliberately smaller than the maximum
penalty TIG can levy, and the difference is pool risk rather than member
collateral — `accounting.md` §13 item 21 requires the aggregate to be
reported for that reason.

TIG selects the track only after precommit, which is why admission uses the
maximum. The reservation is specific to that member and benchmark and cannot
collateralize another unresolved benchmark. [accounting.md](accounting.md)
defines its lifecycle and slashing boundary.

A deposit or newly paid joining fee does not turn invalid work into valid work
and does not erase an outstanding liability. An accepted package releases its
compute slot, but the benchmark remains unverified for tier concurrency until
TIG verifies it or records a terminal outcome; its method exposure can remain
reserved for longer. Active historical benchmarks do not occupy member compute
slots.

The pool also enforces:

```text
pool_unverified < internal_pool_unverified_limit
```

If this gate alone prevents work, a live availability offer is queued as
defined in section 4.1. A tier grants eligibility and a concurrency ceiling,
not guaranteed immediate work.

### 6.2 Compute-compatible challenges

CPU offers consider only CPU challenges. GPU offers consider only GPU
challenges. A challenge is eligible only when the pool can construct valid
settings for every active track using an eligible algorithm and known source
settings.

### 6.3 Projected raw balancing factor

For each challenge `c`, calculate current raw counts across its active tracks:

```text
pool_q[c] = sum(pool qualifiers on every active track in c)
network_q[c] = sum(all network qualifiers on every active track in c)

raw_factor[c] =
    0                                  if network_q[c] = 0
    pool_q[c] / network_q[c]           otherwise
```

For an eligible algorithm `a` and track `t`, the simple live qualifier rate is:

```text
qualifier_rate[a,t] =
    qualifiers[a,t] / active_bundles[a,t]
```

If there are no active bundles for that algorithm and track, its rate is
unavailable and its expected addition is zero.

For each confirmed in-flight benchmark `j` whose selected algorithm and track
are known:

```text
expected_q[j] = num_bundles[j] * qualifier_rate[algorithm[j], track[j]]
```

No adjustment is made for legacy multipliers, future competitor work,
active-quality probability, verification probability, or forecast confidence
in v0.

For each challenge:

```text
expected_addition[c] =
    sum(expected_q[j] for confirmed in-flight benchmarks j in c)

projected_raw_factor[c] =
    0
        if network_q[c] + expected_addition[c] = 0

    (pool_q[c] + expected_addition[c])
    / (network_q[c] + expected_addition[c])
        otherwise
```

Our expected qualifiers appear in both numerator and denominator because they
would also increase the network total.

Choose the compatible eligible challenge with the lowest projected raw
balancing factor. If multiple challenges tie, resolve the tie with the
recorded, block-derived random draw defined below so the decision is
reproducible.

The draw is a pure function of the decision's anchor snapshot, so an auditor
can re-derive it from the persisted decision record alone. Derive a versioned
seed from the snapshot's immutable block identity, then one draw rank for
every compute-compatible eligible challenge:

```text
challenge_tie_seed = BLAKE3(utf8(
    "tig-pool-challenge-tie-v1" || "\n" || network || "\n" || block_id))

draw_rank[c] = BLAKE3(challenge_tie_seed || utf8("\n" || challenge_id))
```

- `network` is the pool's operating network name, exactly `testnet` or
  `mainnet`.
- `block_id` is the id of the decision's anchor snapshot block — the block
  returned by the snapshot's opening `get-block` call. One decision uses one
  block-consistent snapshot (invariant 10), and the decision must use the
  newest complete persisted snapshot available when its decision transaction
  begins. Because anchored reads require TIG's latest block
  (`tig_integration.md` §8), the anchor is always the newest block the pool
  had observed when the snapshot was opened.
- `challenge_id` is the verbatim challenge id string from the snapshot. All
  three inputs are newline-free ASCII strings, so the newline-joined UTF-8
  encoding is byte-exact and unambiguous.
- `challenge_tie_seed` enters the second hash as its raw 32 bytes.

Compare draw ranks as 32-byte unsigned big-endian integers (equivalently,
lexicographic byte order). Among the tied challenges, the smallest draw rank
wins. If two tied challenges produce equal ranks, the smaller `challenge_id`
in byte order wins.

The seed deliberately contains no per-decision input. Every decision anchored
to the same block uses the same draw, matching the per-block draws of
section 7.1, and the pool gains no per-decision re-roll.

The controller derives the seed and the complete rank map before calling the
decision engine and supplies the ranks as explicit input; the engine never
derives randomness (`architecture.md` §3, §5.1 step 4). Store with the
decision record: the domain string, network, anchor `block_id`, the supplied
rank map, and — when a tie occurred — the tied candidate set and the selected
winner.

Worked example and test vector, using the `fixtures/tig/v1` anchor block
(`network = testnet`, `block_id = block_100080`) with its active challenges
`c001` and `c003` tied on projected raw factor:

```text
challenge_tie_seed
  = BLAKE3(utf8("tig-pool-challenge-tie-v1\ntestnet\nblock_100080"))
  = 2fa3fc89f50afd1b38d2209af931ae2e1e3d49ab555292f0cb5aa07a6d017c32

draw_rank[c001]
  = BLAKE3(challenge_tie_seed_bytes || utf8("\nc001"))
  = 5d0f453f4d0c62aa01a37dcdad7ab31caeb7f14f05c32e321d1125ca7d115eb2

draw_rank[c003]
  = BLAKE3(challenge_tie_seed_bytes || utf8("\nc003"))
  = f16ab636834eae6322acd3d00286a23830bcc8a0f24cc23d1c2e06bce784b41c

draw_rank[c001] < draw_rank[c003]  ->  select c001
```

`crates/pool-domain/tests/challenge_tie_vector.rs` proves this vector.
Rationale, the manipulation analysis, and rejected alternatives are recorded
in ADR 0005.

### 6.4 Algorithm choice

Within the selected challenge:

1. keep active, non-banned algorithms with a usable successful binary;
2. ignore algorithms whose current challenge-wide adoption is zero;
3. select the algorithm with the highest current challenge-wide adoption; and
4. resolve equal adoption deterministically by `algorithm_id`.

TIG does not publish adoption per track. Track-specific performance is handled
separately below.

### 6.5 Track-specific algorithm performance

For every active track, compare eligible algorithms using:

```text
track_qualifier_rate[a,t] = qualifiers[a,t] / active_bundles[a,t]
```

Algorithms with no active bundles on the track do not have a usable rate. An
algorithm is best on the track when it has the greatest available rate. Ties
are resolved deterministically by `algorithm_id`.

This track-best result does not change the already selected challenge-wide
algorithm. It controls the CPU bundle-count rule.

### 6.6 Hyperparameters and fuel

For the selected algorithm and each active track:

1. consider current active, verified, non-fraudulent benchmarks using that
   algorithm and track;
2. find the benchmark containing the highest-quality active bundle;
3. copy that benchmark's hyperparameters and fuel budget; and
4. if highest quality is tied, prefer the most recently confirmed source and
   then the lowest benchmark ID.

If an active track has no valid source benchmark for the selected algorithm,
the orchestrator cannot build the required complete precommit settings and the
challenge is ineligible.

### 6.7 Number of bundles

Bundle counts are selected separately for every possible track because TIG
chooses the final track during precommit processing.

For a GPU member:

```text
num_bundles[t] = min_num_bundles
```

For a CPU member when the selected algorithm is not best on track `t`:

```text
num_bundles[t] = min_num_bundles
```

For a CPU member when the selected algorithm is best on track `t`, let:

```text
n = num_nonces_per_bundle[t]
c = offered CPU core count
m = min_num_bundles

bundle_multiple = c / gcd(c, n)

num_bundles[t] = ceil(m / bundle_multiple) * bundle_multiple
```

This is the smallest number of bundles at or above the protocol minimum for
which:

```text
(num_bundles[t] * num_nonces_per_bundle[t]) mod c = 0
```

V0 does not resize this result using runtime estimates, expected rewards, TIG
fees, or an estimate of whether the member will finish.

### 6.8 Protocol fee

The current protocol computes the precommit fee as:

```text
fee = base_fee + per_nonce_fee * num_bundles
```

Despite its name, current protocol code multiplies `per_nonce_fee` by bundles.
Settled during the protocol spike (S6) against the pinned upstream commit
`ad08d1ea001a73ff5aab3b556d7f59246fece14e`:
`tig-protocol/src/contracts/benchmarks.rs` lines 98–99 compute
`submission_fee = base_fee + per_nonce_fee * PreciseNumber::from(num_bundles)`
while line 113 sets `num_nonces = num_bundles * num_nonces_per_bundle` — the
fee scales with **bundles**, not nonces, exactly as stated here
(`docs/protocol_spike_report.md`; the per-nonce derivation in
`fixtures/tig/v1/expected.json` is the refuted side). At the time this
document was written, mainnet configured `per_nonce_fee` as zero for every
active challenge, so the effective fee was the fixed base fee per benchmark.
This observation is not a constant: the orchestrator reads the live challenge
configuration and requires sufficient pool fee balance before submitting.

The fee check is protocol admission, not a bundle-sizing rule.

## 7. Per-block qualifier attribution

TIG publishes enough information to infer which member-owned pool benchmarks
supplied the pool's qualifiers, except that it does not expose the result of
equal-quality tie-breaking at the individual benchmark level.

For each confirmed block `b`, challenge `c`, and active track `t`:

1. read `Q`, the pool's published qualifier count for `(c,t)` from OPoW data;
2. read the block's active benchmark ID set;
3. select active benchmarks owned by the pool whose confirmed challenge and
   selected track are `(c,t)`;
4. expand every selected benchmark's `average_quality_by_bundle` into local
   bundle entries;
5. attach each entry to the member who owns that benchmark;
6. group bundle entries by quality and visit qualities from highest to lowest;
7. accept complete quality groups while enough qualifier positions remain;
8. when a group of `N` equally ranked bundles crosses the remaining boundary
   of `M` positions, randomly select `M` of those `N`; and
9. aggregate the accepted entries by member to produce `q[m,b]`.

Each local bundle entry has the stable identity:

```text
(benchmark_id, position_in_average_quality_by_bundle)
```

TIG randomly assigns a benchmark's nonces to bundles, but the missing nonce
membership is not needed for pool payouts. Every resulting bundle is still
owned by the one member who owns the whole benchmark, and TIG publishes the
resulting bundle qualities.

### 7.1 Random equal-quality boundary

The tie draw is performed independently in every block, matching TIG's own
per-block random tie handling.

The pool's draw must be random but publicly reproducible. For a boundary group,
derive a versioned seed from immutable block context:

```text
tie_seed = BLAKE3(canonical_encode([
    "tig-pool-qualifier-tie-v1",
    network,
    block_id,
    pool_player_id,
    challenge_id,
    track_id,
    tied_quality
]))
```

Canonicalize the tied bundle entries first. Give each entry a draw rank:

```text
draw_rank[entry] = BLAKE3(tie_seed || canonical_encode(entry.identity))
```

Choose the `M` entries with the smallest draw ranks. A hash collision is broken
by the canonical bundle identity. Store the boundary candidates, seed inputs,
draw ranks, and selected entries with the block payout so every member can
reproduce the result.

The result is the pool's authoritative payout attribution. It agrees with the
quality ordering and qualifier counts published by TIG; only TIG's unexposed
identity-level choice among equal-quality bundles is replaced by the pool's
auditable random draw.

## 8. Member failures and trust

V0 uses flat tiers rather than a reputation score. Tier `k` provides exactly
`k` concurrent unverified-benchmark positions. It does not provide `k` compute
slots, bypass collateral, or reserve a share of the pool-wide capacity.

At every accepted TIG block in which a tier membership is active, the pool
updates these round aggregates without storing a separate row for every
member/block pair:

```text
U[m, b] = member m benchmarks currently unverified at block b
V[m, b] = member m benchmarks currently TIG-verified/active at block b

unverified_exposure[m, round] += U[m, b]
verified_exposure[m, round]   += V[m, b]
sample_count[m, round]        += 1
```

`V` is current verified/active inventory, not a lifetime success count. At the
end of the round, the averages can be compared by comparing their sums because
they have the same sample count:

```text
if unverified_exposure[m, round] > verified_exposure[m, round]:
    remove tier membership
```

This is the v0 slow-work/clogging rule. It deliberately avoids estimating a
different completion deadline for each machine. The technical package deadline
needed to stop work before TIG expiry remains a protocol-safety boundary, not
the ordinary tier-performance metric.

For tier `k`, let `f` be the number of chargeable failed benchmarks owned by
the member during the round. Each failure charges that benchmark's own amount
from reserved security collateral — the sum is not `f` times a constant, and
`accounting.md` §11.6 owns how each is calculated. If `f > k`, the member's
tier membership is removed at round close in addition to those charges. A
benchmark with no bundles meeting TIG's minimum verification quality counts as
a chargeable failure for this tier policy even though it is not described as
fraud.

**Cause does not enter either count.** Pool, TIG, compatibility and unresolved
incidents were previously neither tier failures nor chargeable member
failures. They are now both: `accounting.md` §11.6 charges a member
independently of fault, and `f` counts every chargeable failure the member
owned. The word "attributed" has gone from this rule because nothing is being
attributed — ownership is the whole test, and §10 invariant 1 already makes
ownership exact.

That is the owner's decision and it has a cost this document should state:
pool-caused failures arrive together. One proof-construction defect can fail
many members' benchmarks in a single round, and that round closes with every
affected member past their `k` demoted and charged. Nothing here reverses a
demotion — repurchase costs `J[k]` again — so unwinding one runs through
`accounting.md` §10's correction path.

Method-verification loss remains a separate calculation, not a separate
question of blame: the pool reserves the live bundle-scaled TIG exposure and
charges the evidenced loss under §11.6's table.

Removal means `tier = NONE`, concurrency zero, and no new precommits. The
member can immediately purchase any allowed tier by paying `J[k]` again; there
is no cooldown. Rejoining does not clear open benchmarks, round evidence,
security reservations, fines, or method penalties. Because evaluation occurs
at round close, a removed member normally begins a newly purchased membership
in the new round with new round aggregates.

Removal cancels that member's queued offers and unconfirmed ready checks because
they contain no protocol commitment. A precommit intent already created before
removal remains an outstanding member-owned benchmark and follows its normal
terminal path. After rejoining, the member sends fresh availability offers.

A member with no current unverified or verified/active benchmarks contributes
zero to both aggregates. Dormancy by itself therefore has no tier consequence.
The repeated non-refundable joining fee is the economic deterrent against
building and cycling dormant identities.

The system must distinguish the following outcomes.

### Member-attributable false or failed work

- the package supplied by the member is incomplete, malformed, or internally
  inconsistent even though the received bytes match the member's declared
  checksum;
- required nonce data is missing, duplicated, or does not match the confirmed
  assignment;
- the member abandons an assigned benchmark or misses the computation or
  package-upload deadline before durable acceptance;
- TIG finds the member-owned benchmark fraudulent or non-reproducible; or
- another confirmed member-side fault prevents the benchmark from completing.

The incomplete, abandoned, solution-invalid, and similar pre-verification
outcomes are chargeable failures of the owning member. They no longer wait on
a `MEMBER` classification: `accounting.md` §11.6 charges independently of
cause, so [member_attack_model.md](member_attack_model.md)'s fault
classification remains useful for understanding and operating the pool, and is
no longer a gate on whether a charge happens. Method non-reproducibility is
charged by the same table under its own bundle-scaled term rather than as a
second flat charge.

### Outcomes that are not member fraud

- a complete, valid benchmark has no active bundles;
- active bundles fail to become qualifiers;
- the pool selected poor challenge, algorithm, hyperparameter, fuel, or bundle
  settings;
- the pool incorrectly rejected a structurally usable package;
- the pool acknowledged durable acceptance and then lost or corrupted the
  package;
- the pool constructed a wrong proof from a correct accepted package;
- the pool missed, duplicated, or incorrectly formed a protocol submission;
- the member-to-pool upload failed because of confirmed pool infrastructure
  failure; or
- TIG or pool infrastructure failed independently of the member.

The first item is still a chargeable tier failure under the explicit v0
capacity policy when no bundle meets TIG's verification threshold. That charge
does not relabel the outcome as fraud. Ordinary non-qualifying work that TIG
does verify is not a tier failure merely because it earns no qualifier.

After durable package acceptance, availability of the member or deletion of
the member's local artifacts is not something the member is expected to
maintain — §9 releases both obligations at that point, and the pool must never
ask for either again.

It does not follow that such a benchmark is free for the member. Under
`accounting.md` §11.6 a charge no longer turns on whose failure it was, so a
benchmark that fails after durable acceptance is charged to its owner like any
other. The same holds where the origin of a corruption is never established:
the system still records an unresolved operational outcome for the operator's
purposes, and that record no longer decides whether a charge happens.

The database records the outcome and reason for every member-owned benchmark,
the per-round `U`/`V` aggregate, every chargeable failure, the tier decision,
and its policy version. It must retain enough event detail to correct an
incorrectly attributed failure. Reversing an incorrect result uses a
compensating event and ledger entry rather than rewriting history.

## 9. Artifact retention

Before durable acceptance, the member retains its complete local package and
is responsible for successfully uploading it. Once the pool acknowledges
durable acceptance, the member may delete that copy and has no further
artifact-retention or proof-serving obligation.

The pool's accepted proof-material package is temporary operational data. It
must remain available through:

- benchmark commitment submission and confirmation;
- publication of TIG's sampled nonces;
- sampled proof construction and submission;
- proof confirmation; and
- TIG's verifying phase.

Once TIG confirms the benchmark as `ACTIVE`, the large full-output and Merkle
artifacts may be deleted. They are not required for TIG's later public method
verification, which re-executes public benchmark settings and compares the
result with public quality data.

Artifacts for stopped, expired, failed, or fraudulent benchmarks may be
deleted once the benchmark is terminal and no pending protocol submission
depends on them.

Heavy proof material belongs in the configured temporary file or object store,
not in relational database values. The database stores only the package
identity, location, checksum, size, format version, acceptance state, retention
state, and deletion result needed for restart safety and auditing.

The pool permanently retains the compact record needed for ownership,
decisions, trust, attribution, and payouts, including:

```text
benchmark ID and member ID
confirmed challenge, algorithm, track, compute type, and settings
bundle and nonce counts
active bundle qualities
Merkle root and package integrity metadata
protocol lifecycle and terminal reason
algorithm, runtime, verifier, and worker versions where available
per-block qualifier attributions
member payout records
```

## 10. Required invariants

Implementation must preserve these invariants:

1. One TIG benchmark has exactly one member owner, **except** for pool-owned
   bootstrap benchmarks created before members exist. Those carry a permanent
   pool-owned placeholder rather than a member, are confined to `testnet` —
   `tig_integration.md` §2.1 and slice-1 criterion A2 constrain the network,
   and the schema refuses a mainnet placeholder outright — and can never be
   re-owned: the mapping is immutable
   once written, so a bootstrap benchmark's faults can never be attributed or
   charged to a member (§8). The carve-out exists because the mapping must be
   written when a benchmark is created and the registration slice arrives
   later; without it the invariant would be satisfied only by back-filling an
   owner nobody chose. Every benchmark created once members exist has exactly
   one member owner, with no exception.
2. A member never receives the pool's TIG credentials.
3. Work starts only from a confirmed precommit and its confirmed selected
   track.
4. All settings used by the member match the confirmed precommit.
5. A benchmark commitment is never submitted before durable acceptance of a
   complete proof-material package.
6. Durable acceptance means the pool can complete the benchmark without the
   member reconnecting.
7. Once durable acceptance is acknowledged, artifact loss, corruption, proof
   construction, and proof availability are pool responsibilities. This is a
   statement about duty, not about money: `accounting.md` §11.6 charges the
   owning member for a failed benchmark whatever caused it, so the pool owing
   the member correct handling and the member being charged when the pool
   fails at it are both true. The invariant governs what the pool must do,
   never who pays.
8. Every pool-constructed proof uses the retained package whose Merkle root was
   submitted for that benchmark.
9. A member compute slot becomes available at durable package acceptance and
   does not wait for TIG sampling or verification.
10. One orchestration decision uses one block-consistent TIG snapshot.
11. Unconfirmed precommits do not contribute projected qualifiers.
12. The raw balancing factor never includes the legacy multiplier.
13. Per-block attributed qualifier counts sum to TIG's published pool qualifier
   count for every challenge and track.
14. Equal-quality boundary selection is random, block-specific, stored, and
   reproducible.
15. A block payout uses post-delegator-sharing pool coinbase, not gross OPoW
    reward.
16. Member payout weights are raw attributed qualifier counts.
17. Heavy proof artifacts are never deleted before the benchmark is active or
    otherwise terminal.
18. Valid low-quality or non-qualifying work is not mislabeled as fraud.
19. Every protocol write and payout operation is restart-safe and idempotent.
20. A slot receives work only when its exact current generation, compute facts,
    and runtime inventory have a successful qualification.
21. A member package deadline leaves the protocol reserve defined by the
    member-pool contract before local workflow expiry.
22. Tier `k` never has more than `k` member-owned unverified benchmarks,
    including benchmarks created before the current tier purchase.
23. The pool never creates a precommit at or above
    `internal_pool_unverified_limit`; queued capacity offers consume no TIG
    pending capacity.
24. Tier removal or repurchase never clears an outstanding benchmark,
    reservation, fine, or method-verification exposure.
25. Challenge-factor tie resolution is block-derived per section 6.3, stored,
    and reproducible from the persisted decision record.

## 11. Decisions intentionally left open

These choices are outside the settled mining logic. They are classified by the
first delivery gate they block so they do not silently enter implementation and
do not unnecessarily block earlier validation.

### Required for the protocol spike

- choose any assignment-specific package limits below the settled protocol
  ceilings using the spike host and S3 capacity;
- choose the deployed testnet S3 region, bucket names, orphan grace period, and
  capacity alarms without changing the storage contract; and
- define only the physical database fields and indexes required by the spike's
  first workflow and restart-recovery slice.

The TIG pins and live lookups are now settled in
[tig_integration.md](tig_integration.md). Language, process, database,
migration, temporary-storage, safe-publication, and deletion ownership are now
settled in [architecture.md](architecture.md).

### Required before full product implementation

- numerical values for `J[k]` and
  `internal_pool_unverified_limit`/recovery headroom, all as versioned policy.
  `X` is no longer among them: the failure charge is derived from the
  benchmark's own precommit fee and penalty rather than chosen
  (`accounting.md` §11.6, ADR 0013);
- review the technical package deadline and global capacity headroom using
  spike measurements without replacing the settled tier rule;
- the **trust-label mechanism** is no longer open: it landed on 2026-09-16 as
  the per-member collateral multiplier `M` (§6.1, `accounting.md` §11.4,
  ADR 0010). It gives a trusted member more concurrent bundles for the same
  balance by lowering what each bundle reserves, rather than by raising an
  admission limit above §6.1's formula — which this entry forbade and which
  stays forbidden. §8's flat-tier rule is no longer held open by it, and a
  tier still does not bypass collateral. The entry is kept rather than deleted
  because `accounting.md` §14 and this list both recorded the constraint;
- use spike measurements to decide the mandatory pool-side solution-verification
  and hidden method-reexecution checks before benchmark commitment;
- account, alias, and enrollment-ticket user experience. **Login is decided**:
  the member's connected wallet is the account identity (ADR 0011), which also
  settles account-level recovery — there is none, and a lost wallet ends both
  the balance and the member's authority to recover a worker under
  `member_protocol.md` §3.3;
- public member APIs, operator tooling, monitoring, and support workflows; and
- each additional physical database schema slice when its first behavior is
  implemented.

### Required before accounting or public funds

- the exact definition, recognition, and custody of a member deposit;
- production TIG credential and payout-key custody;
- payout reconciliation, production signing limits, and emergency controls;
  and
- the legal, tax, and member-terms requirements for the operating
  jurisdiction.

### Deferred beyond v0

- splitting one TIG benchmark across multiple members;
- allowing a compute slot to receive its next assignment while its prior
  package is still uploading;
- redundant or member-independent artifact escrow before durable pool
  acceptance; and
- decision models that add profitability forecasts, competitor forecasts,
  confidence adjustments, or TIG's legacy multiplier.

These open decisions must not change the settled v0 principles silently. Any
change to benchmark ownership, raw-factor balancing, qualifier attribution, or
block payout requires an explicit design update. Changing which party retains
proof material or constructs sampled-nonce proofs also requires an explicit
design update because it changes the member-pool protocol and failure boundary.

## 12. Protocol references

The implementation must follow the target network's live configuration and
the pinned TIG revision used by the project. The exact network, source,
container, schema, API, compatibility, and recovery contract is defined in
[TIG integration contract](tig_integration.md).

The exact member-worker identities, authentication, capacity offers,
assignments, proof-material package, upload, acknowledgement, retry, and trust
contract is defined in [member-pool protocol](member_protocol.md) and its
versioned schemas. Storage and deployment choices may implement that contract
but must not change its wire semantics silently.

The component ownership, TIG credential boundary, workflow database,
artifact-store implementation, transaction, concurrency, observability, and
deployment contract is defined in [system architecture](architecture.md) and
its [decision records](adr/README.md).

The local source paths below are convenient research references. They do not
override the revision and immutable image digests pinned by the integration
contract:

- [TIG benchmark contracts](../../tig-monorepo/tig-protocol/src/contracts/benchmarks.rs)
- [TIG OPoW calculation](../../tig-monorepo/tig-protocol/src/contracts/opow.rs)
- [TIG reward calculation](../../tig-monorepo/tig-protocol/src/contracts/rewards.rs)
- [TIG core structures](../../tig-monorepo/tig-structs/src/core.rs)
- [TIG API schema](../../tig-monorepo/swagger.yaml)
- [Local benchmarking research](../../tig-miner/docs/benchmarking.md)

Public API documentation is available at
[swagger.tig.foundation](https://swagger.tig.foundation/).
