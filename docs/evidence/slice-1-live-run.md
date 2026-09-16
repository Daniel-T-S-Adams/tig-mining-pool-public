# Slice 1 live run: criteria K3 and K4

Evidence for the two criteria that can only be met against live TIG. Gathered
by `scripts/live-run-evidence.sh` and `scripts/live-crash-test.sh` from the
runs described below, on **testnet**, driven by the production binaries built
from `main`.

Recorded here rather than only in a pull request body because K3 asks for the
run's block heights and intent/attempt rows to be recorded, and a PR body is
not in the repository. The scripts reproduce it; this is what they produced.

- Endpoint: `https://testnet-api.tig.foundation`
- Player: `0x2935a721068da756b28cba896efdb64e8909dfae`
- Date: 2026-09-16

## K3 — a confirmed precommit and a created assignment

One decision, one write, one confirmation.

```
decision   anchor 1331629  block 23ed5328c5b9ee2d4c003998504ce24c
           challenge c001  algorithm c001_a004  compute_type aws_t4g
           precommit_reserve 40001000000000000000

intent     CONFIRMED   trace bf2dd1fd086ed7dae192768946a85d62
attempt    no 1  ACCEPTED  HTTP 200   05:03:32.569 -> 05:03:32.719

workflow   PRECOMMIT_CONFIRMED
           benchmark_id 5b0f5335b473d3c12386997c8c356189
           owner POOL_BOOTSTRAP / pool-bootstrap
           unverified_from_block 1331629, interval open
           confirmed_track_id "n_vars=400,ratio=3000"   (TIG's, not the pool's)
           confirmed_num_nonces 40
           precommit_confirmed_block 1331631
```

**The assignment** is criterion F6's owner mapping: a permanent
`benchmark_id -> owner` record created with the workflow and bound to the
benchmark TIG confirmed. Slice 1 has no members, so the owner is the
pool-owned placeholder F6 requires, and F6a's negative test asserts no
slice-1 workflow is attributable to a member.

**The reserve** is `accounting.md` §11.4 computed from live values —
`P[s] * B[t] + F[s,t] + X` — where `P[s]` is testnet's own
`config.reports.penalty_amount` of 10^19 and `B[t]` is 4 bundles:

```
method reserve   50 * 4 ... 40000000000000000000
TIG fee                        1000000000000000    (base_fee; per_nonce_fee is 0 on c001)
failure charge X                              0    (unchosen: pre_build_checklist.md §5.2)
assignment reserve           40001000000000000000
```

The pool then refused further work at `internal_pool_unverified_limit = 1`,
which is criterion D2a holding live.

### Operational facts the run established

- The live active-benchmark set was **468 benchmarks**. At the configured
  budget of 10 per poll that is a **~12 minute cold start** before any
  decision can be made, because §9's snapshot is unusable for a decision
  until the cache covers the block's active set (criterion C5).
- The cache survives a restart: a second run resumed with 73 missing rather
  than 468.
- TIG answered the write in **150 ms**, and confirmed it **two blocks** later.

## K4 — G2's crash point, live

G2's second crash point: the attempt row written, the process killed before
the HTTP response was recorded.

```
crash point   attempt_no 1  outcome NULL  http_status NULL  intent PREPARED
tuple         block 6ecef51e728ba91947e74e19e81da9cf
              challenge c002  algorithm c002_a004
TIG holds     0 precommits for that tuple
recovery      stopped for an operator; no resend, no duplicate
```

G2 requires "exactly one where a write reached TIG, and exactly zero where it
did not ... one recovers by finding the write and the other by refusing to
guess". This run is the second branch: the kill landed before the request
left, TIG holds nothing, and the pool refused to guess rather than resending
into a possible second fee.

The `§10 tuple scan` over the whole window shows **one** confirmed precommit —
K3's — and none for this tuple. A duplicate there would fail the criterion;
`scripts/live-run-evidence.sh --selftest` proves the scan detects one.

## What these runs also found

Three defects, each fixed in its own change:

- the gateway raised a stop-for-operator on **every healthy write** between
  acceptance and confirmation, six times in ninety seconds on the pool's first
  live precommit (#41);
- the block that completes the cache warm-up was never decided from, so every
  restart with a non-empty active set skipped one (#37);
- a workflow recorded as `PRECOMMIT_SUBMITTED` is never bound, because the
  binding selects `DECIDED` only (issue #44, latent — nothing reaches that
  state today).

None was reachable by a test against `fake-tig`: the first exists only in the
seconds between a real acceptance and a real confirmation.
