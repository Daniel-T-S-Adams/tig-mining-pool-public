# Decision-engine fixture v1

Provenance: **constructed** (not captured). Deterministic input/expected-output
cases for the pure decisions of `decide(...)` (`docs/architecture.md` §3),
whose rules are owned by `docs/mining_system.md` §6. Every expected value was
derived by hand from the cited section, independently of any implementation
(issue #3); no value here may be treated as a live protocol constant.

Layout follows the convention set by `fixtures/tig/v1/README.md`: versioned
immutable directory, obvious-fake IDs, no credentials anywhere.

## Files

| File | Cases | Rules covered |
|---|---|---|
| `challenge-selection.json` | 7 | §6.2 compute compatibility and eligibility; §6.3 raw/projected factor, zero counts, random ties; §6.6 missing-source ineligibility |
| `algorithm-selection.json` | 9 | §6.4 adoption-based selection and filters; §6.5 track qualifier rates, best-on-track, deterministic ties |
| `hyperparameter-source.json` | 6 | §6.6 source-benchmark selection, quality ties, invalid-source filters, missing source |
| `bundle-sizing.json` | 9 | §6.7 GPU/CPU minimums and the CPU core-alignment formula |
| `projected-qualifiers.json` | 6 | §6.3 projection with multiple confirmed in-flight benchmarks; §2 in-flight definition |

Each file is `{"_comment": …, "cases": [{name, description, input, expected,
rule}, …]}`. Every `rule` cites the deriving section of
`docs/mining_system.md` (and `docs/architecture.md` §3 where the pure-function
boundary matters).

## Inputs and the snapshot shape

Case inputs are **reduced synthetic snapshots**: compact projections of the
block-consistent snapshot exemplified by `fixtures/tig/v1/`, keeping only the
fields the decision under test reads. Mapping to the v1 endpoint shapes:

| Case input field | fixtures/tig/v1 source shape |
|---|---|
| `challenges[].active_tracks.*.num_nonces_per_bundle`, `min_num_bundles`, `type` | `get-challenges.json` → `challenges[].config` |
| `challenges[].network_qualifiers_by_track` | `get-challenges.json` → `challenges[].block_data.num_qualifiers_by_track` |
| `pool_qualifiers_by_challenge_by_track` | `get-opow.json` → `opow.block_data.num_qualifiers_by_challenge_by_track` |
| `algorithms[].adoption` (PreciseNumber decimal string) | `get-algorithms.json` → `algorithms[].block_data.adoption` |
| `algorithms[].banned` | `get-algorithms.json` → `algorithms[].state.banned` |
| `algorithms[].has_successful_binary` | `get-algorithms.json` → `binary.details.compile_success && download_url != null` |
| `algorithm_track_stats[a][t].qualifiers` | `get-algorithms.json` → `algorithms[].block_data.num_qualifiers_by_track_by_player`, summed over players |
| `algorithm_track_stats[a][t].active_bundles` | `get-tracks-data.json` → per-challenge `[].num_bundles` for the (track, algorithm) row |
| `active_benchmarks[].average_quality_by_bundle` | benchmark data field named in `mining_system.md` §2 "Active bundle" |
| `confirmed_in_flight_benchmarks` | pool workflow state (`mining_system.md` §2 "In-flight benchmark"), not a TIG endpoint |
| `offered_compute` | member capacity offer (`mining_system.md` §4.1) |
| `supplied_randomness` | explicit input per `architecture.md` §3 — the engine never generates randomness |

`hyperparameter_sources_available` is a deliberate simplification in the
challenge-selection and projected-qualifier families: it states, per challenge,
which active tracks have at least one valid §6.6 source benchmark. The full
source-selection logic (including what "valid" filters out) is exercised
case-by-case in `hyperparameter-source.json`; repeating full benchmark lists in
every challenge-selection case would obscure the decision under test.
Similarly, `bundle-sizing.json` supplies `selected_algorithm_best_on_track` as
an explicit flag; its derivation is covered by the §6.5 cases in
`algorithm-selection.json`.

Numeric convention: factors and rates are recorded as exact fractions
(`"fraction": "29/137"`) with a rounded `decimal_approx` for readability only.
Expected comparisons hold for the exact ratios; no case depends on a
floating-point rounding direction.

## Recorded conventions and open questions

Where `mining_system.md` genuinely does not determine a value, the fixture
records an explicit convention in the case (never a silent invention) and lists
it here. If the design docs later settle these differently, corrections go in a
`v2` directory (files here are immutable once merged).

1. **Challenge-tie randomness mapping (§6.3) — resolved.** Settled by
   `docs/adr/0005-challenge-tie-derivation.md` and the normative derivation
   now in `mining_system.md` §6.3: the controller derives
   `challenge_tie_seed = BLAKE3("tig-pool-challenge-tie-v1" \n network \n
   block_id)` from the decision's anchor snapshot block and a BLAKE3 draw
   rank per candidate challenge; the tied candidate with the smallest rank
   (32-byte big-endian comparison) wins, with a byte-order-smallest
   `challenge_id` fallback on collision. This matches the fixture convention
   exactly (per-candidate rank supplied as input, smallest wins), so the v1
   case data here is unchanged and remains valid — the supplied integer
   ranks stand in for derived 32-byte ranks by order. v2 cases should carry
   concretely derived seed and rank values (recorded on issue #31). The
   §6.3 worked example is proven by
   `crates/pool-domain/tests/challenge_tie_vector.rs`.
2. **Direction of `algorithm_id` ties (§6.4 step 4, §6.5).** "Resolve … 
   deterministically by `algorithm_id`" does not state an ordering direction.
   Fixture convention: the lexicographically **smallest** `algorithm_id` wins.
3. **All eligible algorithms at zero adoption (§6.4).** Step 2 ignores
   zero-adoption algorithms but no step says what happens when nothing
   remains. The fixture derives "no algorithm selectable → challenge
   ineligible" from §6.2's requirement of an eligible algorithm; recorded as a
   derivation, not an explicit rule.
4. **"Most recently confirmed" (§6.6 step 4).** Read as the highest
   `block_confirmed` height. The spec does not name the field or define
   recency for same-block confirmations; same-height ties fall through to the
   lowest-benchmark-ID rule, which the fixtures exercise.
5. **Numeric representation of rates and factors (§6.3, §6.5).** The spec
   writes real-valued ratios without fixing rational vs floating-point
   arithmetic. Fixtures record exact fractions and avoid cases whose outcome
   depends on representation (the closest comparison, 29/137 vs 1/5, is safe
   in both).
6. **Fuel budget bounds (§6.6).** The fuel budget is copied verbatim from the
   source benchmark. Whether it must additionally be clamped to the
   challenge's `max_fuel_budget` is not stated; no fixture case exercises a
   source whose fuel exceeds the cap.

Out of scope for this fixture set: §6.1 member admission and financial
reservation (Controller policy over accounting state, not a pure
`decide(...)` rule family named by issue #3), §6.8 protocol-fee admission, and
§7 qualifier attribution (its own fixture family per
`docs/pre_build_checklist.md` §6).
