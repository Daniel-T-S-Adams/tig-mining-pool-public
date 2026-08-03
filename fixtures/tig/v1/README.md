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
| `expected.json` | — | Derived values recorded independently of implementation |

## Amendments

- **v1 amendment (PR after #18):** `get-opow.json` coinbase values corrected to
  satisfy the `accounting.md` §2 equality
  `sum(coinbase.values()) == reward − reward_share`
  (2.1 + 0.2 = 2.5 − 0.2 = 2.3 TIG). The original values (0.8 + 0.2 = 1.0)
  violated it — found by the attribution-payout fixture work (issue #7).
  Documented exception to the immutability convention: nothing consumed the
  original values, so amending v1 was preferred over minting a v2.

## Assumptions to verify during the protocol spike (#10)

Recorded per `docs/tig_integration.md` §1 (OpenAPI alone is not authoritative):

1. **Response envelopes** — each response nests under a top-level resource key
   (`{"block": …}`, `{"challenges": […]}`). Verify against live testnet and
   correct in a `v2` fixture if wrong.
2. **`get-algorithms` shape** — assumed to merge the upstream `Code` struct
   with its confirmed `Binary` under a `binary` key. Verify.
3. **Enum key casing** — `TxType`/`ActiveType` map keys are lowercase
   (`"precommit"`, `"benchmark"`), per upstream serde attributes. Verify on
   the wire.
4. **Activation timing** — the fake server approximates
   `block_active = proof_confirmed + ceil(submission_delay ×
   submission_delay_multiplier)`. The pool never computes activation from this
   rule (it reads `active_ids`); the approximation only sequences the fake
   world. Verify plausibility during the spike.
