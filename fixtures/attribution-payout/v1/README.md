# Qualifier-attribution and payout fixture v1

Provenance: **constructed** (not captured), per the layout convention in
`fixtures/tig/v1/README.md`: versioned immutable directories, expected values
recorded independently of any implementation with the rule that derives each
one, no credentials, and all money as attoTIG decimal strings — never floats
(`docs/accounting.md` §3). Issue #7; checklist items in
`docs/pre_build_checklist.md` §6.

Anchor: the fixture consumes the `fixtures/tig/v1` snapshot context — block
`block_100080`, height `100080`, round `834`, pool player
`0xp00l00000000000000000000000000000000000`, `blocks_per_round` 120 (so round
834 spans heights 100080–100199 and `block_100200` opens round 835).

## Files

| File | Case family | Cases |
|---|---|---|
| `qualifier-attribution.json` | Per-block qualifier attribution (`docs/mining_system.md` §7, §7.1) | 5 |
| `payouts.json` | Proceeds, fees, allocation, suspense, settlement, round payouts (`docs/accounting.md`) | 10 |

Each file holds a `cases` array of `{name, description, input, expected,
rule}`. Every expected value cites the exact section that derives it. All
integer sums were computed by hand with exact integer arithmetic and
cross-checked with an independent big-integer script before recording.

## Case inventory

`qualifier-attribution.json`:

1. `no_tie_full_groups` — whole quality groups accepted highest-to-lowest;
   last group fits exactly (N == M), no draw.
2. `equal_quality_boundary_draw` — an equal-quality group of N=3 crossing the
   remaining boundary of M=2, with the persisted random seed inputs, tie seed,
   draw ranks, and resulting boundary selection recorded (§7.1).
3. `draw_rank_collision_identity_tiebreak` — identical draw ranks broken by
   canonical bundle identity.
4. `zero_published_count` — bundle entries exist but the published qualifier
   map has no entry for the track; nothing is accepted.
5. `per_member_aggregation` — aggregation to `q[m,b]`, reconciled to the
   snapshot's published pool counts (5 + 2 + 3 = 10).

`payouts.json` (every journal batch lists full entries — account, side,
amount — plus explicit `balance_check` sums per `docs/accounting.md` §8):

1. `ordinary_block_exact_division` — several members; weights are the
   attribution aggregate (4/3/3, Q=10); §2 proceeds equality checks; R=0;
   zero-weight member gets no line.
2. `ordinary_block_fee_floor_and_largest_remainder` — fee floor discards a
   sub-atto fraction; R=2 remainder atoms by greatest `fraction_rank`.
3. `remainder_tie_hash_ordering` — R=1 with two members tied on
   `fraction_rank`; ordered by the smaller remainder hash (§6 rule 2).
4. `dust_tiny_distributable_zero_fee` — 5-atom proceeds; fee floors to 0 (no
   zero-amount line); every atom allocated, no unallocated dust.
5. `zero_qualifier_suspense` — positive proceeds, Q=0; full proceeds to
   block-specific suspense, no fee; retained indefinitely by default (§7).
6. `attribution_mismatch_suspense` — stored selections (4) disagree with the
   published count (5); whole block to suspense (§6, §7).
7. `suspense_resolution` — resolution batch debits the block's suspense and
   applies the original block's fee policy and reconciled weights (§7, §9).
8. `delayed_round_settlement` — round reconciliation, the weeks-long wait for
   TIG's actual payment (wall clock is never settlement evidence, §4), and the
   §8.3 settlement batch with on-chain evidence.
9. `automatic_round_funding_and_intents` — automatic §8.4 funding batch and
   §12.3 immutable intents; a member under the 48h destination hold stays in
   `MEMBER_EARNED_PENDING` without blocking others; custody invariant checked.
10. `round_transfer_confirmation` — §12.5 finalized-transfer confirmation
    batches; gas paid by the pool; post-state custody reconciles to remaining
    liabilities.

## Cross-file consistency

The round-834 chain is internally exact: per-block proceeds sum to
`3050000000000000010` atoms; fees sum to `61000000000000000`; member credits
sum to `2989000000000000010`; fees + members = proceeds
(`docs/accounting.md` §13 invariant 4). The attribution aggregate of
`block_100080` is the weight input of payout case 1.

## Derivation decisions

1. **Zero-amount lines are omitted.** `docs/accounting.md` §8 requires
   `all line amounts > 0`, so a floored-to-zero fee and a zero-weight member
   produce no journal line (payout cases 1 and 4).
2. **Fee basis points are fixture-pinned at 200.** Live testnet uses `0` bps
   (`docs/accounting.md` §5); this fixture pins an explicit policy row
   (`feepol_fixture_200bps`) at the approved initial public rate (§14.1) so the
   fee arithmetic is exercised. This mirrors §14's allowance for explicit
   fixture policy values on testnet.
3. **The permanent zero-qualifier suspense block lives in round 835**
   (`block_100200`) so that unresolved suspense does not block round 834's
   settlement and payout cases (`docs/accounting.md` §12.1).
4. **Attribution benchmarks are constructed cache contents.** The snapshot's
   `get-benchmarks.json` is empty at the anchor; bundle qualities come from the
   active-benchmark metadata cache (`docs/tig_integration.md` §5.2). The
   constructed benchmarks are consistent with the snapshot's published
   qualifier counts in `get-opow.json`.

## Open questions

0. **Cases 8, 9 and 10 are superseded by ADR 0008 and await a `v2`.**

   Case 8 `delayed_round_settlement` pins `ASSET:TIG_PAYOUT_CUSTODY` as the
   §8.3 debit — an account ADR 0008 replaces with `ASSET:TIG_REWARD_WALLET` —
   and reaches `ASSETS_SETTLED` from TIG's payment alone, which
   `accounting.md` §4 rule 8 and §12.1 now forbid without §8.3a's member leg
   completing from its own finalized token event. `v1` carries no such event.

   Cases 9 and 10 pin the automatic per-round payout model:
   `LIABILITY:ROUND_PAYOUT_PENDING`, "no member withdrawal request", and the
   intent key
   `(network, round, member_id, payout_generation)`. ADR 0008 replaced that
   with one member balance and member-initiated withdrawal, so `accounting.md`
   §8.4 now credits `LIABILITY:MEMBER_BALANCE` and §8.5/§12.3 own the
   withdrawal intent. The custody asset names changed too:
   `ASSET:TIG_PAYOUT_CUSTODY` and `ASSET:SECURITY_DEPOSIT_CUSTODY` are one
   `ASSET:TIG_MEMBER_CUSTODY`, with `ASSET:TIG_REWARD_WALLET` for the
   benchmarker wallet TIG pays into.

   What all three still assert correctly is *attribution and allocation*.
   What is superseded is the custody account names, the funding batch's
   destination, the settlement precondition, and the intent's shape. `v1` is
   immutable, so the corrections go in a `v2` (recorded on issue #31); until
   then no implementation may take cases 8, 9 or 10 as the current model.
   Cases 1-7 are unaffected.

1. **`canonical_encode` byte layout is not yet specified.**
   `docs/mining_system.md` §7.1 and `docs/accounting.md` §6 define BLAKE3
   derivations over `canonical_encode([...])`, but no document fixes the byte
   encoding. All BLAKE3 outputs in these fixtures (`tie_seed`, `draw_rank`,
   remainder hashes) are therefore **fixture-pinned** stand-ins modelling the
   persisted draw artifact that §7.1 requires the pool to store; the seed
   *inputs* are authoritative. Selection and ordering logic downstream of the
   ranks is exact and testable now. Once the protocol spike settles the
   encoding, recompute the hashes in a `v2` fixture (immutability convention —
   never edit `v1`).
2. **`fixtures/tig/v1/get-opow.json` does not satisfy the `accounting.md` §2
   equality as literally written**: `reward - reward_share` =
   `2500000000000000000 - 200000000000000000` = `2300000000000000000`, but
   `sum(coinbase)` = `1000000000000000000`. Either the snapshot values or the
   §2 field semantics (is `reward_share` an absolute amount or a fraction?)
   need verification against the pinned `rewards.rs` during the spike. Payout
   case 1 therefore carries case-local OPoW values that satisfy §2 exactly.
   As written, ingesting the v1 snapshot block itself would put it into
   accounting suspense.
3. **`docs/accounting.md` §14 remaining owner decision** (dynamic
   slashable-deposit and malicious-work policy, §11.3–11.5) is **not needed**
   by these case families — no case here touches security deposits, tier fees,
   or collateral, so nothing was parameterized against it. Collateral and tier
   fixtures (separate checklist items) will need it.
4. **Suspense-reason vocabulary** (`ZERO_QUALIFIERS`,
   `ATTRIBUTION_MISMATCH`) and the payout policy version string
   (`payoutpol_fixture_v1`) are fixture inventions; `docs/accounting.md` §7
   requires "one stable reason" but defines no enumeration.

No credentials anywhere: addresses, hashes, and transaction IDs are obvious
fakes (`0xp00l…`, `0x716fake…`, `0xfee…`).
