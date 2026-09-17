# TIG mining pool: security baseline

Status: settled spike baseline and v0 tier controls; numerical limits pending  
Last updated: 2026-09-04

This document defines the minimum security contract for the protocol spike and
the controls that the v0 implementation must preserve. It implements the trust
boundaries in [architecture.md](architecture.md), the worker authentication and
package contract in [member_protocol.md](member_protocol.md), and the mining
ownership rules in [mining_system.md](mining_system.md).
The full inventory of member-caused mining, liveness, and capacity threats is
maintained in [member_attack_model.md](member_attack_model.md); this document
defines the technical controls that apply to those threats.

This is not a claim that the public-funds system is production-ready. Production
credential custody, penetration testing, disaster recovery, payout/deposit
signer operations, legal review, and a private pilot remain launch gates in
[pre_build_checklist.md](pre_build_checklist.md).

## 1. Security objectives

The spike must demonstrate that:

1. a member machine or public API compromise cannot disclose or directly use a
   TIG protocol credential;
2. one worker cannot act on another worker's resources;
3. member-controlled packages cannot escape their parser or exhaust the pool
   without a configured bound;
4. no package earns a durable receipt until its complete identity, coverage,
   structure, and stored bytes are mechanically consistent;
5. request, upload, TIG-write, and operator-command retries cannot duplicate an
   effect;
6. secrets and raw solutions do not enter normal telemetry; and
7. every privileged or financially relevant action leaves a durable audit fact.

Availability is important but does not override these rules. If identity,
configuration, storage integrity, or TIG compatibility is uncertain, new work
and protocol writes fail closed.

## 2. Threat and trust model

### 2.1 Untrusted parties and inputs

Assume that a member may deliberately:

- forge or replay requests, guess resource IDs, share a credential, or use a
  revoked credential;
- lie about compute, runtime, progress, file sizes, checksums, nonce coverage,
  or package identity;
- upload truncated, conflicting, highly compressed, malformed, or very large
  data;
- construct hostile Zstandard, tar, JSON, Merkle, and solution values;
- open many slow requests, uploads, or qualification attempts;
- return semantically false work that is structurally well formed; or
- try to cause another member to be charged with its failure.

Also assume ordinary process crashes, lost responses, database or object-store
outages, TIG API ambiguity, an operator mistake, and compromised public API or
artifact-parser processes. A PostgreSQL administrator, host root compromise,
cloud-account compromise, or malicious release pipeline is outside the spike's
isolation guarantee and must be addressed before public funds.

### 2.2 Data classification

| Class | Examples | Minimum handling |
|---|---|---|
| Secret | TIG API key, account/payout/deposit-custody private keys, enrollment-ticket HMAC key, database passwords, TLS keys | Never in database business rows, artifacts, source, CLI arguments, logs, or traces; provide only to the process that needs it |
| Sensitive | Enrollment ticket before use, signed authentication headers, worker-recovery ticket data, security events, wallet-link nonces | Encrypt in transit; restrict by role; never expose cross-member; redact ordinary telemetry |
| Untrusted bulky | Package chunks, manifest, outputs, qualities, Merkle data, derived parse errors | Quarantine; stream under hard bounds; never general-purpose extract or execute |
| Financial/audit | Decisions, TIG attempts, receipts, qualifier attribution, ledger and operator commands | Append-only or immutable history; durable IDs, hashes, actor, evidence, and timestamps |
| Public/low sensitivity | Protocol discovery, supported versions, confirmed public TIG facts | Integrity and availability controls still apply |

Raw solutions are not secrets in the cryptographic sense, but their size,
member linkage, and propensity to leak through parser errors make them
`Untrusted bulky`; they are never routine log content.

### 2.3 Malicious benchmark attack surface

Structural package safety is not the same as mining safety. A member can return
a perfectly formed package whose solutions are invalid or whose claimed method
execution is non-reproducible.

| Attack | Pool exposure | Required layers |
|---|---|---|
| Accept a precommit, then abandon or miss the technical package cutoff | Non-refundable fee, lost protocol/write capacity, balancing distortion | Tier concurrency, reserved `X`, stopped recovery, round failure count |
| Return malformed or incomplete data repeatedly | Parser/storage/worker exhaustion plus fee if submitted | Quotas, bounded parser, tier concurrency, reserved `X`, no commitment before acceptance |
| Return structurally valid outputs that fail TIG solution verification | Verification-pipeline congestion, fee loss, delayed useful benchmarks | Reserved `X`, tier failure threshold, correlation breaker, semantic-screening experiment |
| Return outputs that later fail method verification | TIG penalty scaling with penalized bundles, loss of rewards/reputation | Dynamic bundle-scaled collateral, retained reserve through reports, exact slash, optional hidden re-execution |
| Fund many identities or slots | Bypass per-worker limits and occupy a large fraction of pool work | Non-refundable tier fee per identity, global internal limit, FIFO offer leases, collateral cannot be reused |
| Submit work with zero bundles meeting TIG verification quality | Capacity and fee loss without protocol fraud | Reserved `X` and tier failure count; correlated pool/config failures are exempt |
| Submit TIG-verified work that earns no qualifiers | Pool may simply earn less | No charge; orchestration and qualification tuning |
| Exploit a TIG penalty/configuration change | Previously sufficient collateral becomes insufficient | Per-block config monitoring, compatibility stop, explicit residual-risk policy |

Collateral covers measurable financial exposure but does not restore lost time,
write-rate budget, verification capacity, or challenge balance. Public admission
therefore requires both financial reservation and independent throughput/trust
limits. This table is a summary; control and consequence decisions must use the
separate cases in [member_attack_model.md](member_attack_model.md).

## 3. TIG credential custody

### 3.1 Testnet provisioning

Testnet uses a dedicated TIG identity with no production authority or mainnet
funds. For the current slice this is an operator-controlled MetaMask wallet;
that testnet hot wallet is not an acceptable production-custody design. The
operator follows the testnet issuance procedure owned by
[`tig_integration.md` section 4](tig_integration.md#4-api-transport-and-authentication).
Its recovery phrase, private key and password remain exclusively in the wallet
and the operator's recovery custody. They and the signed proof never enter the
repository, workspace, runtime hosts, CI, logs, environment variables or agent
sessions.

Local development stores the key at `secrets/tig-testnet-api-key` with mode
`0600`; a deployed gateway receives it as a mode-`0400`, read-only secret file
or equivalent orchestrator secret. The gateway reads it at startup, does not
return it from diagnostics, and refuses to start if the file is absent,
group/world-readable, or malformed. Testnet and mainnet use different
identities, secret names, deployment roles, and configuration profiles.

Issuing, rotating, or revoking an API key is an audited operator procedure.
Replacement issuance has been exercised, but whether it invalidates an older
key remains unverified; the pool operator owns that confirmation and any needed
revocation in [issue #1](https://github.com/Daniel-T-S-Adams/tig-mining-pool-public/issues/1).
Until invalidation is confirmed, suspected exposure disables the gateway and
requires TIG operator coordination as well as replacement.

### 3.2 Runtime boundary

Only `tig-gateway` may read the TIG API key or send an authenticated TIG write.
It has:

- no public listener;
- no member-authentication or package-parsing code;
- a database role that may claim existing TIG intents and append attempts and
  results, but cannot create intents, change workflow state, or post ledger
  entries; and
- read-only access only to canonical derived payloads named by an intent.

The Pool API, controller, Artifact Worker, member agent, telemetry collector,
and operator dashboards never receive the key. Network policy permits TIG write
egress from the gateway identity only. A successful TIG HTTP response remains
an attempt, not confirmed state.

### 3.3 Production and mainnet prohibition before custody design

All current configurations have `mainnet_enabled = false` and accept only the
pinned testnet base URL and testnet player identity. No production signing key,
API key, payout key, or member funds may be introduced before a separate
production-custody decision. Production keys require hardware- or managed-key
protection, multi-person recovery, and a tested rotation/runbook.

### 3.4 Public-funds signing boundary

Before public deposits or payouts, the private Funds Gateway described in
[architecture.md](architecture.md) uses two different signer identities and
custody addresses: one holding all member value (`accounting.md` §11.7) and one
holding the pool's own operating funds. Neither is the reward wallet, whose key
is the pool's TIG protocol identity and signs no member transfer (§2.2 of
[architecture.md](architecture.md)). Neither key is available to the Pool API,
controller, TIG Gateway, database, CI, member software, or operator CLI.

The signer receives only immutable approved intents, allow-lists the Base chain
and TIG token contract, enforces transaction/rolling/hot-balance limits, and
records the nonce and signed transaction hash before broadcast. The
member-custody signer additionally requires the approved policy state for the
transfer's cause and **multi-person authorization for every transfer except a
member withdrawal within ADR 0009's per-member caps**. This section owns that
rule; `accounting.md` §12.4 points here rather than restating it, and
thresholds there govern operating custody only.

That control guarded every collateral movement before ADR 0008 merged the
pots, and merging them widened what one signature reaches rather than
narrowing it. It was therefore unconditional — every transfer, at any amount,
with no threshold — until ADR 0009, and the argument for keeping it that way
is recorded here rather than deleted: relaxing it weakens a check while the
reason to keep it grows.

**What ADR 0009 changed, and what it did not.** A member withdrawal is signed
with no human authorization when the amount is at or below the per-transaction
cap and the member's rolling seven-day total stays at or below the weekly cap;
both caps are per member and both are versioned policy. Every other transfer
out of member custody — a larger withdrawal, and `accounting.md` §8.6's sweep
of pool value to operating custody — keeps unconditional multi-person
authorization. Operating-custody thresholds are unchanged.

**What the rolling total counts, and who counts it.** This section owns both,
because a cap defined loosely is a cap with a bypass. The total counts every
withdrawal for that member **already signed or still pending completion**, not
only finalized ones: Base finality is minutes, and counting finalized
transfers alone would let a member open several inside that window and clear
the cap with each. The evaluation happens once, in the Controller accounting
projector at intent creation, and its result is stamped into the immutable
intent. The signer checks the stamp and never computes a member's history —
`architecture.md` §13 invariant 10 and §4's component table keep withdrawal
calculation out of the Funds Gateway, and §3.4's own rule that the signer
receives only immutable approved intents would not survive it doing arithmetic
over a member's recent activity.

The relaxation is bounded by what it can reach. Under ADR 0011 a member's
withdrawal destination is the wallet that authenticated the session and cannot
be changed, so the automated path can only ever move a member's own money to
that member's own address. A compromised member session is therefore not a
theft; a compromised *signing key* still is, and the caps do nothing about it,
because a key holder signs directly and the signer's limits are never
consulted. ADR 0009 records that the owner accepts that exposure for v0 and
has declined to bound it with a hot-wallet limit, which is the control named
in the paragraph above that would.

It signs exactly two transfer kinds: a member withdrawal to that member's
verified address, and `accounting.md` §8.6's sweep of pool value to the one
allow-listed operating-custody address. A finalized slash is a ledger
batch rather than a signed transfer — it reclassifies a liability to equity,
and only §8.6's sweep moves its tokens. The signer cannot create an intent,
change a destination or amount, or decide a slash.

One pot means this key's compromise reaches every member's balance at once;
`accounting.md` §11.7 records that cost and ADR 0008 records the decision. The
allow-list is what bounds it: the only non-member destination it can reach is
the pool's own operating custody.
Production custody and recovery use an approved hardware/managed or multisig
design and remain a public-launch gate; no such key enters the protocol spike.

## 4. Worker identity, authentication, and authorization

The exact Ed25519 messages, freshness window, headers, credential lifecycle,
and application idempotency keys are normative in
[member_protocol.md](member_protocol.md). The implementation adds these
enforcement rules.

### 4.1 Enrollment and credentials

- The Pool API stores enrollment and recovery tickets only as HMAC-SHA-256
  values using a dedicated server key; it never stores the bearer value.
- Ticket lookup, expiry check, one-time consumption, and worker/credential
  creation are one transaction.
- Worker public keys are ordinary database facts. A worker private key is
  generated and retained only on the member machine.
- Signature verification and exact body-hash verification occur before JSON
  decoding, database work, or upload quota reservation.
- Revocation is checked on every authenticated request and is not hidden behind
  a long-lived authorization cache.
- Authentication failures do not reveal whether a worker, credential, or
  resource ID exists.

### 4.2 Resource authorization

Authorization is relational, not inferred from an opaque UUID. For every
request the API:

1. authenticates the active `credential_id` and obtains its stored `worker_id`;
2. requires any path/body worker ID to be exactly that worker;
3. resolves slot, offer, assignment, package, upload, or receipt through a query
   constrained by that worker ID;
4. derives the member from the stored worker binding and never trusts a
   member-supplied `member_id`; and
5. returns the same non-enumerating denial for absent and cross-worker objects.

Account and operator credentials are separate from worker credentials. A
worker key cannot perform worker recovery, link a payout wallet, request or
alter a withdrawal, or invoke an operator endpoint. Operator access cannot
impersonate a worker request; recovery is an explicit audited action.

### 4.3 Request abuse controls

The edge enforces TLS, header/body timeouts, maximum header count and size,
connection limits, and rate limits by source IP. After authentication, the API
also limits by member, worker, credential, and endpoint class. Authentication,
enrollment, qualification, control messages, upload-session creation, and chunk
traffic have separate budgets so large uploads cannot starve heartbeats.

Limits return stable `429`/`Retry-After` responses where retry is safe. A rate
limit never mutates mining trust by itself. Repeated signature probing,
cross-worker access, or quota evasion creates a security event and can place the
credential or account into an explicit security suspension.

### 4.4 Mining admission and circuit breakers

For public work, the Controller reserves the bundle-scaled collateral defined
in [accounting.md](accounting.md) before creating a precommit intent. Deposit
size never bypasses slot qualification, tier `k`'s `k`-unverified limit, or the
pool-wide `internal_pool_unverified_limit`. A paid tier grants no guaranteed
work when the pool is full.

When only the global limit blocks an otherwise eligible offer, the pool keeps a
renewable FIFO capacity offer. A queued offer contains no protocol commitment
or collateral reservation. Promotion requires a fresh signed availability
confirmation and an atomic recheck of the tier, member count, collateral,
compatibility, and global count. Stale offers expire without a trust penalty.

A confirmed member-caused tier failure atomically preserves the evidence,
freezes the reserved `X` pending the normal attribution/appeal path, increments
the round failure count, and checks correlated runtime/configuration failures.
It does not invent a separate reputation score or automatic first-failure ban.
The financial gate stops new work when collateral is insufficient; `f > k` or
round `U > V` removes tier `k` at round close. A confirmed method-verification
fraud or independent account-security event may still trigger an immediate
security suspension under its separate policy.

A cluster across otherwise independent members or one newly deployed runtime
trips the global compatibility breaker, because treating a pool software fault
as coordinated member fraud would be unsafe.

## 5. Untrusted package ingestion

### 5.1 Admission before bytes

The Pool API accepts an upload only when all of these are true:

- the signed worker owns the assignment and current package generation;
- the assignment is in an upload-eligible state and before its block deadline;
- declared compressed/uncompressed sizes and media type are within the exact
  assignment limits and protocol ceilings;
- the package and manifest SHA-256 strings are canonical;
- one accepted package does not already exist; and
- storage and concurrency quota can be reserved without crossing a hard limit.

The reservation covers simultaneous quarantine and accepted publication, plus
bounded derived/temp overhead. A finalized, rejected, expired, or abandoned
session releases quota exactly once. New sessions stop before exhaustion; the
service does not rely on an object store being financially or physically
unlimited.

### 5.2 Chunk transport

Every chunk has an exact offset, positive bounded `Content-Length`, and SHA-256.
The body is streamed through the hash; it is never buffered at the declared
maximum. The deterministic quarantine object is made durable before the
database advances the committed offset. An identical old range is idempotent;
a different byte range or checksum at that offset is a terminal conflict for
that upload generation.

Slow-body deadlines, per-worker active-chunk limits, and global in-flight byte
limits prevent slow uploads from holding all sockets or memory. Authentication
is rechecked for every chunk and status request. Object keys are generated by
the pool from validated IDs and never contain a member path.

### 5.3 Bounded Zstandard and tar parsing

The Artifact Worker treats the complete byte stream as hostile and runs in a
separate process identity with no TIG credential. It:

- verifies the compressed byte count and whole SHA-256 before `RECEIVED`;
- permits one Zstandard frame, requires its checksum and exact declared content
  size, rejects dictionaries, skippable frames, trailing frames/data, and a
  decompression window above 64 MiB;
- counts decompressed bytes while streaming and aborts before the
  assignment-specific uncompressed limit;
- parses POSIX `ustar` itself or through a narrowly configured streaming parser;
- accepts exactly the four root regular files in the required order;
- rejects directories, duplicate/unknown names, absolute paths, `..`,
  backslashes, links, devices, FIFOs, sparse files, PAX/GNU extensions, and
  non-zero trailing data; and
- never calls a general-purpose extraction function or creates a
  member-selected filesystem path.

The member agent must produce a Zstandard frame with a window no greater than
64 MiB. This bound is part of proof-material format v1 and is normative in the
member protocol and its schema companion.

### 5.4 Bounded record and integrity validation

Each file and record is processed under both its declared and observed limit.
The worker rejects integer overflow before multiplication or allocation and
checks:

- manifest size, UTF-8, canonical JSON, schema, and identity digest;
- exact network, benchmark, assignment, member/worker/slot generation,
  challenge, algorithm, selected track, settings, binary, runtime, verifier,
  nonce range, and format versions;
- `qualities.i32le` length of exactly `4 * num_nonces` and valid signed values;
- `leaf-hashes.bin` length of exactly `32 * num_nonces`;
- exactly one bounded NDJSON record for every nonce in ascending order, with no
  duplicates, gaps, unknown fields, or lossy integers;
- every declared file length and SHA-256 while streaming;
- every reproduced TIG output leaf hash; and
- the Merkle root reconstructed from the ordered leaves.

Merkle and index data use bounded arrays, memory mapping, or external-memory
construction; declared nonce counts never directly drive an unchecked
allocation. A single output record may not exceed its assignment limit or the
16 MiB ceiling. Parser error messages name the field and byte/nonce position
but never copy the raw solution into an error.

These checks establish safe structure and internal consistency only. The worker
does not execute a supplied solution or claim semantic validity; TIG remains the
protocol verifier.

### 5.5 Resource isolation and exhaustion response

Ingestion, commitment construction, and proof construction have separate
bounded queues and semaphores. The spike begins with one concurrent structural
ingestion and one urgent proof job per Artifact Worker process; proof work has
deadline priority but cannot consume an unbounded number of threads. The spike
records measurements before these values change.

The process has explicit container/cgroup CPU, memory, open-file, process, and
scratch limits. It streams rather than extracts, does not spawn child processes,
does not make arbitrary network requests, and cannot write outside quarantine,
accepted, derived, and its dedicated scratch locations. Exceeding a limit
terminates the job with a typed result; an out-of-memory/process death is
reclaimed through the fenced lease and cannot commit a stale success.

At configured storage high-water marks the pool stops new upload sessions and
assignments, continues already accepted proof work, alerts the operator, and
returns retryable capacity errors where deadlines permit. It never deletes an
accepted artifact early to free space.

### 5.6 Semantic-screening experiment

Mechanical acceptance remains separate from TIG's authoritative semantic
verdict. The pool has not yet decided that it should semantically validate
member work before every benchmark commitment. The spike will measure two
candidate defenses after durable package receipt:

1. run the pinned challenge solution verifier over every returned output, if
   its measured worst-case cost fits the protocol reserve; and
2. privately select nonces after receipt, re-execute the assigned algorithm in
   the exact pinned runtime, and compare output/runtime evidence to screen for
   method non-reproducibility.

The second check is probabilistic and cannot replace collateral or TIG. Its
sample seed must be unpredictable to the member until the complete package is
durable, then retained for audit. A local mismatch holds the commitment and
triggers independent reproduction; it is not by itself enough to finalize a
slash. The result will inform whether either check is required, sampled, or
omitted. If a check is proposed for production, its measured cost, detection
bound, deadline impact, and residual risk must be reviewed explicitly.

These checks run in a separate sandboxed semantic-verifier worker after the
durable receipt. They neither change structural acceptance nor keep the
member's compute slot occupied; the member may already be computing its next
assignment while the pool screens the accepted package.

## 6. Safe local files and object publication

Local storage directories are owned by the Artifact Worker or Pool API service
identity as appropriate, mode `0700`, on a dedicated filesystem. Files are
opened relative to a pre-opened directory using no-follow, create-exclusive
semantics; an existing unexpected file, link, mount crossing, or non-regular
type fails closed. Temporary names are random pool values, not member fields.

Accepted filesystem publication requires a complete verified write, file
`fsync`, atomic same-filesystem rename to a deterministic immutable name, and
parent-directory `fsync`. S3 publication uses a pool-generated key, provider
part checksums, complete-object size and protocol SHA-256 metadata, conditional
non-overwrite behavior where supported, and a strongly consistent HEAD check.
Multipart ETags are not content hashes.

After publication, only the controller may commit the transaction containing
the artifact reference, exact hashes and sizes, assignment state, slot release,
and immutable receipt. Before that transaction the object is an unattached
candidate. Deterministic keys and an orphan grace period make every crash point
retryable without treating an object as accepted merely because it exists.

Only the Artifact Worker may physically delete data, and only from an
idempotent controller-issued deletion job naming the expected artifact and
version. The accepted prefix has no automatic age rule that can race protocol
retention. Quarantine and incomplete multipart cleanup still checks that no
live upload or accepted reference exists.

## 7. Replay and duplicate-effect prevention

| Boundary | Replay defense |
|---|---|
| Enrollment/recovery | Random single-use ticket, keyed hash, 15-minute expiry, atomic consumption, enrollment request ID |
| Signed worker HTTP | Exact Ed25519 signed method/path/version/identity/request ID/timestamp/body hash; 300-second skew; request IDs remembered 24 hours |
| Member commands | Application key and canonical body hash retained for the entire workflow; changed body conflicts |
| Events and acknowledgements | Event/ack ID, strict sequence/revision, permanent assignment binding |
| Upload chunks | Upload ID, exact committed offset/range, content length, chunk hash, immutable quarantine object |
| Package acceptance | Package ID/generation, one accepted-package constraint, assignment revision, fenced result, immutable receipt |
| Internal jobs | Unique generation, lease expiry, incrementing fence token, compare-and-set result |
| TIG writes | Immutable intent and payload hash, unique workflow/write generation, serialized precommit lane, attempt-before-send, confirmed-state reconciliation before retry |
| Operator commands | Authenticated command ID, actor, reason, expected revision, policy check, recorded outcome |

A lost response is recovered by reading authoritative state. Neither HTTP
success nor process memory is the idempotency record.

## 8. Logging, metrics, traces, and error hygiene

Telemetry uses an allow-list of compact fields. The following are prohibited
from ordinary logs, traces, metrics, exception strings, support bundles, and
crash reports:

- TIG API keys and account/payout or deposit-custody private keys;
- database, object-store, TLS, HMAC, enrollment, recovery, or session secrets;
- authorization/signature headers and full request/response bodies;
- raw package chunks, manifests containing unnecessary member data, solutions,
  quality vectors, Merkle leaves/branches, or canonical TIG submission bodies;
- wallet-signature challenges before expiry; and
- unbounded aliases, URLs, paths, or parser-controlled strings.

Permitted correlation fields are opaque IDs, block/round numbers, state names,
byte counts, durations, status classes, typed reason codes, and SHA-256 values
that are already protocol identifiers. Member IDs remain logs/traces rather
than metric labels.

Redaction is applied before serialization and tested with canary secrets. HTTP
middleware does not log headers or bodies by default. Panic hooks and dependency
debug formatting are reviewed so they cannot dump configuration or payloads.
Access to centralized security/audit logs is read-only and role restricted.

## 9. Audit events

Security and financial audit events are append-only facts. Every event records:

```text
audit_event_id and occurred_at
deployment and network
actor_type and actor_id
action and outcome
resource_type and resource_id
request_id / command_id / intent_id where applicable
prior_state and resulting_state where applicable
typed reason and compact evidence hashes
TIG block/round and configuration version where applicable
operator reason and approval identities where applicable
originating trace_id
```

Raw secrets and solutions are never audit evidence. When bulky evidence is
needed, the event stores its content hash and access-controlled reference.

At minimum, audit these events:

- enrollment ticket creation/consumption, credential creation/rotation/
  recovery/revocation, authentication replay, and cross-resource denial;
- slot registration/reconfiguration and qualification outcome;
- offer admission/rejection, decision input digest, precommit intent,
  confirmed assignment ownership, member acknowledgement, cancellation, and
  deadline outcome;
- upload reservation, range conflict, whole-package receipt, structural
  acceptance/rejection, immutable publication, durable receipt, early loss,
  deletion eligibility, deletion attempt, and deletion result;
- every TIG write intent, canonical payload hash, transport attempt, ambiguous
  result, reconciliation, and confirmed state;
- terminal outcome and `MEMBER`/`POOL`/`TIG`/`UNRESOLVED` fault attribution,
  including any later correction;
- qualifier attribution and accounting batch/correction identifiers;
- deposit recognition, freeze, appeal, finalized slash, and withdrawal;
- round settlement, withdrawal request, withdrawal or custody-sweep intent,
  signing attempt, ambiguous result, finalized transfer, and emergency
  withdrawal hold;
- configuration or fee-policy activation;
- operator command request, approval, application, rejection, and emergency
  write disable/enable; and
- secret provisioning, rotation, suspected exposure, and gateway authentication
  failure without recording the secret itself.

Ordinary application roles cannot update or delete audit rows. Retention and
export policy is finalized before public membership; spike audit facts remain
for the life of the test environment and its report.

## 10. Operator and dependency controls

The testnet operator endpoint is private, bound to loopback or the private
network, and protected by a distinct operator identity. Remote access uses the
approved administrative tunnel and mutual authentication; it is never exposed
through member routes. `pool-admin` requires command ID, explicit action,
reason, and expected revision. Operators cannot edit workflow or ledger tables
directly.

Dependencies and container images are pinned, locked, and scanned during the
repository-preparation section. Package parsers, signature verification,
integer conversion, Merkle construction, and idempotency paths receive
deterministic malformed fixtures and fuzz/property tests. A dependency upgrade
that changes serialization, decompression, tar parsing, cryptography, or TIG
schemas is compatibility work, not an automatic patch deployment.

The service exposes separate liveness and readiness. Missing database/storage,
TIG schema incompatibility, missing/unsafe gateway secret, corrupt accepted
artifact, stale snapshot, storage high-water mark, and accounting mismatch
produce the fail-closed states and alerts specified in the architecture.

## 11. Security invariants

1. A testnet process never receives a production credential or mainnet-enabled
   configuration.
2. The TIG signing key never enters a pool runtime, repository, CI system, log,
   or agent session. It has exactly two manual uses, both in
   `architecture.md` section 2.2: API-key provisioning or rotation, and
   `accounting.md` §8.3a's per-round reward-wallet sweep. On testnet an
   operator browser wallet may perform both — ADR 0008 records the judgement
   that the value a sweep moves there is testnet TIG only, which keeps it
   inside ADR 0007's bound. Production signing remains offline or
   hardware-/managed-key protected for both uses. Only the TIG
   Gateway receives the testnet API key, and only a separately deployed Funds
   Gateway can receive a production custody signer.
3. A worker credential authorizes exactly one stored worker and its descendant
   resources.
4. Authentication precedes parsing or storage reservation.
5. Member-controlled names never become filesystem paths, object keys, log
   templates, or network destinations.
6. Compressed, decompressed, file, record, nonce, memory, concurrency, storage,
   and time consumption all have enforced bounds.
7. Package parsing streams exact allowed formats and never general-purpose
   extracts or executes member content.
8. Mechanical checks do not become a false claim of semantic correctness.
9. `RECEIVED` and `STRUCTURALLY_ACCEPTED` release neither slot nor member
   retention obligation.
10. A durable receipt cannot exist without the verified immutable object and
    atomic workflow reference.
11. An accepted artifact is never deleted to relieve capacity before confirmed
    retention eligibility.
12. Every replayable boundary has an application idempotency key and durable
    result.
13. Secrets and raw solutions cannot appear in ordinary telemetry.
14. Every protocol write, durable receipt, terminal attribution, accounting
    batch, artifact deletion, and operator override is auditable.
15. Public precommits require both the assignment's full financial-risk reserve
    and available member-tier/global unverified capacity; neither substitutes for the
    other.
16. A queued capacity offer consumes no TIG unverified position and can never
    bypass a fresh atomic admission check.
17. Tier removal or repurchase cannot erase a benchmark, financial exposure,
    failure record, or security evidence.
16. One semantic failure stops further exposure before it becomes an automatic
    slash, and correlated failures are investigated as a compatibility incident.
