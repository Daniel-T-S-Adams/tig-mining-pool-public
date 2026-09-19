# ADR 0013: A charge does not depend on fault, and its amount is derived

Status: accepted  
Date: 2026-09-18

## Context

`accounting.md` §11.6 previously decided *whether* a member was charged by
attributing a cause. A method penalty was slashed only where it was "caused by
member-produced data"; `POOL`, `TIG` and `UNRESOLVED` attribution each slashed
zero; and `mining_system.md` §8 said in terms that pool, TIG, compatibility and
unresolved incidents were "neither tier failures nor chargeable member
failures".

Around that sat an apparatus for deciding the question: a proposed charge
froze the amount and notified the member, the member had a seven-day appeal,
an undisputed proposal became final on that deadline, and a disputed one went
to a reviewer who had not made the original decision.

Two things made it untenable.

**The notice had no delivery channel.** ADR 0011 made the member's wallet the
account, so the pool holds no email, phone or address. A seven-day forfeiture
deadline in front of a notice the member cannot receive is not an appeal; it
is a delay before taking their money. Review caught this while the wallet
decision was being recorded.

**The amount was unknowable.** `X[policy]`, the per-failure charge, was one of
the numbers `mining_system.md` §11 still listed as unset — and it was a term
of §11.4's reserve, so no member's collateral requirement could be computed
at all until someone picked it.

## Decision

**A charge does not depend on fault.** The member who owned the benchmark is
charged whether the cause was the member, the pool, TIG, or was never
established.

**The amount is derived from the benchmark, not chosen by policy:**

```text
earned active bundles, no successfully arbitrated report  ->  0
earned active bundles, R nonces successfully arbitrated   ->  P * min(R, B) + F
anything else                                             ->  F
```

`P` is the live `reports.penalty_amount` at the charge block, `R` and `B` are
`tig_integration.md` §14.1's distinct arbitrated nonces and bundle count, and
`F` is that benchmark's own precommit fee. `X[policy]` ceases to exist as a
design concept, and with it the last unset number in the collateral formula.

It does not cease to exist in code on the day this ADR is accepted. Slice 1
shipped `precommit_failure_charge_atoms` as a config value and reserves
`method + fee + that value`; it is set to zero, so the arithmetic already
equals the formula above, and issue #61 removes the term. This ADR is the
design decision, not a claim about what is currently compiled.

The first branch turns on the benchmark having **earned at least one active
bundle**, not on TIG's `Active` state. `tig_integration.md` §6 defines the
latter as membership in `block.data.active_ids.benchmark`, which a complete
benchmark reaches with `num_active_bundles = 0` — work that returned nothing,
which §11.6 already counts as a chargeable failure. Keying on `Active` alone
would charge nothing for it.

**The reserve carries the fee once, and holds it to terminality.** §11.4
previously held `F + X`; with the failure charge derived from `F`, those were
the same money twice — a member held two fees to begin work while never being
able to lose more than one. The fee portion is now held under the same
condition as the method portion, until every report and arbitration is
terminal, because a reported benchmark is charged `P * min(R, B) + F` and
releasing `F` at verification would leave the reservation short by a fee even
at `10_000` bps.

**There is no in-system appeal.** Nothing remains for a member to contest,
because no attribution is made. A member who believes a charge was wrong
contacts the pool out of band; a pool investigation that agrees reverses it
through §10's correction path, as an audited batch with a stated reason.

**Every failure counts toward tier removal.** `f` in `mining_system.md` §8
counts every chargeable failure the member owned, and each contributes its own
amount rather than a multiple of a constant.

## What this costs

**A member can be charged for a failure they could not have prevented.** A
benchmark can arbitrate `NONREPRODUCIBLE` for a pool-side reason —
`mining_system.md` §8 names pool-constructed wrong proofs and packages the
pool corrupted after durable acceptance — and the member pays. Worse than
"could not have prevented": after durable acceptance the member has deleted
their local copy at the pool's instruction, so they could not have observed it
either. `mining_system.md` §10 invariant 7 still assigns the pool
responsibility for that handling; this ADR separates who is responsible from
who pays, and that separation is the whole of what the owner chose.

**Pool-caused failures are correlated.** One proof-construction defect or one
artifact-store incident fails many members' benchmarks in the same round, and
that round closes with every affected member past their `k` demoted and
charged. Tier removal has no reversal path and repurchase costs `J[k]` again,
so unwinding a mass demotion runs through §10. An operator meeting that shape
of incident should expect to use it, and `security.md` §3's correlation check
is now the only thing that flags the shape at all — it no longer excuses the
charge.

**The remedy is a promise, not a mechanism.** "Contact the pool" is real
recourse only while someone answers. It is not enforceable by the member, has
no deadline, and leaves no protocol trace. That is acceptable for v0 at
testnet amounts and is a liability at public-funds scale, which is why the
member terms `pre_build_checklist.md` §9 requires must say plainly that a
charge does not depend on fault and that there is no appeals process. A member
should learn that before depositing, not after a charge.

## Consequences

- `accounting.md` §11.2, §11.4, §11.5, §11.6, §13 item 19 and §14 change
  together; §11.4's reserve becomes `scaled_method_reserve + F`, held whole to
  terminality.
- §11.5 previously said a price-rise shortfall "is not recoverable from the
  member: the reservation is that member's whole committed exposure". That was
  the same clause as ADR 0010's and falls with it: the member owes the charge,
  and only *where the excess comes from* is left open. A charge can now exceed
  the encumbered amount for two reasons — a multiplier below `10_000` bps and
  a price rise — and issue #56 owns both.
- `mining_system.md` §8 loses the fault exemption and the `f * X` arithmetic;
  §10 invariant 7 gains a sentence separating duty from payment; §11 drops `X`
  from the values still to be chosen, leaving `J[k]` and the headroom limit.
- `architecture.md` §6's slash row no longer keys on member-fault evidence or
  appeal state, and its §7.6 storage list no longer stores a failure-charge
  policy value, because there is no longer one to version.
- **One charge posts one batch with two credit lines**, and this is a ledger
  choice made while recording the decision rather than one the owner made.
  Folding the method penalty and the failure charge into a single table gave
  one event two candidate credit accounts. The batch now credits
  `EQUITY:SECURITY_LOSS_RESERVE` for the penalty portion and
  `REVENUE:FAILURE_CHARGES` for the fee portion — they replace different
  things, TIG value lost versus a pool outlay reimbursed — under **one** §8.6
  cause identifier. One cause matters more than the split: §13 item 13 allows
  one sweep per cause so a retry cannot sweep twice, and §13 item 12's
  charged-but-unswept margin has to stay computable.
- **The table is cumulative and the pool charges the increment.** `R` is
  pooled across the benchmark, so a second successful arbitration raises the
  total and the pool charges the difference. Re-applying the table per
  arbitration would take the fee twice, exceed §11.4's reservation, and
  violate §13 item 19.
- `fixtures/collateral-tier/v1` asserts the superseded reserve shape, the
  early `X` release, and the `f * X` arithmetic. `v1` is immutable, so the
  supersession is recorded in its README as open question 0a and the corrected
  cases go in a `v2`. Unlike ADR 0008's rename, this is not the same
  arithmetic under new names: those cases would pass against behaviour §11.4
  now calls under-reserving.
- `security.md` §2.3's three control rows and §3's tier-failure rule follow;
  `member_protocol.md` §17 drops `X`, and its failure-classification table
  stops exempting pool/TIG incidents.
- `member_attack_model.md` keeps its fault classification as an operational
  tool. It is no longer a gate on whether a charge happens, which is a
  demotion in authority worth stating: that document explains the pool's
  exposure, and no longer decides anyone's money.
- **This supersedes part of ADR 0010**, which recorded that an uncovered
  shortfall "cannot be recovered from the member: the whole point of the
  reservation is that it is the only member value the pool may take". Under
  this ADR the reservation is a floor on what a member must hold, not a cap on
  what they owe. ADR 0010 stays as written; this is where a reader learns that
  clause no longer holds.

## Alternatives rejected

**Keep fault attribution.** The status quo, and the fairer rule. Rejected by
the owner as unnecessary machinery for a first version: a member who believes
they were wronged can say so, and the pool can investigate without the
protocol modelling it.

**A narrow exception list.** Proposed during the decision: charge regardless
of fault *except* for a short closed set of pool-side causes the pool detects
from its own records — a corrupted artifact, a pool-built bad proof — which
need no member input because the pool already knows. This keeps almost all the
simplification, since the member still never argues anything, while not
charging members for failures they were instructed to make themselves
defenceless against. **The owner considered and declined it**, preferring the
simpler rule for v0 with out-of-band investigation as the remedy. Recorded
because it is the obvious thing to reach for if the cost above starts landing.

**A flat `X`.** Keeps the charge a policy number. Rejected because the amount
a wasted benchmark actually costs the pool is its own precommit fee, which
varies with bundle count, and a flat figure is either punitive on small
assignments or under-recovers on large ones.

## Revisit when

- A pool-side incident charges and demotes a group of members at once. That is
  the case the narrow exception list above exists for, and it is written down
  rather than left to be re-derived under pressure.
- Out-of-band complaints stop being answerable by one person, which is the
  point at which "contact the pool" stops being a remedy.
- Public funds, where `pre_build_checklist.md` §9's member-terms and legal
  gates apply and this must be re-taken rather than inherited.
