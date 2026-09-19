-- Slice 2, criteria B1-B9 and L2: the member account, its workers, and their
-- credentials.
--
-- Slice 1 ran with a sentinel where a member should be: `pool.workflow`'s
-- `owner_kind = 'POOL_BOOTSTRAP'`, bounded to testnet, existing so that
-- `mining_system.md` §10 invariant 1's permanent benchmark → owner mapping had
-- something to point at before members existed. These are the rows that make an
-- owner real. Nothing here changes `pool.workflow`; the offer-driven path that
-- writes `owner_kind = 'MEMBER'` arrives with slice 2's admission work, and E6
-- is where the sentinel stops being creatable.
--
-- Four tables, in the order one needs the last:
--
--   pool.member                        the account, which is a wallet
--   pool.member_collateral_multiplier  ADR 0010's per-member scaling, versioned
--   pool.worker                        one installed member agent
--   pool.worker_credential             one Ed25519 public key for a worker
--   pool.enrollment_ticket             a one-time authority to create or
--                                      recover a worker
--
-- `member_protocol.md` §2 owns the identities and their issuers; §3.1 and §3.3
-- own enrollment, rotation, recovery and revocation; `security.md` §4.1 owns
-- what the pool may store. `architecture.md` §6 owns who may write each row,
-- and the grants at the bottom are that table expressed as privileges.

-- ---------------------------------------------------------------------------
-- The account
-- ---------------------------------------------------------------------------

-- ADR 0011: "A member signs in by connecting a wallet, and that wallet is where
-- their money goes." The Base address is the identity and the withdrawal
-- destination; there is no payout-destination setting and no operation that
-- changes one.
--
-- `member_id` stays pool-issued and opaque all the same, because
-- `member_protocol.md` §2 says it is ("One registered pool account. It is never
-- selected by the worker") and because every other table here refers to the
-- account by something stable and internal rather than by a chain address that
-- appears in signatures and logs. The address is a unique attribute of the row,
-- not its key.
CREATE TABLE pool.member (
    network        text        NOT NULL,
    member_id      uuid        NOT NULL,

    -- Lowercase hex with the 0x prefix, exactly as `accounting.md` §12.2's
    -- proof carries it. Case is not folded on read: a mixed-case address is
    -- refused rather than normalised, so two spellings of one account can never
    -- both be inserted while a UNIQUE index believes them different.
    wallet_address text        NOT NULL,

    -- `security.md` §4.3's explicit security suspension, which D3's admission
    -- reads as "account status". SUSPENDED stops new work; it does not erase
    -- state, release an outstanding reservation, or reassign a benchmark —
    -- `member_protocol.md` §16 invariant 16 and §3.3 both say removal of
    -- standing never releases exposure.
    state          text        NOT NULL DEFAULT 'ACTIVE',

    -- A suspension says when, why and who, for the same reason a worker's
    -- revocation does: `architecture.md` §13 invariant 6 wants an auditable
    -- result, and `security.md` §4.3's explicit security suspension is a
    -- decision someone has to be able to review and undo. A bare state flip
    -- would leave none of that.
    suspended_at      timestamptz,
    suspension_reason text,
    suspended_by      text,

    created_at     timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, member_id),

    CONSTRAINT member_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT member_state_known
        CHECK (state IN ('ACTIVE', 'SUSPENDED')),
    CONSTRAINT member_wallet_is_lowercase_base_address
        CHECK (wallet_address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT member_suspension_is_recorded_or_absent
        CHECK (
            (state = 'ACTIVE'
                AND suspended_at IS NULL
                AND suspension_reason IS NULL
                AND suspended_by IS NULL)
            OR
            (state = 'SUSPENDED'
                AND suspended_at IS NOT NULL
                AND suspension_reason IS NOT NULL
                AND suspended_by IS NOT NULL)
        ),
    CONSTRAINT member_suspension_reason_known
        CHECK (suspension_reason IS NULL
               OR suspension_reason IN ('SECURITY', 'OPERATOR', 'MEMBER_REQUEST'))
);

-- ADR 0011: "There is no payout-destination setting, and no operation that
-- changes one." The wallet is simultaneously the account identity and the
-- withdrawal destination, which makes it the one column in this migration where
-- an edit is theft rather than a mistake — so it is defended the way the worker
-- binding is, against the table owner and against any UPDATE a later slice
-- grants, not only against the role that exists today.
CREATE OR REPLACE FUNCTION pool.member_identity_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.member_id IS DISTINCT FROM OLD.member_id
       OR NEW.wallet_address IS DISTINCT FROM OLD.wallet_address
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'the wallet is the account and its destination; neither changes (ADR 0011)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER member_identity_immutable
    BEFORE UPDATE ON pool.member
    FOR EACH ROW
    EXECUTE FUNCTION pool.member_identity_is_immutable();

-- One account per address per network. Network-scoped like every other row in
-- this schema: the same person may hold the same wallet on testnet and mainnet,
-- and a testnet account must never authorize mainnet work.
CREATE UNIQUE INDEX member_wallet_is_the_account
    ON pool.member (network, wallet_address);

-- ADR 0010's multiplier, append-only.
--
-- The ADR calls it "versioned policy in the same append-only form as §5's fee
-- policy", so it is not a mutable column on `pool.member`. Each row is one
-- decision; the member's current multiplier is the highest `version`, and no
-- row for a member means `10_000` — the default, and the safe direction, since
-- a member with no recorded discount is collateralized in full.
--
-- `accounting.md` §11.4: "The value is read when the assignment reserve is
-- computed and fixed into that reservation; a later change never reaches an
-- open reservation, in either direction." That is why this table records
-- history rather than a current value: the reservation stores the bps it used,
-- and this table explains where that number came from.
CREATE TABLE pool.member_collateral_multiplier (
    network    text        NOT NULL,
    member_id  uuid        NOT NULL,
    version    bigint      NOT NULL,

    -- Integer basis points, never a fraction: `accounting.md` §3 admits only
    -- exact integer attoTIG arithmetic, and §11.4 says so of this value in
    -- particular. `10_000` means no discount; `5_000` halves the method term.
    -- Zero is not a discount, it is an uncollateralized member, so the range
    -- starts at 1.
    bps        integer     NOT NULL,

    -- Why the pool set it. Free text for an operator, not a machine value: the
    -- decision is the pool's and §13 item 21 makes it reportable, so a row
    -- without a reason would report nothing worth reading.
    reason     text        NOT NULL,

    -- Who decided. ADR 0010 rule 3 asks for "the value, when it takes effect,
    -- who set it, and why", in the append-only form `accounting.md` §5 uses for
    -- fee policy — and §5's form carries an actor. A discount that is
    -- append-only but unattributable keeps the history and loses the
    -- accountability half of it. Free text until the operator-command table
    -- exists, at which point this becomes its identifier.
    set_by     text        NOT NULL,

    -- When it takes effect, on the axis `accounting.md` §5 uses for the fee
    -- policy this is required to follow: a TIG height, not a wall clock. §5 is
    -- explicit that a policy "cannot be edited, backdated, or selected using
    -- processing time", and admission already reads its other inputs from the
    -- decision snapshot's block — so the multiplier in force for a decision is
    -- the highest version whose height is at or below that block's.
    --
    -- `recorded_at_height` is the pool's latest confirmed height when the
    -- version was written, and the CHECK below makes every version take effect
    -- strictly after it. That is what "not backdated" means with no wall clock
    -- involved: a change is scheduled ahead, never applied to blocks already
    -- decided.
    recorded_at_height    bigint NOT NULL,
    effective_from_height bigint NOT NULL,

    set_at     timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, member_id, version),

    CONSTRAINT member_multiplier_version_positive
        CHECK (version >= 1),
    CONSTRAINT member_multiplier_in_range
        CHECK (bps BETWEEN 1 AND 10000),
    CONSTRAINT member_multiplier_reason_not_blank
        CHECK (length(trim(reason)) > 0),
    CONSTRAINT member_multiplier_actor_not_blank
        CHECK (length(trim(set_by)) > 0),
    CONSTRAINT member_multiplier_heights_are_positive
        CHECK (recorded_at_height >= 0 AND effective_from_height >= 0),
    CONSTRAINT member_multiplier_is_not_backdated
        CHECK (effective_from_height > recorded_at_height),
    CONSTRAINT member_multiplier_has_a_member
        FOREIGN KEY (network, member_id)
        REFERENCES pool.member (network, member_id)
);

-- Append-only, enforced rather than intended. `accounting.md` §11.4 has the
-- reservation fix the value it used; a row edited afterwards would make the
-- history disagree with the reservations taken under it, which is the one
-- property this table exists to provide.
CREATE OR REPLACE FUNCTION pool.member_multiplier_is_append_only()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.member_collateral_multiplier is append-only (accounting.md §11.4); set a new version instead'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER member_multiplier_no_update
    BEFORE UPDATE OR DELETE ON pool.member_collateral_multiplier
    FOR EACH ROW
    EXECUTE FUNCTION pool.member_multiplier_is_append_only();

-- Versions and effective times advance together, so "the policy in force at T"
-- has one answer. Without this a later version could take effect earlier than
-- an older one, and two readers ordering by different columns would re-derive
-- two different reservations from the same history.
CREATE OR REPLACE FUNCTION pool.member_multiplier_versions_advance()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    previous record;
BEGIN
    SELECT version, effective_from_height INTO previous
      FROM pool.member_collateral_multiplier
     WHERE network = NEW.network AND member_id = NEW.member_id
     ORDER BY version DESC
     LIMIT 1;

    IF previous IS NULL THEN
        RETURN NEW;
    END IF;

    IF NEW.version <> previous.version + 1 THEN
        RAISE EXCEPTION
            'member multiplier versions are consecutive: expected %, got %',
            previous.version + 1, NEW.version
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.effective_from_height <= previous.effective_from_height THEN
        RAISE EXCEPTION
            'member multiplier version % takes effect at or before version %',
            NEW.version, previous.version
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER member_multiplier_ordered
    BEFORE INSERT ON pool.member_collateral_multiplier
    FOR EACH ROW
    EXECUTE FUNCTION pool.member_multiplier_versions_advance();

-- ---------------------------------------------------------------------------
-- Workers and credentials
-- ---------------------------------------------------------------------------

-- `member_protocol.md` §2: one installed member agent, "permanently scoped to
-- one member unless an audited worker-recovery action says otherwise" — and
-- recovery attaches a new key to the *same* worker under the same member, so
-- the binding below never moves. §3.3: "Revoking a worker ... does not erase
-- state or reassign a benchmark."
CREATE TABLE pool.worker (
    network   text        NOT NULL,
    worker_id uuid        NOT NULL,
    member_id uuid        NOT NULL,

    -- §3.1: the enrollment response "records the exact negotiated protocol
    -- version". §4 keeps supporting it for an open assignment even after the
    -- pool stops offering it, so it is a fact of the worker and not a lookup.
    protocol_version text   NOT NULL,

    state     text        NOT NULL DEFAULT 'ACTIVE',

    -- §3.1's idempotency, kept on the row that is the result rather than in a
    -- generic ledger: "Retrying an identical enrollment request with the same
    -- `enrollment_request_id` returns the same response; changing any field for
    -- that ID is a conflict." The hash is over the canonical request body, so
    -- "identical" is decided by bytes and not by field-by-field comparison
    -- (`member_protocol.md` §5).
    enrollment_request_id     uuid  NOT NULL,
    enrollment_request_sha256 bytea NOT NULL,

    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,

    -- Why revocation happened, because §3.3 makes the two reasons behave
    -- differently: an accidental revocation may be undone by worker recovery,
    -- and a security revocation may not — "only an explicit, audited pool
    -- decision reinstates it".
    revocation_reason text,

    PRIMARY KEY (network, worker_id),

    CONSTRAINT worker_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT worker_state_known
        CHECK (state IN ('ACTIVE', 'REVOKED')),
    CONSTRAINT worker_revocation_is_recorded_or_absent
        CHECK (
            (state = 'ACTIVE'  AND revoked_at IS NULL     AND revocation_reason IS NULL)
            OR
            (state = 'REVOKED' AND revoked_at IS NOT NULL AND revocation_reason IS NOT NULL)
        ),
    CONSTRAINT worker_revocation_reason_known
        CHECK (revocation_reason IS NULL
               OR revocation_reason IN ('MEMBER_REQUEST', 'OPERATOR', 'SECURITY')),
    CONSTRAINT worker_sha256_is_32_bytes
        CHECK (length(enrollment_request_sha256) = 32),
    CONSTRAINT worker_has_a_member
        FOREIGN KEY (network, member_id)
        REFERENCES pool.member (network, member_id)
);

-- One enrollment request creates one worker. Without this, a retry racing
-- itself creates two workers for one ticket, and §3.1's "consuming it and
-- creating the worker are one transaction" would hold while still producing a
-- duplicate.
CREATE UNIQUE INDEX worker_enrollment_request_is_idempotent
    ON pool.worker (network, enrollment_request_id);

CREATE INDEX worker_by_member
    ON pool.worker (network, member_id);

-- Redundant as a uniqueness claim — `worker_id` is already unique — and not
-- redundant as a foreign-key target. It is what lets `pool.enrollment_ticket`
-- reference `(network, member_id, worker_id)` and so be unable to name a worker
-- belonging to a different member, which is the difference between the ticket
-- chain being checked by the database and being checked by code that does not
-- exist yet.
CREATE UNIQUE INDEX worker_identity_is_member_scoped
    ON pool.worker (network, member_id, worker_id);

-- `member_protocol.md` §2: a worker is "permanently scoped to one member", and
-- §3.3 says recovery "preserves access to the worker's existing assignments
-- without changing ownership". `mining_system.md` §10 invariant 1 and §8 charge
-- faults through that binding, so it is enforced here the way
-- `migrations/0006` enforces `pool.workflow`'s owner: a trigger, not a comment.
--
-- The column grants below already stop the Pool API from reaching these
-- columns. This holds against every other writer too, including the table
-- owner and any role a later slice grants — which is the case a grant cannot
-- cover.
CREATE OR REPLACE FUNCTION pool.worker_binding_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.member_id IS DISTINCT FROM OLD.member_id
       OR NEW.worker_id IS DISTINCT FROM OLD.worker_id
       OR NEW.network IS DISTINCT FROM OLD.network
       OR NEW.enrollment_request_id IS DISTINCT FROM OLD.enrollment_request_id
       OR NEW.enrollment_request_sha256 IS DISTINCT FROM OLD.enrollment_request_sha256
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'a worker is permanently scoped to one member (member_protocol.md §2)'
            USING ERRCODE = 'raise_exception';
    END IF;

    -- A recorded revocation is fixed while it stands. Without this the
    -- security guard below is two statements away from useless: rewrite
    -- `revocation_reason` to `MEMBER_REQUEST` on the still-REVOKED row, then
    -- reinstate. Both statements satisfy every CHECK, and the second one would
    -- see a reason that is no longer the one recorded.
    IF OLD.state = 'REVOKED' AND NEW.state = 'REVOKED'
       AND (NEW.revocation_reason IS DISTINCT FROM OLD.revocation_reason
            OR NEW.revoked_at IS DISTINCT FROM OLD.revoked_at)
    THEN
        RAISE EXCEPTION
            'a revocation''s reason and time are fixed while it stands (member_protocol.md §3.3)'
            USING ERRCODE = 'raise_exception';
    END IF;

    -- §3.3: "A worker revoked as a security action cannot be recovered by this
    -- path; only an explicit, audited pool decision reinstates it." That
    -- decision needs the operator-command path, which does not exist yet, so
    -- reinstatement after a security revocation is refused outright rather
    -- than left to whoever holds an UPDATE. An accidental revocation is
    -- reversible, which is what §3.3 allows.
    IF OLD.state = 'REVOKED' AND NEW.state = 'ACTIVE'
       AND OLD.revocation_reason = 'SECURITY'
    THEN
        RAISE EXCEPTION
            'a security revocation is reinstated only by an audited pool decision (member_protocol.md §3.3)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER worker_binding_immutable
    BEFORE UPDATE ON pool.worker
    FOR EACH ROW
    EXECUTE FUNCTION pool.worker_binding_is_immutable();

-- §2: "One Ed25519 public key authorized for a worker. More than one may exist
-- briefly during rotation." §3.3 bounds that overlap: the old credential stays
-- valid "for a server-declared grace period no longer than 10 minutes, then
-- becomes `REVOKED`".
CREATE TABLE pool.worker_credential (
    network       text        NOT NULL,
    credential_id uuid        NOT NULL,
    worker_id     uuid        NOT NULL,

    -- The raw 32-byte Ed25519 public key. A public key is an ordinary database
    -- fact (`security.md` §4.1); the private half is generated on the member
    -- machine and never reaches the pool (§3.1, B-group criterion K1).
    public_key    bytea       NOT NULL,

    state         text        NOT NULL DEFAULT 'ACTIVE',

    -- When this credential stops being accepted, and when that grace began.
    -- Both are set by the rotation that starts the grace on the *old*
    -- credential, and NULL means "until revoked".
    --
    -- Two columns because the ten-minute bound in §3.3 is measured from the
    -- rotation, not from enrollment. Bounding `not_after` against `created_at`
    -- — the obvious single-column form, and what this migration had first —
    -- makes the column unusable for the case it exists for: a credential
    -- enrolled a week ago can be rotated today, and no grace ending in the
    -- future is within ten minutes of its creation.
    grace_started_at timestamptz,
    not_after        timestamptz,

    -- §3.3's rotation idempotency: "Repeating the same `rotation_id` and new
    -- public key returns the same result. A different key for the same rotation
    -- ID is a conflict." NULL on the credential created by enrollment, which
    -- had no rotation.
    rotation_id   uuid,

    created_at    timestamptz NOT NULL DEFAULT now(),
    revoked_at    timestamptz,

    PRIMARY KEY (network, credential_id),

    CONSTRAINT credential_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT credential_state_known
        CHECK (state IN ('ACTIVE', 'REVOKED')),
    CONSTRAINT credential_revocation_is_recorded_or_absent
        CHECK (
            (state = 'ACTIVE'  AND revoked_at IS NULL)
            OR
            (state = 'REVOKED' AND revoked_at IS NOT NULL)
        ),
    CONSTRAINT credential_public_key_is_ed25519_sized
        CHECK (length(public_key) = 32),
    -- §3.3 bounds the rotation grace at ten minutes, measured from when the
    -- rotation declared it. Stored rather than computed so the grace a rotation
    -- promised survives a change to the configured grace — which is exactly the
    -- case where an out-of-range value would otherwise persist.
    CONSTRAINT credential_grace_is_recorded_whole
        CHECK ((not_after IS NULL) = (grace_started_at IS NULL)),
    CONSTRAINT credential_grace_is_bounded
        CHECK (not_after IS NULL
               OR (not_after > grace_started_at
                   AND not_after <= grace_started_at + interval '10 minutes')),
    CONSTRAINT credential_has_a_worker
        FOREIGN KEY (network, worker_id)
        REFERENCES pool.worker (network, worker_id)
);

-- A key authorizes exactly one worker. Two workers sharing a public key would
-- make `security.md` §4.2's rule — "authenticate the active `credential_id` and
-- obtain its stored `worker_id`" — ambiguous at the one point where ambiguity
-- is authorization.
CREATE UNIQUE INDEX credential_key_belongs_to_one_worker
    ON pool.worker_credential (network, public_key);

-- One rotation produces one credential, so a retried rotation cannot mint a
-- second key. Partial because enrollment's credential has no rotation.
CREATE UNIQUE INDEX credential_rotation_is_idempotent
    ON pool.worker_credential (network, worker_id, rotation_id)
    WHERE rotation_id IS NOT NULL;

CREATE INDEX credential_by_worker
    ON pool.worker_credential (network, worker_id, state);

-- `security.md` §4.2 authenticates a credential and reads its stored worker, so
-- an editable key or owner is an editable authorization. Rotation and recovery
-- both work by *adding* a credential and revoking the old one (§3.3); neither
-- rewrites one in place, so nothing legitimate needs these columns to move.
CREATE OR REPLACE FUNCTION pool.credential_identity_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.public_key IS DISTINCT FROM OLD.public_key
       OR NEW.worker_id IS DISTINCT FROM OLD.worker_id
       OR NEW.network IS DISTINCT FROM OLD.network
       OR NEW.credential_id IS DISTINCT FROM OLD.credential_id
       OR NEW.rotation_id IS DISTINCT FROM OLD.rotation_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       -- A grace is declared once. Moving either end afterwards is how ten
       -- minutes becomes an hour one UPDATE at a time.
       OR (OLD.grace_started_at IS NOT NULL
           AND (NEW.grace_started_at IS DISTINCT FROM OLD.grace_started_at
                OR NEW.not_after IS DISTINCT FROM OLD.not_after))
    THEN
        RAISE EXCEPTION
            'a credential''s key and owner are fixed; rotate by adding one (member_protocol.md §3.3)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF OLD.state = 'REVOKED' AND NEW.state = 'ACTIVE' THEN
        RAISE EXCEPTION
            'a revoked credential is never reactivated (member_protocol.md §3.3)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER credential_identity_immutable
    BEFORE UPDATE ON pool.worker_credential
    FOR EACH ROW
    EXECUTE FUNCTION pool.credential_identity_is_immutable();

-- ---------------------------------------------------------------------------
-- Tickets
-- ---------------------------------------------------------------------------

-- `security.md` §4.1: "The Pool API stores enrollment and recovery tickets only
-- as HMAC-SHA-256 values using a dedicated server key; it never stores the
-- bearer value." There is therefore no column here for the secret, and the
-- absence is the point rather than an omission: a table that could hold it
-- would eventually hold it.
--
-- `member_protocol.md` §3.1 and §3.3: single-use, bound to one member and
-- purpose, 15 minutes, and consuming it happens in the same transaction that
-- creates or recovers the worker.
CREATE TABLE pool.enrollment_ticket (
    network     text        NOT NULL,

    -- The HMAC of the bearer value, which is also the lookup key: a ticket is
    -- presented, hashed, and matched. Nothing else identifies a ticket, so a
    -- stolen row leaks no usable authority.
    ticket_hmac bytea       NOT NULL,

    member_id   uuid        NOT NULL,

    -- §3.1 binds a ticket to one purpose. WORKER_ENROLLMENT creates a worker;
    -- WORKER_RECOVERY attaches a new key to the one named below. A ticket
    -- accepted for the other purpose would let an enrollment link a key to an
    -- existing worker, which is the attack the two domains in §3.1 and §3.3
    -- separate.
    purpose     text        NOT NULL,

    -- The worker a recovery ticket is "bound to its exact `worker_id` and
    -- rejected for any other" (§3.3). NULL for enrollment, which has no worker
    -- yet.
    worker_id   uuid,

    expires_at  timestamptz NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),

    -- Single use. The consuming transaction sets both, and its WHERE clause
    -- requires `consumed_at IS NULL`, so two concurrent redemptions cannot both
    -- succeed.
    consumed_at timestamptz,
    consumed_by_worker_id uuid,

    PRIMARY KEY (network, ticket_hmac),

    CONSTRAINT ticket_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT ticket_purpose_known
        CHECK (purpose IN ('WORKER_ENROLLMENT', 'WORKER_RECOVERY')),
    CONSTRAINT ticket_hmac_is_32_bytes
        CHECK (length(ticket_hmac) = 32),
    CONSTRAINT ticket_recovery_names_its_worker
        CHECK ((purpose = 'WORKER_RECOVERY') = (worker_id IS NOT NULL)),
    CONSTRAINT ticket_consumption_is_recorded_whole
        CHECK ((consumed_at IS NULL) = (consumed_by_worker_id IS NULL)),
    -- §3.1 and §3.3: fifteen minutes. A lifetime the row does not bound is a
    -- lifetime enforced only by whoever wrote the INSERT.
    CONSTRAINT ticket_lifetime_is_bounded
        CHECK (expires_at <= created_at + interval '15 minutes'),
    -- And the expiry has to bound redemption, not just issuance. `security.md`
    -- §4.1 puts "lookup, expiry check, one-time consumption" in one
    -- transaction; the row refusing a late consumption is what makes the
    -- expiry a property of the ticket rather than of that transaction.
    CONSTRAINT ticket_is_consumed_before_it_expires
        CHECK (consumed_at IS NULL OR consumed_at <= expires_at),
    -- A recovery ticket is consumed by the worker it names, and by no other.
    -- §3.3 binds it "to its exact `worker_id`", so a consumption recorded
    -- against a different worker of the same member is a recovery of something
    -- the ticket did not authorize.
    CONSTRAINT ticket_recovery_is_consumed_by_its_own_worker
        CHECK (purpose <> 'WORKER_RECOVERY'
               OR consumed_by_worker_id IS NULL
               OR consumed_by_worker_id = worker_id),
    CONSTRAINT ticket_has_a_member
        FOREIGN KEY (network, member_id)
        REFERENCES pool.member (network, member_id),

    -- The recovery ticket's worker belongs to the ticket's member. Composite
    -- rather than a plain `worker_id` reference, and this is the whole point:
    -- §3.3 has consuming a recovery ticket attach a new key to the named worker
    -- and revoke every old credential, so a ticket able to name *another
    -- member's* worker is a path to that member's agent. MATCH SIMPLE leaves
    -- enrollment tickets, whose `worker_id` is NULL, unconstrained.
    CONSTRAINT ticket_worker_belongs_to_its_member
        FOREIGN KEY (network, member_id, worker_id)
        REFERENCES pool.worker (network, member_id, worker_id),

    -- And the worker that consumed it is that member's too — for an enrollment
    -- ticket that is the worker just created, which is the only thing §3.1's
    -- one transaction may produce.
    CONSTRAINT ticket_consumer_belongs_to_its_member
        FOREIGN KEY (network, member_id, consumed_by_worker_id)
        REFERENCES pool.worker (network, member_id, worker_id)
);

-- What a ticket is cannot change after it is issued, and a spent one cannot be
-- re-armed. `security.md` §4.1 and `member_protocol.md` §3.1 state single use,
-- the fifteen-minute lifetime and the purpose binding as properties of the
-- ticket; with only a grant behind them they would be properties of the UPDATE
-- statement that happened to be written.
CREATE OR REPLACE FUNCTION pool.ticket_terms_are_fixed()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.ticket_hmac IS DISTINCT FROM OLD.ticket_hmac
       OR NEW.member_id IS DISTINCT FROM OLD.member_id
       OR NEW.purpose IS DISTINCT FROM OLD.purpose
       OR NEW.worker_id IS DISTINCT FROM OLD.worker_id
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'a ticket''s terms are fixed when it is issued (member_protocol.md §3.1)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF OLD.consumed_at IS NOT NULL
       AND (NEW.consumed_at IS DISTINCT FROM OLD.consumed_at
            OR NEW.consumed_by_worker_id IS DISTINCT FROM OLD.consumed_by_worker_id)
    THEN
        RAISE EXCEPTION
            'a consumed ticket is never re-armed or re-attributed (member_protocol.md §3.1: single use)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER ticket_terms_fixed
    BEFORE UPDATE ON pool.enrollment_ticket
    FOR EACH ROW
    EXECUTE FUNCTION pool.ticket_terms_are_fixed();

-- ---------------------------------------------------------------------------
-- Grants (architecture.md §6, slice-2 criterion L2)
-- ---------------------------------------------------------------------------

-- The Pool API owns every row above: §6 gives it "Enroll or rotate worker
-- credential", "Recover/revoke worker credential", and the account system that
-- authenticates a member. It creates a member on first sign-in and never
-- updates one — the wallet is the identity and there is nothing else on the row
-- to change but `state`, which is a security action the operator path applies
-- through the controller.
-- The UPDATEs are column-scoped, for the reason `migrations/0010` scoped its
-- own: a table-wide grant would let this role rewrite the columns the triggers
-- above defend, and a refusal is better delivered at the privilege than at the
-- exception. The two together are deliberate — the grant says what the role
-- does, the trigger says what nobody does.
GRANT SELECT, INSERT ON pool.member TO pool_api;
GRANT SELECT, INSERT ON pool.worker TO pool_api;
GRANT UPDATE (state, revoked_at, revocation_reason) ON pool.worker TO pool_api;
GRANT SELECT, INSERT ON pool.worker_credential TO pool_api;
GRANT UPDATE (state, grace_started_at, not_after, revoked_at)
    ON pool.worker_credential TO pool_api;
GRANT SELECT, INSERT ON pool.enrollment_ticket TO pool_api;
GRANT UPDATE (consumed_at, consumed_by_worker_id) ON pool.enrollment_ticket TO pool_api;

-- The controller reads the account to admit work (D3's account status, D11's
-- per-member bound, D13's multiplier) and to attribute a benchmark to its
-- owner. It does not create members or workers: that is the API's boundary, and
-- `architecture.md` §13 invariant 6 gives one operation one owner.
GRANT SELECT ON pool.member TO pool_controller;
GRANT SELECT ON pool.worker TO pool_controller;
GRANT SELECT ON pool.worker_credential TO pool_controller;

-- Suspension is an audited operator command applied by the controller (§6,
-- "Apply an administrative override"), so the columns the API may not change
-- are the ones the controller may — the state and the record of why it moved,
-- which the CHECK above requires to travel together.
GRANT UPDATE (state, suspended_at, suspension_reason, suspended_by)
    ON pool.member TO pool_controller;

-- The multiplier is the pool's decision, applied through the same operator
-- path, and read by admission. Nothing else writes it and the API never sees
-- it: a member's collateral terms are not member-facing data.
GRANT SELECT, INSERT ON pool.member_collateral_multiplier TO pool_controller;

-- Monitoring, deliberately partial. `migrations/0001` refuses a blanket default
-- so each table makes this decision explicitly, and the decision here is that
-- an operator may see accounts, workers and their standing, and may not see
-- ticket rows or public keys:
--
--   * `pool.enrollment_ticket` is authority-bearing material. Even hashed, a
--     readable ticket table tells a reader which member is mid-enrollment and
--     when a ticket expires, and the credential it stores is the only thing
--     between an attacker and a worker.
--   * `pool.worker_credential` is withheld for the narrower reason that
--     monitoring has no question that needs a public key, and §8's log hygiene
--     keeps key material out of ordinary telemetry.
--
-- Standing is visible through `pool.worker.state`, which is what an operator
-- actually asks about.
GRANT SELECT ON pool.member TO pool_readonly;
GRANT SELECT ON pool.member_collateral_multiplier TO pool_readonly;
GRANT SELECT ON pool.worker TO pool_readonly;

-- The gateway holds the TIG key and touches nothing member-facing
-- (`architecture.md` §2.2, §6). No grant is the decision, not an oversight.
