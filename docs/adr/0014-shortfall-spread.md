# ADR 0014: An uncovered charge is spread across the round's earners

Status: accepted  
Date: 2026-09-19

## Context

ADR 0010 gave the pool a per-member collateral multiplier, and ADR 0013 made a
member owe the evidenced charge in full rather than the multiplier-scaled part.
Below `10_000` bps those two produce a gap by construction: the member owes
`P * min(R, B) + F` and holds `ceil(P * B * M_bps / 10_000) + F`.

`accounting.md` §11.2 and §11.7 bound the posted debit to encumbered balance,
so the charge stops at the reservation whatever the evidenced amount. The
remainder had no route at all, and three rules stood against the obvious one:

- §10 rule 7 — "never charge other members or a future block silently for the
  shortfall";
- §11.2 — the balance "cannot pay another member or silently cover a pool
  error"; and
- ADR 0010 itself, which recorded that the uncovered portion "cannot be
  recovered from the member".

A price rise produces the same gap by a different route. `tig_integration.md`
§14.1 determines that `penalty_amount` is read live at the charge block and can
rise after a reservation was taken, and §11.5 records the owner choosing to
carry that rather than buffer against it.

## Decision

**What a reservation does not cover is deducted from the round's earnings
before they are credited.**

`accounting.md` §8.4 is where it happens, and the placement is the decision as
much as the rule is:

```text
S[round]  = sum of uncovered remainders on benchmarks whose
            own round is this one
E[m]      = member m's MEMBER_EARNED_PENDING for this round
E[total]  = sum(E[m])

spread    = min(S, E[total])
share[m]  = spread * E[m] / E[total], by §6's largest-remainder method
```

The cap is part of the rule. An uncapped `S * E[m] / E[total]` exceeds every
member's own earnings whenever `S` is larger than the round's, and divides by
zero on a round that earned nothing — both of which this decision expects,
since a pool-wide incident produces exactly them.

Debited from each member's pending account and credited to the accounts
§11.6's charge credits — penalty portion to the loss reserve, fee portion to
failure charges. What remains settles into balances.

**The deduction is its own §8.6 cause, one per round, keyed
`(network, round)`.** It cannot borrow the originating charge's identifier:
that one was consumed when the charge itself was swept, and §13 item 13
permits one sweep per cause, so the deducted tokens would be stranded in
member custody. `S` also aggregates several charges in a bad round, and one
batch may carry only one identifier (§11.2). One round, one deduction, one
sweep.

**A benchmark's remainder belongs to its own round**, not to whichever round
its qualifiers were attributed in. A benchmark can earn across a boundary but
its charge is one amount, and the benchmark's own round is how everything else
about it is keyed — §14.2's report selection and §11.6's per-benchmark charge.

**This refines ADR 0012's settlement condition**, which waited only on
benchmarks whose qualifiers were attributed in the round. That leaves a
benchmark created in `R` which earned only after the boundary, or earned
nothing, blocking nothing — so `R` could settle before its charge existed, and
the remainder would have no lawful home, with §9 forbidding a reopen and this
decision forbidding a carry-forward. §8.4 now also waits on every benchmark
whose own round is `R`. ADR 0012 stays as written.

Where `S` exceeds `E[total]` the round's earnings are exhausted and the
remainder falls on the pool — §5's fee revenue for that round first, then
operating funds. **It is never carried into a later round**, which is the half
of §10 rule 7 that survives untouched.

**Why settlement and not the block batch.** A shortfall is not knowable until
the round's arbitrations resolve, long after the per-block allocations of §6
are posted. Deducting at block level would mean reopening a posted batch,
which §9 forbids outright. Deducting at settlement means the money is still in
a pending liability when the figure is finally known — which is only true
because ADR 0012 made the credit wait for the round's own slashing. That ADR
was taken for a different reason; this decision depends on it.

One consequence is a guarantee: no posted batch is reopened, so §13 items 2
and 4 remain exactly true of every block — fee plus member allocations still
equals proceeds there.

The second is not, and saying so is the point. The charged member's earnings
sit in `E[m]` on the same terms as everyone else's, so where they earned in
the round they bear a share of what their own benchmark left uncovered, on top
of losing the reservation. **Where they did not earn in that round they bear
none of it.** A shortfall requires the penalty term, so it comes from a
benchmark that went active and was reported — and that benchmark's owner can
still have no attributed qualifiers in its own round, because its bundles did
not qualify or qualified after the boundary. `E[m]` is zero and the whole
remainder falls on the members who earned alongside them.

That is the worst case this decision has, and it is not a rare one: the
straddle it describes is the same case the settlement condition above had to
be extended for. It follows from spreading pro-rata by earnings and is not
separately fixable without changing that rule — taking the charged member's
share from a round they *did* earn in would mean a charge reaching across
rounds, which §10 rule 7 forbids for good reason.

Worked example, the owner's own: after slashing, Ben is owed 100 TIG and Jim
10, with 11 TIG uncovered. `E[total]` is 110, so Ben bears 10 and Jim 1,
leaving 90 and 9. Both are reduced by the same tenth.

## What this costs

**A member's earnings are reduced by another member's failure, including one
neither of them caused.** ADR 0013 made a charge independent of fault, so a
benchmark that failed because the pool corrupted it is charged to its owner —
and now, where that owner's collateral falls short, to everyone who earned
alongside them. Nothing in an innocent member's own conduct bounds their
exposure to this.

What bounds it is **the pool's multiplier policy**, which is the pool's choice
and not the members'. A member at `10_000` bps leaves no remainder to spread;
every basis point below that is a slice of risk moved from one member onto the
rest, at a leverage of `(10_000 - M_bps) / M_bps`.

That leverage is granted to exactly the members the pool trusts most, since
they are the only ones given a discount. It is the uncomfortable shape of this
decision and is recorded rather than smoothed: a discount is an expression of
confidence, and here it is also the thing that lets a member impose a loss on
others. §13 item 21 exists so the aggregate is visible before a charge lands
rather than reconstructed after one, and it should be read as a measure of how
much the membership is currently underwriting on the pool's judgement.

**The member terms must say so plainly.** "Your earnings can be reduced by
another member's failed benchmark" is not something a member would assume, and
`pre_build_checklist.md` §9's member-terms gate is where it has to appear. A
member who learns it from a reduced payout has been misled by omission.

**A pool-wide incident concentrates rather than diversifies.** One
proof-construction defect fails many benchmarks in the same round, so the
round with the largest `S` is also the round whose earnings are smallest. The
spread is bounded by `E[total]`, so a bad enough incident simply exhausts the
round and lands on the pool — which is the correct outcome and worth knowing
is the design's behaviour rather than a surprise.

## Consequences

- `accounting.md` §8.4 gains the deduction, its formula, its exactness rule
  and its bound; §13 gains item 22 for the same; §11.5 and §11.6 stop saying
  the excess has no route.
- §10 rule 7 keeps its prohibition and gains the five properties that
  distinguish the permitted case: bounded by the round, computed by a stated
  formula, posted under its own stated cause identifier, visible to every
  member it touches, never carried forward. A shortfall lacking any of them is
  still forbidden.
- §8.6's cause table, §13 item 13's enumeration and `architecture.md` §6's
  operating-sweep row each gain the deduction as a cause keyed
  `(network, round)`.
- §11.2's "cannot pay another member or silently cover a pool error" is
  narrowed to what it was always for: an accounting *error* is never
  mutualised at all, going to `EXPENSE:ACCOUNTING_LOSS` or a member
  receivable.
- `mining_system.md` §6.1 stops calling the sub-`10_000`-bps difference pool
  risk and names it as what the membership underwrites.
- **This supersedes ADR 0010's clause** that the uncovered portion "cannot be
  recovered from the member" — ADR 0013 had already replaced it on the
  member's side, and this ADR states where the money actually comes from.
  ADR 0010 stays as written.

## Alternatives rejected

**The pool absorbs it.** The status quo by omission, and what §13 item 21 read
as before this decision. Rejected by the owner: the loss arises from a member's
benchmark and the pool's own funds are not the first place to look for it.

**Recover it from the charged member's other balance.** Reach past the
reservation into their deposits. The owner considered and declined this: it is
cleaner for the round to settle as one netting step than for a charge to walk a
waterfall through balances, pending withdrawals and reservations.

The cost of declining it is larger than "no profit in it", which is how an
earlier draft of this ADR dismissed it. A member holding a large unreserved
balance can leave a shortfall for others while keeping that balance, and the
leverage is `(10_000 - M_bps) / M_bps`: they forfeit their reservation to
impose that multiple on innocent earners — 1:1 at `5_000` bps, 9:1 at
`1_000`, 99:1 at `100`. It is not *profitable*, since failed work earns
nothing, so the motive is griefing or competitive damage rather than gain. But
"unprofitable" is not "bounded", and the bound here is only the multiplier the
pool chose.

`security.md` §2.1 already assumed a member would "try to cause another member
to be charged with its failure". This decision is what makes that possible,
and both threat models record it — §2.3's table and
`member_attack_model.md` M23, which also notes it amplifies M18's coordinated
accounts.

**Carry the remainder into the next round.** Rejected as the thing §10 rule 7
exists to forbid: a shortfall that outlives its round stops being attributable
and starts being a standing tax.

## Revisit when

- Aggregate uncovered exposure (§13 item 21) grows past what the membership
  would accept if it were told the number, which is the honest test of a
  multiplier policy.
- A single incident exhausts a round's earnings, which is the first real test
  of the pool-absorbs-the-remainder rule.
- Members begin choosing pools on this basis, at which point the multiplier
  stops being an internal capital decision and becomes a competitive one.
