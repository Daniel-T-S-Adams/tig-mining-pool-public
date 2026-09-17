# ADR 0009: Automated member withdrawal below per-member caps

Status: accepted  
Date: 2026-09-16

## Context

ADR 0008 replaced automatic per-round payout with a member-initiated
withdrawal: the member chooses when to ask and how much, up to their
unencumbered balance.

[`security.md`](../security.md) §3.4 requires **unconditional multi-person
authorization** for every member-custody transfer — every transfer, at any
amount, with no threshold — and `accounting.md` §12.4 points there rather than
restating it. That rule predates ADR 0008: it guarded every collateral
movement when collateral sat in its own custody, and merging the pots widened
what one signature reaches rather than narrowing it, so it was deliberately
kept.

§3.4 is a **public-funds** boundary: by its own first line it governs the
posture required *before public deposits or payouts*, not what testnet does
today. So nothing is blocked right now, and this ADR is not unblocking a
stuck process — it decides what the rule should be when it binds.

What neither document stated is the consequence for the member-facing product
at that point. The pool has a single human operator; `plans/ai-workflow.md`
builds the whole development model around one, whose remaining roles are
spend, custody and secret decisions. Under an unconditional rule every
withdrawal — including a member moving their own unencumbered TIG by an amount
the database can check exactly — waits for that operator. A self-service
balance and a rule that routes every transfer through one person cannot both
be true: a withdrawal control on the member website would be honest only if it
said "ask the operator", and the pool would be holding members' money while
unable to return it without someone present.

`CLAUDE.md` reserves funds-custody decisions, and any weakening of a stated
control, to an explicit human decision. This ADR records that the pool owner
made one.

## Decision

**A member withdrawal is signed without human authorization when it is small
enough, and only then.**

The signer may act on an approved withdrawal intent with no multi-person
authorization when every condition holds:

```text
amount <= per_transaction_cap

member's total withdrawn over the preceding rolling 7 days,
  this request included                        <= weekly_cap

per_transaction_cap = 10_000 TIG
weekly_cap          = 10_000 TIG
```

Both caps are **per member**. Every §12.1 payability gate is unchanged and is
checked first: the amount must be unencumbered, §11.6's waits satisfied, no
accounting or security hold in force, destination configuration compatible. A
request that fails any of them is refused as it is today; the caps decide only
whether a *payable* withdrawal needs a human.

Any withdrawal above either cap keeps `security.md` §3.4's unconditional
multi-person authorization, unchanged. So does every transfer out of member
custody that is not a member withdrawal — `accounting.md` §8.6's sweep of pool
value to operating custody is untouched by this ADR, as are operating-custody
thresholds.

The window is rolling rather than calendar-aligned, matching §12.4's existing
"rolling daily" vocabulary for operating custody: a calendar week lets a
member take two full caps minutes apart across a boundary.

Both values are versioned policy carried in the same append-only form as the
fee policy in §5, not constants in the signer. At the values chosen the
per-transaction cap can never bind — the weekly cap is the same number and
always reaches first — and they are recorded as two independent values anyway,
because raising the weekly cap later must not silently also raise the largest
single transfer the signer will make unattended.

**There is no pool-wide automated ceiling.** With the caps per member, the
aggregate the automated path can move in a week is the weekly cap times the
number of members. That is a deliberate consequence, not an oversight.

## What this accepts

The rule being relaxed exists for a specific threat, and the relaxation should
not be read as having answered it.

The caps bound **abuse through the application**: a compromised member account,
a bug in the payability check, an operator error. They bound nothing about the
signing key itself. Someone holding the member-custody key does not submit
withdrawal requests — they sign transfers directly, and neither cap is ever
consulted. ADR 0008 already recorded that one pot means that key reaches every
member's collateral and every member's withdrawable balance at once; the
control this ADR narrows is the one that made holding the key insufficient to
move funds.

The design names a third limit alongside the two chosen here — a hot-wallet
limit, in §12.4's list — which is what would bound that loss by keeping most
member value where the signing key cannot reach it. **The owner has declined
to set one for v0**, and accepts explicitly that a compromise of the
member-custody signing key takes all member funds. That is recorded here as a
position taken rather than a gap: v0 assumes the key is not compromised.

This is stated plainly because `security.md` §3.4 argued the opposite case,
and the reader of that section should find the disagreement rather than
discover it.

## Consequences

- `security.md` §3.4's "unconditional … every transfer, at any amount, with no
  threshold" no longer holds for member withdrawals. §3.4 now owns the
  threshold rule for them and states what it accepts; operating-custody
  thresholds there are unchanged.
- `accounting.md` §12.4 gains the cap check as a signer precondition, beside
  the allow-list, simulation and nonce-lane rules it already carries.
- A member-facing withdrawal control can promise an outcome. It still cannot
  promise instant *settlement*: §12.5's completion rule is unchanged, so a
  transfer is complete only at a finalized Base block, which Base documents as
  roughly twenty minutes.
- The pool's exposure to a compromised member account rises from zero — under
  the old rule a human saw every transfer — to one weekly cap per compromised
  account. §12.2's destination controls are what bound it, and under the
  hardwired-destination model in ADR 0011 a compromised
  session can only ever send to the member's own wallet.
- This is a weakening of a control that `CLAUDE.md` places under human-only
  actions. It is recorded, dated, and attributed rather than applied quietly,
  and the cost is stated above rather than argued away.
- The public-funds gates in `pre_build_checklist.md` §9 are unaffected and
  still apply. Production custody remains an open launch gate under §3.4's
  last paragraph and ADR 0007.

## Alternatives rejected

**Keep the unconditional rule.** It is the safer rule and it was kept
deliberately once already. Rejected because, with one operator, it makes a
self-service member balance impossible: the pool would hold members' money and
be unable to give it back without a person present.

**Set a hot-wallet float now.** The control that would actually bound key
theft, and the one the design already names. Rejected by the owner for v0; see
"What this accepts". It is the first thing to revisit.

**A rolling daily cap.** §12.4's existing vocabulary for operating custody, and
the tighter choice. The owner selected a weekly window.

**Cap the pool in aggregate as well as per member.** Would bound the total the
automated path can move regardless of member count. Not chosen; the
consequence is recorded above so the aggregate is a known quantity rather than
an assumed one.

## Revisit when

- A second person can authorize transfers, which removes the operational
  reason this ADR exists.
- Held member value grows past what a total loss of the member-custody key
  could absorb — the hot-wallet float is the designed answer and is written
  down rather than left to be re-derived.
- Public funds or mainnet, where `pre_build_checklist.md` §9's custody and
  security-review gates apply and this decision must be re-taken rather than
  inherited.
- The number of members makes the un-capped aggregate materially larger than
  the per-member cap suggests.
