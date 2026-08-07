# ADR 0005: Block-derived challenge-tie draw

Status: accepted for v0  
Date: 2026-08-07

## Context

`mining_system.md` §6.3 requires resolving projected-raw-factor ties between
challenges "randomly using a recorded, block-derived random value so the
decision is reproducible". `architecture.md` §3 makes the Decision Engine
pure — random values must be supplied explicitly — and §5.1 step 4 requires
the controller to derive and persist any block-specific tie input before
calling the engine. No document defined the derivation: unlike qualifier
attribution, which has the §7.1 `tie_seed` schema, challenge selection had
no analogue. `fixtures/decision-engine/v1/README.md` recorded this as open
question 1 and adopted a placeholder convention: the input supplies a
per-candidate rank (`supplied_randomness.challenge_tie_draw_ranks`) and the
smallest rank wins. The protocol spike never exercised challenge selection
(challenge and algorithm were operator-chosen), leaving the gap open
(`docs/protocol_spike_report.md`, open question 4; issue #34).

Relevant block facts: the pinned `BlockDetails` exposes `prev_block_id`,
`height`, `round`, `timestamp`, `num_confirmed`, `emissions`, and
`gamma_value` alongside the block `id`; there is no protocol randomness
beacon (no `rand_hash`-style field). Live block ids are opaque 32-hex
strings produced by TIG (`docs/protocol_spike_report.md` finding 9), outside
the pool's influence.

## Decision

Resolve challenge-factor ties with a BLAKE3 draw derived from the decision's
anchor snapshot block, normatively specified in `mining_system.md` §6.3:

```text
challenge_tie_seed = BLAKE3(utf8(
    "tig-pool-challenge-tie-v1" || "\n" || network || "\n" || block_id))

draw_rank[c] = BLAKE3(challenge_tie_seed || utf8("\n" || challenge_id))
```

Ranks are compared as 32-byte unsigned big-endian integers; among tied
challenges the smallest rank wins; an exact rank collision falls back to the
smaller `challenge_id` in byte order. The controller derives the seed and the
full rank map for every compute-compatible eligible challenge, supplies the
ranks to the pure engine as explicit input, and persists the seed inputs,
rank map, tied set, and winner with the decision record. The seed contains no
per-decision input, so every decision anchored to the same block uses the
same draw. New invariant 25 records the reproducibility obligation.

## Requirement arguments

**Deterministic and auditable.** The draw is a pure function of `network`,
the anchor `block_id`, and the snapshot's challenge ids — all persisted
snapshot facts. An auditor re-derives the seed, every rank, and the winner
from the decision record alone; the persisted rank map is a convenience and
a cross-check, not a trust root.

**Block-anchored.** The only variable inputs are the anchor block id and the
challenge ids that the anchored snapshot itself fixes. The decision already
must use exactly one block-consistent snapshot (invariant 10); the draw adds
no second anchor and no out-of-snapshot input.

**Non-manipulable.** The pool's degrees of freedom were analyzed
explicitly:

- *Input fields.* `block_id` is an opaque TIG-produced hash; the pool cannot
  influence it, nor `prev_block_id`, nor the challenge id set. `network` is
  fixed deployment configuration.
- *Anchor choice.* `tig_integration.md` §8 allows a precommit's
  `settings.block_id` to reference TIG's latest **or second-latest** block
  when processed, which superficially offers a one-block re-roll window. But
  the same section pins that reads documented with `block_id` require the
  **latest** block, so a snapshot can only ever be opened against the
  then-latest block: the second-latest allowance covers TIG-side processing
  lag between snapshot and precommit processing, not a pool choice between
  two candidate anchors. The residual freedom — deliberately deciding
  against an older persisted snapshot while a newer complete one exists —
  is closed normatively: §6.3 requires each decision to use the newest
  complete persisted snapshot available when its decision transaction
  begins, and snapshot identities are persisted with every decision, so
  systematic staleness is visible in the audit trail.
- *Timing.* The pool controls when decisions happen and can therefore delay
  a decision to the next block (~60 s) to obtain a fresh draw. This freedom
  is inherent to any block-derived randomness without an external beacon and
  is accepted as bounded: the draw only permutes among challenges the pool's
  own objective scored exactly equal; it gates no member funds, payouts, or
  §7 qualifier attribution (attribution is independent of which challenge
  was selected); delaying costs real mining throughput; and persisted
  decision timestamps against block heights make a re-roll-farming pattern
  auditable.
- *Per-decision salt.* Deliberately excluded. Salting with `offer_id` or a
  decision timestamp would hand the pool a per-decision re-roll through
  ordering and timing of its own records.

**Domain-separated.** `tig-pool-challenge-tie-v1` joins the lowercase
mining-rule domain-string family (`tig-pool-qualifier-tie-v1`,
`mining_system.md` §7.1; `tig-pool-payout-remainder-v1`, `accounting.md`
§6) and is distinct from every existing string and from the uppercase
`TIG-POOL-*-V1` member-protocol signing domains. Future tie uses take new
strings; the `-v1` suffix pins this encoding and hash together, so any
change is a new version, never a silent redefinition.

**Total ordering without collision ambiguity.** Byte-order comparison of
32-byte BLAKE3 outputs is a strict total order except under a hash
collision, which the smaller-`challenge_id` fallback resolves
deterministically — mirroring §7.1's canonical-identity fallback. This keeps
the fixture v1 convention (per-candidate rank, smallest wins) intact: the
v1 integer ranks stand in for derived ranks by order, and no v1 case data
changes.

**Implementation-neutral hash, pinned.** BLAKE3 is already the repository's
convention (§7.1 tie seed, the accounting remainder rule, and the
`tig-merkle-blake3-v1` Merkle material). The byte encoding is explicit
newline-joined UTF-8 over newline-free ASCII inputs rather than the not yet
fully pinned `canonical_encode`, so the §6.3 test vector is byte-exact and
implementable today (`crates/pool-domain/tests/challenge_tie_vector.rs`).

## Alternatives considered

- **TIG-published randomness beacon.** The pinned `BlockDetails` exposes no
  `rand_hash` or seed field. Unavailable.
- **Height-based selection (`height mod n` over the tied set).** Couples the
  winner to the size and enumeration order of the tied set — adding or
  removing an unrelated tied challenge arbitrarily flips the winner — and
  provides no domain separation or per-challenge identity binding. Rejected.
- **VRF over the block id with a pool key.** Verifiable and unpredictable,
  but adds signing-key custody (a human-only concern per the operating
  contract), member-side verifier tooling, and buys nothing: all inputs are
  already public, so a plain hash is equally auditable, and a VRF is
  re-rollable through exactly the same timing freedom. Rejected as
  complexity without benefit.
- **Recorded OS randomness.** Reproducible only by trusting the record; the
  pool could sample draws until one suited it. Fails auditability from facts
  and non-manipulability. Rejected.
- **Per-decision salt (offer id, timestamp).** Grants a per-decision re-roll.
  Rejected (see the non-manipulability argument).
- **`prev_block_id` as input.** Identical properties one step removed from
  the anchor; no benefit over the anchor block id. Rejected.

## Consequences

- `fixtures/decision-engine/v1` case data remains valid unchanged; open
  question 1 in its README is resolved. v2 cases should carry concretely
  derived seed and rank values (recorded on issue #31).
- Every decision record persists the domain string, network, anchor block
  id, rank map, and — on a tie — the tied set and winner.
- `crates/pool-domain` carries the §6.3 test vector as a Rust test with a
  `blake3` dev-dependency.

## Revisit when

- TIG publishes a per-block randomness beacon (switch the seed input under a
  new `-v2` domain string).
- Evidence shows challenge choice materially redistributes member rewards,
  which would justify closing the timing freedom (for example a mandatory
  one-block anchor delay or commit-then-decide anchoring).
- `canonical_encode` is formally pinned repository-wide and this rule should
  migrate to it — that migration is `tig-pool-challenge-tie-v2`, never an
  in-place change.
