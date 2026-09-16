# TIG mining pool: accounting and payout contract

Status: technical contract and v0 tier economics settled; numerical policy values pending  
Last updated: 2026-09-06

This document defines how confirmed TIG proceeds become internal member
credits, how exact integer allocation and corrections work, and the boundary
between the single member balance, custody, and withdrawal. It
implements the payout rule in [mining_system.md](mining_system.md) and the
transaction boundary in [architecture.md](architecture.md).

No public member deposits may begin until the remaining choices in
[section 14](#14-owner-decisions-required) are confirmed and this document's
status changes to `settled`.

## 1. Scope and non-goals

This contract covers:

- the authoritative per-block proceeds field;
- the integer unit and parsing rules;
- pool-fee versioning;
- exact proportional allocation and rounding;
- zero-qualifier suspense;
- TIG block posting and round reconciliation;
- append-only double-entry batches and corrections;
- one member balance holding deposits and settled earnings, and the custody
  that backs each part of it; and
- member-initiated withdrawal authorization, signing, replay protection, and
  confirmation.

It does not make TIG's reward calculation, change qualifier attribution, treat
member balances as TIG protocol coinbase outputs, or treat TIG delegation as
slashable pool collateral. Taxes, legal classification, member terms,
production signer custody, and jurisdiction-specific reporting remain
public-launch gates.

## 2. Authoritative proceeds

For accepted TIG block `b`, let `pool_id` be the pool's one Benchmarker player
address. The authoritative payout base is:

```text
pool_proceeds_atoms[b] = get-opow(pool_id, b).block_data.coinbase[pool_id]
```

The pinned TIG reward code first calculates the pool's gross OPoW `reward`,
subtracts `reward_share` distributed to delegators, applies any configured TIG
coinbase output fractions, and inserts the remainder under the Benchmarker's
own player ID. Therefore the pool's self-entry is the exact post-delegator,
post-external-coinbase amount retained by the pool.

V0 configures no external TIG coinbase outputs. The pool's self-entry should
therefore equal:

```text
opow.reward - opow.reward_share
```

The ingestor stores and checks:

```text
sum(opow.coinbase.values()) == opow.reward - opow.reward_share
pool_proceeds_atoms == opow.coinbase[pool_id]
```

Any missing self-entry or failed equality puts the entire block into accounting
suspense and alerts; it is never replaced by a calculated guess.

Do not use:

- `opow.reward`, because it is gross of delegator sharing;
- the pool player's `reward_by_type.benchmarker`, because it can include a
  coinbase output received from some other Benchmarker; or
- round totals as a substitute for missing per-block proceeds or member
  qualifier weights.

The pinned source evidence is TIG
[`rewards.rs`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-protocol/src/contracts/rewards.rs)
and the schema/field catalogue remains in
[tig_integration.md](tig_integration.md).

## 3. Integer unit and numeric safety

The ledger unit is one **attoTIG**:

```text
1 TIG     = 1,000,000,000,000,000,000 attoTIG
1 attoTIG = 10^-18 TIG
```

At the pinned revision, TIG `PreciseNumber` stores an unsigned `U256` scaled by
`10^18` and serializes its underlying integer as a decimal string. Therefore a
TIG API value such as `"1000000000000000000"` enters the ledger as exactly
that many attoTIG; it is displayed as `1.000000000000000000 TIG` but is never
parsed through a decimal or binary floating-point value. See the pinned
[`PreciseNumber` implementation](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-utils/src/number.rs).

Rules:

- API and on-chain amounts parse from canonical unsigned base-10 or `uint256`
  values into checked 256-bit integers.
- PostgreSQL amount columns use integral `NUMERIC(78,0)` with scale zero and
  explicit range/sign constraints; journal direction is a debit/credit column,
  not a floating sign hidden in an amount.
- Rust uses checked big-integer/U256 conversion at the boundary and never
  `f32`, `f64`, JavaScript number, or PostgreSQL money/float types.
- Every multiplication is widened before division and checked against the
  supported range.
- APIs return amounts as canonical atom strings plus an optional formatted TIG
  display string. Display rounding never changes the ledger.
- One currency code and token/chain configuration version is stored on every
  batch; testnet and mainnet balances can never share an account.

The TIG ERC-20 inherits the standard OpenZeppelin ERC-20 default of 18 decimals,
matching the protocol unit. The pinned token contract is
[`TIGToken.sol`](https://github.com/tig-foundation/tig-monorepo/blob/ad08d1ea001a73ff5aab3b556d7f59246fece14e/tig-token/TIGToken.sol).

## 4. TIG block posting and finality

TIG does not publish a formal irreversible-finality signal for its off-chain
block API. V0 uses an explicit descendant rule rather than treating “currently
latest” as immediately postable.

An observed payout block moves through:

```text
OBSERVED
    -> DESCENDANT_CONFIRMED
    -> POSTED_PENDING_ROUND_RECONCILIATION
    -> ROUND_RECONCILED
```

The containing round then moves through:

```text
ROUND_RECONCILED
    -> TIG_PAYMENT_PENDING
    -> ASSETS_SETTLED
```

The ladder ends there. Under ADR 0008 a round is never *paid*: settlement
credits member balances and starts no transfer, and withdrawal is per member
and per request, decoupled from any round. A round-level `PAYING` or `PAID`
would be a state nothing enters and nothing leaves.

Rules:

1. Snapshot `B` is `OBSERVED` only after the block-consistent snapshot algorithm
   commits all proceeds and qualifier inputs.
2. It becomes `DESCENDANT_CONFIRMED` after two later accepted blocks form one
   uninterrupted `prev_block_id` chain from `B` and no height is missing.
3. Only then may its ordinary journal batch be posted. Until posting, displayed
   earnings are estimates, not ledger balances.
4. A chain conflict before posting discards the observation and rebuilds it from
   authoritative data.
5. A conflict discovered after posting uses an append-only correction; it never
   rewrites the original batch.
6. At round close, the sum of stored per-block pool proceeds is reconciled to
   the pool self-coinbase in `get-round-emissions`. A missing block or mismatch
   prevents the round from becoming reconciled.
7. Credits remain `earned_pending` while the pool waits for TIG's delayed
   payment for that round. A protocol reward entry or claimable amount is a
   receivable, not spendable custody.
8. `ASSETS_SETTLED` requires that round's member share and unresolved suspense
   to be available in the pool's finalized member custody — §8.3a's member leg
   complete from its own finalized token event. The round's pool fee goes to
   operating custody on the other leg and gates nothing here.

   A wall-clock estimate, including the expected several-week delay, is never
   settlement evidence.
9. Settlement credits each eligible positive member amount to that member's
   single balance (§11.7). It does not start an outbound transfer: withdrawal
   is member-initiated under §12, for unencumbered balance only.

The two-descendant rule is for internal TIG history. On-chain Base receipt
finality is separately defined in section 12.

## 5. Pool-fee policy

The approved initial public fee is `200` basis points (`2%`). Testnet uses `0`
bps because its purpose is validation, not revenue.

For an ordinary attributed block:

```text
fee_atoms[b] = floor(pool_proceeds_atoms[b] * fee_bps[b] / 10_000)

distributable_atoms[b] = pool_proceeds_atoms[b] - fee_atoms[b]
```

One basis point is `0.01%`. Fee calculation is exact integer arithmetic and
always rounds the fee down, leaving any sub-atto fractional remainder with the
members' distributable amount.

Fee configuration is an append-only policy table:

```text
fee_policy_id
fee_bps
effective_from_tig_height
announced_at
created_by / approved_by
reason
supersedes_policy_id
```

Each block stores the selected policy ID and bps. The policy with the greatest
`effective_from_tig_height <= block.height` is used. A policy cannot be edited,
backdated, or selected using processing time. A change is scheduled no earlier
than the next TIG round boundary after at least seven days' public notice,
converted to blocks from live `seconds_between_blocks`. Changing the percentage
later therefore adds a new policy row; it never edits or reinterprets an
earlier block.

The pool fee is recognized only when the block attribution posts. If a block is
in suspense, its full proceeds remain a liability and no fee revenue is taken
until resolution.

## 6. Member weights and exact allocation

For block `b`, the authoritative integer weight is the stored number of
pool-attributed qualifiers from [mining_system.md](mining_system.md):

```text
q[m,b] = raw attributed qualifier count for member m
Q[b]   = sum(q[m,b])
D[b]   = distributable_atoms[b]
```

The batch must first prove that stored bundle selections aggregate to TIG's
published pool qualifier count for every challenge and track. A failure puts
the block into suspense.

For `Q > 0`, allocate with the largest-remainder method:

```text
numerator[m]      = D * q[m,b]
base[m]           = floor(numerator[m] / Q)
fraction_rank[m]  = numerator[m] mod Q
R                 = D - sum(base[m])
```

Give one additional attoTIG to exactly `R` members, ordered by:

1. greater `fraction_rank` first;
2. then smaller `BLAKE3(canonical_encode([
   "tig-pool-payout-remainder-v1", network, block_id, member_id]))`; and
3. then canonical `member_id` if the hashes collide.

`R` is less than the number of positive-weight members, so no member receives
more than one remainder atom. This makes the allocation exact:

```text
sum(member_credit_atoms[m,b]) == D[b]
```

There is no unallocated payout dust. The batch stores `q`, `Q`, `D`, numerator,
base, remainder rank, extra-atom flag, and final credit so the result is
independently reproducible.

## 7. Zero-qualifier and mismatch suspense

If pool proceeds are positive but any of these is true:

- `Q == 0`;
- per-challenge/track attribution does not reconcile;
- the pool coinbase entry is absent or inconsistent;
- a required block is missing;
- the block uses an unknown fee policy; or
- required numeric input is malformed or out of range,

the accounting projector posts no member allocation and no fee. It posts the
entire proceeds to `payout_suspense` with one stable reason and alerts.

Suspense is block-specific. It is never:

- distributed to miners from a later block;
- converted into pool revenue because time passed;
- divided using guessed weights; or
- silently removed from liabilities.

If the missing facts become available, a resolution batch debits that block's
suspense and applies the fee policy and member weights that were effective for
the original block. If attribution is proven permanently unrecoverable, release
or disposition requires a separately approved operator command and published
member policy; the default is to retain suspense indefinitely.

## 8. Ledger model and accounts

The ledger is append-only, double-entry, and denominated in one network's
attoTIG. A batch header contains source, idempotency key, TIG block/round,
policy/config versions, evidence hashes, status, and optional correction link.
Lines contain one account, debit or credit atoms, member when applicable, and a
stable line number.

For every posted batch:

```text
sum(debit_atoms) == sum(credit_atoms)
all line amounts > 0
```

Minimum account classes are:

```text
ASSET:TIG_REWARD_RECEIVABLE                    the claim on TIG
ASSET:TIG_REWARD_WALLET                        the benchmarker wallet
ASSET:TIG_MEMBER_CUSTODY                       the one member pot
ASSET:TIG_OPERATING_CUSTODY                    pool funds
LIABILITY:MEMBER_EARNED_PENDING:<round>:<member_id>
LIABILITY:MEMBER_BALANCE:<member_id>           deposits and settled earnings
LIABILITY:MEMBER_WITHDRAWAL_PENDING:<member_id>:<generation>
LIABILITY:PAYOUT_SUSPENSE:<block_id>
REVENUE:POOL_FEE
REVENUE:TIER_JOINING_FEES
REVENUE:FAILURE_CHARGES                        §11.6's chargeable `X`
EQUITY:SECURITY_LOSS_RESERVE               finalized slashes; restricted use
EXPENSE:ACCOUNTING_LOSS                    explicit approved correction only
```

Three addresses with three roles, and three ledger assets. None silently backs
another.

- **The reward wallet** is the pool's TIG benchmarker identity. TIG mints the
  pool's round rewards to it, and it holds nothing else: §8.3a sweeps each
  settled round out of it. Its key is the pool's protocol identity
  (`architecture.md` §2.2): it signs no transfer to a member and to no address
  other than member and operating custody, an operator signs §8.3a's sweep
  manually, and no runtime process holds it.
- **Member custody** is the single pot ADR 0008 decided on. Every member's
  deposits and settled earnings are here, whether they are collateral,
  withdrawable, or frozen — the ledger says which, and the tokens do not move
  when the answer changes.
- **Operating custody** holds the pool's own money: the fee, tier fees, and
  the security loss reserve's realized value.

`MEMBER_BALANCE` is one liability per member: deposits under §11.2 and settled
earnings under §8.4 are the same balance. Encumbrance is a computed property of
it, defined once in §11.7 — three terms, not two — and not a second account, so
no batch can move value between "collateral" and "earnings" because there is
nowhere to move it to, and no *transfer* corresponds to the distinction either.

### 8.1 Ordinary attributed block

```text
Debit   ASSET:TIG_REWARD_RECEIVABLE                         pool_proceeds
Credit  REVENUE:POOL_FEE                                    fee
Credit  LIABILITY:MEMBER_EARNED_PENDING:<round>:<member>    each exact member credit
```

### 8.2 Suspense block

```text
Debit   ASSET:TIG_REWARD_RECEIVABLE                 pool_proceeds
Credit  LIABILITY:PAYOUT_SUSPENSE:<block>           pool_proceeds
```

### 8.3 Protocol asset settlement

When the TIG payment attributable to a reconciled round is finalized in the
pool's reward wallet — the benchmarker identity TIG mints to:

```text
Debit   ASSET:TIG_REWARD_WALLET
Credit  ASSET:TIG_REWARD_RECEIVABLE
```

Settlement is never inferred merely from the internal reward API or a claimable
display. The round, on-chain event/transaction, amount, token, chain, block, and
custody address are stored as evidence. If TIG pays multiple rounds in one
transfer, the cumulative payment must reconcile exactly before the oldest fully
funded round advances; funds are never assigned by a guess.

### 8.3a Sweeping a settled round out of the reward wallet

The reward wallet is a collection point, not custody. Once §8.3 records a
round's payment, it is swept out in **two transfers** — one per destination
address, because an ERC-20 transfer has exactly one recipient:

```text
member leg      Debit   ASSET:TIG_MEMBER_CUSTODY
                Credit  ASSET:TIG_REWARD_WALLET

operating leg   Debit   ASSET:TIG_OPERATING_CUSTODY
                Credit  ASSET:TIG_REWARD_WALLET
```

Two intents, two signatures, two finalized events, two completion batches.
Each posts only from its own event, so neither depends on the other having
landed and a lost broadcast on one leg does not strand the other.

**The member leg carries two things**, split the way §8.1 and §8.2 already
split the liability: the round's member share, and the round's unresolved §8.2
suspense. A round containing a suspense block has a third liability, and
sweeping only the member share and the fee would strand that value in the
reward wallet — under the protocol identity key, which is the one place this
design exists to keep member value away from. Suspense proceeds go to member
custody because that is where most of them end up: §7's resolution credits
members, which moves a liability and no tokens.

Not *all* of them, though. §5 defers a suspense block's pool fee until
resolution, so a member-crediting resolution also recognizes that fee — pool
revenue whose tokens are sitting in member custody. It leaves through §8.6 like
any other pool value, which is why that section's suspense cause covers the
pool's share of **any** resolution, fee or full award. A resolution that awards
everything to the pool is the same path with a larger share. No
operating-to-member transfer is ever needed.

**Of the two legs, only the member leg gates settlement**; the operating leg is
the pool's own money and delays no member. §12.1 lists the full settlement
conditions, of which this leg's completion is one.

The sweep exists so the pool's TIG protocol identity is never also the wallet
member funds are paid out of. `architecture.md` §2.2 keeps the reward wallet's
key out of every runtime process; a withdrawal signed from that wallet would
put it in one, and a stolen key would then take the member funds and the
pool's TIG identity together.

**Who signs them.** The accounting projector creates both intents, keyed
`(network, round, leg)` where `leg` is `member` or `operating`. The
**operator** signs and broadcasts each manually with the offline reward-wallet
key — the Funds Gateway never holds that key, and neither does any other
runtime process. The controller then posts each completion from its own
finalized exact token event, as it does for every other transfer.
`architecture.md` §6 names that division; §2.2 and §9 record that the key
signs these sweeps as well as API-key provisioning.

The per-leg key guarantees **one completion batch per leg**, not one on-chain
transfer: it is a ledger key, and the transfer is signed by hand. So the
operator discipline is the same one §12.4 requires of the Funds Gateway, and
`tig_integration.md` §10 requires of every ambiguous write — before re-signing
or re-broadcasting a leg whose result was ambiguous, reconcile the previous
attempt by signer nonce, transaction, receipt and exact token event. A
re-observed payment cannot produce a second completion batch; only that
reconciliation can stop it producing a second transfer.

### 8.4 Settling a round into member balances

Once a round is fully reconciled and funded, every eligible positive member
amount moves from that round's earned-pending account into the member's single
balance (§11.7) in one batch:

```text
Debit   LIABILITY:MEMBER_EARNED_PENDING:<round>:<member>
Credit  LIABILITY:MEMBER_BALANCE:<member>
```

The credit is unencumbered and immediately withdrawable. It does not create a
transfer intent — under ADR 0008 settlement no longer starts a payout — and it
is not yet collateral-eligible: §11.7's maturity rule governs when it can be
reserved under §11.4.

This is a distinct recognition path from §11.2's inbound deposit, with
different evidence: the reconciled round rather than a finalized transfer
event. Neither path may record the other kind of value.

The batch posts only after §8.3a's **member leg** completes, so the balance it
credits is covered by member custody at the instant it exists. The operating
leg is the pool's own money and gates nothing here. Maturity changes
nothing about where the tokens are (§11.7): it decides only whether the
balance may back work.

### 8.5 Funding a member withdrawal

A member-initiated withdrawal moves the requested unencumbered amount to a
withdrawal-pending account in one batch, which creates one immutable transfer
intent:

```text
Debit   LIABILITY:MEMBER_BALANCE:<member>
Credit  LIABILITY:MEMBER_WITHDRAWAL_PENDING:<member>:<generation>
```

Both accounts are backed by the same custody, so this batch moves no tokens
and needs no transfer to precede it — which is the whole benefit of one pot.
The intent it creates is for the outbound transfer to the member, and §12.5's
confirmation is what finally debits member custody.

A missing, invalid or held destination leaves the amount in `MEMBER_BALANCE`;
no intent is created and no other member is affected.

### 8.6 Sweeping pool value out of member custody

When member value stops being a member liability, the tokens follow it out of
the one pot:

```text
Debit   ASSET:TIG_OPERATING_CUSTODY
Credit  ASSET:TIG_MEMBER_CUSTODY
```

The causes, each with its own cause identifier:

| Cause | Identifier |
|---|---|
| §11.3's tier joining fee | the tier activation |
| §11.6's chargeable failure charge `X` | the charge decision |
| A finalized slash (§11.2) | the finalized slash |
| The pool's share of a §7 suspense resolution — the §5 deferred fee, or a full award | `(network, block_id, resolution_generation)` |
| A §10 correction that moves member value to a pool account | the correction ID |

`sweep_cause_id` in §9 is whichever of these applies. The
`X` charge is listed separately from a slash on purpose: §11.6 and
`mining_system.md` §8 both refuse to describe a chargeable tier failure as
fraud, and sweeping it under a slash's identifier would record it as one. Its
credit side is `REVENUE:FAILURE_CHARGES` (§8), distinct from the pool fee and
from the loss reserve, so §13 item 12's margin term for un-swept charges is
computable from the ledger rather than inferred.

These are the **only** transfers out of member custody other than a member
withdrawal, and their destination is allow-listed to operating custody. There
is no transfer in the other direction and no internal rebalancing at all:
under one pot a member's value never moves because its collateral status
changed, only because it left the pool.

Completion posts only from a finalized exact token event, and each sweep is
keyed to its cause, so a retry after an ambiguous broadcast cannot sweep the
same value twice.

## 9. Idempotency and immutable batches

Uniqueness keys are:

```text
ordinary block batch  (network, block_id)
suspense resolution   (network, block_id, resolution_generation)
correction batch      correction_id
round reconciliation  (network, round)
asset settlement      (network, settlement_source_id)
withdrawal intent     (network, member_id, withdrawal_generation)
reward wallet sweep   (network, round, leg)      leg: member | operating
operating sweep       (network, sweep_cause_id)   §8.6's cause table
on-chain transfer     (chain_id, signer, transaction_nonce)
journal line          (batch_id, line_number)
```

An ordinary block can have exactly one original batch, whether attributed or
suspense. Retries return that batch. Every state transition, line set, and
outbox event commits in one transaction. Posted lines are immutable: no UPDATE
or DELETE privilege is granted to the application roles.

Balances are projections of journal lines and may be rebuilt. A cached balance
is never more authoritative than the journal. A transaction locks one member's
balance and their recorded withdrawal requests before creating a withdrawal
intent (§12.3), so two concurrent requests cannot each pass §12.1's
unencumbered gate for the same value.

## 10. Corrections

A correction is a new balanced batch containing:

```text
correction_id
original_batch_id
reason_code and evidence
detected_at
created_by and approvals
reversal/replacement line set
member-visible explanation
```

The normal pattern fully reverses the incorrect original lines, then posts the
correct replacement in the same correction batch. Partial adjustments are
allowed only when their derivation is equally reproducible.

If a correction reduces a member balance:

1. consume `earned_pending` first;
2. then consume unencumbered `MEMBER_BALANCE`;
3. then consume balance held by a **recorded withdrawal request that has not
   yet posted**, reducing or cancelling that request in the same transaction
   so the encumbrance and the balance move together;
4. then consume **posted** `MEMBER_WITHDRAWAL_PENDING` whose transfer is not
   yet signed;
5. never alter an already signed, broadcast, or finalized transfer, and never
   consume balance encumbered by a §11.4 reservation or a §11.6 freeze — that
   value is held against an open exposure and taking it would silently shift
   the pool's own error onto the cover for a member's work. Rules 3 and 4 are
   the deliberate exceptions: nothing has left the pool and the member has not
   yet been paid, so the two withdrawal states are reachable while a
   reservation and a freeze are not;
6. if the member has already been paid too much, record an explicit member
   receivable/negative future-earnings balance, stop new withdrawal intents
   and work, notify the member, and require an operator resolution; and
7. never charge other members or a future block silently for the shortfall.

Writing off a shortfall to `EXPENSE:ACCOUNTING_LOSS` requires an explicit
approved correction and does not relabel it as mining expense or payout dust.

## 11. Member balance and custody separation

The system recognizes two unrelated things that must never share a field,
balance, custody address, or permission calculation.

### 11.1 Non-custodial TIG delegation to the pool

The member links a TIG/Base player address by signing a domain-separated pool
challenge. The pool recognizes only the amount in a confirmed TIG deposit that
is currently delegated to the pool's Benchmarker identity.

This amount is:

- controlled by the member under TIG's own deposit contract and withdrawal
  delay;
- not an asset or liability of the pool and not posted to the pool ledger;
- never described as pool-held collateral;
- not slashable by the pool; and
- worth zero security-deposit units and therefore grants no member-work
  capacity by itself.

Delegation benefits the TIG Benchmarker under TIG's rules and may earn the
delegator TIG's protocol-level delegator reward. Those rewards are not internal
member mining payouts. The pool's per-block payout base already excludes the
amount TIG shares with delegators. TIG documents that delegated deposits remain
member-controlled and require an unlock waiting period; see
[TIG deposit guidance](https://docs.tig.foundation/deposits/make-deposits).

The approved initial TIG protocol reward share is `15%`. It is versioned
separately from the pool's `2%` member-mining fee. A later change must satisfy
TIG's live update constraints and is announced as a new effective policy; it
does not reinterpret earlier proceeds or member allocations.

### 11.2 Slashable pool security deposit

A member may transfer TIG to the pool's dedicated member-custody address
(§8, §11.7) at any time. A deposit is one of the two ways matured balance
arises; §8.4's settled earnings are the other, so a member whose matured
balance already covers §11.4 needs no deposit to keep working.

Recognize a deposit only from a transfer event on the allow-listed token and
chain that:

- names a verified member wallet/source and configured custody destination;
- has a unique `(chain_id, tx_hash, log_index)`;
- is included in a Base `finalized` block;
- has an exact positive attoTIG amount; and
- has not been recognized for another member or purpose.

Recognition posts:

```text
Debit   ASSET:TIG_MEMBER_CUSTODY
Credit  LIABILITY:MEMBER_BALANCE:<member>
```

Under ADR 0008 this is one of two recognition paths into the same balance;
§8.4's settlement credit is the other, with different evidence. A deposit is
recognized only from the transfer event above, and a settled earning only from
a reconciled round. Neither may record the other kind of value.

The balance remains completely separate from delegation and from pool
operating funds. They cannot pay another member or silently
cover a pool error. A proposed slash first freezes the disputed amount without
moving the member liability, notifies the member, and records the evidence and
appeal deadline. A final slash is a new audited journal batch authorized only
by the published member-fault policy, with benchmark evidence, amount, policy
version, actor, and appeal result:

```text
Debit   LIABILITY:MEMBER_BALANCE:<member>
Credit  EQUITY:SECURITY_LOSS_RESERVE
```

A slash reaches only encumbered balance: the frozen amount established by
§11.6 against a specific reservation. It can never take unencumbered balance,
which is the member's to withdraw.

The reserve may reimburse documented member-caused TIG fees, penalties, or
accounting losses. It is not ordinary pool-fee revenue, cannot fund member
payouts, and cannot be distributed to an operator merely because a slash
occurred. Any later use is another approved, auditable batch.

Member value is held in one dedicated custody address with its own production
signing boundary — never the pool's reward wallet, whose key is the TIG
protocol identity (§8, ADR 0008).

The pool must publish enforceable member terms and obtain legal review before
accepting these funds. ADR 0008 makes that obligation larger, not smaller:
the pool now holds member value for as long as the member chooses.

### 11.3 Non-refundable tier joining fee

Joining tier `k` costs `J[k]` attoTIG under the policy version effective at the
join transaction. Tier `k` grants eligibility for at most `k` concurrent
unverified benchmarks; it does not purchase guaranteed work or pool capacity.
The fee is separate from delegated TIG and slashable security collateral.

The member may authorize the fee to be taken only from finalized, **matured**
unencumbered balance after every existing reservation, recorded withdrawal
request, and frozen charge.

Activation of the tier and the balanced fee journal batch commit together:

```text
Debit   LIABILITY:MEMBER_BALANCE:<member>
Credit  REVENUE:TIER_JOINING_FEES
```

The corresponding custody value is no longer member backing and is swept out
of member custody under §8.6:

```text
Debit   ASSET:TIG_OPERATING_CUSTODY
Credit  ASSET:TIG_MEMBER_CUSTODY
```

A failed, ambiguous, or unfinalized fee debit never activates the tier.

Tier removal creates no refund. A removed member may immediately buy a tier by
paying its then-current `J[k]` again; there is no cooldown or tier-admission
queue. Every purchase has a new idempotent fee batch and membership period.
Rejoining does not release or reset outstanding benchmarks, reservations,
fines, appeals, pending withdrawals, or method-verification exposure.

### 11.4 Dynamic per-assignment collateral

A fixed deposit per benchmark is unsafe because the direct method-verification
penalty scales with the benchmark's bundle count. The pool must reserve risk
for the proposed assignment before it spends a TIG precommit fee.

For decision snapshot `s` and proposed track settings `t`, define:

```text
B[t]       = proposed num_bundles for track t
P[s]       = live config.reports.penalty_amount at snapshot s
F[s,t]     = exact precommit fee implied by live challenge config and B[t]
X[policy]  = charge reserved for one chargeable failed benchmark
M[m]       = member m's collateral multiplier, default 1 (ADR 0010)

method_reserve[s,t] = P[s] * B[t]
assignment_reserve[m,s,t] = M[m] * method_reserve[s,t] + F[s,t] + X[policy]

precommit_reserve = max(assignment_reserve[m,s,t] for every proposed track t)
```

`M[m]` is set by the pool, never by the member, in the range `0 < M[m] <= 1`,
and is versioned policy in the same append-only form as §5's fee policy. It
scales the method reserve **only**: `F` is an outlay the pool certainly makes
and `X` is a charge it has already decided to levy, so neither is a risk that
trust can discount. The value is read when the assignment reserve is computed
and fixed into that reservation; a later change to `M[m]` never reaches an
open reservation, in either direction. ADR 0010 records the decision, what the
uncovered `(1 - M[m]) * P[s] * B[t]` costs the pool on a slash, and why this
is the trust-label mechanism §14 and `mining_system.md` §11 held open.

The maximum is necessary because TIG selects the track only after the
precommit. The pool atomically reserves that amount from the member's
`eligible_collateral` below before creating the precommit intent. Once
TIG confirms the selected track and exact `fee_paid`, the reservation may be
reduced to that track's exact requirement, never increased by silently applying
a later pool policy.

For illustration only, if `P = 10 TIG` and the fee and `X` are omitted,
the method reserve is `40 TIG` for 4 bundles, `100 TIG` for 10 bundles, and `250
TIG` for 25 bundles. The implementation always reads `P`, fees, bundle counts,
and applicable configuration from the block-consistent decision snapshot; none
of these example values is compiled into admission logic.

For member `m`:

```text
eligible_collateral[m] = matured[m]
                         - matured portion of recorded withdrawal requests
                           not yet posted (§11.7)
                         - frozen charge/slash amounts

reserved_exposure[m]   = sum(open assignment reservations)

new work is allowed only if:
eligible_collateral[m] - reserved_exposure[m] >= precommit_reserve
```

`matured[m]` is the part of §11.7's single balance that may back work:
finalized recognized deposits, plus settled earnings whose round has matured
under §11.7. Unmatured earnings are withdrawable but count zero here, which is
why only the *matured* portion of a request is deducted: spending an unmatured
earning must not cost admission capacity it never contributed.

One reservation remains attached to one member-owned benchmark even after the
member's compute slot is released. Its method portion remains reserved until
the benchmark can no longer generate a method-verification penalty and every
report/arbitration is terminal. Its `X` portion releases when TIG verifies the
benchmark without a chargeable tier failure, or is frozen and charged when a
member-attributable failure is established. This prevents the same TIG from
collateralizing several simultaneous risks.

**When that should be observable.** `tig_integration.md` §14.2 records that an
arbitration for a benchmark from round `R` is published by the end of round
`R + submission_period + 1` — at the owner-confirmed live value of one round,
the end of round `R + 2`. §14.2 marks both the bound and that value as owner
confirmation rather than pinned-source facts, and instructs holding to
terminality for any other `submission_period`.

The bound says when the answer should be **readable**, and nothing more. It is
not a release condition and does not become one: the release condition is the
paragraph above, unchanged, and only an observed terminal arbitration
satisfies it. Passing the bound releases nothing, charges nothing and slashes
nothing — it raises an operator discrepancy under §13 item 16, which stops
rather than compensating with a guess. Fail-closed is the only safe direction
here: releasing a method reserve on a deadline, against a slash that is still
possible, is the pool absorbing a loss it reserved against and cannot recover
from the member.

**Practical scale.** The protocol spike measured testnet
`blocks_per_round = 10080` (`protocol_spike_report.md` §9 item 9), which at a
60-second target is exactly seven days. Round length is live configuration and
may differ from that snapshot, so this is scale rather than a constant: at it,
a benchmark created early in its round can hold its method reserve for close to
three weeks.

That is a capital-efficiency property of the product and not an implementation
detail. A member cannot recycle one deposit from benchmark to benchmark; to
work continuously they must hold enough collateral to cover everything still
inside its window. The admission gate below is where that bites — new work
needs `eligible_collateral - reserved_exposure` to cover the next reservation,
so how *long* a reservation lives caps a member's throughput independently of
which tier they bought.

### 11.5 Configuration changes and residual risk

The pool reads `penalty_amount` every accepted block and records the exact
value and policy used by each reservation. Any change immediately pauses new
precommits until compatibility has been reviewed and requirements recomputed.

`tig_integration.md` §14.1 determined that the governing configuration is the
one live when the penalty is applied, so a `penalty_amount` increase **can
apply retroactively** to an already open benchmark. No formula based only on
the assignment block can therefore guarantee full collateral. The spike's own
conclusion, recorded in `protocol_spike_report.md` §7 finding 7, was that a
risk buffer is required.

**The owner has stated a different intent: carry the exposure rather than
buffer it**, on the expectation that `penalty_amount` will not change without
several weeks of public notice, and that the pause above — which stops new
precommits the moment the value changes — bounds what is exposed to
benchmarks already open when a change lands.

This is recorded as the owner's position on the exposure, not as a derived
result. §14's collateral hold has since been lifted — ADR 0010 settled the
§11.3–§11.5 policy as §11.4 with a per-member multiplier — but that settles
the *formula*, not the two premises below, and it does not make this position
into a proof. One precondition for accepting public member collateral
therefore survives the lifting of §14's hold: the penalty **basis** must be
established, per
[member_attack_model.md](member_attack_model.md)'s section on the same
formula. A settled formula over an unverified basis is not a settled reserve.
`CLAUDE.md` reserves security-deposit decisions to an explicit human decision,
and this section records one rather than deriving it.

Two premises it rests on are **not established**, and the position is taken
knowingly rather than derived from them:

- how long an open benchmark remains exposed is **partly** known now, and the
  part that matters here still is not. §14.2 records, as owner confirmation
  rather than a pinned-source fact, that an arbitration for a benchmark from
  round `R` is published by the end of round `R + submission_period + 1`, so
  the *reporting and arbitration* leg is bounded — §11.4 states the scale. That
  leaves §14.1's leg open: it cannot exclude a charge block later than the
  arbitration block, so the interval from arbitration to TIG applying a penalty
  is still unmeasured. "Several weeks of notice exceeds the horizon" therefore
  compares against a horizon that is now bounded at one end and open at the
  other, which is better than unmeasured and is not enough to settle the
  position; and
- the notice expectation is an observation about how TIG has behaved, not a
  property of the protocol, and nothing in the pinned source guarantees it.

If `penalty_amount` rose with less notice than the horizon of the benchmarks
then open, the pool absorbs the difference between what was reserved and what
is charged. It is not recoverable from the member: the reservation is that
member's whole committed exposure, and §11.4 never increases a reservation
after the fact — neither recognition path into the balance, §11.2's transfer
or §8.4's settlement, enlarges one that already exists.

Should either premise fail, this is the section to revisit, and the
mechanism to add is a buffer or an additional-collateral call — never a
retroactive slash.

### 11.6 Failure charges, method losses, and tier effects

The settled boundaries are:

- for method verification/report penalties caused by member-produced data,
  slash the exact TIG penalty attributed to that benchmark after the evidence
  and appeal process, up to its reserved method amount;
- for TIG-verified work that merely earns no qualifiers, charge zero;
- for zero bundles meeting TIG's minimum verification quality, charge `X`
  under the tier policy without describing the outcome as fraud;
- for `POOL`, `TIG`, or `UNRESOLVED` fault attribution, slash zero; and
- multiple failing benchmarks use their own reservations and batches; one
  benchmark never consumes another benchmark's reserve silently.

**Freeze on report, resolve on arbitration.** A method report is an
accusation, not a finding: `ArbitrationDetails` resolves to
`NONREPRODUCIBLE`, `REPRODUCIBLE` or `INCONCLUSIVE`. A report is observable
before its arbitration resolves, and the pool acts on that earlier signal
without treating it as proof. How much earlier is not stated here: reports
against a benchmark can still arrive until the end of round
`benchmark_round + submission_period`, and `tig_integration.md` §14.2 pins
that unit as rounds while its value stays live configuration.

On observing a report against a member-owned benchmark, the pool freezes that
benchmark's reserved method amount, holding it against the reported outcome
instead of releasing it when the benchmark would otherwise stop being able to
generate a penalty.

**When the arbitration should be readable.** `tig_integration.md` §14.2
records that an arbitration for a benchmark from round `R` is published by the
end of round `R + submission_period + 1`. That bounds when the pool should be
able to *read* an answer. It does not bound the freeze.

A freeze is resolved by an observed terminal arbitration and by nothing else.
Past the bound the freeze persists under the release condition below, and the
breach is raised as an operator discrepancy under §13 item 16 — a mismatch
stops and alerts rather than creating a compensating guess. The passage of a
deadline never releases, charges or slashes anything.

That direction is deliberate. The bound is owner confirmation and not a
pinned-source fact, so acting on it as though it were would risk releasing a
method reserve against a slash that is still possible — a loss the pool
reserved against and cannot recover from the member. Holding costs the member
liquidity and is visible and correctable; releasing early costs the pool money
and is neither.

The bound also covers **arbitration publication only**. It says nothing about
the pool's own attribution and appeal process, which §11.6 and
`mining_system.md` §8 govern, and nothing about when TIG applies a penalty —
`tig_integration.md` §14.1 cannot exclude a charge block later than the
arbitration block.

The freeze keeps that amount inside `reserved_exposure` in §11.4; it does not
also become a `frozen charge/slash amount`. The distinction is not
presentational: counting it in both terms would deduct one outcome from
admission capacity twice, which §13 item 19 forbids. Because the amount was
already reserved against this benchmark, the member's admission capacity is
exactly what it was before the report. The freeze does not slash, and does not
act on the member: no suspension, no effect on admission, and no reach beyond
the reported benchmark's own reservation.

**A `NONREPRODUCIBLE` arbitration does not by itself slash anything.** It is
the trigger for the ordinary evidence, attribution and appeal process, and the
outcome of that process decides. The bullets above still govern: a slash
follows only where the penalty was "caused by member-produced data", and
`POOL`, `TIG` or `UNRESOLVED` fault attribution slashes zero. A benchmark can
arbitrate `NONREPRODUCIBLE` for a pool-side reason — `mining_system.md` §8
names pool-constructed wrong proofs, packages the pool corrupted after durable
acceptance, and work whose origin was never established — and charging a
member's deposit for those would invert the rule this section exists to state.
On `REPRODUCIBLE` or `INCONCLUSIVE`, and on any adverse arbitration not
attributed to the member, that report is settled and the member is charged
nothing for it.

Settling a report is not the same as releasing the reservation. The freeze
lifts, but the method portion goes on being held under §11.4's ordinary
condition — until the benchmark can no longer generate a
method-verification penalty and *every* report and arbitration against it is
terminal. A benchmark that survives one report is still reportable, and a
second report may already be open, so releasing collateral on the first
non-adverse outcome would leave the remaining exposure uncovered. What the
non-adverse outcome ends is the charge, not the cover.

The scope is deliberately this narrow, and the narrowness is what makes acting
on an unproven accusation defensible at all. Reports are public and cost only
`ReportsConfig.submission_fee`, so a third party can file them against the
pool's honest work. Because the only effect is holding a reservation that was
already reserved against this benchmark, a released freeze leaves an
honestly-behaving member exactly where they started — no lost capacity, no
charge — while each false report still costs its filer a fee. That is judged
sufficient and no further countermeasure is specified.

A member-level response would not have that property, and is ruled out
elsewhere: `mining_system.md` §8 holds that an unresolved incident penalizes
nobody. Bounding a member's total exposure across concurrent work is §11.4's
job, through `reserved_exposure`, and does not need this rule to reach further
than one benchmark.

For every chargeable tier failure attributed to a member, freeze and then
charge exactly `X` under the assignment's policy version:

```text
Debit   LIABILITY:MEMBER_BALANCE:<member>
Credit  REVENUE:FAILURE_CHARGES
```

`REVENUE:FAILURE_CHARGES` is its own account, not the pool fee and not the
loss reserve. §8.6 sweeps its tokens out of member custody under the charge
decision's own identifier, and §13 item 12 counts what is charged but not yet
swept as a named margin term — both of which need this credit side to exist
before they can be computed. V0 chargeable tier
failures are an abandoned or unusable package, TIG solution-verification
failure, and a benchmark with zero bundles meeting TIG's minimum verification
quality. The last outcome is a capacity/economic failure, not an allegation of
fraud. Slow but eventually correct work is not charged `X`; it is handled by
the round `unverified_exposure > verified_exposure` tier-removal rule.

For tier `k`, if the round's chargeable failure count `f > k`, remove the tier
at round close after recording the ordinary `f * X` charges. Method-report
losses use the separate bundle-scaled rule above and are not charged `X` a
second time unless an independently evidenced tier-failure outcome also
occurred. `POOL`, `TIG`, compatibility, and `UNRESOLVED` outcomes charge zero
and do not increment `f`.

After a charge or removal, the ordinary admission formula prevents more work
unless enough finalized, unreserved collateral remains for every existing
exposure and the next assignment. A newly paid tier fee never bypasses that
gate. The complete evidence and outcome inventory is in
[member_attack_model.md](member_attack_model.md).

A proposed `X` charge freezes only the evidenced amount and notifies the member;
it does not by itself create a first-failure ban. The reduced eligible
collateral may still prevent further admission. A proposed method-loss slash
may additionally impose the separate method/security suspension. Both provide
a seven-day appeal. An undisputed proposal becomes final after that deadline.
A dispute remains frozen until a reviewer who did not make the original fault
decision records a reasoned result. Pool/TIG fault or insufficient evidence
releases the freeze; custody is not evidence of member fault.

A withdrawal request immediately removes from admission collateral the
**matured portion** §11.7's draw order assigns to it, and encumbers its whole
amount against withdrawal. The two figures differ whenever a request draws on
unmatured earnings, which never counted as collateral.

It is payable only after all affected reservations are released,
the last relevant benchmark is no longer reportable, every report/arbitration
is terminal, and one additional TIG round has passed. A pending appeal keeps
only the disputed amount locked.

Under ADR 0008 there is one outbound member path, so a withdrawal may combine
matured balance and unmatured earnings; what it may never include is
encumbered balance. §11.7's draw order decides the split: unmatured earnings
first, then matured balance. Unmatured earnings are subject to none of the
waits above — nothing was ever reserved against them — so a member whose
request fits inside them is paid without waiting. Every withdrawal uses the
verified member wallet under §12.2.

Tier fees, failure charges, collateral formulas, concurrency rules, slash
rules, and effective TIG heights are append-only policy versions. A later
policy does not change the amount that can be charged for an earlier assignment
or tier purchase.

### 11.7 One member balance

ADR 0008 settles what this section previously held open. A member has **one
balance** with the pool. §11.2's recognized deposits and §8.4's settled round
earnings are the same liability, `LIABILITY:MEMBER_BALANCE:<member>`;
§11.4 reserves against it and §12 withdraws from it.

**Maturity.** A settled earning is withdrawable at once but is not
collateral-eligible until it can no longer be destroyed by a method penalty.
Until then the same TIG would be covering the penalty that could take it, so
counting it as collateral would be counting nothing.

"Can no longer be destroyed" is the same window §11.6 freezes on, so the same
bound says when the answer should be readable: `tig_integration.md` §14.2 puts
an arbitration for a benchmark from round `R` at the end of round
`R + submission_period + 1` at the latest.

Maturity is still reached only when every report against every contributing
benchmark is **observed** terminal. The bound is not a maturation schedule and
an earning does not mature by the clock — a round still unmatured past the
bound is an alertable discrepancy under §13 item 16, exactly as an outstanding
freeze is. What the bound gives a member is an expectation of when their
earnings should become usable as collateral, not a guarantee that they will;
the difference matters because maturing an earning early would let it back
§11.4's admission gate while the penalty that could destroy it is still
live.

The condition is **per contributing benchmark**, not per earning round. A
round's earnings mature only when *every benchmark whose qualifiers were
attributed in that round* has both a closed reporting window — the end of round
`benchmark_round + submission_period`, keyed to that benchmark's own round —
and every report against it terminal.

Keying it to the earning round alone would be wrong, and not rarely. A
benchmark's lifespan is measured in blocks while a round is far longer, so a
benchmark started before a round boundary earns qualifiers attributed after it.
`tig_integration.md` §14.2's `?round=` selects by the *benchmark's* round, so
round-R earnings produced by a round R-1 benchmark would be judged against a
window that had nothing to do with them — and could be called matured, and
reserved against under §11.4, while a report against that very benchmark was
still open.

Maturity is an admission property and nothing else. **No transfer corresponds
to it.** Matured and unmatured value sit in the same custody address, so a
round maturing moves no tokens, changes no asset, and posts no batch — it
changes what §11.4's formula may count. This is the simplification one pot
buys: a member's TIG never moves because its collateral status changed, only
because it left the pool.

How the pool observes those reports is `tig_integration.md` §14.2's, not this
section's: that document owns the TIG reads, and §11.6 already depends on the
same observation for its freeze rule.

A recognized §11.2 deposit is matured on recognition. Nothing about it came
from a round, so no method penalty can reach back and destroy it.

**Encumbrance.** The balance splits into encumbered and unencumbered parts:

```text
matured[m]      = the matured part of LIABILITY:MEMBER_BALANCE:<m>
unmatured[m]    = the rest of it
balance[m]      = matured[m] + unmatured[m]

encumbered[m]   = reserved_exposure[m]
                  + frozen charge/slash amounts
                  + recorded withdrawal requests not yet posted

unencumbered[m] = balance[m] - encumbered[m]
```

`balance[m]` is `MEMBER_BALANCE` alone. A withdrawal that has reached §8.5's
batch has already left that account, so counting `MEMBER_WITHDRAWAL_PENDING`
as balance *and* deducting it as an encumbrance would deduct it twice.

The third encumbrance term is therefore a *recorded request that has not yet
posted*, and it exists for the window §11.6 creates: a request removes its
amount from admission collateral immediately, while §11.6's waits run and
before any batch exists. Without it two requests for the same value would each
pass §12.1's gate. The request itself is durable state with an owner —
`architecture.md` §6 names it — not an intention held in memory.

The term deducts the request's **whole recorded amount**, matured and
unmatured alike. It is what stops a second request claiming value the first
already claimed, and the motivating case of ADR 0008 — a member whose balance
is all unmatured earnings, which the draw order takes first — is exactly the
case where deducting only a matured portion would deduct nothing and let two
requests for the same value both pass §12.1's gate.

The *other* deduction is narrower, and the two must not be confused. §11.4's
`eligible_collateral[m]` deducts only the **matured portion** the draw order
assigns to a request, because its base is `matured[m]` and an unmatured
earning never contributed admission capacity to begin with. Same request, two
questions: how much can still leave (here, the whole amount) and how much can
still back work (there, the matured part).

The matured/unmatured split of a recorded request is re-evaluated whenever the
round it draws on matures, since maturity moves value between the two
questions. A request recorded entirely against unmatured round R encumbers the
same total before and after R matures; what changes is how much of it §11.4
also deducts.

Only unencumbered balance may leave under §12. Only encumbered balance may be
slashed under §11.2. A slash therefore cannot take value a member could have
withdrawn, and a withdrawal cannot take value the pool is holding against an
open exposure. Encumbrance is computed from §11.4, §11.6 and the request
record, not stored as a separate account, so no batch can quietly reclassify
value.

**Backing.** One address backs the whole pot:

```text
ASSET:TIG_MEMBER_CUSTODY  >=  sum(balance[m])
                              + sum(withdrawal-pending amounts)
```

Coverage rather than equality. The pot also holds, briefly and by name: a
round's member share swept under §8.3a but not yet settled by §8.4, unresolved
§8.2 suspense proceeds swept with it, and value that became the pool's — a
tier fee, an `X` charge, a finalized slash, a correction — awaiting §8.6's
sweep. §13 item 12 enumerates the same terms, and daily reconciliation
accounts for each rather than treating it as a mismatch.

The inequality holds at every batch boundary without any in-flight allowance,
because no batch in this document moves value between member custody and
anywhere else except at a finalized token event: §8.3a's inbound sweep, §8.6's
outbound sweep, §11.2's recognized deposit, and §12.5's completed withdrawal.
The batches in between — settlement, reservation, freeze, maturation — move
liabilities inside the one pot.

**What one pot costs, recorded plainly.** The pool's earlier design split
member value across two custody addresses so that a stolen payout key could
not reach collateral and a stolen deposit key could not pay anyone. One pot
gives that up: a compromise of the member-custody signing key reaches every
member's collateral and every member's withdrawable balance at once. ADR 0008
records the decision and what was weighed. What remains, and what §13 and
`architecture.md` §13 invariant 9 now enforce, is the separation that survives:
member value never shares an address or key with pool operating funds, and
never with the pool's TIG protocol identity.

**Withdrawal draw order.** A withdrawal draws unmatured settled earnings
first, oldest round first, and then matured balance. Without a fixed order the
amount and the waits would depend on an implementation's choice, because one
fungible liability carries no record of which atoms were once collateral.
Oldest-round-first also makes "how much of round R's earnings remain"
derivable from the ledger, which is what maturity is evaluated against.
§11.6's waits apply to exactly the matured portion drawn.

**What did not change.** §11.1's delegated TIG is still not pool value and
still grants no capacity. Pool revenue, the security loss reserve, and
operating custody remain distinct from the member balance and from each other.

## 12. Member withdrawals

### 12.1 Settlement and withdrawal eligibility

Two separate gates, because ADR 0008 separated the two events.

**A round settles into member balances** (§8.4) only when:

- every block batch in the round is posted and no unresolved payout suspense
  remains for that round;
- the complete round is reconciled to TIG's round data; and
- the exact corresponding TIG payment is finalized and reconciled in the
  reward wallet (§8.3); and
- that round's §8.3a **member leg** has completed from its own finalized token
  event, so the balances §8.4 credits are covered by member custody at the
  instant they exist.

The expected TIG payment delay may be several weeks. The pool waits for actual
funds, not elapsed time. Settlement is automatic and needs no member action;
what it produces is a balance, not a transfer.

**A withdrawal becomes payable** only when:

- the requested amount is unencumbered under §11.7 at the moment the intent is
  created, and remains so until it is spent. The check excludes *this*
  request's own recorded amount and no other: a recorded request encumbers
  balance against every **competing** request, but a gate that also counted
  the request it is deciding would refuse every withdrawal ever made;
- §11.6's waits are satisfied for the matured portion §11.7's draw order
  assigns to this withdrawal;
- no accounting/security hold affects the member; and
- member, token, chain, and destination configuration remains compatible.

The member chooses when to ask and how much, up to their unencumbered balance.
There is no minimum, no cadence, and no automatic payout. A request above the
unencumbered amount is refused with the available figure, never partially
filled: a partial fill would silently choose an amount for the member, which
is what this model exists to stop doing.

**Payable is not the same as unattended.** The gates above decide whether a
withdrawal may happen at all; ADR 0009's per-member caps decide only whether a
payable one is signed without a human. A withdrawal at or below the
per-transaction cap whose rolling seven-day member total stays at or below the
weekly cap is signed automatically; anything larger keeps `security.md` §3.4's
multi-person authorization and waits for it. Exceeding a cap is never a
refusal and never a partial fill — the request stands and is authorized by a
person. §12.4 carries the signer-side rule.

The pool pays Base gas as an operating cost and the exact TIG liability is not
reduced for gas.

### 12.2 Destination authorization

**The destination is the wallet that authenticated the session** (ADR 0011).
It is not an account setting, not a worker setting, and there is no operation
that changes it. Under ADR 0008 a withdrawal is member-initiated, which made
account compromise the direct route to a member's funds; hardwiring the
destination removes that route rather than guarding it, because a compromised
session can only ever send a member's own money to that member's own address.

The account system must require:

- a Base address proved by a domain-separated EIP-191 or EIP-712 signature
  containing pool domain, chain ID, address, random nonce, purpose, and
  expiry. That address *is* the member identity, so it is not carried
  separately in the signed payload; and
- one-time nonces and exact-domain validation to prevent signature reuse on a
  different pool or chain.

The address-change controls this section previously carried — reauthentication
and phishing-resistant MFA or a passkey for a change, a 48-hour security delay,
and out-of-band notification — are not listed because there is no change
operation to guard. ADR 0011 records what that costs: a member who loses
control of their wallet loses their balance permanently, and with it the
authority `member_protocol.md` §3.3 gives the member account to recover a
worker. The member terms required by `pre_build_checklist.md` §9 must say so
before a member can deposit.

The service never accepts a destination from a worker credential, a
member-supplied field, or an unaudited operator edit. A member can explicitly
request a withdrawal hold. A held destination leaves the amount in the
member's balance and affects no other member.

### 12.3 Deterministic withdrawal intents

One transaction checks the requested amount against §11.7's unencumbered
balance, moves it to `MEMBER_WITHDRAWAL_PENDING` (§8.5), and creates one
immutable withdrawal intent. The intent fixes:

```text
network
member and withdrawal_intent_id
exact attoTIG amount
verified destination
chain ID and TIG token contract
withdrawal policy/config version
```

The unique `(network, member_id, withdrawal_generation)` makes retries return
the same intent. The accounting cohort is one request, not one round: a
withdrawal is no longer part of a round cohort at all. A failed or dropped
transaction remains pending until chain state proves whether that same intent
can be rebroadcast. The amount has already left `MEMBER_BALANCE`,
so a second request cannot spend it — §11.7's `encumbered[m]` deliberately
excludes a posted withdrawal for exactly that reason, and counting it in both
places would deduct the same value twice.

### 12.4 Signing and broadcast

Production signing is the separate private Funds Gateway and key-custody design;
the Pool API, workers, controller, TIG Gateway, database, and CI never receive
the private key. The signer:

- accepts only approved immutable intents of two kinds out of member custody:
  a member withdrawal to that member's verified address, and §8.6's sweep of
  pool value to operating custody. Nothing else leaves member custody. A
  finalized slash is a ledger batch, not a signed intent: it reclassifies a
  liability to equity, and only §8.6's sweep moves its tokens;
- allow-lists the sweep destination to the one operating-custody address and
  to nothing else, so a compromised member-custody signer can reach a member's
  own verified address or the pool, and no third party;
- never holds the reward wallet's key. §8.3a's sweep out of that wallet is
  signed by an operator, manually, with the offline protocol identity key that
  `architecture.md` §2.2 and §6 keep out of every runtime process — the Funds
  Gateway included;
- allow-lists chain ID, token contract, transfer method, destination, and exact
  amount;
- simulates the ERC-20 transfer before signing;
- uses one serialized nonce lane per signing address;
- records transaction nonce and signed transaction hash before broadcast;
- never substitutes a destination or amount during retry; and
- enforces per-transaction, rolling, and hot-wallet limits. Multi-person
  authorization is `security.md` §3.4's rule and is not restated here. For
  member custody it is required for every transfer **except** a member
  withdrawal within ADR 0009's per-member caps: at or below the
  per-transaction cap, with the member's rolling seven-day withdrawn total
  also at or below the weekly cap. Both caps are versioned policy read by the
  signer, not constants in it, and a transfer that exceeds either is signed
  only with authorization. Thresholds for operating custody are unchanged.
  The hot-wallet limit is a capability the signer must have and a value the
  owner has not set: ADR 0009 declines one for v0 and records what that
  accepts. An unset limit is not an absent control — it must be configurable
  and reported, so setting it later is policy rather than a code change.
  One pot means the signer must evaluate these caps against the member's own
  recent history rather than the amount alone — which amounts qualify is
  policy, never an implementer's choice.

An ambiguous broadcast is reconciled by transaction hash, signer nonce, receipt,
and token `Transfer` event. A fee replacement uses the same nonce and exact
transfer call. A new nonce is never used to “retry” an outcome that could have
succeeded.

The pool pays Base gas as an operating cost; no TIG is silently deducted from
the member amount.

### 12.5 Confirmation and completion

The pool uses the configured Base network and verifies `eth_chainId` on every
signer/RPC start: Base mainnet is `8453` and Base Sepolia is `84532`. An
outgoing transfer becomes `CONFIRMED` only when the receipt succeeded, the
expected token `Transfer` event exists with exact signer/destination/amount,
and the containing block is at or below the RPC's `finalized` head.

Base describes L1 batch finality for ordinary L2 transactions as roughly 20
minutes; the pool follows the `finalized` block tag rather than a timer or fixed
confirmation count. See [Base transaction finality](https://docs.base.org/base-chain/network-information/transaction-finality)
and [Base network identifiers](https://docs.base.org/base-chain/quickstart/connecting-to-base).

On confirmation, post:

```text
Debit   LIABILITY:MEMBER_WITHDRAWAL_PENDING:<member>:<generation>
Credit  ASSET:TIG_MEMBER_CUSTODY
```

Operator recovery may hold an unsigned intent, re-run reconciliation,
or rebroadcast the exact signed transaction. It cannot mark a transfer paid
without finalized chain evidence, edit a posted batch, redirect funds, bypass
approval limits, or convert a failure into a new transfer silently. Emergency
controls can disable all new withdrawal-intent creation and signing while
reads, settlement and reconciliation continue — settlement produces no
transfer, so halting withdrawals does not stall the ledger.

## 13. Reconciliation and invariants

Before and after every batch, enforce:

1. one ordinary original batch per TIG block;
2. exact post-sharing pool self-coinbase as proceeds;
3. stored member qualifier weights sum to TIG's published pool count;
4. fee plus member allocations equals proceeds;
5. member allocations sum exactly to distributable atoms;
6. every journal batch balances and every line uses integral attoTIG;
7. no posted line is updated or deleted;
8. suspense remains a liability until an approved resolution batch;
9. cached balances equal a rebuild from journal lines;
10. withdrawal-pending liabilities are covered by member custody, which is
    item 12's check — they are not a separate custody's problem under one pot;
11. delegated TIG, member balance, pool revenue and the security loss reserve
    are distinct and cannot be silently netted, and encumbered balance is
    never spent as unencumbered (ADR 0008);
12. member custody **covers** every member liability (§11.7):
    `ASSET:TIG_MEMBER_CUSTODY >= sum(balance[m]) + sum(withdrawal-pending)`.
    Coverage, not equality. The margin is named, and daily reconciliation
    accounts for each term rather than treating it as a mismatch:
    (a) a round's member share swept under §8.3a but not yet settled into
    balances by §8.4, still `MEMBER_EARNED_PENDING`;
    (b) unresolved §8.2 suspense proceeds swept with it;
    (c) pool value awaiting §8.6's sweep — every cause in §8.6's table,
    including the pool's share of a §7 suspense resolution, which is the §5
    deferred fee even when the resolution credits members.
    No in-flight allowance is needed, because value enters or leaves member
    custody only at a finalized token event;
13. one withdrawal intent spends one liability once, and one sweep per cause:
    `(network, round, leg)` for each leg of a reward-wallet sweep, and §8.6's
    cause identifier — tier activation, `X` charge decision, finalized slash,
    suspense resolution, or correction ID — for an operating sweep;
14. one signed intent fixes chain, token, destination, amount, signer, and
    nonce;
15. only a finalized exact token event completes a withdrawal or a sweep;
16. any mismatch stops posting/transfers and alerts rather than creating a
    compensating guess;
17. a tier activation and its non-refundable fee post atomically;
18. tier removal or repurchase never releases existing financial exposure;
19. the same outcome cannot consume `X` or a method reserve twice under one
    policy reason;
20. settled earnings count zero toward `eligible_collateral` until their round
    has matured under §11.7; and
21. aggregate uncovered method exposure — the sum of
    `(1 - M[m]) * P[s] * B[t]` over every open reservation — is reported, not
    merely derivable. A multiplier below 1 is the pool choosing to stand
    behind a member (§11.4, ADR 0010); the amount it stands behind must be
    visible before a slash lands, not reconstructed after one.

Daily reconciliation compares:

- accepted TIG blocks, per-block proceeds, and round totals;
- reward receivable versus identified TIG settlement;
- Base finalized token balances and transfer events versus the reward wallet,
  member custody, and operating custody assets;
- member balance split by maturity, suspense, and withdrawal-pending
  liabilities versus member custody's coverage inequality (§13 item 12), with
  each named margin term accounted for;
- withdrawal and sweep intents versus signer nonces, transactions, receipts,
  and events;
- aggregate uncovered method exposure (§13 item 21) against the pool's own
  funds, and each member's rolling seven-day withdrawn total against ADR
  0009's weekly cap, so an automated path that has stopped binding is seen;
  and
- ledger cached balances versus a journal rebuild.

## 14. Owner decisions required

The owner has confirmed:

1. `2%` initial public pool fee, changeable by a new round-boundary policy after
   at least seven days' notice; and
2. a `15%` initial TIG protocol reward share for non-custodial delegators,
   adjustable later through a new effective policy; and
3. automatic settlement of each round into member balances after TIG's actual
   delayed payment is finalized in pool custody, with no request, minimum, or
   daily batch rule. Automatic *distribution* was the earlier decision; ADR
   0008 replaced it with member-initiated withdrawal, keeping settlement
   automatic;
4. the pool pays Base ETH gas for member transfers without deducting it from a
   member's earned TIG;
5. one member balance holding both deposits and settled earnings, withdrawable
   by member request up to the unencumbered amount, and held in one member
   custody address separate from the reward wallet and from operating funds
   (§11.7, ADR 0008);
6. **the collateral policy is §11.4 as written** — the method reserve is
   `P[s] * B[t]` with `P` read live from the decision snapshot. `10 TIG` is
   what `P` reads today, not a constant, and the fixed-per-benchmark deposit
   this section previously rejected stays rejected (ADR 0010);
7. a pool-set per-member collateral multiplier `M[m]`, default 1, range
   `0 < M[m] <= 1`, scaling the method reserve only (§11.4, ADR 0010);
8. member withdrawals signed without human authorization at or below
   `10_000 TIG` per transaction and `10_000 TIG` per member per rolling seven
   days, both versioned policy, with multi-person authorization kept for
   everything above and for every other member-custody transfer. The owner
   accepts, for v0, that a compromise of the member-custody signing key takes
   all member funds, and has declined to bound it with a hot-wallet limit
   (§12.1, §12.4, `security.md` §3.4, ADR 0009); and
9. the member's connected wallet is both the account identity and the
   withdrawal destination, unchangeable, with no account recovery
   (§12.2, ADR 0011). This supersedes the earlier confirmation that a changed
   payout address is held for 48 hours with out-of-band notification: there is
   no address change to hold, and the pool holds no contact channel to notify.

The remaining decision is:

1. numerical `X` — the charge reserved for one chargeable failed benchmark. It
   is a term of §11.4's assignment reserve, so a member's exact collateral
   requirement is determined only once it has a value. `mining_system.md` §11
   carries it, with `J[k]` and `internal_pool_unverified_limit`, among the
   values required before full product implementation. The threats and
   unresolved evidence/consequence choices behind the malicious-work side are
   enumerated in [member_attack_model.md](member_attack_model.md).

Decisions 6 and 7 lift this section's former prohibition: an implementation
may accept member collateral under §11.4 and may present it as settled policy.
The public-funds gates in `pre_build_checklist.md` §9 are untouched and still
govern real member money, and testnet continues to exercise deposit fixtures
with explicit fixture policy values.
