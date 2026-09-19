---
title: "Slice 2: member agent, member API, and artifact ingestion"
status: active
created: 2026-09-17
source: docs/pre_build_checklist.md §10 step 2
last_verified: 2026-09-17
---

# Slice 2 plan: member agent, member API, and artifact ingestion

Plans describe intended work and may go stale. `docs/pre_build_checklist.md`
§10 owns the slice order; `docs/member_protocol.md` owns the member wire
contract; `docs/architecture.md` owns component and storage boundaries;
`docs/security.md` and `docs/member_attack_model.md` own the controls against a
hostile member. This plan **references** those rules and does not restate them —
where a criterion cites a section, that section is authoritative and this
document is not. Once slice-2 code exists, its tests outrank this plan.

Status meanings: `active` (being executed), `implemented` (superseded by the
code and its tests), `abandoned`.

## 1. Goal

**One member machine, one slot, one benchmark, end to end.**

A member agent enrols, registers and qualifies a slot, offers capacity, receives
an assignment built from a *confirmed* TIG precommit, computes every nonce,
packages the proof material, uploads it resumably, and receives an immutable
durable-acceptance receipt that releases its slot. The pool then submits the
benchmark commitment TIG has been waiting for.

Slice 1 built the half of that sentence facing TIG: precommits, the write
ledger, the confirmation-driven state machine, restart safety. It ran with a
**pool-owned placeholder** where a member should be (criterion F6) and a
**feature-gated stub** where durable acceptance should be (F4a, F4c, F4d).
Slice 2 replaces both with the real thing, and retires the stub.

The proof of this slice is the same shape as slice 1's: the full ladder runs
deterministically against `fake-tig` in CI, and once against live testnet with
a real member agent on a real machine, ending in a TIG-confirmed benchmark
commitment.

## 2. Non-goals

Deferred with the step that owns them, not dropped:

- **Sampled proof construction and serving** (`architecture.md` §5.3) and
  **artifact retention and deletion** (§8.4) → checklist §10 **step 3**. Slice 2
  publishes the accepted package and the commitment payload; it does not build a
  proof branch or delete anything.
- **Tiers, collateral, joining fees, deposits, and the member balance** →
  **steps 6–8**. What is deferred is the **tier** as the source of the number:
  `tier_number` needs a finalized joining-fee batch (`architecture.md` §7.6),
  which needs the accounting slices.

  The *bound itself* is not deferred, and slice 1's D2b reasoning does not carry
  here. D2b was justified by there being no member to test against; slice 2 is
  the slice that creates members, and a member with N registered slots would
  otherwise hold N concurrent unverified benchmarks bounded only by the
  pool-wide limit — which `member_protocol.md` §6 forbids ("absence of an
  applicable policy produces `NO_ACTION`, never unlimited work") and
  `security.md` §11 invariant 15 keeps distinct from the global limit. D11 below
  is the compensating bound slice 2 enforces, from configuration instead of from
  a tier. Slice 2 also keeps the per-slot rule: one open assignment occupies
  exactly one slot (`member_protocol.md` §16 invariant 1).

  Two things changed here after this plan was written. ADR 0010 added a
  per-member collateral multiplier to §11.4's reserve, and ADR 0013 removed the
  separate failure-charge term `X` from it — the charge is now derived from the
  benchmark's own fee, and it no longer depends on fault at all. D10, D13 and F6
  carry those.
- **The member website** → **step 7**. The *account* is no longer deferred with
  it. ADR 0011 chose the login mechanism after this plan was written: a member
  signs in by proving control of a Base address, and that address **is** the
  member account (`member_protocol.md` §3.1, §17; `accounting.md` §12.2). So
  slice 2 implements the proof rather than an interim stand-in — B8 and B9 below
  — and what waits for step 7 is the user interface that calls it. An earlier
  draft of this plan had `pool-admin` mint enrollment tickets; that would now be
  building something to throw away.

  There is **no account recovery**, by design rather than by deferral: the
  account is the wallet, so a member who loses it loses the authority that
  issues worker-recovery tickets (ADR 0011, `member_protocol.md` §3.3). Only the
  *worker*-recovery HTTP route stays deferred (§17).
- **Metrics and alert tests** → **step 5**, with slice 1's I2 and I4.
- **GPU work.** v0 defines GPU slots (`member_protocol.md` §6) and the schema
  accepts them, but the pool serves `cpu` today and TIG's live testnet offers no
  active GPU challenge. A GPU slot may register and qualify; no GPU assignment
  is published, and the live run is CPU. The measurement gap — GPU member
  overhead never exercised — stays open as issue #54.
- **Semantic screening of solutions** (`security.md` §5.6). TIG remains the
  verifier; slice 2 does the mechanical checks §5.4 lists and no more.

## 3. What ships

Two new private binaries, one member-side binary, and the controller's
member-facing half:

```text
pool-api           public member HTTPS service (architecture.md §4)
artifact-worker    private bounded ingestion worker
member-agent       the member machine's agent
```

plus, inside `pool-controller`: offer admission and disposition, assignment
publication from confirmed precommits, the durable-acceptance transaction, slot
release, and member fault attribution; and the migrations these need.

`crates/pool-identity` already implements `member_protocol.md` §2, §3.1, §3.2
and §3.3 against the spike's fsynced-document store. Slice 2 moves it behind
PostgreSQL and the Pool API rather than rewriting it (§7 below).

## 4. Acceptance criteria

Each criterion is a property with a test, not a task. Where a criterion says
"Test:", that test is the criterion.

### A. Pool API surface and request authentication

- A1. `pool-api` starts from exactly one explicit TOML path under the same
  fail-closed rules as slice 1's A1, holds **no** TIG API key
  (`architecture.md` §2.2), and its database role can neither create a TIG write
  intent nor change a workflow (§6's grant table). Test: the grant tests run
  under the real `pool_api` role.
- A2. `GET /member/v0/protocol` is public, returns the exact protocol and
  package versions the server accepts plus server time, and is the only
  unsigned route besides enrollment (`member_protocol.md` §4).
- A3. Every other route verifies, **in this order**, the body hash and the
  Ed25519 signature over §3.2's exact signed string, before JSON decoding,
  database work, or upload quota reservation (`security.md` §4.1). Test: a
  request whose body does not match `X-Body-SHA256` is rejected without the
  handler ever decoding it.
- A4. Timestamp freshness is 300 seconds either side of server time, and
  `(credential_id, request_id)` is remembered for 24 hours; a reused request ID
  with different signed bytes is rejected **and audited** (§3.2).
- A5. Authorization is relational: any worker ID in path or body must equal the
  signed header, every resource is resolved through a query constrained by that
  worker, `member_id` is derived from the stored binding and never from the
  request, and an absent object and another worker's object return the **same**
  denial (`security.md` §4.2). Test: worker B cannot observe whether worker A's
  assignment exists.
- A6. Every state-changing JSON route stores the idempotency key, the canonical
  body hash, and the result in one transaction; the same key and body return the
  recorded result and the same key with a different body returns `409
  IDEMPOTENCY_CONFLICT` (`member_protocol.md` §5). Test: one property test over
  the whole route table, so a route added later without a key fails it.
- A7. Rate limits are per member, worker, credential, and endpoint class, with
  separate budgets for authentication, control, upload-session creation, and
  chunk traffic, so upload traffic cannot starve heartbeats (`security.md`
  §4.3). A limit returns `429` with `Retry-After` and never affects trust.
- A8. An error echoes `request_id` (or `enrollment_request_id`), and never
  reveals whether a worker, credential, or resource exists (§3.2, `security.md`
  §4.1).

### B. Enrollment, rotation, revocation

- B1. An enrollment ticket is a single-use bearer value of at least 256 bits,
  stored only as an HMAC-SHA-256 under a dedicated server key, bound to one
  member and purpose `WORKER_ENROLLMENT`, expiring in 15 minutes; consuming it
  and creating the worker are **one** transaction (`member_protocol.md` §3.1,
  `security.md` §4.1). Test: the stored row never contains the bearer value.
- B2. Enrollment verifies possession of the member's new key over §3.1's exact
  `TIG-POOL-ENROLLMENT-V1` string. A repeated `enrollment_request_id` with the
  same body returns the same response; a changed field is a conflict.
- B3. Rotation issues a new credential, proves possession of the new key over
  §3.3's `TIG-POOL-ROTATION-V1` string, and the old credential stays valid for a
  server-declared grace no longer than 10 minutes, then is `REVOKED`. Test: the
  old key works inside the grace and fails after it.
- B4. Each signing domain is separate: an enrollment signature cannot be
  replayed as a rotation or recovery proof, and vice versa (§3.1, §3.3). Test:
  cross-domain replay of each of the three.
- B5. Revocation takes effect on the **next** request — not after a cache
  expiry — and prevents new offers, heartbeats, events, and uploads while
  erasing no state and reassigning no benchmark (§3.3, `security.md` §4.1).
- B6. Recovery attaches a new key to an existing worker and revokes every older
  credential, under §3.3's `TIG-POOL-RECOVERY-V1` proof and a 15-minute
  `WORKER_RECOVERY` ticket, preserving ownership. The ticket is authorized by
  the member's wallet signature (B8), not by an operator command. A worker
  revoked as a security action is not recoverable by this path, and the account
  behind it cannot be recovered at all (ADR 0011).
- B7. Authentication failures, duplicate requests, and stale timestamps have no
  mining-trust effect (`member_protocol.md` §15).
- B8. **The member account is a Base address, proved by signature.** A member
  authenticates with a domain-separated EIP-191 or EIP-712 signature carrying
  pool domain, chain ID, address, a one-time nonce, purpose, and expiry, and the
  pool validates the exact domain so a signature cannot be replayed against
  another pool or chain (ADR 0011, `accounting.md` §12.2). Tests: a signature
  for another domain, another chain id, an expired one, and a reused nonce are
  each refused; a valid one authenticates exactly the address that signed it.
- B9. **That signature is what authorizes an enrollment or worker-recovery
  ticket.** The `member_id` remains pool-issued and opaque as
  `member_protocol.md` §2 requires; the address is unique per member and is the
  only thing a session proves. The ticket HMAC key stays in the Pool API alone
  (`security.md` §4.1), so no other process holds it. Slice 2 supplies no
  address-change operation, because ADR 0011 leaves nothing for one to do.

### C. Slots and qualification

- C1. A slot records exactly the facts `member_protocol.md` §6 lists, and
  `(worker_id, client_slot_key)` locates it. Registration is idempotent under
  `slot_registration_id`.
- C2. Reconfiguration names the current generation as `prior_slot_generation`,
  atomically increments the generation, and requires requalification; a stale
  prior generation conflicts; reconfiguration is refused while an offer,
  assignment, or upload is open (§2).
- C3. A repeated registration ID with the current prior generation and unchanged
  compute and runtime facts is a **no-op** returning the existing generation and
  qualification — it does not manufacture a generation because the request ID
  changed (§2). Test: the generation counter is unchanged.
- C4. `qualification_spec_digest` is computed exactly as §6 defines it, over RFC
  8785 canonical JSON, with `available_image_manifest_digests` sorted by
  ascending UTF-8 byte order so sender order cannot change it. Test: a shuffled
  array produces the same digest, and any changed fact produces a different one.
- C5. The qualification task carries one complete known-output execution (§6's
  list), expires after 15 minutes, and is bound to that exact digest.
- C6. The pool independently hashes the canonical result, compares expected
  quality and output, and re-derives the digest from the **observed** inventory
  before marking that exact generation `QUALIFIED`; a mismatch is `FAILED` with
  a reason. Test: each of the three mismatches fails on its own.
- C7. A new generation invalidates every older qualification, and a slot may
  offer only with a successful qualification for its exact current generation
  (`member_protocol.md` §16 invariant 2).
- C8. A failed or timed-out qualification prevents offers and is **not** a
  mining-trust failure (§6, §15).

### D. Capacity offers, admission, and the queue

- D1. An offer is valid only with `slot_state = AVAILABLE`, repeats the current
  compute facts and qualification digest, and fails closed on drift (§6). Test:
  a digest that no longer matches the slot is refused.
- D2. Another open offer or assignment for the same slot is rejected atomically;
  other slots on the same worker continue (§6 step 1, §9).
- D3. Admission applies account status, qualification, compatibility, and
  unresolved-exposure limits, and enforces `pool_unverified <
  internal_pool_unverified_limit` **in the same serialized transaction** that
  creates the precommit intent (`architecture.md` §7.6) — the transaction slice 1
  built, now entered from an offer instead of a placeholder.
- D4. An ineligible member gets `NO_ACTION` or `REJECTED` **without** queuing
  (§6 step 4).
- D5. An eligible member blocked only by the pool-wide limit gets `QUEUED` with
  a renewable lease and a FIFO position keyed `(queue_accepted_at, offer_id)`,
  and that queue entry creates **no** TIG write and **no** financial reservation
  (§6 steps 5, `member_protocol.md` §16 invariant 15).
- D6. If any live entry is queued, a new eligible offer appends behind it even
  if the global count has just fallen below the limit; only the promotion path
  consumes that position (§6).
- D7. Promotion selects the oldest eligible live offer, issues `CONFIRM_AVAILABLE`
  with a fresh `ready_check_id`, and only the worker's echo of that ID in a
  signed heartbeat permits the atomic capacity check and precommit intent (§6).
  Test: a promotion without the echo creates no intent.
- D8. An expired ready check or offer lease removes the offer **without** a
  member penalty and examines the next entry (§6, §15).
- D9. A pending offer heartbeats every 30 seconds against a server-declared
  `lease_expires_at`; a lease that expires before a precommit is submitted
  cancels the offer without trust effect, and once `PRECOMMIT_SUBMITTED` is
  recorded a missing heartbeat marks the worker unreachable but never silently
  reassigns the benchmark (§6).
- D10. Reserving the bundle-scaled financial exposure precedes the precommit,
  and failure to reserve cancels the pending offer **without** a TIG write (§6
  final paragraph). In slice 2 "reserve" means what slice 1's D2c means: the
  `accounting.md` §11.4 amount is computed and durably recorded with the
  decision, and nothing is posted.

  The amount is §11.4's **current** expression, which changed after this plan
  was written: `ceil(P[s] * B[t] * M_bps[m] / 10_000) + F[s,t]`, maximised over
  the proposed tracks. There is no `X` term — ADR 0013 derives the failure
  charge from the benchmark's own fee, so the fee appears once rather than
  twice. Slice 1 ships `precommit_failure_charge_atoms` set to zero, which makes
  the arithmetic already equal; issue #61 removes the term, and slice 2 must not
  reintroduce it.

  **What is deferred, and what that costs.** §11.4's admission gate —
  `eligible_collateral[m] - reserved_exposure[m] >= precommit_reserve` — needs a
  balance, and balances need the deposit path in step 8. So slice 2 records the
  reservation and does not check it against a member's collateral, which means
  a slice-2 member does work that nothing of theirs backs. That is safe only
  because slice 2 is testnet, where the pool's exposure is testnet TIG; the gate
  must land before any deposit is accepted or any mainnet work is decided, and
  `pre_build_checklist.md` §9's launch gates are where that is checked.
- D11. **The per-member concurrency bound, without tiers.** A member's
  concurrent unverified benchmarks are capped by a required configuration value
  standing in for `tier_number`, enforced in the same serialized transaction as
  the pool-wide limit and the reserve (`architecture.md` §7.6). It is required,
  not defaulted: a deployment with no value loads no orchestration policy, and
  an offer under no applicable policy is `NO_ACTION`, never unlimited work
  (`member_protocol.md` §6 final paragraph). Tests: the (N+1)th concurrent offer
  from one member is refused while another member's is admitted; concurrent
  offers from one member cannot both take the last slot; and a missing value
  fails the config load, the way slice 1's `internal_pool_unverified_limit`
  does.
- D12. **The queue allowance follows the same number.** One member may have no
  more than `max(0, limit - member_unverified)` queued or reserved offers, and a
  slot may have only one (`member_protocol.md` §6), with `limit` from D11 until
  tiers supply it.
- D13. **The collateral multiplier is a member fact the reservation fixes.**
  The member row carries `M_bps`, an integer in `1..=10_000` defaulting to
  `10_000`, set by the pool and never by the member, versioned in the same
  append-only form as the fee policy (ADR 0010, `accounting.md` §11.4).
  Admission reads it from the snapshot's member row and writes the value it
  used into the reservation, so a later change reaches no open reservation in
  either direction. Rounding is **up**, toward the pool, so a scaled reserve is
  never an atom short. Tests: the default reserves exactly the unscaled amount;
  `5_000` halves the method term and leaves `F` untouched; a change after
  admission does not move an existing reservation; and `0` or `10_001` is
  refused.

### E. The confirmed assignment

- E1. An assignment is published only after the precommit is confirmed and the
  selected track is known, and embeds the **confirmed** TIG facts rather than the
  pool's proposal (`member_protocol.md` §6 step 8, §7, §16 invariant 4). Test:
  a proposed-settings value that TIG changed does not appear in the assignment.
- E2. `assignment_digest` is SHA-256 over RFC 8785 canonical JSON of exactly
  §7's identity object, and the non-identity fields (download URLs, display
  names, estimates) are outside it (§7).
- E3. The assignment refuses to be published if `compute_type` disagrees with
  the slot's compute, the runtime platform architecture disagrees with
  `cpu_arch`, or the qualification does not cover the stated generation (§7,
  §16 invariant 5).
- E4. `workflow_expiry_block`, `proof_reserve_blocks`, `package_due_before_block`
  are derived exactly as §7 states, and an assignment that would be published at
  or after its package deadline is **not** published as executable work (§7,
  §16 invariant 6).
- E5. The permanent ownership mapping `benchmark_id -> assignment_id -> slot_id
  -> worker_id -> member_id` is written in the same transaction as the
  assignment and is immutable afterwards — the row slice 1's F6 wrote a
  placeholder into now carries a real member (§2, `architecture.md` §6).
- E6. **The bootstrap owner stops being creatable.** `migrations/0006` permits
  an `owner_kind = 'POOL_BOOTSTRAP'` workflow on testnet, which was slice 1's
  only way to own a benchmark. `mining_system.md` §10 invariant 1 confines that
  carve-out to benchmarks created *before members exist*, so once the
  offer-driven path works, admission must not be able to create one. Test: no
  code path creates a `POOL_BOOTSTRAP` workflow after slice 2's admission path
  exists. The constraint stays in the schema — existing rows keep their history,
  and the negative test keeps something to assert against.

### F. Heartbeats, events, cancellation

- F1. Events carry a strictly increasing `event_seq` from 1 and a unique
  `event_id`; a duplicate ID returns the original acceptance; a lower sequence
  is harmless only if ID and body match; a gap is rejected with the expected
  sequence (`member_protocol.md` §8).
- F2. Events are **facts**, never authority: no member event advances TIG state
  or a workflow state that §7's confirmation mapping owns (§8,
  `architecture.md` §6 "Apply member-reported assignment progress"). Test: every
  event type against a workflow whose TIG state is unchanged.
- F3. A member cancellation before precommit submission is accepted without
  trust effect; after submission it records the reason, preserves ownership, and
  leaves the terminal classification to the pool (§8).
- F4. A pool `CANCEL` directive names whether the reason is pool, TIG, security,
  or member attributed (§8).
- F5. Missing heartbeats and transient network failures are operational signals,
  never trust penalties (§8, §16 invariant 13).
- F6. **A terminal outcome is classified, and that classification decides
  nothing financial.** Slice 2 records one of `MEMBER`, `POOL`, `TIG` or
  `UNRESOLVED` with its machine reason and evidence, as `member_protocol.md` §15
  requires. Under ADR 0013 that record is operational reporting: `accounting.md`
  §11.6 charges the owning member whatever the cause, and there is no in-system
  appeal for the classification to feed. Slice 2 posts no charge at all —
  charges are steps 6–8 — so the criterion is that the classification is
  recorded and that nothing in slice 2 reads it to decide money. Test: a
  `POOL`-attributed terminal outcome and a `MEMBER`-attributed one leave
  identical financial state.

### G. Upload sessions and quarantine

- G1. An upload is admitted only when every condition in `security.md` §5.1
  holds — ownership, eligible state, before the block deadline, declared sizes
  and media type within the assignment limits and protocol ceilings, canonical
  digests, no accepted package already, and reservable quota.
- G2. Each chunk carries an exact offset, bounded `Content-Length`, and SHA-256;
  the body is streamed through the hash and never buffered at the declared
  maximum (`security.md` §5.2).
- G3. The quarantine object is durable **before** the database advances the
  committed offset, and the returned offset never runs ahead of durability
  (`architecture.md` §8.2, §6). Test: a crash between the object write and the
  transaction lets an identical retry adopt the chunk, and a conflicting retry
  fails.
- G4. Offset rules are exactly `member_protocol.md` §11's four: equal appends
  once; lower with the identical range and checksum returns the current offset;
  lower with different bytes is `CHUNK_CONFLICT`; higher is `OFFSET_MISMATCH`.
  Bytes beyond the declared size are never accepted.
- G5. Starting the same `package_id` with identical declarations returns the
  same live upload; changed declarations conflict (§11).
- G6. Object keys are generated by the pool from validated IDs; no
  member-provided name reaches a path or key (`security.md` §5.2,
  `architecture.md` §8.2, `CLAUDE.md`).
- G7. Finalization verifies the durable chunk ledger covers the declaration
  exactly, compares the whole compressed SHA-256, is idempotent, and creates
  **one** structural-ingestion job per finalized package generation — it does
  not read or unpack the package in the public API (`architecture.md` §5.2
  steps 1–2, §6).

### H. Bounded ingestion (artifact worker)

- H1. `artifact-worker` runs as a separate process identity with **no** TIG
  credential (`security.md` §5.3, `architecture.md` §2.2). Test: the
  credential-boundary gate covers the new crate.
- H2. It verifies the compressed byte count and whole SHA-256 before reporting
  `RECEIVED` (`member_protocol.md` §12, `security.md` §5.3).
- H3. Zstandard parsing permits one frame with a declared content size and
  checksum, a window no greater than 64 MiB, and no dictionary, skippable frame,
  or trailing data; decompressed bytes are counted while streaming and abort
  before the assignment's uncompressed limit (§5.3, `member_protocol.md` §10.1).
- H4. Tar parsing accepts exactly the four root regular files in order and
  rejects directories, duplicate or unknown names, absolute paths, `..`,
  backslashes, links, devices, FIFOs, sparse files, PAX/GNU extensions, and
  non-zero trailing data. No general-purpose extractor is called and no
  member-selected path is ever created (§5.3). Test: one case per rejection
  class.
- H5. Every check in `security.md` §5.4 holds: manifest size, canonical JSON,
  schema and identity digest; exact identity fields; `qualities.i32le` of length
  `4 * num_nonces`; `leaf-hashes.bin` of length `32 * num_nonces`; exactly one
  bounded NDJSON record per nonce in ascending order with no duplicates, gaps,
  unknown fields, or lossy integers; declared lengths and hashes while
  streaming; every reproduced output leaf hash; and the reconstructed Merkle
  root.
- H6. `fuel_budget`, `runtime_signature`, and `fuel_consumed` cross the boundary
  as canonical unsigned-64 decimal strings and are parsed losslessly; any other
  integer outside the IEEE-754 safe range is rejected rather than rounded
  (`member_protocol.md` §10.2).
- H7. Parser errors name the field and byte or nonce position and never copy a
  raw solution into an error or a log (`security.md` §5.4, §8).
- H8. Declared nonce counts never drive an unchecked allocation; integer
  overflow is rejected before multiplication (§5.4).
- H9. The twelve cases in `fixtures/benchmark-artifact/v1/cases` each produce
  their recorded outcome, and the golden case produces the recorded accepted
  artifact byte for byte. Test: the fixture drives the test, as slice 1's K2
  does — read at run time, not transcribed.
- H10. Ingestion, commitment construction, and (later) proof work have separate
  bounded queues and semaphores; one hostile package cannot starve the others
  (`security.md` §5.5).

### I. Publication, durable acceptance, and the receipt

- I1. Publication writes an immutable accepted object under the deterministic
  key `architecture.md` §8.2 gives, verifies size and hash after writing
  (fsync/rename for filesystem; completed multipart plus strongly consistent
  HEAD for S3), and only then commits the fenced job result.
- I2. The controller's durable-acceptance transaction records the artifact
  reference, transitions the assignment to `PACKAGE_DURABLY_ACCEPTED`, releases
  the slot, creates the immutable receipt, and creates the controller event —
  **in one transaction** (`architecture.md` §5.2 step 4, §6).
- I3. `RECEIVED` and `STRUCTURALLY_ACCEPTED` release nothing — not the slot, not
  the member's retention obligation (`member_protocol.md` §12, §16 invariant 9).
- I4. One assignment durably accepts exactly **one** package; a retry cannot
  create a second accepted artifact (§16 invariants 8 and 12). Test: concurrent
  finalizes accept exactly one.
- I5. The receipt is immutable and recoverable: a lost response returns the
  same receipt byte for byte from upload or assignment status (§3.2, §12).
- I6. The saga is crash-safe at each of `architecture.md` §8.2's four points —
  before publication, after publication and before the commit, after the commit
  and before the response, and an abandoned publication. Test: one crash test
  per point, the shape slice 1's G2 used.
- I7. **No orphan sweep runs in slice 2.** An unreferenced accepted object is
  left in place, and the criterion is the predicate only: given an object with
  no database reference, the eligibility test is false before the grace period
  and true after it (§8.2). Physical deletion stays where `architecture.md` §13
  invariant 13 and §8.4 put it — the Artifact Worker, from a controller-issued
  deletion job — and arrives with retention in step 3.

### J. The benchmark commitment

- J1. The artifact worker **builds and publishes** the canonical commitment
  payload — §6.2's `solution_quality`, exactly `precommit.details.num_nonces`
  signed entries in nonce order, and the `merkle_root` — under §8.2's derived
  key, and reports the key and digest in its fenced job result. The
  **controller** records `pool.commitment_payload` from that result, because
  `architecture.md` §6 gives the worker "build proof or commitment bytes" and
  "publish accepted artifact and derived payload" while applying a fenced result
  is the controller's, and `migrations/0015` grants INSERT on that table to
  `pool_controller` alone. That row guards §13 invariant 4; widening the
  worker's grants to reach it would move an owner, which §13 invariant 6 forbids
  doing by accident.
- J2. A `BENCHMARK` write intent can be created only after durable acceptance
  **and** a recorded commitment payload — `architecture.md` §13 invariant 4,
  which `migrations/0011` and `0015` already enforce against the stub; slice 2 supplies the real records, and the stub
  acceptance path behind the `stub-acceptance` feature is **deleted** along with
  its runtime guard and its entry in `scripts/feature-gate.sh` (slice 1's F4a,
  F4c, F4d).
- J3. The commitment is transmitted by the gateway through slice 1's unchanged
  write path, and the workflow advances only on a confirmed read
  (`tig_integration.md` §7).

### K. The member agent

- K1. `member-agent` generates its Ed25519 key locally; the private key never
  leaves the member machine and never appears in a request, log, or crash report
  (`member_protocol.md` §3.1; `security.md` §4.1, "a worker private key is
  generated and retained only on the member machine").
- K2. It recomputes `assignment_digest` and refuses to start if any identity
  field is inconsistent, and verifies every downloaded binary and image against
  its digest before use (`member_protocol.md` §7). Test: each of the three
  refusals.
- K3. It executes every nonce in `[0, num_nonces)` exactly once using the
  confirmed assignment, and builds a package that satisfies §10 byte for byte —
  the same archive the pool's H-group checks accept.
- K4. Its Zstandard frame declares content size, sets the frame checksum, and
  uses a window no greater than 64 MiB (§10.1).
- K5. It retries on network failure, `408`, `429`, and `5xx` with full-jitter
  backoff from 1 second capped at 30, honours `Retry-After`, and does **not**
  blindly retry the other `4xx` classes §13 lists.
- K6. It persists the receipt before deleting local proof material, and recovers
  a lost receipt from status rather than re-uploading (§9, §12).
- K7. It refuses to reconfigure a slot while an offer, assignment, or upload is
  open, and resumes an interrupted upload from the server's committed offset
  (§2, §11).

### L. Schema, roles, and migrations

- L1. Every new table follows slice 1's rules: forward-only migrations applied
  by `pool-admin migrate`, tested against an empty database and against the
  preceding schema (`architecture.md` §7.1).
- L2. `pool_api` and `pool_artifact_worker` exist already. They are provisioned
  by `scripts/provision-db-roles.sql`, which a superuser runs before
  `pool-admin migrate` — a migration connects as `pool_migration` and so cannot
  create the role it runs as. `migrations/0001` grants them `USAGE` on the
  schema and nothing else, because "object-level grants arrive with the objects
  themselves, one slice at a time", so no slice-2 migration contains
  `CREATE ROLE`. Slice 2 is
  that slice for the member-facing tables: `pool_api` may write member rows and
  quarantine ranges and may **not** create intents, change workflows, or post
  ledger rows; `pool_artifact_worker` may write artifact and job rows and may not
  change workflows (`architecture.md` §6). Each table's migration also makes its
  own `pool_readonly` decision explicitly, as `0001` requires. Test: the negative
  grants, under the real roles, the way slice 1's D4 tests do.
- L3. The artifact row carries exactly the fields `architecture.md` §8.3 lists
  that implemented behaviour requires, and an S3 ETag is never used as the
  integrity check.
- L4. No transaction stays open across member network I/O or artifact-store I/O
  (`architecture.md` §7.2). Slice 1 waived the *measurement* of this for the TIG
  path (issue #50); slice 2 carries the same structural rule and the same
  waiver, and both are revisited in step 5.

### M. Evidence required to call the slice done

- M1. `make check` passes from a clean checkout, including the new gates.
- M2. The full ladder — enrol, qualify, offer, assignment, compute, package,
  upload, durable acceptance, commitment — runs deterministically against
  `fake-tig` in CI with no network, driven by the `member-agent` binary rather
  than by a test harness that stands in for it.
- M3. The three lifecycle cases slice 1 deferred —
  `duplicate_capacity_offer_request`, `duplicate_durable_acceptance_receipt`,
  `member_package_timeout_failed` — and the fourth found at slice 1's close,
  `fraud_confirmed_after_proof`, are driven from
  `fixtures/queue-lifecycle/v1/lifecycle.json`. Any still out of scope must be
  named with its reason, as slice 1's record does.
- M4. **One live testnet run** with a real member agent on a real machine
  reaches a TIG-confirmed benchmark commitment, with block heights, the
  assignment digest, the package digest, the receipt, and the intent and attempt
  rows recorded in `docs/evidence/`.
- M5. At least one crash test from I6 is repeated against live testnet on the
  upload path, with the durable-acceptance transaction interrupted, and shows
  exactly one accepted artifact.
- M6. Every assertion in `crates/spike/tests/member_package.rs`,
  `pool_upload.rs`, and `pool_upload_authed.rs` is ported or explicitly
  re-homed with a reason, in the disposition table §7 will carry.

## 5. Sequencing

Each is one PR unless it grows past what one review can hold.

1. **Migrations and the member domain rows** — workers, credentials, slots,
   qualifications, offers, assignments, events, uploads, chunk ledger, receipts,
   artifacts; the `pool_api` and `artifact_worker` roles (L1, L2, L3).
2. **`pool-api` skeleton and request authentication** — A1–A5, A8; the
   `pool-identity` verification path moved behind PostgreSQL.
3. **The member account, enrollment, rotation, revocation** — B1–B9. The wallet
   signature (B8) comes first in this PR, because it is what authorizes a ticket
   to exist at all.
4. **Idempotency and abuse controls** — A6, A7.
5. **Slots and qualification** — C1–C8.
6. **Offers and admission** — D1–D4, D9, D10, D11, D13 (the serialized
   transaction slice 1 built, entered from a real offer and carrying the member
   bound and the multiplier).
7. **The queue** — D5–D8, D12.
8. **Assignment publication** — E1–E6.
9. **Heartbeats, events, cancellation** — F1–F6.
10. **Upload sessions and quarantine** — G1–G7.
11. **`artifact-worker`: bounded parsing and structural acceptance** — H1–H10.
12. **Publication, durable acceptance, receipt, slot release** — I1–I7.
13. **Commitment payload and the `BENCHMARK` intent; delete the stub** — J1–J3.
14. **`member-agent`** — K1–K7.
15. **Live run and the closing PR** — M1–M6.

Steps 1–4 are the foundation everything else needs. After step 5, steps 6–9 and
10–12 are independent enough to run in parallel if two agents work the slice.

## 6. Risks and carried questions

- **Qualification needs real runtime images.** C5's task runs `tig-runtime` and
  `tig-verifier` on the member machine. CI cannot pull and run those for every
  test, so the CI ladder uses the pinned fixture execution and the *live* run is
  what exercises the real images. The risk is that a qualification bug is only
  visible live; M4's run is the mitigation.
- **Package sizes.** The protocol ceiling is 64 GiB compressed. CI runs small
  nonce counts, so the streaming bounds in H3 and H8 are exercised by
  constructed cases rather than by size. A case per bound, not a big file.
- **The stub is load-bearing until step 13.** Slice 1's tests use it to assert
  that a benchmark intent cannot exist without acceptance. Deleting it before
  the real record exists would remove a guard; J2 sequences the deletion after
  the replacement.
- **The spike crate holds a TIG key read**, which `scripts/credential-boundary.sh`
  excludes by name. Slice 2 is the slice that ports its member and upload
  prototypes (§7), so it is also the slice that can delete `crates/spike` and
  retire that exclusion. Whether the spike's *gateway* and *active* prototypes
  are also clear to delete is decided in the disposition table, not assumed.
- **Two open questions from the spike report** that slice 2 touches: issue #28
  (`fixtures/tig/v1/get-algorithms.json` does not match what TIG serves) affects
  the algorithm the agent downloads, and issue #54 (GPU overhead never measured)
  stays open by §2's scope.

  Numbers in this plan are **this repository's**. `pre_build_checklist.md` and
  the spike report carry numbers from the repository they were written in — the
  note in `plans/slice-1-gateway.md` §9 says which is which — so the GPU gap that
  checklist calls "issue #35" is issue #54 here.
- **The funding model moved under this plan.** ADRs 0009–0014 landed after it
  was written: the member account became a wallet (0011), the method reserve
  gained a per-member multiplier (0010), the separate failure-charge term was
  removed (0013), and settlement, shortfall and withdrawal-cap rules changed
  around them. The criteria above were rewritten against the current
  `accounting.md` §11.4 and §11.6 rather than the versions this plan was drafted
  from. The carried risk is the reverse: a slice-2 PR written from memory of the
  old model would reintroduce `X` or an appeal path. Issue #61 removes the
  shipped `X` term, and D10 says explicitly that slice 2 must not reintroduce
  it.
- **`proof_reserve_blocks = 10` is a spike constant** (`member_protocol.md` §7).
  Slice 2 measures real package upload and acceptance timings on the live run;
  replacing the constant with a reviewed value is step 3's or step 5's, and this
  slice records the measurement rather than changing the number.

## 7. Spike code disposition

`crates/spike/tests/member_package.rs`, `pool_upload.rs` and
`pool_upload_authed.rs` hold 30 assertions that already describe this slice's
behaviour. They are ports, not inspiration: the slice-2 test for each must
assert the same property against the production crates. The disposition table —
one row per spike test, naming the slice-2 test that replaces it or the step it
re-homes to with a reason — is written in the PR that closes the slice, as
slice 1's K5 table was, and M6 is where it is checked.

`crates/spike` itself is deleted only when every row of that table is closed and
no non-spike code depends on it. That deletion retires the
`scripts/credential-boundary.sh` exclusion, which is the point of doing it here
rather than later.

## 8. Definition of done

- Every criterion in §4 has a passing test or recorded evidence, or an explicit
  written waiver recorded the way slice 1's were — in the repository, naming the
  decision, with the human decision for anything that weakens a test.
- Design-doc diffs required by slice-2 findings are merged in the PR that found
  them (`CLAUDE.md` mandatory workflow §6).
- `pre_build_checklist.md` §10 step 2 is recorded as complete and this plan's
  status is flipped to `implemented`.
- `docs/evidence/slice-2-criteria.md` records where each criterion is satisfied,
  as `slice-1-criteria.md` does.
