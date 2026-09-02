# ADR 0006: Allocating the TIG read budget across reader processes

Status: accepted for v0  
Date: 2026-08-31

## Context

`tig_integration.md` §11 pins a conservative client read budget: 2 GET
requests per second with a burst of 2, and at most 2 concurrent GETs. TIG
publishes no numerical quota and returned no remaining-limit headers during
the review, so the number is the pool's own caution rather than a measured
ceiling. Critically, TIG documents its rate limiting as **per IP**.

The pool does not read from one process. `architecture.md` §4 and §11.2 run
the controller and the TIG gateway as separate processes, deliberately —
their trust and resource boundaries differ — and §11.2's smallest deployed
testnet puts them on one host behind one egress address. Both read TIG: the
controller for block-consistent snapshots (§9), the active-benchmark
metadata cache (§5.2) and lifecycle reconciliation (§10); the gateway for
the §13 check 5 compatibility reads and the reconciliation §10 requires
before any write retry.

A limiter scoped to a process therefore does not bound what TIG sees. Two
processes each holding the full pinned allowance present 4 requests per
second and 4 concurrent GETs against a per-IP limit set at 2 — exceeding the
pinned budget by construction, not by accident.

The consequence is not merely a throttled request. §11's `get-block` poll
runs every 15 seconds, and §10 records that when the newest accepted height
is more than one above the last local height, every missing height becomes a
data gap whose per-block qualifier attribution **cannot be reconstructed** —
`get-round-emissions` can audit round totals but cannot recover per-block
member weights. Sustained throttling of the poll is therefore not a
performance problem; it is permanent, unrecoverable accounting loss.

An earlier version of the read client made the limiter per-process and
recorded that in §11 as though it were the pinned rule. That reinterpreted a
budget whose whole purpose is to stay under a per-IP ceiling into one that
scales with however many processes happen to read.

## Decision

The pinned 2 req/s, burst 2, 2 concurrent budget is a **pool-wide per-IP
ceiling**, not a per-process allowance. Each reading process is configured
with an explicit share, and the shares must sum to no more than the ceiling.

For v0 the split is even: the controller and the gateway each take 1 req/s,
burst 1, 1 concurrent. Equal rather than weighted because no measurement yet
justifies a weighting — the spike measured TIG call counts but not the
steady-state split between the two readers (`protocol_spike_report.md` §5.6).

Consequences of that choice, stated rather than left to be discovered:

- `ReadLimits` has **no `Default`**. A reader must name itself, because a
  reader that did not would take the whole allowance and reintroduce exactly
  the doubling this ADR exists to prevent.
- `ReadLimits::shares_within_ceiling` exists so the sum is asserted by a
  test rather than maintained by attention.
- The `get-block` poll carries a whole-call deadline no larger than its own
  15-second interval, so a stalled poll fails and is retried rather than
  silently skipping the next one.

## Consequences

- A third reading process cannot simply be added: it needs a share, which
  means reducing someone else's. That friction is intended — it forces the
  allocation to be a decision.
- The even split may prove wrong. The controller polls continuously while
  the gateway reads in bursts around writes, so the gateway may be starved
  at 1 req/s during a reconciliation burst while the controller's share sits
  idle. Revisit when there is measurement, not before.
- Sharing a budget across *processes* is not solved here, only divided. A
  cross-process limiter (a token table or advisory-lock lease in PostgreSQL)
  would allow dynamic borrowing, and is deliberately not built in v0: it adds
  a database round-trip to every TIG read to solve a problem a static split
  already bounds.
- If TIG ever publishes a real quota or remaining-limit headers, this whole
  allocation should be replaced by respecting what the server reports.

## Revisit when

- TIG publishes a numerical quota or returns remaining-limit headers.
- Measurement shows one reader starved while another's share idles.
- A third TIG-reading process is proposed.
