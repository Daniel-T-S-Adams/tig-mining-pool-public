# ADR 0011: The connected wallet is the member account and the payout destination

Status: accepted  
Date: 2026-09-16

## Context

[`member_protocol.md`](../member_protocol.md) §17 deliberately left the member
website login mechanism unchosen, and §3.3 says the same of "the account login
that authorizes ticket creation". The choice was deferred to whichever document
owned the member account; none did, so it stayed open.

[`accounting.md`](../accounting.md) §12.2 meanwhile specifies the withdrawal
destination as an account **setting**: a verified Base address linked by a
domain-separated EIP-191 or EIP-712 signature, changeable by an authenticated
member, with recent reauthentication and phishing-resistant MFA or a passkey
required for the change, one-time nonces, exact-domain validation, and a
48-hour security delay with out-of-band notification during which that
member's withdrawals are held.

Every one of those controls guards a single attack: someone who reaches a
member's account points the payout address at their own wallet and withdraws.
ADR 0008 made that attack worth mounting by replacing automatic payout to a
long-verified address with a member-initiated withdrawal, and §12.2 says so.

The pool owner has chosen the login mechanism, and the choice removes the
attack rather than defending against it.

## Decision

**A member signs in by connecting a wallet, and that wallet is where their
money goes.**

The member account is identified by a Base address. Authentication is proof of
control of that address: a domain-separated EIP-191 or EIP-712 signature
carrying pool domain, chain ID, address, a one-time nonce, purpose, and
expiry, validated for exact domain so a signature cannot be replayed against a
different pool or chain. That is §12.2's existing linking proof, unchanged in
substance — what changes is that it establishes the session and the
destination in one act instead of binding a setting to a separate account.

**There is no payout-destination setting, and no operation that changes one.**
A withdrawal can only ever be sent to the address that authenticated the
session. The pool does not accept a destination from a worker credential, from
a member-supplied field, or from an operator edit.

Consequently §12.2's address-change controls have nothing left to guard and do
not apply: no 48-hour hold, no out-of-band change notification, no
reauthentication or MFA step for a destination change. The remaining §12.2
rules stand — a member may still request a withdrawal hold, and a missing or
held destination still leaves the amount in the member's balance and affects
no other member.

## What this costs

**There is no account recovery, because there is no account to recover into.**
A member who loses control of their wallet loses access to their pool balance,
permanently. The pool cannot re-point the destination — that is precisely the
operation this decision removes — and it cannot verify the claimant, because
control of the address was the only thing it ever verified.

That cost is larger than the balance alone. `member_protocol.md` §3.3 makes
"the authenticated member account" the authority that issues a
`WORKER_RECOVERY` ticket when a worker's private key is unavailable. With the
wallet as the account, a member who loses their wallet also loses the ability
to recover any worker whose key they lose afterwards. The two recovery paths
are no longer independent, and this ADR is what couples them.

**A member cannot migrate wallets.** Moving to a new address means
withdrawing the balance to the old wallet, and enrolling again as a new
member — with a new member ID, no history, and no trust multiplier under
ADR 0010. There is no supported transfer of a member account between
addresses, deliberately: any such operation is the address-change attack with
a different name.

Both costs are accepted. They buy the removal of an entire attack class rather
than a set of controls that mitigate it, and they are the ordinary bargain of
self-custody, which the pool's members already accept to hold TIG at all.
What the pool owes in exchange is saying so plainly at sign-up rather than
letting a member discover it after losing a key — that belongs in the member
terms `pre_build_checklist.md` §9 already requires.

## Consequences

- `member_protocol.md` §17's deferred login choice lands, and §3.3's open
  "account login that authorizes ticket creation" is answered: it is the
  wallet signature.
- `accounting.md` §12.2 is restated for this model. The destination remains
  member-scoped and signature-verified; it is no longer changeable, so the
  change controls go with it.
- **This supersedes one bullet of ADR 0008.** That ADR's consequences recorded
  "§12's controls are unchanged in substance and now apply to withdrawal
  intents: verified destination with a 48-hour change hold …". The verified
  destination survives; the change hold does not, because the change does not.
  ADR 0008 is left as written, per the immutability rule — this is where the
  reader learns that part of it no longer holds.
- A compromised member session can move funds, but only to that member's own
  wallet. Combined with ADR 0009's per-member caps, the worst a stolen session
  achieves is moving one weekly cap of the member's own money to the member's
  own address — which is not a theft at all. This is why ADR 0009's relaxation
  and this decision are safe together and would not be safe apart.
- The member website needs no password, no email, no MFA enrolment, no
  password reset and no session-recovery flow. It also cannot send a member a
  security notification, because it holds no contact channel — an out-of-band
  notification is not available to any part of this design.
- One wallet is one member. A person operating two wallets is two members,
  with two balances, two multipliers and two trust histories.
- The pool never holds a member credential that is worth stealing from it. It
  stores addresses and signatures, both public by construction.

## Alternatives rejected

**Keep §12.2 as written: separate account, changeable verified destination.**
The safer-looking option, and it preserves account recovery. Rejected because
it keeps the attack that its own controls exist to blunt — a compromised
account redirecting funds — and pays for the mitigation with a 48-hour hold on
every member's withdrawals after any address change, MFA enrolment, and a
notification channel the pool would have to collect and hold.

**Wallet login, but a separately settable payout address.** Login and
destination decoupled, so a member could pay out to a cold wallet. Rejected
because it reintroduces the address-change operation in full, and with it
every control in §12.2 — the decision would buy nothing.

**Wallet login plus an optional recovery contact.** Would restore a recovery
path. Rejected for v0: any recovery path that can re-point a destination is
the address-change attack, and one that cannot re-point the destination
recovers nothing worth having.

## Revisit when

- Held member balances grow to the point where permanent loss from a mislaid
  wallet is a support and reputational problem rather than an understood
  risk of self-custody.
- Smart-contract accounts or session keys make delegated, revocable control of
  an address ordinary, which would allow recovery without an address-change
  operation — the property this decision could not get any other way.
- The member terms review under `pre_build_checklist.md` §9 finds that "no
  recovery, ever" is not something the pool can fairly ask a member to accept.
