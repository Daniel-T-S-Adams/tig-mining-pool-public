-- Slice 2, criterion A4: the pool remembers a request it has already been
-- asked to honour.
--
-- `member_protocol.md` §3.2: the server "remembers each
-- `(credential_id, request_id)` for 24 hours. Reusing a request ID with
-- different signed bytes is rejected and audited." That is one table, and
-- almost all of its value is in what it refuses to forget — a record dropped
-- early reopens the window it existed to close, and a record whose signed
-- bytes can be rewritten never notices a replay at all.
--
-- `security.md` §4.1 puts verification before database work, so the row is
-- written only for a request whose signature already verified: this table
-- never grows from traffic that failed to authenticate, and cannot be used to
-- fill a disk by an unauthenticated caller.

-- ---------------------------------------------------------------------------
-- The record
-- ---------------------------------------------------------------------------

CREATE TABLE pool.request_replay (
    network       text        NOT NULL,
    credential_id uuid        NOT NULL,

    -- "<new UUID for this HTTP attempt>" (§3.2). Not an application
    -- idempotency key: §3.2 says "an HTTP retry normally uses a new request ID
    -- but retains the message's application idempotency key", so this identity
    -- is per attempt and the key that makes a retry safe lives elsewhere.
    request_id    uuid        NOT NULL,

    -- SHA-256 over the §3.2 signed byte string, as 64 lowercase hex
    -- characters. The signed string already commits to the method, path,
    -- protocol version, worker, credential, request ID, timestamp and body
    -- hash, so comparing this one value answers "the same signed bytes?" for
    -- every field the signature covers.
    --
    -- Hex rather than bytea to match how the value arrives and is compared:
    -- the header it is derived from is hex, and the surrounding code never
    -- needs the bytes.
    signed_sha256 text        NOT NULL,

    first_seen_at timestamptz NOT NULL,

    -- The moment this record may be removed. Stored rather than computed from
    -- `first_seen_at`, for the same reason `worker_credential.not_after` is
    -- stored: the window a record was accepted under must survive a change to
    -- the configured window, and a computed column would silently re-date
    -- every existing row.
    forget_after  timestamptz NOT NULL,

    -- One record per credential and attempt. This is the uniqueness §3.2's
    -- rule is expressed in, so it is the key rather than an index beside one:
    -- a second INSERT for the same pair is a conflict the database reports,
    -- not a race the application has to win.
    PRIMARY KEY (network, credential_id, request_id),

    CONSTRAINT request_replay_network_known
        CHECK (network IN ('testnet', 'mainnet')),

    CONSTRAINT request_replay_digest_is_hex
        CHECK (signed_sha256 ~ '^[0-9a-f]{64}$'),

    -- §3.2's 24 hours is a floor, not a target. A deployment may remember
    -- longer; it may not remember for less, because the window is what makes
    -- the rule mean anything.
    CONSTRAINT request_replay_remembers_for_at_least_a_day
        CHECK (forget_after >= first_seen_at + interval '24 hours'),

    -- The credential is what the record is about, and `worker_credential` is
    -- where revocation lives: without this a record could outlive — or precede
    -- — the credential whose reuse it is meant to detect.
    CONSTRAINT request_replay_has_a_credential
        FOREIGN KEY (network, credential_id)
        REFERENCES pool.worker_credential (network, credential_id)
);

-- Pruning reads this, and so does nothing else: every lookup is by primary
-- key. Partial on nothing, because every row eventually qualifies.
CREATE INDEX request_replay_by_expiry
    ON pool.request_replay (network, forget_after);

-- ---------------------------------------------------------------------------
-- What nobody may do
-- ---------------------------------------------------------------------------

-- A record's signed bytes are the whole of its evidence. If they could be
-- rewritten, a replay would be recorded as a first sighting and §3.2's rule
-- would report success on exactly the case it exists to catch.
--
-- Nothing else on the row is editable either: `first_seen_at` is when it was
-- seen, and moving `forget_after` earlier is the deletion this file refuses
-- below, spelled differently.
CREATE OR REPLACE FUNCTION pool.request_replay_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.request_replay is append-only (member_protocol.md §3.2); a recorded attempt is evidence, not a draft'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER request_replay_immutable
    BEFORE UPDATE ON pool.request_replay
    FOR EACH ROW
    EXECUTE FUNCTION pool.request_replay_is_immutable();

-- Deleting a record before its window closes is the same failure as never
-- writing it: the next reuse of that request ID is accepted as new. Pruning is
-- therefore allowed only once the record has served its 24 hours, and the
-- database is where that is decided rather than the sweep that issues the
-- DELETE — a sweep with a wrong clock or a wrong WHERE clause is exactly the
-- thing this catches.
CREATE OR REPLACE FUNCTION pool.request_replay_forgets_only_when_due()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF now() < OLD.forget_after THEN
        RAISE EXCEPTION
            'request_replay record for credential % is remembered until % (member_protocol.md §3.2)',
            OLD.credential_id, OLD.forget_after
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN OLD;
END;
$$;

CREATE TRIGGER request_replay_forget_when_due
    BEFORE DELETE ON pool.request_replay
    FOR EACH ROW
    EXECUTE FUNCTION pool.request_replay_forgets_only_when_due();

-- ---------------------------------------------------------------------------
-- Who may touch it
-- ---------------------------------------------------------------------------

-- The API and nobody else. `architecture.md` §3 gives it request
-- authentication, and no other component verifies a member signature, so no
-- other component has a reason to read or write an attempt record.
--
-- DELETE is granted because the same process prunes what it wrote; the trigger
-- above is what makes that safe, and a grant without it would let a bug in a
-- sweep undo the rule the table exists for. There is no UPDATE grant at all:
-- the trigger refuses one, and the missing privilege says so before the
-- statement runs.
GRANT SELECT, INSERT, DELETE ON pool.request_replay TO pool_api;

GRANT SELECT ON pool.request_replay TO pool_readonly;
