# ADR 0010: Per-member collateral multiplier

Status: accepted  
Date: 2026-09-16

## Context

[`accounting.md`](../accounting.md) §14 carried two unresolved owner
decisions, and both blocked accepting public member collateral:

1. the dynamic slashable-deposit and malicious-work policy in §11.3–§11.5,
   with a note that a fixed `10 TIG` per benchmark is **explicitly not
   accepted**, because direct method-verification exposure scales with
   `num_bundles` and with TIG's live report penalty; and
2. a **trust-label mechanism** that would let a trusted member's allowed
   bundle count exceed what their collateral alone permits, with the
   instruction that nothing may raise an admission limit above §11.4's formula
   until it lands.

[`mining_system.md`](../mining_system.md) §11 records the same trust-label
item from the mining side, deferred with its direction confirmed, and holds
§8's flat-tier rule in place until it is designed.

§11.4 already specifies the reserve in full. What was missing was not a
formula but two confirmations: that §11.4 **is** the policy rather than a
proposal awaiting replacement, and by what mechanism the pool may relax it for
a member it trusts.

`CLAUDE.md` reserves security-deposit and slashing decisions to an explicit
human decision. The pool owner has made both.

## Decision

**§11.4's formula is the collateral policy, unchanged.** For decision snapshot
`s` and proposed track settings `t`, the method reserve stays

```text
method_reserve[s,t] = P[s] * B[t]
```

where `B[t]` is the proposed bundle count and `P[s]` is the **live**
`config.reports.penalty_amount` read from the block-consistent decision
snapshot. `P` is not `10`. Ten is what `P` reads today and what §11.4 already
used as its worked illustration; the rejection §14 recorded stands, and a
compiled-in `10` would both under-collateralize the pool the moment TIG raises
the penalty and violate `tig_integration.md` §12, which
`scripts/no-observed-constants.sh` enforces.

**A per-member multiplier scales the method reserve.** For member `m`:

```text
assignment_reserve[m,s,t] = M[m] * method_reserve[s,t] + F[s,t] + X[policy]

M[m] defaults to 1, and the pool may set 0 < M[m] <= 1
```

Everything else in §11.4 is unchanged: `precommit_reserve` is still the
maximum assignment reserve across proposed tracks, still reserved atomically
from `eligible_collateral` before the precommit fee is spent, and still
reducible to the selected track's exact requirement once TIG confirms — never
increased afterwards by applying a later policy.

Three rules bound the mechanism.

**1. The multiplier scales risk, not cost.** It applies to the method reserve
alone. `F[s,t]` is the precommit fee the pool is about to spend on this
member's behalf — a certain outlay, not a risk that may or may not land — and
`X[policy]` is the reserved charge for a chargeable failed benchmark.
Discounting either would leave the pool short of money it definitely spends,
or short of a charge it has already decided to levy, which is a different
thing from deciding a trusted member is less likely to be slashed.

**2. A multiplier change never reaches an existing reservation.** `M[m]` is
read when the assignment reserve is computed and fixed into that reservation,
exactly as §11.4 already forbids increasing a reservation by later policy. A
member whose multiplier rises does not retroactively owe more on work already
admitted, and one whose multiplier falls does not have capacity handed back
against exposure that is still open.

**3. `M[m]` is versioned policy, not an operator edit.** It is carried in the
same append-only form as §5's fee policy — the value, when it takes effect,
who set it, and why — so a reservation can always be re-derived from the
policy that was in force. The pool sets it; a member cannot influence it.

Raising `M[m]` above 1 to demand *more* collateral from a member the pool
distrusts is not part of this decision. The admission formula, the tier rule,
and `mining_system.md` §8's failure handling are the instruments for that.

## What this costs

At multiplier `M[m]`, the pool holds `M[m] * P * B` against an exposure whose
maximum is `P * B`. If that benchmark is slashed for the full
method-verification penalty, the pool absorbs

```text
(1 - M[m]) * P * B
```

from its own funds. It cannot be recovered from the member: the whole point of
the reservation is that it is the only member value the pool may take, and
`mining_system.md` §8 does not make a member liable beyond it.

That shortfall is deliberate credit risk extended to the pool's most trusted
members, and it scales with two things at once — how many members carry a
multiplier below 1, and how large their bundle counts are. Bundle count is the
same quantity that made a fixed per-benchmark deposit unsafe in the first
place, so the exposure grows fastest exactly where §11.4 was designed to be
careful.

Stated as a number the pool can watch: aggregate uncovered exposure is the sum
of `(1 - M[m]) * P * B` over every open reservation. It belongs in the §13
reconciliation set as a reported quantity, so a policy of generous multipliers
is visible before a slash rather than after one.

## Consequences

- §14 decision 2 is resolved. The multiplier is the trust-label mechanism: it
  gives a trusted member more concurrent bundles for the same balance by
  lowering what each bundle reserves, rather than by raising an admission
  limit above the formula — which §14 and `mining_system.md` §11 both forbade.
  There is one mechanism rather than two, and the admission inequality in
  §11.4 is untouched.
- `mining_system.md` §11's trust-label item lands, and §8's flat-tier rule is
  no longer held open by it. A tier still does not bypass collateral.
- §14 decision 1 is resolved **in shape**: the policy is §11.4, as written.
  It is not resolved in every number. `mining_system.md` §11 still lists
  `J[k]`, `X`, and `internal_pool_unverified_limit` as numerical values
  required before full product implementation, and `X` is a term of the
  assignment reserve — so a member's exact collateral requirement is
  determined only once `X` has a value.
- §14's prohibition lifts: an implementation may accept member collateral
  under this policy and may present it as settled. The public-funds gates in
  `pre_build_checklist.md` §9 are untouched and still govern real member money.
- **One precondition survives the lifting, and is not this ADR's to clear.**
  `member_attack_model.md` records that the penalty *basis* is unconfirmed —
  whether a benchmark incurs `penalty_amount` once or per reported nonce,
  which differs by a per-track factor and is not small. `tig_integration.md`
  §14.1 explains why it cannot be read off the pinned tree. Settling the
  formula does not verify what the formula's units are, so a settled formula
  over an unverified basis is still not a settled reserve, and that
  verification remains a precondition for accepting public member collateral.
  `accounting.md` §11.5 and `member_attack_model.md` both now say so without
  routing through §14.
- Member-facing documentation can now answer "what must I put up, and what can
  I lose?", which it could not while the policy was a proposal.

## Alternatives rejected

**A fixed amount per benchmark.** Rejected in §14 before this ADR and still
rejected: exposure scales with bundle count and with a live TIG penalty, so a
flat figure is either wasteful at small bundle counts or uncovered at large
ones.

**Apply the multiplier to the whole assignment reserve.** Simpler to state and
to implement. Rejected because `F` is a fee the pool certainly pays; a
trusted member would then be admitted while the pool is short of an outlay it
has already committed to, which trust has nothing to do with.

**A literal trust label raising the bundle limit.** The shape §14 and
`mining_system.md` §11 described. Rejected in favour of the multiplier because
it would create a second admission path alongside the collateral inequality,
and both documents warned specifically against anything that raises a limit
above that formula.

**Let the member buy a lower multiplier.** Turns trust into a product and
gives the member influence over the pool's own risk pricing. Not considered
further.

## Revisit when

- TIG changes the method-report penalty structure such that the reserve no
  longer scales with bundle count — the same trigger ADR 0008 records.
- Aggregate uncovered exposure across trusted members grows past what the pool
  can absorb from its own funds; the §13 reported quantity above is what makes
  that visible.
- `X` receives a value, which completes §14 decision 1 and may change how much
  of the reserve the multiplier is scaling.
- A slash actually lands on a member with a multiplier below 1, which is the
  first real test of whether the pool priced the trust correctly.
