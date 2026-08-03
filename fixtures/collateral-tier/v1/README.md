# Collateral and tier fixture v1

Provenance: **constructed** (not captured). Deterministic cases for the
collateral rules ([`docs/accounting.md`](../../../docs/accounting.md) §11.4–§11.6,
[`docs/mining_system.md`](../../../docs/mining_system.md) §6.1) and the flat-tier
rules (`docs/mining_system.md` §2, §6.1, §8, §10;
[`docs/architecture.md`](../../../docs/architecture.md) §7.6;
`docs/accounting.md` §11.3). Source task:
`docs/pre_build_checklist.md` §6, issue #5.

Follows the layout convention of [`fixtures/tig/v1/README.md`](../../tig/v1/README.md):
versioned immutable directory (corrections create a `v2`), expected values
recorded independently of any future implementation with the rule that derives
each one, no credentials, and obvious-fake IDs (`member_m01`, `bench_b50`,
`fee_batch_fake_001`). Round names (`r834`) echo the `fixtures/tig/v1` anchor
for familiarity, but no case depends on that snapshot's contents.

## Files

| File | Cases | Coverage |
|---|---|---|
| `collateral.json` | 9 | Per-track bundle counts and the max-across-tracks precommit reserve; post-confirmation reduction (never increase); simultaneous reservations (second admitted, third denied); insufficient balance including pending-return/frozen deductions; joining fee never bypassing the gate; live `penalty_amount` change pausing new precommits; mid-flight reservation immune to a later policy; X-vs-method release split |
| `tier.json` | 15 | Paid join and atomic fee batch; unfinalized fee never activating; `k`-unverified enforcement at the limit; unverified span surviving package acceptance; outstanding exposure across re-entry; rejoin with no cooldown/refund and persistent liabilities; dormancy; round `U > V` removal and the `U = V` strict-inequality edge; per-failure `X` charges; the `f > k` removal boundary (`f = k` retained, `f = k + 1` removed); non-chargeable outcomes; zero-quality-bundle charge without a fraud label; removal cancelling offers but not precommit intents; fresh aggregates after round-close removal and rejoin |

## Case format

Each file holds a `cases` array of
`{name, description, input_state, action_or_event, expected, rule}`. Every
`expected` entry includes a `derivation` showing the exact integer arithmetic,
and every `rule` cites the owning document section verbatim enough to audit.

## Money and numeric conventions

All money values are attoTIG decimal strings in the TIG `PreciseNumber` style
(`1 TIG = 10^18 attoTIG`, `docs/accounting.md` §3). Every derived amount was
computed with integer math only; no value passed through a float.

## Derivation decisions

1. **Fees are inputs, not derivations.** `F[s,t]` ("exact precommit fee implied
   by live challenge config and `B[t]`", accounting.md §11.4) is supplied
   directly in `input_state` rather than derived from `base_fee`/`per_nonce_fee`,
   so the collateral arithmetic embeds no fee-formula assumption (see open
   question 3).
2. **Fixture policy values are inventions.** `P = 10 TIG` (and `12 TIG` after
   the change case), `X = 2 TIG`, `J[1..3] = 3/5/8 TIG` are internally
   consistent inventions carried as versioned policy inputs
   (`policy_v1_fixture`). None is a live protocol or settled pool constant.
3. **Strict inequalities taken literally.** `U > V` removal and `f > k` removal
   are documented as strict comparisons, so the fixtures pin the boundary:
   `U = V` retains, `f = k` retains, `f = k + 1` removes.
4. **Round aggregates use the §7.6 model.** One aggregate row per
   member/tier-membership/round with `sample_count`, `U`, `V`, `f`, and the
   tier decision; sums are compared directly because both aggregates share the
   same sample count (mining_system.md §8).

## Open questions (recorded, not invented)

Checked against `docs/mining_system.md` §11 ("Decisions intentionally left
open"):

1. **Numerical `J[k]` and `X`** are explicitly open ("required before full
   product implementation", mining_system.md §11). The values here are fixture
   inventions; implementation tests must treat them as versioned policy inputs,
   never as expected production constants.
2. **Penalty retroactivity.** Whether TIG computes a later report penalty from
   the benchmark, report, arbitration, or charge block is an explicit
   pre-mainnet spike question (accounting.md §11.5). The
   `penalty_change_mid_flight_no_retroactive_increase` case therefore asserts
   only pool-side reservation behavior (pause, recorded values, no silent
   increase) and deliberately asserts nothing about TIG-side retroactive
   penalties or the risk-buffer sizing.
3. **Precommit fee formula tension.** `docs/mining_system.md` §6.8 states the
   current protocol multiplies `per_nonce_fee` by *bundles* despite its name,
   while `fixtures/tig/v1/expected.json` derives its example fees as
   `base_fee + per_nonce_fee × num_nonces`. This fixture takes no side: fees
   are inputs (decision 1). The discrepancy should be resolved during the
   protocol spike and, if needed, corrected in a `v2` of whichever fixture is
   wrong.
4. **Same-instant rejoin timing.** mining_system.md §8 says a removed member
   "normally" begins a new membership in the new round with new aggregates.
   The word "normally" leaves the same-boundary edge (rejoin transacted in the
   same instant as round close) unpinned;
   `removal_at_round_close_rejoin_gets_fresh_aggregates` covers only the
   normal next-round case.
