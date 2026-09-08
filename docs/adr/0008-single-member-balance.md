# ADR 0008: One member balance for earnings, collateral, and withdrawal

Status: accepted  
Date: 2026-09-06

## Context

`accounting.md` §11.7 recorded, as an open owner decision, whether a member's
earnings may back their working capacity. §14 listed it among the decisions
that block accepting public member collateral.

The problem it names is real and structural. §11.4's reserve scales with
bundle count and with TIG's live `reports.penalty_amount`, so an established
member needs collateral proportional to the work they do, while §11.2
recognizes collateral only from an inbound on-chain transfer and §4 rule 9
pays every positive round amount straight out to the member's external
address. A member who earns inside the pool therefore has to be paid, wait
for finality, and transfer the same TIG back to raise their capacity. The
member's own earnings — value the pool is already holding — cannot back the
work that produced them.

§11.7 declined to specify a mechanism because every route crosses the custody
boundary that `architecture.md` §13 invariant 9 drew between payout custody
and slashable security-deposit custody. That is still true, and it is why this
is a funds-custody decision rather than an accounting one — `CLAUDE.md`
reserves those to an explicit human decision. What has changed is that the pool
owner has made it.

**Where that decision is recorded.** The owner chose the one-pot model, and
separately chose to keep member value off the reward wallet, in the pull
request that introduced this ADR; the options put to them and the option
chosen are recorded in a comment on it. Auto-merge removed the merge click
that used to carry a decision's provenance, so it is written down instead.

## Decision

**A member has one balance with the pool.** Deposits under §11.2 and settled
round earnings both land in it. Collateral reservations are taken from it.
The member chooses when to withdraw, and may withdraw only the part of it
that is not encumbered.

This replaces automatic per-round payout. §4 rule 9, §8.4 and §12.1
previously moved every eligible positive amount out of the pool each round
with no member choice; settlement now credits the member's balance instead,
and an outbound transfer happens when the member asks for one.

Four rules fix what "one balance" means.

**1. Earnings are recognized by settlement, not by a transfer event.** §11.2's
recognition rule stands unchanged for inbound deposits: a unique
`(chain_id, tx_hash, log_index)` in a finalized Base block. A settlement
credit has different evidence — the reconciled round — and is a distinct,
separately audited recognition path. Neither path may be used to record the
other kind of value.

**2. Earnings mature before they can be collateral.** Credited earnings are
withdrawable immediately but are not collateral-eligible until the round that
produced them can no longer generate a method penalty against the pool: every
report against that round's benchmarks is terminal, and the reporting window
`tig_integration.md` §14.2 bounds has closed. Collateralizing unmatured
earnings would let the same TIG cover the penalty that could destroy it.

Maturity is an admission property and moves no tokens. Matured and unmatured
value sit in the same address, so a round maturing posts no transfer at all.

**3. Encumbrance decides what can leave.** A withdrawal intent may be created
only for balance that is not reserved under §11.4, not frozen under §11.6,
and past §11.6's waiting rule where that applies. The member names an amount;
the pool refuses any amount above the unencumbered part rather than
partially filling it.

**4. One custody address holds all member value.** Deposits, unmatured
earnings, matured collateral and frozen amounts are one balance at one
address. Value leaves it only when it leaves the pool: a member withdrawal to
that member's verified address, or `accounting.md` §8.6's sweep of value that
has become the pool's to the one allow-listed operating-custody address. §8.6
enumerates the causes; this ADR deliberately does not restate them, so the two
documents cannot drift.

Member custody is **not** the pool's reward wallet. TIG mints the pool's
rewards to its benchmarker identity, and §8.3a sweeps each settled round out
of that wallet in two transfers, one per destination address: the member leg
carries the members' share plus the round's unresolved suspense, the operating
leg carries the pool's fee. Two, because an ERC-20 transfer has one recipient.

Both are signed by an **operator, manually**, with the offline key: the
accounting projector creates the `(network, round, leg)` intents and the
controller confirms each from its own finalized event, but no runtime process
— the Funds Gateway included — ever holds the reward wallet's key. They are
infrequent by construction, two transfers per paid round against a payment
delay measured in weeks.

Paying withdrawals from that wallet instead would put the pool's TIG protocol
identity in a runtime process, and a single theft would take the member funds
and the identity together.

## What one pot costs

`architecture.md` §13 invariant 9 previously required TIG payout custody and
slashable security-deposit custody to share no address, signing key, ledger
asset, or transfer intent. That cannot survive a balance which is
simultaneously slashable and withdrawable: the same TIG would have to be in
two addresses at once.

The alternative that preserved it was two custody addresses with an
allow-listed rebalance between them, and it was designed in full before being
rejected. It kept a real property — a stolen payout key could not reach
collateral, and a stolen deposit key could not pay anyone. What it cost was a
second machine: maturity had to become a ledger status flipped by a finalized
rebalance rather than a plain condition, every maturation and every
matured-portion withdrawal needed an on-chain transfer between pool addresses,
those transfers had to post in the same batch as their ledger effect so the
per-custody coverage rules were never momentarily false, and a withdrawal draw
order existed partly to decide which portion needed a transfer at all.

**The pool owner chose one pot.** The cost is stated rather than argued away:
a compromise of the member-custody signing key reaches every member's
collateral and every member's withdrawable balance at once. Two things bound
it, and both are now what invariant 9 enforces:

- member value shares no address or key with the pool's operating funds, and
  the only non-member destination the member-custody signer can reach is the
  one allow-listed operating-custody address; and
- member value shares no address or key with the pool's TIG protocol
  identity, so a funds compromise is not also an identity compromise.

What the pool gains is proportionate. A member's TIG never moves because its
collateral status changed — only because it left the pool — so there are no
internal transfers, no gas spent moving the pool's own money between its own
addresses, no maturity-as-ledger-status, and one coverage inequality instead
of two. Every batch between the inbound sweep and the outbound withdrawal
moves liabilities inside one address.

## Consequences

- A member's capacity grows with their earnings without a round trip through
  their own wallet, which is the effect §11.7 wanted and would not invent a
  custody rule to get.
- The pool holds member value for longer, and by member choice rather than by
  a fixed schedule. That is a larger and less predictable custody position
  than automatic payout produced, and the member terms and legal review §11.2
  already requires must cover it before public funds.
- §12's controls are unchanged in substance and now apply to withdrawal
  intents: verified destination with a 48-hour change hold, one immutable
  intent per generation, allow-listed simulated signing, and completion only
  from a finalized exact token event. What changes is who starts the transfer
  and how the amount is chosen.
- Withdrawal is a member-initiated outbound path that did not exist. It is the
  obvious target for a member-account compromise, which is why §12.2's
  reauthentication, MFA and destination-change hold matter more under this
  model than under automatic payout to a long-verified address.
- Reconciliation gains one coverage term instead of two: member custody must
  cover every member balance plus every pending withdrawal. Coverage rather
  than equality, because the pot also briefly holds pool value awaiting §8.6's
  sweep. No in-flight allowance is needed, because value enters or leaves
  member custody only at a finalized token event.
- `accounting.md` §13 invariant 11 kept pending earnings and security deposits
  distinct and un-nettable. They are now one liability with an encumbrance
  split, so the invariant is restated in those terms rather than dropped:
  delegated TIG, member balance, pool revenue and the security loss reserve
  remain distinct, and encumbered balance is never spent as unencumbered.
- The reward wallet gains a job it did not have: §8.3a's per-round sweep into
  member and operating custody. It is signed by the protocol identity key, so
  it is a deliberate, low-frequency, out-of-runtime operation rather than a
  hot path.
- **This extends ADR 0007's testnet wallet scope**, and says so rather than
  widening it silently. That ADR authorised a dedicated operator browser
  wallet to hold the TIG account signing key for API-key provisioning, and
  listed "a testnet wallet would receive non-trivial value or authority" as a
  trigger to revisit. Signing a sweep leg is new authority: the key now moves
  member value, not only proofs of ownership. The trigger is judged not to
  fire on testnet, where the value moved is testnet TIG and the wallet still
  holds no production authority and no mainnet funds — ADR 0007's own bound.
  It does fire for production, where ADR 0007 already requires an offline or
  hardware-/managed-key custody decision; that decision must now also cover
  sweep signing, not only provisioning.

## Alternatives rejected

**Keep automatic payout and let members re-deposit.** This is the status quo
§11.7 described as "already achievable today". It is rejected because the
round trip is not merely inconvenient: TIG's own payment delay plus Base
finality plus a member's reaction time is a long interval during which a
member's capacity is bounded by cash they hold outside the pool, which is
what §11.4's bundle-scaled reserve makes expensive.

**Two custody addresses with an allow-listed rebalance.** Designed in full and
rejected by the owner; "What one pot costs" above records what it kept and
what it cost.

**Everything on the reward wallet.** The simplest arrangement of all: no
sweep, one address for rewards and member value together. Rejected because
that wallet's key is the pool's TIG protocol identity. Signing withdrawals
from it would put that key in a runtime process, against `architecture.md`
§2.2 and ADR 0007, and one theft would take the member funds and the pool's
TIG identity together.

**Split the balance into an earnings pot and a collateral pot the member
transfers between.** The two-custody model with the internal transfer exposed
to the member. Rejected: it reproduces the round trip the decision exists to
remove, in a nicer wrapper.

## Revisit when

- The pool's held member balance grows past what the custody, insurance, or
  regulatory posture assumed when automatic payout was the model.
- The member-custody key's blast radius stops being acceptable — held value
  grows, or an incident elsewhere shows the concentration was underpriced.
  The two-address design in "What one pot costs" is the thing to revisit, and
  it is written down rather than left to be re-derived.
- TIG changes the method-report penalty structure such that §11.4's reserve no
  longer scales with bundle count, which is the pressure that motivated this.
