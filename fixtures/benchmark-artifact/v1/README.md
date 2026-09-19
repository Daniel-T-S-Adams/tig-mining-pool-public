# Benchmark artifact fixture v1

Provenance: **constructed** (not captured). One golden 8-nonce proof-material
package plus the negative cases the Artifact Worker and fault attribution must
distinguish (`docs/pre_build_checklist.md` §6, issue #4). Wire format per
`docs/member_protocol.md` §10–§12 and `schemas/member_protocol/v0.1.0/`;
failure classification per `docs/member_protocol.md` §15 and
`docs/mining_system.md` §8. Follows the fixture conventions of
`fixtures/tig/v1/README.md`: versioned immutable directory, expected values
recorded independently in `expected.json` with the rule that derives each one,
no credentials anywhere (ids and digests are obvious fakes; opaque digests are
SHA-256 of documented `fixture:*` preimage strings listed in
`tools/build_fixture.py`).

The world here is deliberately small and self-contained: challenge `c001`,
algorithm `a011`, and the pool player id echo `fixtures/tig/v1`, but track
`t900` (4 nonces per bundle × 2 bundles = 8 nonces) is invented so this set
does not contradict the snapshot's track data. No value is a live protocol
constant.

## Layout

| File | Meaning |
|---|---|
| `assignment.json` | The confirmed `AssignmentIdentity` (schema-valid) that every package here is validated against. Pretty-printed; its digest is SHA-256 over its RFC 8785 canonical form (`member_protocol.md` §7), recorded in `expected.json`. |
| `expected.json` | Expected schema-validation result, first failing check, and fault classification for every case, each with a rule citation. |
| `cases/<case>/…` | One complete logical package per case (see encoding below). |
| `tools/build_fixture.py` | Deterministic builder/verifier; regenerates every derived file byte-for-byte. Requires `pip install blake3`. |

## Package encoding in this fixture

The real wire object is one `tar+zstd` archive (`member_protocol.md` §10.1).
This fixture pins the **logical archive members**, not the compressed frame
(see open questions):

- `manifest.json` — the exact canonical bytes of the archive member:
  RFC 8785 canonical JSON, single line, **no trailing newline** (§10.2 "no BOM
  or trailing whitespace"). The `manifest_sha256`/sizes in `expected.json`
  are over these bytes.
- `outputs.ndjson` — exact bytes: one canonical JSON object + one LF per
  nonce (§10.2).
- `qualities.i32le.hex` / `leaf-hashes.bin.hex` — the binary members
  hex-encoded for reviewability: one line per record (8 hex chars per i32
  little-endian quality; 64 hex chars per 32-byte leaf hash). Decode by
  stripping newlines and hex-decoding the concatenation; the manifest's
  `size_bytes`/`sha256` refer to the **decoded** bytes.
- Byte-level cases (`bad-package-checksum`, `truncated-upload`) have no
  archive members; `upload.json` records the §11 upload declarations and the
  observed state.

## Merkle and leaf-hash convention (pinned)

Transcribed from upstream commit
`ad08d1ea001a73ff5aab3b556d7f59246fece14e` of
`github.com/tig-foundation/tig-monorepo` (the commit pinned in
`assignment.json` `runtime.tig_upstream_commit`):

1. **Canonical JSON** (`tig-utils/src/json.rs` `jsonify`): serde_json compact
   output with object keys sorted ascending, recursively. For the ASCII-only,
   integer-only values used here this equals RFC 8785.
2. **Solution signature** (`tig-structs/src/core.rs`
   `OutputData::calc_solution_signature`, `tig-utils/src/hash.rs`):
   `u64::from_le_bytes(blake3(jsonify(solution))[0..8])`, where the solution
   is jsonified as a JSON string (quotes included).
3. **Leaf hash** (`core.rs` `From<OutputMetaData> for MerkleHash`):
   `blake3(jsonify({fuel_consumed, nonce, runtime_signature,
   solution_signature}))` with all four as **bare JSON integers** (serde_json
   prints `u64` exactly, even above 2^53), keys in the alphabetical order
   shown. On the member wire, `runtime_signature`/`fuel_consumed` are decimal
   strings (§10.2); the pool parses them to u64 before reproducing the leaf.
4. **Root** (`tig-utils/src/merkle_tree.rs` `calc_merkle_root`): fold the
   ordered leaf list pairwise with `blake3(left32 ‖ right32)`; an unpaired
   trailing node is **promoted unchanged**; leaves are **not padded** to the
   power-of-two `tree_capacity`. With 8 leaves the tree is perfect and
   promotion is never exercised.
5. **Branch** (`merkle_tree.rs` `calc_merkle_branch` /
   `MerkleBranch`): ordered `(depth, sibling_hash)` pairs; serialized as the
   concatenation of `{depth:02x}{hash:064hex}` per element.
   `MerkleBranch::calc_merkle_root(leaf_hash, nonce)` must reproduce the
   manifest root.

`tools/build_fixture.py` implements exactly this and verifies the committed
files; run it with no arguments to check, `--write` to regenerate, and
`--summary` to print all derived values.

## Cases

| Case | Category | Schema | First failing check / outcome | Classification |
|---|---|---|---|---|
| `golden` | complete artifact | pass | none — STRUCTURALLY/DURABLY acceptable; sampled proofs (nonces 1, 4, 6) verify | `NONE` |
| `bad-package-checksum` | corrupt upload | n/a | finalize checksum mismatch, never `RECEIVED` (§11, §12) | retry + investigate (§15); `MEMBER` only with stable-bytes evidence |
| `truncated-upload` | incomplete upload | n/a | bytes incomplete at `package_due_before_block` (§7, §11) | `MEMBER` (deadline missed, services available); `POOL` variant if pool outage evidenced |
| `missing-manifest-entries` | corrupt package | **fail** | manifest schema: `files.leaf_hashes`, `merkle.root` missing | `MEMBER` (§15 row 1) |
| `wrong-declared-size` | corrupt package | pass | declared `files.outputs.size_bytes` ≠ observed (§12) | `MEMBER` (§15 row 1) |
| `missing-nonce` | incomplete package | pass | `record_count` 7 ≠ `num_nonces` 8; coverage not `[0,8)` (§10.2, §16 inv. 7) | `MEMBER` (§15 row 2) |
| `duplicate-nonce` | corrupt package | pass | nonce 3 duplicated, 4 missing; order/coverage (§10.2) | `MEMBER` (§15 row 2) |
| `leaf-hash-mismatch` | corrupt package | pass | leaf 3 does not reproduce from its record (§12) | `MEMBER` (§15 row 1) |
| `merkle-root-mismatch` | corrupt package | pass | calculated root ≠ declared root (§12) | `MEMBER` (§15 row 1) |
| `identity-mismatch` | wrong binding | pass | manifest ≠ assignment (digest, slot generation, binary digest) (§10.2, §12, §14) | `MEMBER` (§15 rows 1–2); `POOL` variant if pool published inconsistent assignment |
| `solution-invalid` | solution-invalid | pass | mechanically perfect; zero bundles meet min verification quality | no fraud; **chargeable tier failure** (§15; mining_system §8) |
| `method-non-reproducible` | non-reproducible | pass | mechanically perfect; re-execution disagrees at nonces 2, 5 (`reproduction.json`) | `MEMBER` via **method-loss rule**, not `f` (§15; mining_system §8) |

`expected.json` additionally records four package-less attribution scenarios
(pool loses artifact after receipt, pool rejects a valid package, transient
network retry, unresolved corruption origin) so the `POOL` / retry /
`UNRESOLVED` sides of the §15 table are represented.

## Assumptions to verify during the protocol spike

Recorded honestly rather than invented:

1. **Compressed frame bytes are not pinned.** Zstandard output depends on the
   compressor build/level, so the whole-package SHA-256 and compressed sizes
   in `upload.json` are documented placeholders, not golden values. The
   fixture pins the logical member bytes and the canonical `manifest.json`
   bytes. Decide during the spike whether a canonical compression recipe is
   worth pinning, or whether tests should compress on the fly and treat the
   compressed digest as run-scoped.
   **Settled (spike S3/S6, PR #28):** compressed digests are run-scoped; no
   canonical compression recipe is pinned. The member-declared whole-package
   SHA-256 binds each upload (`member_protocol.md` §11) and no protocol step
   requires two parties to reproduce identical compressed bytes. The recipe
   is recorded per run for audit (spike runs: single zstd frame, level 19,
   frame checksum, pledged content size).
2. **`jsonify` vs RFC 8785.** Equivalence is exact for the ASCII/integer-only
   values used here. Upstream `jsonify` does not escape or sort exactly like
   JCS for non-ASCII or exotic strings; if real solutions can contain such
   data, verify against the upstream implementation before relying on JCS.
3. **Leaf preimage integer width.** Leaf hashing uses bare integers via
   serde_json `u64` (exact above 2^53), while member-wire JSON integers are
   capped at 2^53 and full-width values ride in decimal strings. Verify that
   the pool's reproduction path parses the strings and hashes bare integers,
   as this fixture assumes (most golden runtime signatures exceed 2^53, and
   nonces 1, 5, 6 exceed 2^63).
   **Verified live (spike S4/S6):** the pool parsed the member's decimal
   strings to u64 and hashed/submitted bare integers; TIG confirmed the
   proofs and activated the benchmarks, which requires every leaf hash (and
   the quoted-solution signature preimage, assumption 2's ASCII case) to be
   byte-exact. Non-ASCII/exotic solution strings remain unexercised
   (assumption 2 stays open for those); see `tig_integration.md` §6.3.
4. **Non-power-of-two leaf counts.** The promotion rule for unpaired nodes is
   taken from the pinned source but is not exercised by the 8-leaf golden
   tree (only by the internally-consistent `missing-nonce` tree over 7
   leaves). Confirm against TIG verification before relying on it.
   **Confirmed against live TIG verification (spike S4/S6):** both live
   10-nonce benchmarks (`894e4d4f…`, `818b03c1…`) build 10-leaf trees whose
   pairwise fold exercises promotion at two levels; TIG confirmed the
   sampled-nonce proofs and activated both benchmarks, which requires every
   branch over the promoted tree to resolve to the committed root.
5. **Published chargeable reason codes — the gate is gone (ADR 0013).**
   `member_protocol.md` §15 used to say only *published* chargeable
   tier-failure reason codes increment `f`, and this question recorded that
   the list did not exist yet. ADR 0013 removed that gate: every chargeable
   failure increments `f` whatever reason code it carries, because a charge no
   longer depends on cause.

   **One case is now wrong, not merely differently gated.**
   `expected.json`'s `method-non-reproducible` asserts
   `chargeable_tier_failure: false` with `method_loss_rule: true`, citing the
   rule that a method-verification outcome "does not increment `f` unless
   another chargeable failure also occurred". `accounting.md` §11.6 now counts
   a successfully arbitrated report among its four chargeable failures and
   `mining_system.md` §8 counts it toward `f > k`, so that assertion would
   under-count `f` on the tier-removal path. It is invalid as an oracle for
   the tier count and the correction goes in a `v2` (issue #56).

   The `pool-lost-after-receipt` scenario needs the same reading: it is still
   a pool failure and is now charged to its owner anyway (§11.6,
   `member_protocol.md` §12).

   Every other `chargeable_tier_failure` value here asserts the structural
   member-fault classification, which is unchanged — what has gone is the
   published-list condition they were waiting on, so read those as asserting
   the classification alone.
6. **`min_verification_quality = 40`** and the sampled nonce set `{1, 4, 6}`
   are fixture inventions standing in for TIG-published values.
7. **Bundle-average rounding** (`average_quality_by_bundle`) is not pinned by
   this fixture and is intentionally unused.
