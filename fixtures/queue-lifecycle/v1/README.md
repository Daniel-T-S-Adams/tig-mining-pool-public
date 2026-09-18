# Queue and lifecycle fixture v1

Provenance: **constructed** (not captured). Every case was derived by hand from
the design documents — `docs/mining_system.md` (§4, §6, §8, §10),
`docs/architecture.md` (§5.1, §5.2, §5.3, §7.3, §7.5, §7.6, §12),
`docs/member_protocol.md` (§5, §6, §8, §9), `docs/tig_integration.md`
(§7, §8, §10), and `config/tig_integration.json`
(`spike.workflow_guardrails`) — independently of any implementation, per
`docs/pre_build_checklist.md` §6. Every expected transition carries the exact
rule citation it was derived from. This directory follows the layout
convention of `fixtures/tig/v1/README.md`: versioned, immutable once merged
(corrections create a `v2`), no credentials anywhere.

All identifiers, timestamps, block heights, and numeric policy values are
internally consistent inventions. Block heights sit near the
`fixtures/tig/v1` anchor (height 100080, round 834) for cross-fixture
coherence only. **No value here is a live protocol or policy constant.**

## Files

| File | Family | Cases |
|---|---|---|
| `availability-queue.json` | Compute-availability queue admission and promotion | 12 |
| `lifecycle.json` | Benchmark workflow lifecycle | 9 |

## Case format

Each case is `{name, description, rule, initial_state, events,
expected_transitions, final_state}`. `events` and `expected_transitions` are
ordered. Each transition is `{subject, from, to, rule}`; a transition whose
`from` equals its `to` is a deliberate **no-op assertion** (the rule requires
that nothing changes — e.g. idempotent replay, an entry not selected, a
precommit intent surviving tier removal).

## Coverage map

Availability queue (`availability-queue.json`):

1. `direct_pending_empty_queue` — baseline PENDING path, no queue entry.
2. `queued_when_global_limit_full` — QUEUED with lease; no TIG write, no
   collateral.
3. `fifo_tie_broken_by_offer_id` — equal `queue_accepted_at`, smallest
   `(queue_accepted_at, offer_id)` wins.
4. `multi_member_multi_slot_promotion_order` — two positions, three entries,
   two members; strict FIFO across members; per-member cap respected.
5. `member_queue_cap_rejects_extra_offer` — `max(0, tier - unverified)` cap.
6. `stale_lease_expired_entry_skipped` — lapsed lease removed without penalty.
7. `worker_cancelled_queued_offer` — cancellation leaves the queue, no penalty.
8. `ready_check_expiry_advances_queue` — unanswered `CONFIRM_AVAILABLE`.
9. `ready_check_replay_stale_id_ignored` — stale id echo, fresh id promotion,
   duplicate heartbeat replay creating no second precommit intent.
10. `tier_removal_cancels_queued_and_ready_check` — one transaction; existing
    precommit intent untouched.
11. `new_capacity_does_not_bypass_live_queue` — newly opened position consumed
    only via queue promotion.
12. `atomic_recount_limit_failure_at_promotion` — fresh in-transaction recount
    fails the global gate after a confirmed ready check.

Lifecycle (`lifecycle.json`):

1. `duplicate_capacity_offer_request` — idempotent replay + body-mismatch 409.
2. `duplicate_durable_acceptance_receipt` — lost response, same receipt, slot
   released exactly once.
3. `delayed_precommit_confirmation` — `OUTCOME_UNKNOWN`, lane reconciliation,
   late confirmation, confirmed settings replace proposed.
4. `member_package_timeout_failed` — deadline `block_started + 110`,
   MEMBER-attributed `FAILED`, one chargeable failure charge.
5. `stopped_benchmark_no_proof` — `details.stopped = true`, no proof,
   chargeable capacity failure but not fraud.
6. `expired_workflow_pool_side_stall` — age ≥ 120 expiry after durable
   acceptance; not a member failure.
7. `active_full_happy_path` — all seventeen states to `ACTIVE`, slot released
   at durable acceptance.
8. `fraud_confirmed_after_proof` — confirmed `frauds` entry, method-penalty
   charge.
9. `restart_recovery_reconciles_three_workflows` — monotonic reconciliation of
   an unknown precommit, a confirmed-and-active proof, and a pending
   benchmark commitment gated on the artifact availability check.

## Derivation decisions

1. **Unverified counting.** `pool_unverified` and `member_unverified` count
   from creation of the pool precommit intent until TIG records the benchmark
   verified or terminal (`mining_system.md` §6.1). Queue promotions in these
   fixtures therefore increment the count at `PRECOMMIT_INTENT_CREATED`, and
   terminal/verified events decrement it.
2. **State vocabulary.** Offers use the member-visible names of
   `member_protocol.md` §9 (`OFFER_QUEUED` → `OFFER_READY_CHECK` →
   `OFFER_PENDING`); `architecture.md` §5.1 uses the equivalent server-side
   `QUEUED`/`PENDING`. Workflow states are the `mining_system.md` §4.5 machine.
3. **Timing values.** The 90-second offer lease and 30-second heartbeat are
   the documented spike defaults (`member_protocol.md` §6). The 60/110/120
   block guardrails come from `tig_integration.md` §8 and
   `config/tig_integration.json` `spike.workflow_guardrails`; all are spike
   safeguards, not permanent constants.
4. **Tie-break ordering.** "Smallest `(queue_accepted_at, offer_id)`"
   (`architecture.md` §5.1 step 3) is modeled as ascending lexicographic order
   of `offer_id` after equal timestamps; fixture ids are zero-padded so
   lexicographic and numeric order agree.
5. **Policy numbers.** `internal_pool_unverified_limit`, tier numbers and
   `J[k]` are versioned policy whose numeric values are intentionally open
   (`mining_system.md` §11, "Required before full product implementation").
   The per-failure charge `X` was one of them until ADR 0013 derived it from
   the benchmark's own fee and penalty; these cases predate that.
   Fixture values carry `policy_version: "fixture-v1"` and bind nothing.
6. **Slot release on offer termination.** When a queued offer expires or is
   cancelled before any assignment exists, the fixtures return the slot to
   `AVAILABLE`, reading the `member_protocol.md` §9 slot chain as ending with
   the offer that reserved it.

## Open questions (recorded, not invented)

Checked against `mining_system.md` §11: none of these is a decision that
document intentionally leaves open *and* answers elsewhere; they are genuine
gaps or readings that a reviewer should confirm before the implementation
treats these fixtures as normative.

1. **Numeric policy values** — `J[k]` and
   `internal_pool_unverified_limit`/recovery headroom are explicitly open
   (`mining_system.md` §11). The fixture numbers are placeholders only.
   `X` is no longer among them: ADR 0013 abolished it as a policy value, so a
   case asserting a flat per-failure charge tests a rule that no longer
   exists. `v1` is immutable; the correction goes in a `v2` (issue #56).
2. **Ready-check expiry window** — no document assigns the ready check its own
   numeric deadline; `member_protocol.md` §6 says only that expiry of "the
   ready check or ordinary offer lease" removes the offer. The fixtures bound
   it by the 90-second offer lease. Confirm the real window.
3. **Queue position after a failed global recount** — `architecture.md` §7.6
   forbids creating the intent, and the documented removal reasons
   (expiry, cancellation, member-side ineligibility — `mining_system.md`
   §4.1, `member_protocol.md` §6) do not cover a pool-side capacity recount
   failure, so `atomic_recount_limit_failure_at_promotion` keeps the entry
   live with its immutable FIFO key. The docs never state this outcome
   explicitly. Confirm retention (and whether the entry returns to
   `OFFER_QUEUED` or stays in a ready-confirmed state awaiting the next
   position).
4. **Response for a per-member queue-cap violation** —
   `member_protocol.md` §6 defines the cap and the general
   `NO_ACTION`/`REJECTED` no-queue rule but does not name the exact response
   for this specific failure; the fixture records `REJECTED` with reason
   `member_queue_cap_exceeded` as a placeholder.
5. **Disposition of a stale `ready_check_id` echo** — only the fresh id
   permits promotion (`member_protocol.md` §6); whether a stale echo is
   silently ignored or produces an explicit error/directive in the heartbeat
   response is unspecified. The fixture models "ignored, offer remains in
   ready-check state".
6. **Expiry attribution edge** — `expired_workflow_pool_side_stall` assumes a
   workflow that reached durable acceptance and then stalled on the pool/TIG
   side expires without a chargeable member failure
   (`mining_system.md` §8 non-member outcomes). A workflow that expires while
   *assigned but undelivered* is instead the member-timeout case. Whether any
   third expiry flavor (e.g. never assigned because the member vanished before
   assignment) is chargeable is left to `member_attack_model.md` attribution
   and is not fixed by these fixtures.

## Where the implementation knowingly diverges

These are not open questions about the protocol. They are places where a
shipped slice cannot reach the fixture's expected outcome, recorded here so
the fixture stays load-bearing instead of being quietly worked around.

1. **`restart_recovery_reconciles_three_workflows`, wf_2008 reaching
   `ACTIVE`** — the case expects `PROOF_SUBMITTED -> PROOF_CONFIRMED ->
   ACTIVE`, the last step taken from `block.data.active_ids.benchmark`
   (`tig_integration.md` §7: "This set is authoritative"). Slice 1 reaches
   `PROOF_CONFIRMED` and stops.

   Two reasons, and the second is the load-bearing one. `mining_system.md`
   §4.5's ladder runs `PROOF_CONFIRMED -> VERIFYING -> ACTIVE`, and slice 1's
   twelve workflow states end at `VERIFIED` — there is no `ACTIVE` row to
   write. More importantly, the nearest state slice 1 *could* write is wrong:
   `confirm_verified` closes `mining_system.md` §6.1's unverified interval
   **at a block**, `confirmed_ids.verified` is per-block, and a verification
   that happened while the controller was down is not recoverable from any
   later read. Closing the interval at the current block instead would date it
   after the fact for every block in between, which is exactly what §7.6's
   per-block recount reads.

   The restart pass therefore reports the case as
   `NeedsAttention::VerificationMissed` rather than advancing it, and
   `crates/pool-workflow/tests/restart.rs` asserts that outcome against this
   fixture's shape. The consequence is that such a workflow keeps §6.1's
   interval open, holding a slot against `internal_pool_unverified_limit`,
   until an operator resolves it. Closing it properly needs either an `ACTIVE`
   state or a way to recover the verification block, and neither is in slice
   1's scope.
