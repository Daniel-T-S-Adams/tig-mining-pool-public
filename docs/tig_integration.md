# TIG integration contract v0

Status: settled baseline for the testnet protocol spike  
Last verified: 2026-07-30

This document defines the pool's boundary with TIG. It pins the upstream code
and containers used to build the first integration, catalogues the required
API operations, defines confirmation and recovery rules, and separates live
network configuration from project constants.

The mining policy remains defined by [mining_system.md](mining_system.md).
Machine-readable integration pins live in
[`config/tig_integration.json`](../config/tig_integration.json).

## 1. Authority and fail-closed rule

The pool uses these sources in this order:

1. confirmed state returned by the selected TIG network;
2. the protocol behavior and structures at the pinned upstream commit;
3. the OpenAPI routes and request schemas at the pinned OpenAPI checksum; and
4. TIG's reference benchmarker as implementation guidance.

The OpenAPI document is not sufficient by itself. At the pinned revision, some
of its benchmark response descriptions lag both the Rust structures and live
testnet responses. The pool therefore uses explicit models derived from the
pinned Rust structures and verified live fixtures. It must not generate a
write-capable client blindly from OpenAPI.

If a required field, enum, serialization rule, or lifecycle assumption does
not match the pinned contract, the TIG gateway enters
`READ_ONLY_INCOMPATIBLE`, stops creating or retrying protocol writes, continues
safe reads where possible, and alerts the operator. Unknown extra response
fields may be retained or ignored; missing or type-incompatible required fields
are fatal to write compatibility.

## 2. Pinned baseline

### 2.1 Network

The protocol spike targets TIG testnet only:

```text
network = testnet
API base URL = https://testnet-api.tig.foundation
mainnet writes = disabled
```

The mainnet URL is recorded for future configuration but must not be used as a
fallback. Enabling mainnet requires an explicit reviewed configuration change;
failure to contact testnet must never redirect a request to mainnet.

`https://test.tig.foundation` is a manual, operator-facing credential-issuance
UI, not a pool runtime endpoint. It is deliberately outside the machine-readable
runtime pins and no pool process contacts it. On every use, the operator must
verify the official origin and check its prompt and result against the pinned
`POST /request-api-key` contract in section 4; a mismatch fails closed.

### 2.2 Source and schema

```text
repository = https://github.com/tig-foundation/tig-monorepo.git
commit = ad08d1ea001a73ff5aab3b556d7f59246fece14e
commit observed as official upstream HEAD = 2026-07-30
OpenAPI version = 1.0.0
OpenAPI SHA-256 = fad74baea1a52f9dbc07c095d6ed0b0d7c6b5897288a6dd7cf10d096df17d1cc
Cargo.lock SHA-256 = 52fd067662f577d14a3e9f7029aa0ef968e65b990875781db8cc157cb9431d0e
```

The existing local `tig-monorepo` checkout was at
`b776621403ade410d1e3d04464dd547c0cf0b2bb` during this review and is not the
authoritative pool pin. Spike builds must acquire the exact commit above or
fail before building.

Relevant pinned upstream files include:

- [`tig-structs/src/core.rs`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-structs/src/core.rs)
- [`tig-structs/src/config.rs`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-structs/src/config.rs)
- [`tig-protocol/src/contracts/benchmarks.rs`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-protocol/src/contracts/benchmarks.rs)
- [`tig-benchmarker`](https://github.com/tig-foundation/tig-monorepo/tree/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-benchmarker)
- [`swagger.yaml`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/swagger.yaml)

### 2.3 Containers

The pinned TIG benchmarker container version is `0.0.7`. The runtime image for
each challenge contains the compatible `tig-runtime` and `tig-verifier`; those
executables do not receive an independent version number in this baseline.

Every image is pulled by the manifest-list digest in
[`config/tig_integration.json`](../config/tig_integration.json), not by the tag
alone. The pinned images support both `linux/amd64` and `linux/arm64` manifests.

V0 pins runtime images for:

```text
c001 satisfiability
c002 vehicle_routing
c003 knapsack
c004 vector_search
c005 hypergraph
c006 neuralnet_optimizer
c007 job_scheduling
c008 energy_arbitrage
```

An active TIG challenge without a pinned compatible runtime is ineligible for
pool decisions. It must not be assigned merely because it appears in live
configuration.

Algorithm binaries remain live protocol inputs because the selected algorithm
can change each block. For every assignment, the pool records the
`algorithm_id`, confirmed binary download URL, and a SHA-256 calculated over
the downloaded binary archive. The member and pool must agree on that digest.

## 3. Supported member compute

TIG's pinned protocol accepts these compute types:

| Worker class | Architecture/vendor | Accepted v0 compute types |
|---|---|---|
| CPU | `linux/amd64`, Intel | `aws_t3`, `aws_c7i`, `aws_m7i` |
| CPU | `linux/amd64`, AMD | `aws_t3a`, `aws_c7a`, `aws_m7a` |
| CPU | `linux/arm64`, ARM | `aws_t4g`, `aws_c7g`, `aws_m7g` |
| GPU | `linux/amd64`, NVIDIA/CUDA | `aws_g4dn` |

The protocol spike is pinned to an ARM64 CPU worker reporting `aws_t4g`.
GPU work is disabled for that spike because the current spike host has no
NVIDIA runtime.

V0 GPU enrollment requires an NVIDIA GPU, a compatible driver and NVIDIA
Container Toolkit, successful launch of the digest-pinned challenge runtime,
and a reproducibility qualification run. ARM64 GPU workers are not supported
in v0.

A member-supplied label is not enough to establish compatibility. Enrollment
must detect architecture and vendor, restrict the reported `compute_type` to
the compatible set above, and complete a known-output qualification fixture.
Unknown architectures, vendors, compute types, or runtime combinations are
ineligible rather than coerced to the nearest type.

## 4. API transport and authentication

The TIG API uses HTTPS and JSON. Read endpoints used by the pool are public.
The three normal mining writes require the `X-Api-Key` header.

An API key is obtained separately through `POST /request-api-key` using a
lowercase TIG address and a signature over exactly:

```text
I am signing this message to prove that I control address <address>
```

For testnet, the official browser flow is the simplest way to perform that
exchange (observed 2026-09-04):

1. Open **only** `https://test.tig.foundation`, connect the dedicated testnet
   wallet, and continue to the verification page.
2. Select **Obtain API-Key**. Before approving MetaMask's signature request,
   verify that the selected address is the intended lowercase pool address and
   that the displayed message is exactly the text above. A transaction, gas
   request, token approval, or different message is not this flow and must be
   rejected.
3. The site submits the address and signature to the testnet
   `/request-api-key` endpoint and keeps the returned key in the authenticated
   browser session. While that session is active,
   `https://test.tig.foundation/api/auth/get-api-key` returns the key for the
   operator to copy into the gateway's untracked secret file.
4. Copy only the `api_key` value, clear the clipboard after installing it, and
   log out of the site. Never copy the wallet recovery phrase, private key,
   MetaMask password, or signed proof into the repository or an agent session.

The website is an operator convenience, not a second protocol or pool runtime
dependency. If its URL, prompt, or response changes, stop and validate it
against the canonical `POST /request-api-key` contract above before proceeding.

The production signing key is not required during ordinary benchmark
submission after an API key has been issued. The API key is available only to
the TIG gateway, is never placed in member assignments, and must be redacted
from logs and traces.

Requests and responses use lossless numeric handling:

- TIG `PreciseNumber` values are decimal strings and must not pass through
  floating-point numbers;
- qualities use signed 32-bit integers;
- nonce, count, signature, and fuel fields use unsigned 64-bit integers; and
- block heights and rounds use unsigned 32-bit integers.

The reference benchmarker Brotli-compresses write bodies larger than 10 KiB
using `Content-Encoding: br`. The TIG gateway may do the same, but its
uncompressed canonical request and payload hash remain the audit record.

## 5. Required API reads

Unless stated otherwise, a read with `block_id` uses the snapshot anchor
returned by the opening `GET /get-block?include_data=true` call.

| Endpoint | Required data and use |
|---|---|
| `GET /get-block?include_data=true` | Latest block ID, previous ID, height, round, timestamp, live `config`, `confirmed_ids`, and `active_ids`. Starts and closes every accepted snapshot. |
| `GET /get-challenges?block_id=...` | Challenge state, per-challenge live configuration, network qualifier totals and qualifier qualities by track. |
| `GET /get-algorithms?block_id=...` | Algorithm ownership/state, banned and active status, challenge-wide adoption, per-track qualifiers by player, and confirmed binary status/download URL. |
| `GET /get-opow?block_id=...` | The pool's qualifiers by challenge/track, post-sharing `coinbase`, gross reward, delegators, influence, cutoff, and audit fields. |
| `GET /get-player-data?block_id=...&player_id=...` | Pool player state, especially available fee balance, plus deposit/top-up audit data. |
| `GET /get-benchmarks?block_id=...&player_id=...` | Confirmed pool precommits, benchmarks, proofs and frauds from the latest 120-block API window. Primary lifecycle-reconciliation read. |
| `GET /get-tracks-data?block_id=...&challenge_id=...` | Per-track records containing algorithm ID, bundle count and benchmark average quality from the latest 120 blocks. Used as a compact performance/discovery input, not as sufficient proof of active status or source hyperparameters. |
| `GET /get-benchmark-data?benchmark_id=...` | Full precommit, benchmark, proof and fraud data for an individual benchmark. Used incrementally to build the compact active-benchmark cache needed for source hyperparameters, bundle qualities and algorithm/track active-bundle counts. |
| `GET /get-binary-blob?algorithm_id=...` | The confirmed algorithm archive used by the member runtime. The pool calculates and records its digest. |
| `GET /get-round-emissions?round=...` | Round-level accounting reconciliation only. It cannot reconstruct the pool's missing per-block qualifier attribution. |
| Method reports and arbitrations against the pool's benchmarks | The reports filed against the pool as a benchmarker, the exact nonces reported, and each arbitration outcome. Required for the freeze rule in `accounting.md` §11.6, which cannot be evaluated without it. **The exact endpoint, parameters and response shape are not yet pinned** — see §14.2. |

### 5.1 Active challenge and algorithm tests

A challenge is active for a snapshot only when:

```text
challenge.state.round_active <= block.details.round
```

An algorithm is eligible only when its pinned fields show that it is active in
the snapshot round, is not banned, and has a confirmed binary with
`compile_success = true` and a non-null download URL. The complete eligibility
rules remain those in `mining_system.md`.

### 5.2 Active-benchmark metadata cache

The public compact track endpoint does not expose a benchmark ID or source
hyperparameters. It is therefore insufficient for the agreed top-bundle
hyperparameter rule. The controller must:

1. read the current `block.data.active_ids.benchmark` set;
2. identify active IDs absent from its compact metadata cache;
3. fetch `GET /get-benchmark-data` for those IDs under the read limiter;
4. retain only confirmed settings, selected hyperparameters and fuel, algorithm
   and track, counts, and `average_quality_by_bundle`; and
5. discard the large solution-quality and proof payloads returned by that
   endpoint unless another implemented behavior requires them.

Confirmed precommit and benchmark facts are immutable enough to cache by
`benchmark_id`; active membership is always taken from the current block. An
initial cache warm-up may span several blocks. Until every active benchmark
needed by a decision is represented, the affected algorithm/track is
unavailable and the orchestrator returns `no_action` rather than using partial
denominators or guessing source hyperparameters.

## 6. Required protocol writes

Only the TIG gateway sends these writes. HTTP 200 means that TIG accepted the
request for processing; it is not confirmed protocol state.

### 6.1 Submit precommit

`POST /submit-precommit` with `X-Api-Key`:

```json
{
  "settings": {
    "player_id": "<lowercase pool address>",
    "block_id": "<snapshot block id>",
    "challenge_id": "<challenge id>",
    "algorithm_id": "<algorithm id>",
    "track_id": ""
  },
  "track_settings": {
    "<every active track id>": {
      "hyperparameters": null,
      "fuel_budget": 0,
      "num_bundles": 0
    }
  },
  "compute_type": "aws_t4g"
}
```

The real values replace the illustrative zeroes. `track_settings` contains
exactly every live active track for the selected challenge. The submitted
`track_id` is empty because TIG selects it; the confirmed precommit's
`settings.track_id`, `details.rand_hash`, counts, fuel, hyperparameters and fee
are authoritative.

The response contains `benchmark_id`. The pool records the canonical request,
decision ID, request hash, transport result and returned ID before doing any
member assignment.

### 6.2 Submit benchmark commitment

`POST /submit-benchmark` with `X-Api-Key`:

```json
{
  "benchmark_id": "<benchmark id>",
  "stopped": false,
  "merkle_root": "<64 lowercase hex characters>",
  "solution_quality": [0, 1, 2]
}
```

For a non-stopped benchmark, `solution_quality` has exactly
`precommit.details.num_nonces` signed integer entries and `merkle_root` is
present. For an explicit stopped submission, `stopped` is true and both other
fields are null. Durable proof-material acceptance is a local prerequisite for
the non-stopped write.

### 6.3 Submit proof

`POST /submit-proof` with `X-Api-Key`:

```json
{
  "benchmark_id": "<benchmark id>",
  "merkle_proofs": [
    {
      "leaf": {
        "nonce": 0,
        "runtime_signature": 0,
        "fuel_consumed": 0,
        "solution": "<canonical TIG solution encoding>",
        "cpu_arch": "arm64"
      },
      "branch": "<TIG MerkleBranch encoding>"
    }
  ]
}
```

The proof set must contain every sampled nonce exactly once and no other
nonce. Array order is not significant. Each branch must resolve its output
metadata hash to the exact root previously submitted for that benchmark.

Wire-encoding facts verified end-to-end on live testnet (spike S4, benchmark
`894e4d4f2b865ee33eb85e334931bc6d` confirmed and activated, which requires
every branch to resolve to the committed root):

- The leaf's `nonce`, `runtime_signature`, and `fuel_consumed` are **bare
  JSON u64 integers** on this write (serde_json prints `u64` exactly, even
  above 2^53), while the member-protocol wire carries full-width values as
  decimal strings — the pool parses those strings before hashing or
  submitting.
- The `solution` rides as a JSON string, and its signature preimage includes
  the JSON quoting: pinned `OutputData::calc_solution_signature`
  (`tig-structs/src/core.rs`) computes
  `u64_le(blake3(jsonify(solution))[0..8])` where the solution is jsonified
  **as a JSON string, quote characters included**. Reproducing the signature
  from the raw solution bytes without the quoting produces a different leaf
  and a rejected proof.
- `MerkleBranch` serializes as the concatenation of
  `{depth:02x}{sibling_hash:064x}` per element, matching both pinned Rust
  (`tig-utils/src/merkle_tree.rs`) and the pinned Python benchmarker
  (`common/merkle_tree.py`).

### 6.4 Writes deliberately excluded

The pool does not use TIG's `set-coinbase` operation to pay internal members in
v0. Internal member credits follow the accounting rule in
`mining_system.md`. Profile, delegation, voting, code, advance, report, deposit,
and withdrawal operations are outside the mining gateway.

## 7. Confirmation and lifecycle mapping

The controller advances local state only from confirmed reads:

| Local event | Authoritative TIG evidence |
|---|---|
| Precommit submitted | A recorded transport attempt and response; not confirmation. |
| Precommit confirmed | Matching entry in `get-benchmarks.precommits` with non-null `state.block_confirmed`. Confirmed settings/details replace proposed values. |
| Benchmark submitted | A recorded attempt for the known `benchmark_id`; not confirmation. |
| Benchmark confirmed | Matching entry in `get-benchmarks.benchmarks` with non-null `state.block_confirmed`. If `details.stopped` is true, no proof is sent. Otherwise `details.sampled_nonces` drives proof construction. |
| Proof submitted | A recorded attempt; any synchronous `verified` response is not block confirmation. |
| Proof confirmed | Matching entry in `get-benchmarks.proofs` with non-null `state.block_confirmed`. |
| Fraud confirmed | Matching entry in `get-benchmarks.frauds` with non-null `state.block_confirmed`. |
| Verification event | Benchmark ID in `block.data.confirmed_ids.verified`, when published. |
| Active | Benchmark ID in `block.data.active_ids.benchmark`. This set is authoritative even if a locally calculated activation height differs. |
| No longer active | A previously active ID is absent from the current active set. Keep the compact historical ownership and payout record. |

The `ProofDetails.block_active` and `submission_delay` fields are stored for
explanation and monitoring, but the pool does not calculate or override TIG's
activation decision.

## 8. Block-age and timing rules

Pinned protocol behavior and public API constraints are:

- a precommit's `settings.block_id` must reference TIG's latest or second-latest
  block when processed;
- reads documented with `block_id` require the latest block;
- `get-benchmarks` returns confirmed player benchmarks started within the
  latest 120 blocks;
- the reference benchmarker declines to create a job from a precommit already
  60 blocks old and locally stops unfinished jobs at `block_started + 120`;
- each live challenge publishes `lifespan_period` and
  `submission_delay_multiplier`; and
- the pinned benchmark and proof contract does not itself expose a separate
  explicit benchmark-upload or proof-upload age check.

For the protocol spike, the pool adopts the reference guardrails exactly:

```text
do not assign a confirmed precommit at age >= 60 blocks
require durable member-package acceptance before block_started + 110
mark unfinished local workflow expired at age >= 120 blocks
construct and submit a proof immediately after confirmed sampled nonces appear
```

The ten-block interval from package deadline to local expiry is reserved for
pool-owned commitment, sampling, proof construction, submission, and
confirmation. These are spike safeguards, not permanent constants. Production
timeouts will be replaced by reviewed values after the spike measures actual
block and upload timing. Active-benchmark expiration always follows TIG's
current active-ID set and live configuration.

The four spike values are also recorded under `spike.workflow_guardrails` in
[`config/tig_integration.json`](../config/tig_integration.json); code reads that
configuration and checks the `110 = 120 - 10` relationship at startup.

The testnet block observed during this review used 60-second target blocks,
120-block challenge lifespans, two greater-than-or-equal-to-average samples and
one lower-than-average sample. Those observations are evidence only. The pool
reads these values from each block and does not compile them as constants.

## 9. Block-consistent snapshot algorithm

One accepted decision and payout snapshot is built as follows:

1. Fetch `GET /get-block?include_data=true` and record block ID `B`.
2. Validate the required block, configuration and ID-set shapes.
3. Fetch challenges, algorithms, OPoW, pool player data, pool benchmarks and
   per-challenge track data using `B` wherever the endpoint accepts it.
4. Advance the incremental active-benchmark metadata cache without mixing its
   cached immutable facts with an incorrect active-ID set.
5. Fetch `GET /get-block?include_data=true` again.
6. Accept the snapshot only if the closing block ID is still `B`; otherwise
   discard the assembled snapshot and restart at the new block.
7. Atomically persist the accepted compact snapshot and its completeness
   status before allowing a decision or payout derived from it.

API calls returning “block must be latest,” inconsistent required IDs, or a
different snapshot are a refresh conflict, not partial success. The
orchestrator does no work until a complete snapshot and required active cache
are available.

Within one block, each endpoint response is cached by its complete request key.
No component may independently refetch and substitute one field into an
already accepted snapshot.

## 10. Restart reconciliation and missing blocks

After restart, the controller:

1. loads all local nonterminal workflows and their last confirmed evidence;
2. fetches and validates the latest block snapshot;
3. fetches the pool's latest `get-benchmarks` window;
4. matches TIG records by `benchmark_id` and advances local state monotonically;
5. checks confirmed and active ID sets;
6. verifies accepted artifact availability before any benchmark or proof retry;
   and
7. reconciles a write before retrying it.

For benchmark and proof writes, `benchmark_id` makes reconciliation direct. A
lost precommit HTTP response is harder because the client may not know the
generated ID. The gateway permits only one unresolved precommit request per
serialized submission lane, searches newly confirmed precommits for the exact
player, decision block, challenge, algorithm, compute type and selected-track
settings, and stops for operator resolution if more than one candidate matches.
It never blindly resubmits an ambiguous precommit.

The public latest-state API does not expose arbitrary historical block
snapshots. If the newest accepted height is more than one above the last local
height, the pool records every missing height as a data gap and alerts. It may
resume current mining after reconciliation, but it must not invent per-block
qualifier attribution or payouts for the gap. `get-round-emissions` can audit
round totals but cannot recover the missing per-block member weights. Public
operation will therefore require reliable continuous ingestion and a separately
tested snapshot archive or other approved recovery source.

## 11. Rate limits, caching, timeouts and retries

TIG publishes that GET and POST operations are rate-limited per IP, and write
endpoint documentation says they may be invoked only once every few seconds.
No numerical quota or remaining-limit headers were published or returned
during this review.

The pool uses these conservative client limits, originating with the spike.
The whole-call backstop and the per-reader shares are later pool policy, not
spike observation:

```text
get-block poll interval: 15 seconds
GET limiter (pool-wide, per IP): 2 requests/second, burst 2
maximum concurrent TIG GETs: 2 pool-wide, counted while a request or its
                             body is in flight
per-reader share of the above: see ADR-0006
POST lane: serialized, minimum 5 seconds between initial writes
write retry interval after reconciliation: 60 seconds
connect timeout: 5 seconds
GET total timeout: 30 seconds, per attempt
GET whole-call backstop: 300 seconds, covering every retry and wait
POST total timeout: 60 seconds
```

Five of these need saying precisely, because each was originally ambiguous
enough to be implemented wrongly:

- The GET budget is a **pool-wide per-IP ceiling**, not a per-process
  allowance: TIG rate-limits per IP, and the controller and gateway are
  separate processes behind one egress address. Each reader is configured
  with an explicit share and the shares must sum to no more than the
  ceiling; ADR-0006 records the allocation and why an even split was chosen
  for v0. Two clients inside one process share that process's share.
- The concurrency ceiling counts requests whose **body is still
  streaming**, not merely those awaiting response headers, and a caller
  waiting on a token or a `Retry-After` occupies no slot. A ceiling on
  waiting rather than on transfer would let two stalled reads block the
  `get-block` poll that §9 and §10 depend on.
- Response caching splits, and both halves now exist. §9's per-block caching
  by complete request key is snapshot assembly's (criterion C3) and keeps one
  snapshot from mixing blocks. The `Cache-Control` rule below is the
  transport half, in `crates/tig-client`, and keeps the pool from re-asking
  for something TIG said would not change yet. Two permissions are required,
  not one: the request must be within the scope that closing paragraph
  defines, **and** the server must allow it — a response with no
  `Cache-Control`, or one carrying a directive the client does not
  understand, is not stored at all. The scope is an endpoint allow-list, not
  a test for a block anchor: `get-benchmarks` and `get-player-data` carry one
  and are still latest-state reads, and `get-block` names none and is the
  value §9 step 6 compares.
- These limits are read from `config/tig_integration.json` under
  `read_limits`, the way §8's guardrails are (slice-1 criterion E5). The
  pool-wide ceiling, each reader's share and the poll interval are all
  configuration, and there is no compiled fallback: a missing or incomplete
  `read_limits` refuses to load rather than substituting a default, since a
  default ceiling would be a protocol constant in the binary (§12) reached
  exactly when configuration was absent. The reader shares are summed
  against the ceiling when the policy loads, so a config that over-allocates
  the per-IP budget cannot produce a usable policy at all. The POST-lane
  values above are not yet read from configuration because no component
  writes yet; they become configuration with the TIG gateway.
- The 30-second GET timeout bounds **one attempt**. The whole-call backstop
  exists only so a read cannot hang indefinitely; it is deliberately loose
  enough that the retry policy below stays reachable, and it is not derived
  from block timing — §8 treats observed block intervals as evidence, not
  constants. A caller needing a read to complete within a block imposes that
  bound itself from live configuration.

These are client policy, not claims about server capacity. The spike records
observed headers, latency and throttling and may propose reviewed changes.

Retry rules:

- honor `Retry-After` when present. The call that receives it waits the
  full instructed time, failing if that does not fit its own whole-call
  budget rather than retrying early. The pause it imposes on *other* readers
  sharing the limiter is capped at the backoff ceiling above: that state is
  process-global and written from a server response, so an unbounded one
  would let a single `Retry-After: 86400` — or a far-future HTTP-date under
  clock skew — halt every read in the process. A reader released early meets
  the throttle again and pauses again, so the bound costs one request per
  reader per interval and cannot compound;
- retry read-only calls on network failure, 408, 429 and 5xx using full-jitter
  exponential backoff capped at 60 seconds;
- do not retry schema failures, authentication failures, or other 4xx errors
  automatically;
- before retrying any write, query confirmed state and the local attempt ledger;
- never send two concurrent writes for the same benchmark;
- never issue a replacement precommit while the outcome of the previous one is
  ambiguous; and
- stop all write retries when the workflow is confirmed, terminal, locally
  expired, or schema-incompatible.

The live `Cache-Control` header is respected, within a scope the client
allow-lists by endpoint rather than deny-lists. Only `GET /get-challenges`,
`GET /get-algorithms` and `GET /get-opow`, each naming a `block_id`, are
cacheable: those are configuration and state as of the named block, which is
what makes §9's anchored reads block-consistent at all. Nothing else is
cacheable whatever the server permits.

Carrying a block anchor is not sufficient. `GET /get-benchmarks` is the
latest 120-block window and §7's sole authority for lifecycle confirmation,
so a stale copy makes §10's lost-precommit search find no candidate and
licenses the resubmission §10 forbids; `GET /get-player-data` carries the
live fee balance §12 lists; `GET /get-tracks-data` is a rolling window; and
`GET /get-block` names no block at all and is the value §9 step 6 compares.
Errors and incomplete snapshots are not cached as successful data.

## 12. Live values that must not be hard-coded

The following come from the opening block and associated anchored reads:

- block identity, height, previous block, round, timestamp and active/confirmed
  ID sets;
- active challenge and algorithm status;
- challenge type and quality type;
- active track IDs, nonces per bundle and minimum active quality;
- minimum bundle count, maximum fuel, qualifier cap and sampling counts;
- challenge lifespan, delay multiplier, base fee and per-nonce fee;
- algorithm adoption, ban status, qualifier counts and binary availability;
- network and pool qualifiers by challenge and track;
- pool fee balance, OPoW reward, post-sharing coinbase and reward sharing;
- round timing and any reward/deposit/OPoW configuration used by an implemented
  behavior; and
- active benchmark IDs, bundle qualities and confirmed lifecycle records.

Code may contain field names and pinned enum values, but never copies a value
from the testnet observation as a permanent protocol constant.

## 13. Compatibility checks before enabling writes

At startup and after any deployment, the TIG gateway must pass all of these
checks before entering `WRITE_READY`:

1. project config parses and the network is exactly `testnet`;
2. the acquired upstream source commit matches the pin;
3. every required container resolves to the pinned manifest digest for the
   current platform;
4. the hosted OpenAPI checksum matches the reviewed checksum, or an explicit
   reviewed local schema override is active;
5. the latest block, challenges, algorithms, OPoW and pool lifecycle responses
   validate against required models;
6. all live active challenges considered by the decision engine have a pinned
   runtime and supported compute path;
7. lossless numeric parsing and canonical request serialization fixtures pass;
8. the API key is present in the TIG gateway without being readable by member
   services; and
9. the pool player ID returned by confirmed data matches configured identity.

A changed OpenAPI checksum or required response shape creates an operator task:
compare a newly pinned upstream commit, update explicit models and fixtures,
run the protocol spike tests, then review the config change. Moving a Git
branch or container tag is never accepted automatically.

## 14. Known upstream discrepancies at this pin

These discrepancies are recorded so implementation does not accidentally
choose the wrong shape:

- OpenAPI's `Benchmark` and `BenchmarkDetails` response schemas still describe
  older solution/non-solution counts, while pinned Rust and live testnet return
  `solution_quality`, `stopped`, `num_active_bundles`,
  `average_quality_by_bundle`, `merkle_root`, and `sampled_nonces`.
- The `/submit-benchmark` prose mentions `solution_nonces`, while its request
  schema, pinned contract and live benchmarker use `solution_quality`.
- The `/submit-precommit` prose places `rand_hash` under state; pinned Rust and
  live testnet place it in `PrecommitDetails`.
- OpenAPI's `OutputData` shape is incomplete and disagrees with pinned Rust.
  The authoritative v0 proof leaf has `nonce`, scalar `runtime_signature`,
  `fuel_consumed`, `solution`, and `cpu_arch`.
- The root README mentions an older container version, while the pinned
  benchmarker `.env` specifies `0.0.7`.
- The reference method-verification helper contains a `g4dn` spelling while the
  protocol enum and API use `aws_g4dn`.
- The `per_nonce_fee` field name is misleading: pinned
  `tig-protocol/src/contracts/benchmarks.rs` lines 98–99 compute
  `submission_fee = base_fee + per_nonce_fee * PreciseNumber::from(num_bundles)`
  (field declared in `tig-structs/src/config.rs` line 88), so the fee scales
  with **bundles**, not nonces (line 113 separately derives
  `num_nonces = num_bundles * num_nonces_per_bundle`). Settled in spike S6;
  `mining_system.md` §6.8 states the correct basis. Live testnet could not
  discriminate (S1 observed `per_nonce_fee = 0` on all active challenges).
- Live envelope shapes verified in spike S1 (2026-08-03): `get-algorithms`
  returns `{"advances": […], "binarys": […], "codes": […], "player_details":
  …}` — codes and binaries are separate arrays joined by `algorithm_id`, not
  a merged record; `get-player-data` carries top-level `deposits` and
  `round_earnings` beside `player`. Other read envelopes nest under a single
  top-level resource key as expected. `fixtures/tig/v1` predates this finding;
  a `v2` fixture must adopt the real shapes before pool code consumes
  `get-algorithms`.

These known discrepancies do not authorize accepting new mismatches. They are
handled by the explicit v0 models and must have regression fixtures before
writes are enabled.

### 14.1 Method-report penalty configuration block (S5 determination)

Phase S5 of the protocol spike (issue #14) determined, from the pinned commit
`ad08d1ea001a73ff5aab3b556d7f59246fece14e`, which block's configuration
governs the penalty applied when a method report against a benchmark later
succeeds:

- `ReportsConfig` carries `submission_fee`, `submission_period`,
  `penalty_amount`, and `penalty_address`
  (`tig-structs/src/config.rs` lines 122–129).
- `submit_report` reads the **live latest-block configuration** via
  `ctx.get_config()` (`tig-protocol/src/contracts/players.rs` lines 205–264;
  config read at line 211) and persists into `ReportDetails` only the
  submission `fee_paid` and the benchmark's `round`
  (`tig-structs/src/core.rs` lines 493–502). It does **not** snapshot
  `penalty_amount` or `penalty_address`.
- `ArbitrationDetails` carries only the result enum
  (`NONREPRODUCIBLE`/`REPRODUCIBLE`/`INCONCLUSIVE`) and `ArbitrationState`
  only `block_confirmed` (`tig-structs/src/core.rs` lines 510–526). No
  penalty value is persisted there either.
- `penalty_amount`/`penalty_address` are referenced **nowhere else** in the
  pinned tree (verified by exhaustive search of the pinned commit). The code
  that resolves an arbitration and applies the penalty sits behind the
  `Context` trait hooks `get_arbitration_details` and
  `add_arbitration_to_mempool` (`tig-protocol/src/context.rs` lines 58–63),
  whose implementation is not part of the pinned open-source tree.

Determination: no pinned structure snapshots a penalty from the benchmark's
own block or from the report-submission block, so neither of those blocks can
govern the penalty through persisted state. Whatever applies the penalty must
read a live `ProtocolConfig` at penalty-application time; the only
config-access pattern in the pinned tree is `get_config()` — the latest-block
configuration, as `submit_report` itself demonstrates. The governing
configuration is therefore the one live when the arbitration outcome is
applied (the arbitration/charge block), and a `reports.penalty_amount` change
**can apply retroactively** to already-open benchmarks — confirming the
residual-risk stance of `accounting.md` §11.5: collateral formulas based on
the assignment block alone cannot guarantee coverage, so a pool risk buffer
is required.

Recorded ambiguity (not a guess): the pinned open source cannot distinguish
the arbitration-confirmation block from a hypothetically distinct later
charge block, because the applying code is server-side. Live confirmation
that would settle it: observe one real report → arbitration on testnet
spanning a `reports.penalty_amount` change, or written TIG operator
confirmation. Until then the pool must reserve under the conservative
reading: the penalty may be recalculated with any configuration up to charge
time.

### 14.2 Method report and arbitration visibility (open)

The pool can observe the reports filed against it as a benchmarker, including
exactly which nonces were reported, and the arbitration outcome for each. That
observation is what `accounting.md` §11.6's freeze-on-report rule is evaluated
from: without it, the rule cannot be implemented.

It is recorded here as an open item because the endpoint, its parameters and
its response shape are **not pinned**. §5's other reads each name an exact
path and the fields the pool depends on; this one cannot yet, and §14.1 shows
why the gap is easy to miss — the arbitration code sits behind the `Context`
trait hooks `get_arbitration_details` and `add_arbitration_to_mempool`, whose
implementation is not part of the pinned open-source tree, so reading the
pinned commit alone does not reveal the read.

Before the collateral rules are built, the spike must establish:

- the exact request and response shape, and whether the read is
  block-anchored or latest-state — which decides whether it may be cached
  under §11 and whether it belongs in a §9 snapshot;
- how a reported nonce is attributed to a bundle and therefore to one
  member-owned benchmark, since §11.6 freezes per benchmark, not per member;
- the value and meaning of `ReportsConfig.submission_period`, which bounds how
  long a benchmark's reservation can still be frozen. The pool reads the live
  value rather than assuming one round; and
- whether an arbitration outcome is observable per report, per benchmark, or
  only as an aggregate penalty.

Until it is pinned, no code may assume a path or field name for this read.

## 15. Upgrade procedure

Changing any pin requires a reviewed integration upgrade:

1. select an exact new upstream commit;
2. compare protocol contracts, core/config structs, benchmarker behavior and
   OpenAPI against this contract;
3. resolve and record every relevant schema or lifecycle difference;
4. resolve new image tags to immutable multi-platform digests;
5. update explicit models, live-value routing and deterministic fixtures;
6. run read compatibility tests against testnet;
7. rerun the end-to-end protocol spike; and
8. update the machine-readable config and this document in the same change.

No production upgrade follows upstream `main`, `latest`, or a mutable image tag
without this process.
