# TIG snapshot fixture v1

Provenance: **constructed** (not captured). Shapes are derived from the pinned
upstream structs at commit `ad08d1ea001a73ff5aab3b556d7f59246fece14e`
(`tig-structs/src/core.rs`, `tig-structs/src/config.rs`) per
`docs/tig_integration.md` §2.2. Values are internally consistent inventions —
no value here may be treated as a live protocol constant
(`docs/tig_integration.md` §12).

Anchor: block `block_100080`, height `100080`, round `834`.

## Layout convention (applies to all future fixture sets)

- One directory per fixture set, versioned (`v1`, `v2`, …); files inside a
  version are immutable once merged — corrections create a new version.
- One JSON file per API endpoint response, named after the endpoint.
- `expected.json` records derived values independently of any implementation,
  with the rule that derives each one.
- No credentials anywhere. API keys, signatures, and addresses are obvious
  fakes (`0xp00l…`, `fake.invalid` URLs).

## Files

| File | Endpoint | Notes |
|---|---|---|
| `get-block.json` | `GET /get-block?include_data=true` | Template; the fake server overrides id/height/round/timestamp and merges dynamic confirmed/active IDs |
| `get-challenges.json` | `GET /get-challenges?block_id=…` | c001 + c003 active, c099 inactive (`round_active` 900 > round 834) |
| `get-algorithms.json` | `GET /get-algorithms?block_id=…` | a011/a031 eligible; a012 banned; a013 has no confirmed binary |
| `get-opow.json` | `GET /get-opow?block_id=…` | Pool qualifier/coinbase data |
| `get-player-data.json` | `GET /get-player-data?block_id=…&player_id=…` | Pool fee balance |
| `get-benchmarks.json` | `GET /get-benchmarks?block_id=…&player_id=…` | Empty at anchor; the fake server appends pool writes |
| `get-tracks-data.json` | `GET /get-tracks-data?block_id=…&challenge_id=…` | Compact per-track records |
| `get-benchmark-data.json` | `GET /get-benchmark-data?benchmark_id=…` | §5.2 active-benchmark metadata-cache record (network-side hyperparameter source) |
| `expected.json` | — | Derived values recorded independently of implementation |

## Amendments

- **v1 amendment (PR after #18):** `get-opow.json` coinbase values corrected to
  satisfy the `accounting.md` §2 equality
  `sum(coinbase.values()) == reward − reward_share`
  (2.1 + 0.2 = 2.5 − 0.2 = 2.3 TIG). The original values (0.8 + 0.2 = 1.0)
  violated it — found by the attribution-payout fixture work (issue #7).
  Documented exception to the immutability convention: nothing consumed the
  original values, so amending v1 was preferred over minting a v2.
- **v1 amendment (issue #2 close-out):** added `get-benchmark-data.json` — the
  active-benchmark metadata-cache record required by `mining_system.md` §5.1
  ("active pool benchmarks and their bundle-quality arrays" and "recent
  benchmark lifecycle records needed to find source hyperparameters") and
  `tig_integration.md` §5.2. The anchor world's *pool* benchmark set stays
  empty by design (the fake server generates pool state dynamically); this
  record exemplifies a *network-side* active benchmark supplying the fields
  the hyperparameter-source rule (§6.6) and bundle attribution (§7) consume:
  confirmed settings, selected `hyperparameters` and `fuel_budget`, counts,
  and `average_quality_by_bundle`.

- **v1 annotation (spike S6 close-out, no byte changes):** the
  `precommit_fee_examples` rule in `expected.json`
  (`fee = base_fee + per_nonce_fee × num_nonces`) is **refuted** by the pinned
  source: `tig-protocol/src/contracts/benchmarks.rs` lines 98–99 at
  `ad08d1ea…` multiply `per_nonce_fee` by **`num_bundles`**, not nonces
  (`mining_system.md` §6.8, `docs/protocol_spike_report.md`). Treat those
  example fees as historical; the `v2` fixture set (follow-up issue) corrects
  them together with the real `get-algorithms` envelope and opaque block ids.

## Coverage audit (issue #2)

`mining_system.md` §5.1's ingestion list mapped to this fixture, checked
against what the decision-engine cases (`fixtures/decision-engine/v1`, issue
#3) and attribution cases (issue #7) actually consume:

| §5.1 data group | Covered by |
|---|---|
| Block identity, height, round, active IDs, live config | `get-block.json` |
| Active challenges, tracks, bundle sizes, network qualifier counts | `get-challenges.json` |
| Active algorithms, adoption, binaries, per-track qualifiers | `get-algorithms.json` |
| Pool qualifier counts by challenge/track | `get-opow.json` |
| Pool post-sharing coinbase and gross reward | `get-opow.json` (amended for §2 equality) |
| Active benchmarks and bundle-quality arrays | `get-benchmark-data.json` (amendment) |
| Lifecycle records for source hyperparameters | `get-benchmark-data.json` (amendment) |
| Confirmed fraud and terminal states | `get-benchmarks.json` shape (`frauds`); `fraud: null` in `get-benchmark-data.json` |

Inputs consumed by decision cases that are deliberately *not* snapshot data:
`offered_compute` (member-side), `supplied_randomness` (pool-persisted tie
inputs; §6.3 derivation is an open question recorded in the decision-engine
README).

Known fidelity limitation: the fake server rebuilds `active_ids.benchmark`
and `get-benchmark-data` from pool-created state only, so it does not serve
network-side benchmarks like `bench_net_0001`. Pool code that needs
network-side metadata in tests should load this file directly; extend the
fake server if the spike shows that path matters.

## Assumptions to verify during the protocol spike (#10)

Recorded per `docs/tig_integration.md` §1 (OpenAPI alone is not authoritative):

1. **Response envelopes** — each response nests under a top-level resource key
   (`{"block": …}`, `{"challenges": […]}`). **Partially verified live
   (2026-08-03, S1):** holds for `get-block`, `get-challenges`,
   `get-benchmarks`; but `get-player-data` adds top-level `deposits` and
   `round_earnings` beside `player`, and `get-algorithms` is refuted — see 2.
2. **`get-algorithms` shape** — assumed to merge `Code` + `Binary` under a
   `binary` key. **Refuted live (2026-08-03, S1):** the real envelope is
   `{"advances": […], "binarys": […], "codes": […], "player_details": …}` —
   codes and binaries are separate arrays joined by `algorithm_id`. A `v2`
   fixture must adopt this shape; consumers of `get-algorithms.json` v1
   should treat its shape as historical.
3. **Enum key casing** — `TxType`/`ActiveType` map keys are lowercase
   (`"precommit"`, `"benchmark"`), per upstream serde attributes.
   **Verified live (2026-08-04, S4):** the ACTIVE observation read
   `block.data.active_ids.benchmark` (lowercase key) on live testnet.
4. **Activation timing** — the fake server approximates
   `block_active = proof_confirmed + ceil(submission_delay ×
   submission_delay_multiplier)`. The pool never computes activation from this
   rule (it reads `active_ids`); the approximation only sequences the fake
   world. **Plausibility observed live (S4/S6):** TIG published
   `submission_delay = 4` with `block_active = proof_confirmed + 4`
   (1270241 → 1270245), consistent with the approximation; `active_ids`
   remains the only authority (`tig_integration.md` §7).
