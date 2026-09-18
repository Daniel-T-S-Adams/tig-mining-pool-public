# ADR 0012: A round is credited only after its own slashing is settled

Status: accepted  
Date: 2026-09-18

## Context

ADR 0008 gave a member one balance and credited a settled round into it
immediately. Because a round's earnings can still be destroyed by a method
penalty against one of its own benchmarks, that ADR could not let the credit
back new work straight away, so it added a **maturity** rule: earnings are
withdrawable at once but not collateral-eligible until every report against
every contributing benchmark is terminal.

That rule worked, and it cost four mechanisms to express:

- a maturity flag splitting `LIABILITY:MEMBER_BALANCE` into matured and
  unmatured parts;
- a maturation condition evaluated *per contributing benchmark* rather than
  per earning round, because a benchmark started before a round boundary earns
  qualifiers attributed after it;
- a withdrawal **draw order** — unmatured earnings first, oldest round first —
  without which the amount and the waits would depend on an implementer's
  choice, since one fungible liability carries no record of which atoms were
  once collateral; and
- §13 item 20, an invariant existing solely to stop unmatured value counting
  toward admission.

It also made one recorded withdrawal request deduct two different figures: its
whole amount from withdrawable balance, and only its matured portion from
admission capacity.

The pressure that made all of this necessary is the gap between crediting a
round and knowing what that round owes. `tig_integration.md` §14.2 bounds that
gap: reports against a benchmark from round `R` close at the end of round
`R + submission_period`, and their arbitrations are published by the end of
`R + submission_period + 1`.

## Decision

**A round is not credited to any member until its own slashing is settled.**

§8.4's batch posts only when both hold, and the later governs:

- every benchmark whose qualifiers were attributed in that round has a closed
  reporting window and terminal reports, so every charge against the round is
  known; and
- the exact corresponding TIG payment is finalized in the reward wallet and
  §8.3a's member leg has completed.

What arrives in a member's balance is therefore always settled value.
**Everything in the balance is collateral-eligible the moment it is there**,
and the four mechanisms above are removed: no maturity flag, no draw order, no
two-figure deduction, and §13 item 20 restated as a property of *when the
credit posts* rather than a flag on value already credited.

Two things deliberately survive.

**The per-contributing-benchmark condition**, because it was never really
about maturity. §14.2's `?round=` selects reports by the *benchmark's* round,
so a round's earnings are settleable only when every benchmark that earned in
it is clear — not when the earning round's own window closes. Keying to the
earning round would settle a round while a report against a benchmark that
contributed to it was still open.

**Observation, not the clock.** §14.2's bound says when an answer should be
readable, not when it is true. A round still unsettled past the bound is an
alertable discrepancy under §13 item 16, exactly as an outstanding freeze is.
Settling on a schedule would credit a member against a penalty still live,
which is the failure this ADR exists to make impossible.

## What this costs

**A member waits longer to see a round at all.** Under ADR 0008 they saw the
credit immediately and only its collateral use was delayed; now nothing
appears until the round is clear. The owner states TIG's payment delay already
exceeds the arbitration window, which would make the funding condition the
binding one and the difference small; that is owner confirmation rather than a
measured fact, and it is not what the rule rests on. When arbitration is slow,
the member sees nothing rather than seeing an unusable balance.

That is a real reduction in what the member can observe, and it is the price
of the balance meaning exactly one thing. The member-facing surface should
show work that has earned but not yet settled, or a member who finished a
benchmark will see no movement anywhere and reasonably conclude something
broke.

**Nothing is recoverable from a credited round.** The model depends on the
deduction happening before the credit, so a charge discovered after a round
settles has no earnings left to reach. That is the same exposure ADR 0008 had
and is bounded by the same thing: §14.2's window, and §13 item 16's alert when
it is exceeded rather than a compensating guess.

## Consequences

- `accounting.md` §8.4 gains the settled-slashing condition and loses the
  maturity carve-out; §12.1 gains it as a settlement gate.
- §11.4's `eligible_collateral[m]` is based on `balance[m]`, not `matured[m]`.
- §11.7 loses the maturity subsection, the draw order, and the two-figure
  deduction; §11.3's tier fee no longer needs the word "matured".
- §13 item 20 is restated: no value enters the balance before it is settled.
- §11.6's withdrawal-request paragraph collapses to one figure, and its
  waits apply to a whole withdrawal rather than to a drawn portion.
- **This supersedes ADR 0008 rule 2**, which stays as written under the
  immutability rule. ADR 0008's decision — one balance, deposits and earnings
  together, collateral reserved from it — is untouched; only the timing of the
  credit and the maturity apparatus change.
- It removes one structural obstacle to any future rule that adjusts a round
  before it is distributed: while a round was credited on settlement, such an
  adjustment would have had to reopen a posted batch, which §9 forbids
  outright. This ADR does **not** introduce such a rule, and three settled
  rules currently stand against the obvious one: §11.6 caps a method slash at
  its reserved method amount, §10 rule 7 forbids charging other members or a
  future block for a shortfall, and ADR 0010 records that the uncovered
  portion cannot be recovered from the member. Changing any of that is issue
  #56's, and nothing here should be read as having decided it.

## Alternatives rejected

**Keep ADR 0008's maturity rule.** It works and it is already written down.
Rejected because the four mechanisms it needs exist only to describe a state —
credited but unusable — that this decision makes unreachable. Whether anything
should later adjust a round before it is distributed is issue #56's question,
not a reason weighed here.

**Credit immediately and claw back on a later charge.** Simplest for the
member, and impossible here: §9's batches are immutable and §10's corrections
are for errors, not for outcomes the design expects.

**Credit at the arbitration bound rather than on observed terminality.**
Would make settlement predictable. Rejected for the reason §14.2 gives about
its own bound: it says when an answer should be readable, and acting on it as
though it were the answer risks crediting against a live penalty.

## Revisit when

- TIG's payment delay drops below the arbitration window, making the slashing
  condition the binding one and the member's wait visibly longer.
- `submission_period` changes, which moves the window this depends on.
- Members report that a finished benchmark showing no balance movement for
  weeks is indistinguishable from a fault — the answer is a member-facing
  "earned, not yet settled" view, not a change to this rule.
