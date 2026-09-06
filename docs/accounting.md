# TIG mining pool: accounting and payout contract

Status: technical contract and v0 tier economics settled; numerical policy values pending  
Last updated: 2026-07-31

This document defines how confirmed TIG proceeds become internal member
credits, how exact integer allocation and corrections work, and the boundary
between earned balances, deposits, custody, and automatic round payouts. It
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
- separation of earned balances, deposits, and custody; and
- automatic per-round payout authorization, signing, replay protection, and
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
    -> PAYING
    -> PAID
```

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
8. `ASSETS_SETTLED` requires exact reconciled TIG for the round to be available
   in the pool's finalized payout custody. A wall-clock estimate, including the
   expected several-week delay, is never settlement evidence.
9. Settlement automatically starts that round's member payout; members do not
   request withdrawals or choose an amount.

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
ASSET:TIG_REWARD_RECEIVABLE
ASSET:TIG_PAYOUT_CUSTODY
ASSET:SECURITY_DEPOSIT_CUSTODY
ASSET:TIG_OPERATING_CUSTODY
LIABILITY:MEMBER_EARNED_PENDING:<round>:<member_id>
LIABILITY:ROUND_PAYOUT_PENDING:<round>:<member_id>
LIABILITY:PAYOUT_SUSPENSE:<block_id>
LIABILITY:MEMBER_SECURITY_DEPOSIT:<member_id>
REVENUE:POOL_FEE
REVENUE:TIER_JOINING_FEES
EQUITY:SECURITY_LOSS_RESERVE               finalized slashes; restricted use
EXPENSE:ACCOUNTING_LOSS                    explicit approved correction only
```

Payout, security-deposit, and operating custody are different addresses or
contract vaults and different ledger assets. None silently backs another.

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
pool payout-custody address:

```text
Debit   ASSET:TIG_PAYOUT_CUSTODY
Credit  ASSET:TIG_REWARD_RECEIVABLE
```

Settlement is never inferred merely from the internal reward API or a claimable
display. The round, on-chain event/transaction, amount, token, chain, block, and
custody address are stored as evidence. If TIG pays multiple rounds in one
transfer, the cumulative payment must reconcile exactly before the oldest fully
funded round advances; funds are never assigned by a guess.

### 8.4 Funding an automatic round payout

Once a round is fully reconciled and funded, every eligible positive member
amount moves to that round's payout-pending account in one batch:

```text
Debit   LIABILITY:MEMBER_EARNED_PENDING:<round>:<member>
Credit  LIABILITY:ROUND_PAYOUT_PENDING:<round>:<member>
```

This batch creates immutable automatic transfer intents. The sum of all
round-payout-pending liabilities must never exceed finalized, unencumbered TIG
payout custody allocated to members. An amount held for a missing/recently
changed destination stays in `MEMBER_EARNED_PENDING` until that member becomes
eligible; it does not block the rest of the round.

## 9. Idempotency and immutable batches

Uniqueness keys are:

```text
ordinary block batch  (network, block_id)
suspense resolution   (network, block_id, resolution_generation)
correction batch      correction_id
round reconciliation  (network, round)
asset settlement      (network, settlement_source_id)
round payout intent    (network, round, member_id, payout_generation)
on-chain transfer     (chain_id, signer, transaction_nonce)
journal line          (batch_id, line_number)
```

An ordinary block can have exactly one original batch, whether attributed or
suspense. Retries return that batch. Every state transition, line set, and
outbox event commits in one transaction. Posted lines are immutable: no UPDATE
or DELETE privilege is granted to the application roles.

Balances are projections of journal lines and may be rebuilt. A cached balance
is never more authoritative than the journal. A transaction locks the round
and its member liabilities before creating payout intents, so two workers
cannot pay the same round-member amount twice.

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
2. then consume round-payout-pending value whose transfer is not yet signed;
3. never alter an already signed, broadcast, or finalized round transfer;
4. if the member has already been paid too much, record an explicit member
   receivable/negative future-earnings balance, stop new payout intents and
   work, notify the member, and require an operator resolution; and
5. never charge other members or a future block silently for the shortfall.

Writing off a shortfall to `EXPENSE:ACCOUNTING_LOSS` requires an explicit
approved correction and does not relabel it as mining expense or payout dust.

## 11. Member deposits and custody separation

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

A member must separately transfer TIG to the pool's dedicated security-deposit
escrow before receiving public-pool work. Recognize it only from a transfer
event on the allow-listed token and chain that:

- names a verified member wallet/source and configured custody destination;
- has a unique `(chain_id, tx_hash, log_index)`;
- is included in a Base `finalized` block;
- has an exact positive attoTIG amount; and
- has not been recognized for another member or purpose.

Recognition posts:

```text
Debit   ASSET:SECURITY_DEPOSIT_CUSTODY
Credit  LIABILITY:MEMBER_SECURITY_DEPOSIT:<member>
```

Security deposits remain completely separate from delegation, earnings, round
payouts, and pool operating funds. They cannot pay another member or silently
cover a pool error. A proposed slash first freezes the disputed amount without
moving the member liability, notifies the member, and records the evidence and
appeal deadline. A final slash is a new audited journal batch authorized only
by the published member-fault policy, with benchmark evidence, amount, policy
version, actor, and appeal result:

```text
Debit   LIABILITY:MEMBER_SECURITY_DEPOSIT:<member>
Credit  EQUITY:SECURITY_LOSS_RESERVE
```

The reserve may reimburse documented member-caused TIG fees, penalties, or
accounting losses. It is not ordinary pool-fee revenue, cannot fund member
payouts, and cannot be distributed to an operator merely because a slash
occurred. Any later use is another approved, auditable batch.

Custody must use a dedicated escrow address or contract and production signing
boundary, not the round-payout hot wallet. The pool must publish enforceable
member terms and obtain legal review before accepting these funds.

### 11.3 Non-refundable tier joining fee

Joining tier `k` costs `J[k]` attoTIG under the policy version effective at the
join transaction. Tier `k` grants eligibility for at most `k` concurrent
unverified benchmarks; it does not purchase guaranteed work or pool capacity.
The fee is separate from delegated TIG and slashable security collateral.

The member may authorize the fee to be taken only from finalized,
unencumbered security-deposit value after every existing reservation, pending
return, and frozen charge. Activation of the tier and the balanced fee journal
batch commit together:

```text
Debit   LIABILITY:MEMBER_SECURITY_DEPOSIT:<member>
Credit  REVENUE:TIER_JOINING_FEES
```

The corresponding custody value is no longer security-deposit backing and is
swept to operating custody through a separately reconciled transfer:

```text
Debit   ASSET:TIG_OPERATING_CUSTODY
Credit  ASSET:SECURITY_DEPOSIT_CUSTODY
```

A failed, ambiguous, or unfinalized fee debit never activates the tier.

Tier removal creates no refund. A removed member may immediately buy a tier by
paying its then-current `J[k]` again; there is no cooldown or tier-admission
queue. Every purchase has a new idempotent fee batch and membership period.
Rejoining does not release or reset outstanding benchmarks, reservations,
fines, appeals, deposit returns, or method-verification exposure.

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

method_reserve[s,t] = P[s] * B[t]
assignment_reserve[s,t] = method_reserve[s,t] + F[s,t] + X[policy]

precommit_reserve = max(assignment_reserve[s,t] for every proposed track t)
```

The maximum is necessary because TIG selects the track only after the
precommit. The pool atomically reserves that amount from the member's finalized
eligible security-deposit liability before creating the precommit intent. Once
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
eligible_collateral[m] = finalized security-deposit liability
                         - pending returns
                         - frozen charge/slash amounts

reserved_exposure[m]   = sum(open assignment reservations)

new work is allowed only if:
eligible_collateral[m] - reserved_exposure[m] >= precommit_reserve
```

One reservation remains attached to one member-owned benchmark even after the
member's compute slot is released. Its method portion remains reserved until
the benchmark can no longer generate a method-verification penalty and every
report/arbitration is terminal. Its `X` portion releases when TIG verifies the
benchmark without a chargeable tier failure, or is frozen and charged when a
member-attributable failure is established. This prevents the same TIG from
collateralizing several simultaneous risks.

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

This is recorded as the owner's position, not as settled policy, and it does
not lift §14's hold: that section still holds the §11.3–§11.5 collateral
policy as an unanswered owner decision, and no implementation may accept
public member collateral until it is answered. `CLAUDE.md` reserves
security-deposit decisions to an explicit human decision, and this section
records one rather than deriving it.

Two premises it rests on are **not established**, and the position is taken
knowingly rather than derived from them:

- how long an open benchmark remains exposed is unknown. §14.1 cannot exclude
  a charge block later than the arbitration block; §14.2 now pins
  `ReportsConfig.submission_period`'s unit as rounds, but its value is live
  configuration and the arbitration lag beyond the reporting window is not
  bounded by anything pinned. "Several weeks of notice exceeds the horizon"
  still compares against a horizon nobody has measured; and
- the notice expectation is an observation about how TIG has behaved, not a
  property of the protocol, and nothing in the pinned source guarantees it.

If `penalty_amount` rose with less notice than the horizon of the benchmarks
then open, the pool absorbs the difference between what was reserved and what
is charged. It is not recoverable from the member: the reservation is that
member's whole committed exposure, and §11.2 recognizes a deposit only from an
inbound transfer, so there is no mechanism that would enlarge one after the
fact. Should either premise fail, this is the section to revisit, and the
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

The freeze keeps that amount inside `reserved_exposure` in §11.4; it does not
also become a `frozen charge/slash amount`. The distinction is not
presentational: counting it in both terms would deduct one outcome from
admission capacity twice, which §13 item 18 forbids. Because the amount was
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
charge exactly `X` under the assignment's policy version. V0 chargeable tier
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

An unlock request immediately removes the requested amount from admission
collateral. Return is allowed only after all affected reservations are released,
the last relevant benchmark is no longer reportable, every report/arbitration
is terminal, and one additional TIG round has passed. A pending appeal keeps
only the disputed amount locked. Return uses the verified member wallet and is
never combined with a mining payout.

Tier fees, failure charges, collateral formulas, concurrency rules, slash
rules, and effective TIG heights are append-only policy versions. A later
policy does not change the amount that can be charged for an earlier assignment
or tier purchase.

### 11.7 Earnings as working capital (open)

The reserve in §11.4 scales with bundle count, so an established member can
need more collateral than they can reasonably hold in cash while their own
earnings sit with the pool. Letting those earnings back their capacity is
desirable and is **not specified here**, because every mechanism for it
crosses a custody boundary this document deliberately keeps closed.

The constraints any proposal must satisfy:

- payout and security-deposit custody are different addresses with different
  signing keys, and `architecture.md` §13 invariant 9 forbids them sharing an
  address, key, ledger asset or transfer intent. A journal line moving value
  between them without an on-chain transfer would assert deposit-escrow value
  that is physically in the payout wallet, and §13's daily reconciliation of
  both custody assets against finalized Base balances would fail;
- §11.2 recognizes a deposit only from a transfer event with a unique
  `(chain_id, tx_hash, log_index)` in a finalized Base block, and
  `architecture.md` §6 guards custody recognition on exactly those fields;
- §12.4's signer accepts only approved round-payout intents, so no path exists
  to move value out of payout custody by any other route; and
- §4 rule 9 and §8.4 state that settlement automatically moves every eligible
  positive member amount to payout-pending, with members choosing no amount.
  Any carve-out has to be made in those sections, which own the settlement
  batch.

A member can already achieve the effect today by being paid and depositing
under §11.2. What is missing is only the convenience of doing it without a
round trip, and that convenience is not worth a custody rule invented to
support it. This is a funds-custody decision and belongs to the pool owner.

## 12. Automatic round payouts

### 12.1 Round eligibility

The pool starts a round payout only when:

- every block batch in the round is posted and no unresolved payout suspense
  remains for that round;
- the complete round is reconciled to TIG's round data;
- the exact corresponding TIG payment is finalized and reconciled in payout
  custody;
- no accounting/security hold affects the member; and
- member, token, chain, and destination configuration remains compatible.

The expected TIG payment delay may be several weeks. The pool waits for actual
funds, not elapsed time. Once funded, it automatically pays every positive
member amount for that round. There is no member withdrawal request, chosen
amount, daily cadence, or minimum payout. The pool pays Base gas as an
operating cost and the exact TIG liability is not reduced for gas.

### 12.2 Destination authorization

The payout destination is an account setting, never a worker setting. Before
public funds the account system must require:

- a verified Base address linked through a domain-separated EIP-191 or EIP-712
  signature containing pool domain, member ID, chain ID, address, random nonce,
  purpose, and expiry;
- authenticated account access plus recent reauthentication and phishing-
  resistant MFA/passkey for address change;
- one-time nonces and exact-domain validation to prevent signature reuse on a
  different pool or chain; and
- a 48-hour security delay and out-of-band notification after destination
  change, during which automatic payouts to that member are held.

The service never accepts a destination from a worker credential or unaudited
operator edit. A member can explicitly request a payout hold. A missing,
invalid, or temporarily held destination leaves only that member's round
liability pending and does not delay other members.

### 12.3 Deterministic payout intents

When a round becomes `ASSETS_SETTLED`, one transaction locks the round, moves
each member's exact amount to `ROUND_PAYOUT_PENDING`, and creates one immutable
payout intent for every eligible positive amount. The intent fixes:

```text
network and TIG round
member and payout_intent_id
exact attoTIG amount
verified destination
chain ID and TIG token contract
payout policy/config version
```

The unique `(network, round, member_id, payout_generation)` makes retries return
the same intent. Payouts may be broadcast sequentially or in bounded batches,
but the accounting cohort is one round. A failed or dropped transaction remains
pending until chain state proves whether that same intent can be rebroadcast.

### 12.4 Signing and broadcast

Production signing is the separate private Funds Gateway and key-custody design;
the Pool API, workers, controller, TIG Gateway, database, and CI never receive
the private key. The signer:

- accepts only approved immutable round-payout intents;
- allow-lists chain ID, token contract, transfer method, destination, and exact
  amount;
- simulates the ERC-20 transfer before signing;
- uses one serialized nonce lane per signing address;
- records transaction nonce and signed transaction hash before broadcast;
- never substitutes a destination or amount during retry; and
- enforces per-transaction, rolling daily, and hot-wallet limits, with
  configured multi-person approval above a threshold.

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
Debit   LIABILITY:ROUND_PAYOUT_PENDING:<round>:<member>
Credit  ASSET:TIG_PAYOUT_CUSTODY
```

Operator recovery may hold an unsigned intent, re-run reconciliation,
or rebroadcast the exact signed transaction. It cannot mark a transfer paid
without finalized chain evidence, edit a posted batch, redirect funds, bypass
approval limits, or convert a failure into a new transfer silently. Emergency
controls can disable all new payout-intent creation and signing while reads
and reconciliation continue.

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
10. round-payout-pending liabilities never exceed reconciled, allocated payout
    custody;
11. delegated TIG, security deposits, pending earnings, payout liabilities,
    and pool revenue are distinct and cannot be silently netted;
12. one round-member payout intent spends one liability once;
13. one signed intent fixes chain, token, destination, amount, signer, and
    nonce;
14. only a finalized exact token event completes a round payout; and
15. any mismatch stops posting/payouts and alerts rather than creating a
    compensating guess.
16. a tier activation and its non-refundable fee post atomically;
17. tier removal or repurchase never releases existing financial exposure; and
18. the same outcome cannot consume `X` or a method reserve twice under one
    policy reason.

Daily reconciliation compares:

- accepted TIG blocks, per-block proceeds, and round totals;
- reward receivable versus identified TIG settlement;
- Base finalized token balances and transfer events versus both custody assets;
- member, suspense, security-deposit, and round-payout liabilities versus their
  separate backing assets;
- round payout intents versus signer nonces, transactions, receipts, and events;
  and
- ledger cached balances versus a journal rebuild.

## 14. Owner decisions required

The owner has confirmed:

1. `2%` initial public pool fee, changeable by a new round-boundary policy after
   at least seven days' notice; and
2. a `15%` initial TIG protocol reward share for non-custodial delegators,
   adjustable later through a new effective policy; and
3. automatic distribution of each round after TIG's actual delayed payment is
   finalized in pool custody, with no request, minimum, or daily batch rule;
4. the pool pays Base ETH gas for those automatic transfers without deducting
   it from a member's earned TIG; and
5. a changed payout address is held for 48 hours with out-of-band notification.

The remaining decisions are:

1. the dynamic slashable-deposit and malicious-work policy in sections
   11.3-11.5. A fixed `10 TIG` per benchmark is explicitly not accepted because
   direct method-verification exposure scales with `num_bundles` and TIG's live
   report penalty. The threats and unresolved evidence/consequence choices are
   enumerated in [member_attack_model.md](member_attack_model.md); and
2. whether a member's earnings may back their working capacity, and by what
   mechanism (§11.7). Every route crosses the payout/security-deposit custody
   boundary, so it is a funds-custody decision rather than an accounting one.

Until this is answered, testnet may exercise security-deposit fixtures using
explicit fixture policy values, but no implementation may accept public member
collateral or present the proposal as settled policy.
