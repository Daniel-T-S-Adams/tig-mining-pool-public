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
REVENUE:FAILURE_CHARGES                    §11.6's charge, fee portion
EQUITY:SECURITY_LOSS_RESERVE               §11.6's charge, penalty portion;
                                           restricted use
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

**When the batch may post.** Two conditions, and the later one governs:

- the round's own slashing is complete — **every benchmark whose qualifiers
  were attributed in that round** has a closed reporting window and an
  observed terminal arbitration for every report against it, so every charge
  against the round is known. The condition is per contributing benchmark and
  keyed to that benchmark's own round, never to the earning round: a benchmark
  started before a round boundary earns qualifiers attributed after it, and
  `tig_integration.md` §14.2's `?round=` selects by the benchmark's round. It
  is also satisfied by observation, never by the clock — §14.2's bound says
  when an answer should be readable, and a round still unsettled past it is a
  §13 item 16 discrepancy rather than a licence to credit. §11.7 owns both
  points; and
- the exact corresponding TIG payment is finalized and reconciled in the
  reward wallet, and §8.3a's member leg has completed.

The first is what makes a charge a *deduction* rather than a reversal: the
round is never distributed and then clawed back, because nothing is credited
until every penalty against it is settled. §9's batches stay immutable, and no
settled round is ever reopened. The second is unchanged and is why the pool
never advances its own capital.

The owner states that the payment lands later than the arbitration window, so
the second condition is expected to bind in practice — owner confirmation,
recorded as such, and not something the pinned tree establishes. Both are
required regardless: only the pair is safe under any configuration, and a
payment arriving sooner than arbitration would otherwise credit rewards that a
later charge still has to reach.

The credit is unencumbered, immediately withdrawable, and **immediately
collateral-eligible**. It does not create a transfer intent — under ADR 0008
settlement no longer starts a payout.

This is a distinct recognition path from §11.2's inbound deposit, with
different evidence: the reconciled round rather than a finalized transfer
event. Neither path may record the other kind of value.

The batch posts only after §8.3a's **member leg** completes, so the balance it
credits is covered by member custody at the instant it exists. The operating
leg is the pool's own money and gates nothing here.

**This replaces the maturity mechanism.** ADR 0008 rule 2 credited earnings
immediately and then withheld collateral eligibility until the producing round
could no longer be penalized, which needed a maturity flag, a withdrawal draw
order, and a separate invariant to enforce. Waiting to credit at all collapses
those: everything in a member's balance is collateral-eligible by
construction, because nothing arrives until the round that produced it is
settled. ADR 0008 stays as written under the immutability rule; ADR 0012
records the replacement.

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

A held destination leaves the amount in `MEMBER_BALANCE`; no intent is created
and no other member is affected. It can no longer be missing or invalid: under
ADR 0011 the destination is the wallet that authenticated the session (§12.2).

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
| §11.6's charge, penalty portion and fee portion alike | the charge decision |
| The pool's share of a §7 suspense resolution — the §5 deferred fee, or a full award | `(network, block_id, resolution_generation)` |
| A §10 correction that moves member value to a pool account | the correction ID |

`sweep_cause_id` in §9 is whichever of these applies. **A charge and a slash
are no longer two causes**, because under §11.6 they are no longer two events:
one table produces one batch, whose penalty portion credits the loss reserve
and whose fee portion credits `REVENUE:FAILURE_CHARGES`. Listing both a charge
decision and a finalized slash would put two identifiers on one batch, and
§13 item 13 allows one sweep per cause precisely so a retry after an ambiguous
broadcast cannot sweep the same member-custody value twice under the other
name.

The two credit accounts stay distinct so §13 item 12's margin term for
un-swept charges is computable from the ledger rather than inferred, and so
the pool's recorded revenue never includes TIG value it lost. What does not
follow from the split is a second cause.

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
(§8, §11.7) at any time. A deposit is one of the two ways collateral-eligible
balance arises; §8.4's settled earnings are the other, so a member whose
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
cover a pool error. A proposed charge first freezes the evidenced amount
without moving the member liability, and records the evidence and the outcome
it rests on. There is no notice to deliver and no appeal deadline to run:
§11.6 makes a charge independent of fault, so there is nothing for the member
to contest in-system, and ADR 0011 leaves the pool no channel to notify them
on in any case. A member who believes a charge was wrong contacts the pool
out of band. A final charge is a new audited journal batch, with benchmark
evidence, amount, policy version and actor:

```text
Debit   LIABILITY:MEMBER_BALANCE:<member>
Credit  EQUITY:SECURITY_LOSS_RESERVE        the penalty portion
Credit  REVENUE:FAILURE_CHARGES             the fee portion
```

**One charge, one cause identifier, two credit lines.** §11.6's table produces
a single amount with two components, and they replace different things: the
penalty portion replaces TIG value the pool lost, which is what the loss
reserve records, and the fee portion reimburses an outlay the pool made, which
is what `REVENUE:FAILURE_CHARGES` records. Splitting the credit keeps both
accounts meaning what they meant.

What must not split is the **cause**. §8.6 sweeps under one identifier — the
charge decision — for the whole batch, because §13 item 13 allows one sweep
per cause and a retry must not be able to sweep the same value twice. Treating
one charge as two causes would also make §13 item 12's charged-but-unswept
margin uncomputable from the ledger. Either component may be zero: a benchmark
that produced nothing is charged only the fee portion.

A charge reaches only encumbered balance: the frozen amount established by
§11.6 against a specific reservation. It can never take unencumbered balance,
which is the member's to withdraw.

The reserve may reimburse documented TIG fees, penalties, or accounting
losses arising from a member-owned benchmark, whatever caused them — §11.6
charges without attributing a cause, so restricting the reserve to
*member-caused* losses would leave a pool-caused penalty funding a reserve
with no authority to absorb it. It is not ordinary pool-fee revenue, cannot fund member
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

The member may authorize the fee to be taken only from finalized unencumbered
balance, after every existing reservation, recorded withdrawal request, and
frozen charge.

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
fines, pending withdrawals, or method-verification exposure.

### 11.4 Dynamic per-assignment collateral

A fixed deposit per benchmark is unsafe because the direct method-verification
penalty scales with the benchmark's bundle count. The pool must reserve risk
for the proposed assignment before it spends a TIG precommit fee.

For decision snapshot `s` and proposed track settings `t`, define:

```text
B[t]       = proposed num_bundles for track t
P[s]       = live config.reports.penalty_amount at snapshot s
F[s,t]     = exact precommit fee implied by live challenge config and B[t]
M_bps[m]   = member m's collateral multiplier in basis points,
             integer, 1..=10_000, default 10_000 (ADR 0010)

method_reserve[s,t] = P[s] * B[t]

scaled_method_reserve[m,s,t] =
    ceil(method_reserve[s,t] * M_bps[m] / 10_000)

assignment_reserve[m,s,t] =
    scaled_method_reserve[m,s,t] + F[s,t]

precommit_reserve = max(assignment_reserve[m,s,t] for every proposed track t)
```

**What bounds the reserve, and what does not.** `tig_integration.md` §14.1
records the penalty as `penalty_amount * min(R, B)`, where `R` is the count of
distinct nonces in the benchmark whose reports were successfully arbitrated
against it, pooled across every report rather than applied per report.

That settles the **count**: the bundle count caps how many times the price is
charged, so the penalty can never exceed `penalty_amount * B[t]`. Before it was
settled, the ceiling might have been the benchmark's whole nonce count — larger
by `num_nonces_per_bundle`, a per-track value and not a small one — and the
reserve would have been wrong by that factor rather than merely tight.

It does **not** settle the **price**. §14.1's other determination is that
`penalty_amount` is read live when the penalty is applied, so it can rise after
the reservation was taken. `P[s] * B[t]` is therefore the exact ceiling at an
unchanged `P`, and short by the increase if `P` moves between snapshot `s` and
the charge block. §11.5 records the owner's decision to carry that residual
rather than buffer against it.

Two separate reasons the reserve can fall below the exposure, then: a price
rise, which §11.5 owns, and a multiplier below `10_000` bps, which is
deliberate and which §13 item 21 makes the pool report.

The multiplier is **integer basis points, not a fraction**, for §3's reason:
every amount in this document is exact integer attoTIG and §13 item 6 admits
no other. `10_000` bps is the default and means no discount; `5_000` bps
halves the method reserve.

**Rounding is up, toward the pool** — the opposite of §5's fee, and
deliberately so. A fee rounds down because the remainder belongs to the
members; a reserve rounds up because a remainder left outside it is exposure
the pool carries uncollateralized. The scaled reserve is therefore never one
atom short of the policy.

**The fee appears once, not twice.** This reserve previously carried a separate
`X[policy]` term for the failure charge alongside `F`. §11.6 now derives that
charge from the benchmark's own fee, so the two were the same money described
twice: a member would have held two fees to begin work while never being able
to lose more than one. One `F` does both jobs — it is the fee the pool fronts,
and it is what the member owes if they waste it.

`M_bps[m]` is set by the pool, never by the member, and is versioned policy in
the same append-only form as §5's fee policy. It scales the method reserve
**only**: `F` is an outlay the pool certainly makes, so it is not a risk that
trust can discount. The
value is read when the assignment reserve is computed and fixed into that
reservation; a later change never reaches an open reservation, in either
direction. ADR 0010 records the decision, what the uncovered
`method_reserve - scaled_method_reserve` costs the pool on a slash, and why
this is the trust-label mechanism §14 and `mining_system.md` §11 held open.

The maximum is necessary because TIG selects the track only after the
precommit. The pool atomically reserves that amount from the member's
`eligible_collateral` below before creating the precommit intent. Once
TIG confirms the selected track and exact `fee_paid`, the reservation may be
reduced to that track's exact requirement, never increased by silently applying
a later pool policy.

For illustration only, if `P = 10 TIG` and the fee term is omitted,
the method reserve is `40 TIG` for 4 bundles, `100 TIG` for 10 bundles, and `250
TIG` for 25 bundles. The implementation always reads `P`, fees, bundle counts,
and applicable configuration from the block-consistent decision snapshot; none
of these example values is compiled into admission logic.

For member `m`:

```text
eligible_collateral[m] = balance[m]
                         - recorded withdrawal requests not yet posted (§11.7)
                         - frozen charge/slash amounts

reserved_exposure[m]   = sum(open assignment reservations)

new work is allowed only if:
eligible_collateral[m] - reserved_exposure[m] >= precommit_reserve
```

`balance[m]` is §11.7's single balance in full. Every part of it may back
work, because both ways value enters are already settled: a finalized
recognized deposit (§11.2), and a round credited under §8.4 only once its own
slashing is complete and the money has landed. There is no unmatured tranche
to exclude and no draw order to apply — a distinction ADR 0008 needed and
ADR 0012's settlement timing removes.

One reservation remains attached to one member-owned benchmark even after the
member's compute slot is released, and **both of its portions are held to the
same condition**: until the benchmark can no longer generate a
method-verification penalty and every report and arbitration against it is
terminal. This prevents the same TIG from collateralizing several
simultaneous risks.

The fee portion `F` is held that long deliberately, and not only until TIG
verifies the benchmark. §11.6 charges `P * min(R, B) + F` on a successfully
reported benchmark, so releasing `F` at verification would leave a reservation
of `P * B` facing a charge of `P * B + F` — short by a fee even at
`10_000` bps, where nothing is supposed to be short. Holding both portions to
terminality is what makes the reserve exactly cover the charge at full
multiplier.

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
into a proof. The penalty **basis** that this once also waited on is no
longer open: `tig_integration.md` §14.1 records the charge as
`penalty_amount * min(R, B)` with `R` pooled across the benchmark, so the
number of times the price is charged is bounded by the bundle count. That
removes one unknown and leaves this section's intact: the **price** is still
read live at the charge block, which is the whole subject here, so no formula
based on the assignment block alone can guarantee coverage.
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
then open, the charge exceeds what was reserved. The member still owes it:
under ADR 0013 the reservation is a floor on what they must hold to begin
work, not a cap on what they owe, and §11.6 charges the evidenced amount.
What §11.4 does not do is enlarge an existing reservation after the fact —
neither recognition path into the balance, §11.2's transfer or §8.4's
settlement, retroactively increases one.

So a price rise makes the evidenced amount larger than what is encumbered,
exactly as a multiplier below `10_000` bps does. **The posted batch is still
bounded by the encumbrance**: §11.2 and §11.7 both hold, so a charge debits
only encumbered balance and can never reach value the member was free to
withdraw. The excess is not a larger debit against the balance; it is an
amount the reservation does not cover, and where it comes from is not settled
here. §10 rule 7 forbids charging other members or a future block for a
shortfall, and issue #56 owns that rule.

Should either premise fail, this is the section to revisit, and the
mechanism to add is a buffer or an additional-collateral call — never a
retroactive slash.

### 11.6 Failure charges, method losses, and tier effects

**A charge does not depend on fault.** The member who owned the benchmark is
charged whether the cause was the member, the pool, TIG, or was never
established. This replaces the fault-attribution boundaries this section
previously set, under which `POOL`, `TIG` and `UNRESOLVED` attribution each
slashed zero.

The charge is the pool's whole loss on that benchmark. It has two parts and
they come from different places: `tig_integration.md` §14.1's penalty, which
TIG takes from the pool, and the precommit fee, which the pool spent itself.
Neither this table nor §14.1 should be read as saying TIG charges a fee — it
does not.

```text
the benchmark earned active bundles, and no report against it
  was successfully arbitrated                              ->  charge 0

the benchmark earned active bundles, and R nonces were
  successfully arbitrated against it                       ->  charge
                                                    P * min(R, B) + F

anything else — abandoned, unusable, solution-verification
  failure, or zero bundles meeting TIG's minimum
  verification quality                                     ->  charge F
```

`P` is the live `reports.penalty_amount` at the charge block, `R` and `B` are
`tig_integration.md` §14.1's distinct arbitrated nonces and bundle count, and
`F` is that benchmark's own precommit fee. There is no separate `X` policy
number: the failure charge *is* the fee the benchmark wasted, so a benchmark
that cost more to start costs more to waste.

The second branch is written with one `P` because a benchmark is usually
charged once. Where reports arrive in stages it is charged in increments, each
priced at the `P` live when *it* posts, so the total is the sum of those
increments rather than a single multiplication — see "The count is cumulative"
below. The two agree exactly whenever `penalty_amount` does not move between
charges.

**"Earned active bundles" is not the same as TIG's `Active`.**
`tig_integration.md` §6 defines `Active` as membership in
`block.data.active_ids.benchmark`, which a complete benchmark can reach with
`num_active_bundles = 0`. That benchmark produced nothing the pool can earn
from, and the list below already counts it as a chargeable failure — so the
first branch turns on the benchmark having at least one active bundle, not on
its appearance in the active set. Keying the table on `Active` alone would
charge nothing for work that returned nothing.

Three consequences of charging without attribution, stated rather than left to
be discovered:

- **The member owes the full penalty, not the multiplier-scaled part.**
  ADR 0010's multiplier sets what a member must hold to begin work, not a cap
  on what they owe. A charge exceeds the reservation in two cases: a
  multiplier below `10_000` bps, which is deliberate, and a `penalty_amount`
  rise between the snapshot and the charge block, which §11.5 records the
  owner choosing to carry. At `10_000` bps with an unchanged price the reserve
  covers the charge exactly, which is why §11.4 holds the fee portion to
  terminality rather than releasing it at verification.

  **What happens to the excess is not yet specified**, and the posted batch
  does not reach for it: §11.2 and §11.7 bound a charge to encumbered balance,
  so the debit stops at the reservation whatever the evidenced amount. §10
  rule 7 forbids the obvious recovery by name — "never charge other members or
  a future block silently for the shortfall" — and issue #56 owns that rule
  and the decision replacing it. Until then this section states the liability
  without stating its recovery, and no member is exposed because nothing
  charges anyone before slice 8.
- **There is no in-system appeal.** The evidence, attribution and appeal
  process this section previously required has nothing left to decide. A
  member who believes they were charged wrongly contacts the pool out of band;
  a pool investigation that agrees reverses it through §10's correction path.
  The remedy exists and is deliberately not codified for v0.
- **`mining_system.md` §10 invariant 7 still holds and is not contradicted.**
  Artifact loss, corruption, proof construction and proof availability remain
  the pool's *responsibilities* after durable acceptance. What changes is that
  the charge no longer follows that responsibility: the pool still owes the
  member correct handling, and still bears the reputational and operational
  cost of failing, but the collateral charge is levied regardless. Those are
  different things and the invariant is about the first.

Two boundaries are unchanged. TIG-verified work that merely earns no
qualifiers is charged zero — a benchmark that did its job and did not win is
not a failure. And multiple failing benchmarks use their own reservations and
batches; one benchmark never consumes another's reserve silently.

**Freeze on report, resolve on arbitration.** A method report is an
accusation, not a finding: `ArbitrationDetails` resolves to
`NONREPRODUCIBLE`, `REPRODUCIBLE` or `INCONCLUSIVE`. A report is observable
before its arbitration resolves, and the pool acts on that earlier signal
without treating it as proof. How much earlier is not stated here: reports
against a benchmark can still arrive until the end of round
`benchmark_round + submission_period`, and `tig_integration.md` §14.2 pins
that unit as rounds while its value stays live configuration.

On observing a report against a member-owned benchmark, the pool freezes that
benchmark's **whole reservation** — the scaled method portion and the fee
portion together — holding it against the reported outcome instead of
releasing it when the benchmark would otherwise stop being able to generate a
penalty.

Both portions, because a successfully arbitrated report is charged
`P * min(R, B) + F`. Freezing only the method portion would leave §11.2's
bound — a charge reaches only encumbered balance — under-charging by exactly
one fee, which is the same gap §11.4 closes by holding `F` to terminality
rather than releasing it at verification. The freeze and the reserve now cover
the same thing.

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
when the pool acts on a published outcome, which §11.6 governs, and nothing
about when TIG applies a penalty —
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

**A `NONREPRODUCIBLE` arbitration is what charges the member**, and nothing
further decides it. That is the change this section records: the evidence,
attribution and appeal process this paragraph previously described has been
removed, so the arbitration outcome is the outcome.

**The count is cumulative; each increment is priced when it posts.** `R` is
pooled across the benchmark (`tig_integration.md` §14.1), and a benchmark that
survives one report is still reportable, so a later successful arbitration
raises `R`. What a charge takes is the *newly counted* nonces at the price
live when that charge posts:

```text
counted_so_far = nonces already charged for this benchmark
delta = min(R_total, B) - counted_so_far
charge = P[charge block] * delta      + F on the first charge only
```

**The whole total is never recomputed at a later price.** Doing so would
re-price nonces an earlier increment already charged, against §11.4's rule
that a later change never reaches an open reservation and this section's own
closing rule that a later policy does not change what can be charged for an
earlier assignment. It would also make the increment *negative* whenever
`penalty_amount` fell between charges — an amount nothing in §11.7's coverage
inequality or §13 item 12 can fund.

`delta` cannot be negative: nonces can be reported only once and an
arbitration is terminal, so `min(R_total, B)` only rises. The fee is charged
once because the benchmark wasted one fee, not one per report, and charging it
again would exceed §11.4's reservation, which holds one, and violate §13 item
19.

**An increment is its own charge decision.** It posts its own batch with its
own cause identifier, so §11.2's "one charge, one cause identifier" and §13
item 13's one-sweep-per-cause both hold: what they forbid is two identifiers
for one batch, not two batches for one benchmark. Each decision is idempotent
on its own identifier, and the amount is derived from the total observed `R`
minus what prior decisions on that benchmark already charged — so a retry
recomputes the same increment rather than adding a second one.

A benchmark can arbitrate `NONREPRODUCIBLE` for a pool-side reason —
`mining_system.md` §8 names pool-constructed wrong proofs, packages the pool
corrupted after durable acceptance, and work whose origin was never
established — and the member is charged for those too. That is the owner's
decision, and its cost is real: a member can lose collateral for a failure
they could not have prevented and, after durable acceptance, could not even
have observed. The remedy is the out-of-band contact above, not a protocol
state.

On `REPRODUCIBLE` or `INCONCLUSIVE` the member is charged nothing **for that
report**: those arbitrations levy no penalty, so they add nothing to `R` and
nothing to the charge.

Whether the benchmark is charged at all is the table's question, not this
paragraph's. A benchmark that earned active bundles and ends with no
successfully arbitrated report is charged nothing — branch one. One that ends
with at least one is charged `P * min(R, B) + F`, the fee included — branch
two. The fee is not reserved for benchmarks that produced nothing; branch
three is simply the case where the fee is *all* there is, because no penalty
term exists without an arbitrated report.

Settling a report is not the same as releasing the reservation. The freeze
lifts, but the reservation goes on being held under §11.4's ordinary
condition — both portions, until the benchmark can no longer generate a
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

A member-level response would not have that property. It is also the wrong
instrument: this freeze is about not releasing cover while an accusation is
open, which is a question about the benchmark, not about the member — and
under §11.6 an unresolved cause no longer changes whether a charge lands, so a
member-level suspension would be reaching for a judgement the design stopped
making. Bounding a member's total exposure across concurrent work is §11.4's
job, through `reserved_exposure`, and does not need this rule to reach further
than one benchmark.

For every chargeable failure, freeze and then charge that benchmark's own
amount under the table above — the fee alone where it never went active, the
penalty plus the fee where it was successfully reported:

```text
Debit   LIABILITY:MEMBER_BALANCE:<member>
Credit  EQUITY:SECURITY_LOSS_RESERVE        the penalty portion
Credit  REVENUE:FAILURE_CHARGES             the fee portion
```

This is the same batch §11.2 describes, posted here in the section that
computes it. The split is not cosmetic: crediting the penalty portion to
revenue would overstate what the pool earned and leave the loss reserve empty
of the loss it exists to record. Either line may be zero — a benchmark that
produced nothing is charged only the fee portion.

`REVENUE:FAILURE_CHARGES` is its own account, not the pool fee and not the
loss reserve. §8.6 sweeps the whole batch out of member custody under the
charge decision's single identifier, and §13 item 12 counts what is charged but not yet
swept as a named margin term — both of which need this credit side to exist
before they can be computed. **The table above decides which benchmarks are
charged, and it is the authority.** Its branches are exhaustive by
construction: a benchmark either earned active bundles and was reported, or
earned them and was not, or did neither. Every benchmark reaching a terminal
outcome lands in one of the three.

The cases met most often in v0 are an abandoned or unusable package, TIG
solution-verification failure, a benchmark with zero bundles meeting TIG's
minimum verification quality, and a benchmark with a successfully arbitrated
report against it. The third is a capacity/economic failure, not an allegation
of fraud; the fourth is the method-loss case and carries the penalty term as
well as the fee. That is a list of common instances and **not a closed set** —
a benchmark that failed for a pool-side reason after durable acceptance
appears in none of them and is charged all the same, under the table's third
branch, because §11.6 charges without asking what caused the failure. Every
charged benchmark increments `f`. Slow but
eventually correct work is not charged; it is handled by the round
`unverified_exposure > verified_exposure` tier-removal rule.

For tier `k`, if the round's chargeable failure count `f > k`, remove the tier
at round close after recording each failure's own charge. The sum is no longer
`f` times a constant: a benchmark that cost more to start costs more to waste,
so a member who wastes one large assignment can owe more than one who wastes
several small ones.

**Every failure counts, whatever caused it.** `POOL`, `TIG`, compatibility and
`UNRESOLVED` outcomes previously charged zero and did not increment `f`. They
now do both, because a charge no longer depends on fault and the tier count
follows the charge.

The cost of that is worth naming, because pool-caused failures are
*correlated* in a way member-caused ones are not: one proof-construction bug
or one artifact-store incident can fail many members' benchmarks in the same
round, and that round closes with every affected member above their tier's `k`
demoted and charged. Tier removal has no reversal path of its own and
repurchase costs `J[k]` again, so unwinding a mass demotion caused by one pool
incident runs through §10's correction path. An operator who finds that shape
of incident should expect to use it.

After a charge or removal, the ordinary admission formula prevents more work
unless enough finalized, unreserved collateral remains for every existing
exposure and the next assignment. A newly paid tier fee never bypasses that
gate. The complete evidence and outcome inventory is in
[member_attack_model.md](member_attack_model.md).

A proposed failure charge freezes only the evidenced amount; it does not by
itself create a first-failure ban. The reduced eligible collateral may still
prevent further admission. A proposed method-loss charge may additionally
impose the separate method/security suspension.

**There is no appeal deadline, because there is no in-system appeal.** A
charge becomes final on the evidence that produced it — an observed terminal
arbitration, or a benchmark that never went active — not on a clock a member
failed to beat. The seven-day window this section previously ran, and the
second-reviewer rule behind it, existed to decide *fault*, which §11.6 no
longer makes relevant.

A member who believes a charge was wrong contacts the pool out of band. A
pool investigation that agrees reverses it through §10's correction path,
which is an audited batch with a stated reason like any other correction. The
remedy exists; it is deliberately not a protocol state, and the member terms
required by `pre_build_checklist.md` §9 must say so plainly, because a member
who expects an appeals process and finds an email address should learn that
before they deposit rather than after a charge.

**That reversal is not yet fundable, and issue #56 owns the fix.** §10 details
what a correction does when it *reduces* a member balance and says nothing
about increasing one. Once a charge is swept under §8.6 the tokens are in
operating custody, and §8.6 states there is "no transfer in the other
direction and no internal rebalancing at all" — so a correction restoring the
balance would raise the member liability without raising the custody backing
it, breaching §11.7's coverage inequality and stopping reconciliation under
§13 item 16.

Two shapes resolve it and both are the owner's: hold a charged amount in
member custody for a period before sweeping it, which makes a reversal pure
bookkeeping inside one pot, or admit a funded transfer from operating custody
back to member custody under `security.md` §3.4's authorization. Until one is chosen the
remedy above is a promise the ledger cannot execute, which matters because it
is the only recourse a member has. Nothing charges anyone before slice 8.

A withdrawal request immediately removes its whole amount from admission
collateral and encumbers the same amount against withdrawal. Those are one
figure, not two: every part of the balance is collateral-eligible under
ADR 0012, so anything a request reserves is capacity it was contributing.

It is payable only after all affected reservations are released,
the last relevant benchmark is no longer reportable, every report/arbitration
is terminal, and one additional TIG round has passed. A frozen charge keeps
only the evidenced amount locked.

Under ADR 0008 there is one outbound member path, and what a withdrawal may
never include is encumbered balance. There is no draw order and no fast path:
the maturity split that created both is gone under ADR 0012, so every
withdrawal is subject to the waits above without exception. Every withdrawal
uses the member's authenticating wallet under §12.2.

Tier fees, failure charges, collateral formulas, concurrency rules, slash
rules, and effective TIG heights are append-only policy versions. A later
policy does not change the amount that can be charged for an earlier assignment
or tier purchase.

### 11.7 One member balance

ADR 0008 settles what this section previously held open. A member has **one
balance** with the pool. §11.2's recognized deposits and §8.4's settled round
earnings are the same liability, `LIABILITY:MEMBER_BALANCE:<member>`;
§11.4 reserves against it and §12 withdraws from it.

**There is no maturity split.** Every part of a member's balance may back
work the moment it is there. Both ways value arrives are already settled: a
§11.2 deposit is recognized from a finalized transfer, and §8.4 credits a
round only once its own reports and arbitrations are terminal *and* the money
has landed. Nothing can enter this balance that a method penalty against its
own round could still reach.

That is a change, and ADR 0008 is where the previous rule lives: it credited a
round immediately and then withheld collateral eligibility until the producing
round could no longer be penalized. Making the credit wait instead removes the
maturity flag, the per-contributing-benchmark maturation condition, the
withdrawal draw order, and §13's separate invariant enforcing it — four
mechanisms that existed only to describe a state a member's balance can no
longer be in. ADR 0012 records the decision and what it costs: a member waits
longer to see a round at all, in exchange for the balance meaning one thing.

The condition §8.4 applies is **per contributing benchmark**, not per earning
round, and that subtlety survives the simplification because it was never
about maturity. A benchmark's lifespan is measured in blocks while a round is
far longer, so a benchmark started before a round boundary earns qualifiers
attributed after it. `tig_integration.md` §14.2's `?round=` selects by the
*benchmark's* round, so a round's earnings are settleable only when every
benchmark whose qualifiers were attributed in that round has a closed
reporting window and terminal reports — not when the earning round's own
window closes.

The bound is not a schedule. `tig_integration.md` §14.2 says when the answer
should be readable; a round still unsettled past it is an alertable
discrepancy under §13 item 16, exactly as an outstanding freeze is. Settling
on the clock rather than on observed terminality would credit a member against
a penalty still live.

How the pool observes those reports is `tig_integration.md` §14.2's, not this
section's: that document owns the TIG reads, and §11.6 already depends on the
same observation for its freeze rule.

**Encumbrance.** The balance splits into encumbered and unencumbered parts:

```text
balance[m]      = LIABILITY:MEMBER_BALANCE:<m>

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

The term deducts the request's **whole recorded amount**, which is also what
§11.4's `eligible_collateral[m]` deducts. Under ADR 0008 those two figures
could differ, because a request drawing on unmatured earnings encumbered value
that had never contributed admission capacity; ADR 0012 removes the split, so
a recorded request now costs a member exactly the same amount of withdrawable
balance and of admission capacity. One request, one number, deducted from
both.

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
tier fee, a §11.6 charge, a correction — awaiting §8.6's
sweep. §13 item 12 enumerates the same terms, and daily reconciliation
accounts for each rather than treating it as a mismatch.

The inequality holds at every batch boundary without any in-flight allowance,
because no batch in this document moves value between member custody and
anywhere else except at a finalized token event: §8.3a's inbound sweep, §8.6's
outbound sweep, §11.2's recognized deposit, and §12.5's completed withdrawal.
The batches in between — settlement, reservation, freeze — move liabilities
inside the one pot. There is no maturation batch: under ADR 0012 nothing is
credited before it is settled, so no later event reclassifies value already in
the balance.

**What one pot costs, recorded plainly.** The pool's earlier design split
member value across two custody addresses so that a stolen payout key could
not reach collateral and a stolen deposit key could not pay anyone. One pot
gives that up: a compromise of the member-custody signing key reaches every
member's collateral and every member's withdrawable balance at once. ADR 0008
records the decision and what was weighed. What remains, and what §13 and
`architecture.md` §13 invariant 9 now enforce, is the separation that survives:
member value never shares an address or key with pool operating funds, and
never with the pool's TIG protocol identity.

**No withdrawal draw order.** ADR 0008 needed one, because a withdrawal could
combine matured and unmatured value and the two carried different waits, while
one fungible liability carries no record of which atoms were once collateral.
Under ADR 0012 the balance has no tranches, so a withdrawal is a single amount
against a single figure and §11.6's waits apply to all of it.

**What did not change.** §11.1's delegated TIG is still not pool value and
still grants no capacity. Pool revenue, the security loss reserve, and
operating custody remain distinct from the member balance and from each other.

## 12. Member withdrawals

### 12.1 Settlement and withdrawal eligibility

Two separate gates, because ADR 0008 separated the two events.

**A round settles into member balances** (§8.4) only when:

- every block batch in the round is posted and no unresolved payout suspense
  remains for that round;
- the complete round is reconciled to TIG's round data;
- **every charge against that round is settled** — each benchmark whose
  qualifiers were attributed in it has a closed reporting window and terminal
  reports (§8.4), so nothing credited can still be reached by a penalty
  against its own round;
- the exact corresponding TIG payment is finalized and reconciled in the
  reward wallet (§8.3); and
- that round's §8.3a **member leg** has completed from its own finalized token
  event, so the balances §8.4 credits are covered by member custody at the
  instant they exist.

The expected TIG payment delay may be several weeks, and the owner states it
outlasts the arbitration window, so the funding condition is expected to bind
— owner confirmation, not a measured fact. Both are required regardless: the
pool waits for actual funds, not elapsed time, and it waits for the round's
own slashing to finish, not for a schedule. A payment arriving before
arbitration would otherwise credit a member against a charge still to come.

Settlement is automatic and needs no member action; what it produces is a
balance, not a transfer.

**A withdrawal becomes payable** only when:

- the requested amount is unencumbered under §11.7 at the moment the intent is
  created, and remains so until it is spent. The check excludes *this*
  request's own recorded amount and no other: a recorded request encumbers
  balance against every **competing** request, but a gate that also counted
  the request it is deciding would refuse every withdrawal ever made;
- §11.6's waits are satisfied for the withdrawal, all of it;
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
  withdrawal that ADR 0009's per-member caps admit. §3.4 owns those caps,
  what the rolling seven-day total counts, and where it is evaluated; the signer's part
  is only to read the cap result stamped into the immutable intent by the
  accounting projector, never to compute a member's history for itself.
  Thresholds for operating custody are unchanged.
  The hot-wallet limit is a capability the signer must have and a value the
  owner has not set: ADR 0009 declines one for v0 and records what that
  accepts. An unset limit is not an absent control — it must be configurable
  and reported, so setting it later is policy rather than a code change.

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
    cause identifier — tier activation, charge decision, suspense resolution,
    or correction ID — for an operating sweep. A §11.6 charge has exactly one
    identifier however its credit lines split;
14. one signed intent fixes chain, token, destination, amount, signer, and
    nonce;
15. only a finalized exact token event completes a withdrawal or a sweep;
16. any mismatch stops posting/transfers and alerts rather than creating a
    compensating guess;
17. a tier activation and its non-refundable fee post atomically;
18. tier removal or repurchase never releases existing financial exposure;
19. the same outcome cannot consume a failure charge or a method reserve
    twice under one policy reason;
20. no value enters `LIABILITY:MEMBER_BALANCE` before it is settled — a
    deposit on finalized recognition (§11.2), a round only once its own
    reports and arbitrations are terminal and its TIG payment has landed
    (§8.4). This replaces ADR 0008's rule that settled earnings counted zero
    toward `eligible_collateral` until maturity: there is no unmatured
    balance to exclude, so the property is enforced by when the credit posts
    rather than by a flag on it; and
21. aggregate uncovered method exposure — the sum of
    `method_reserve - scaled_method_reserve` over every open reservation — is
    reported, not merely derivable. Below `10_000` bps the member owes more
    than they hold (§11.5, §11.6), so this is the amount the pool would have
    to collect by a route that does not yet exist (issue #56) or absorb. It
    must be visible before a charge lands, not reconstructed after one.

Daily reconciliation compares:

- accepted TIG blocks, per-block proceeds, and round totals;
- reward receivable versus identified TIG settlement;
- Base finalized token balances and transfer events versus the reward wallet,
  member custody, and operating custody assets;
- member balance split by suspense and withdrawal-pending liabilities versus
  member custody's coverage inequality (§13 item 12), with
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
7. a pool-set per-member collateral multiplier in integer basis points,
   default `10_000`, range `1..=10_000`, scaling the method reserve only and
   rounding up toward the pool (§11.4, ADR 0010);
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
   It also left the slash and charge notices in §11.2 and §11.6 with no
   channel to be delivered on. Decision 11 below closes that by removing the
   notices and the appeal they gated, rather than by finding them a channel.

11. **a charge does not depend on fault, and there is no in-system appeal**
    (2026-09-17). The member who owned the benchmark is charged whether the
    cause was the member, the pool, TIG, or was never established, and every
    failure counts toward tier removal. A member who believes a charge was
    wrong contacts the pool out of band; a pool investigation that agrees
    reverses it under §10 (§11.2, §11.6, `mining_system.md` §8); and
12. **the failure charge is derived, not chosen** (2026-09-18). It is the
    benchmark's own precommit fee where the benchmark never went active, and
    `P * min(R, B)` plus that fee where it was successfully reported. The
    `X[policy]` number this section previously listed as unset no longer
    exists, and §11.4's reserve carries the fee once rather than twice
    (§11.6, ADR 0013).

The remaining decision is:

1. numerical `J[k]` — the non-refundable joining fee for tier `k` (§11.3).
   `mining_system.md` §11 carries it with `internal_pool_unverified_limit`
   among the values required before full product implementation. It is pool
   pricing rather than a gap in any formula: §11.4's reserve is computable
   without it. The threats and unresolved consequence choices behind the
   malicious-work side are enumerated in
   [member_attack_model.md](member_attack_model.md).

Decisions 6 and 7 lift this section's former prohibition: an implementation
may accept member collateral under §11.4 and may present it as settled policy.
The precondition that ADR 0010 recorded as surviving that lifting — the
unverified penalty *basis*, whether a benchmark incurs `penalty_amount` once
or per reported nonce — **is now met**: `tig_integration.md` §14.1 records the
charge as `penalty_amount * min(R, B)`, owner-confirmed 2026-09-18. That
supersedes ADR 0010's consequence bullet to the contrary, which stays as
written under the ADR immutability rule. The public-funds gates in
`pre_build_checklist.md` §9 are untouched and still govern real member money,
and testnet continues to exercise deposit fixtures with explicit fixture
policy values.
