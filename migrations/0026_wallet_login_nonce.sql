-- Slice 2, criterion B8: the nonce that makes a wallet login one use.
--
-- ADR 0011 makes the member account a Base address proved by signature, and
-- `accounting.md` §12.2 requires "one-time nonces and exact-domain validation
-- to prevent signature reuse on a different pool or chain". The domain and
-- the chain are inside the signed text, which `pool_identity::wallet` checks
-- by rebuilding it. One-time is the half that needs to remember, so it is a
-- row.
--
-- The signature is `TIG-POOL-LOGIN-V1` over the pool domain, chain id,
-- purpose, nonce and expiry. The address is deliberately not in it (§12.2:
-- it "*is* the member identity"), so the address here is the one the
-- signature **recovered to**, never one a caller supplied.

CREATE TABLE pool.wallet_login_nonce (
    network        text        NOT NULL,

    -- The address the signature recovered to, lowercase hex, as
    -- `pool.member.wallet_address` stores it. Part of the key, which is what
    -- makes a nonce one-time *for a member* rather than globally.
    --
    -- Globally would be worse, not better: anyone can sign any nonce with
    -- their own key, so a global key would let one party consume a value and
    -- deny it to another. Scoping to the recovered address means a consumer
    -- can only ever spend nonces for an address they control.
    wallet_address text        NOT NULL,

    nonce          text        NOT NULL,

    -- Which ticket the signature authorised. Recorded rather than trusted
    -- from the request: it is inside the signed text, so it is a fact about
    -- what the member agreed to.
    purpose        text        NOT NULL,

    consumed_at    timestamptz NOT NULL DEFAULT now(),

    -- The expiry the signature itself carries. This is what bounds the row's
    -- usefulness: once the signature it consumed could no longer be accepted,
    -- remembering it prevents nothing.
    signature_expires_at timestamptz NOT NULL,

    PRIMARY KEY (network, wallet_address, nonce),

    CONSTRAINT wallet_login_nonce_network_known
        CHECK (network IN ('testnet', 'mainnet')),

    CONSTRAINT wallet_login_nonce_address_is_lowercase_hex
        CHECK (wallet_address ~ '^0x[0-9a-f]{40}$'),

    -- Lowercase hex and at least 128 bits. The key above already stops one
    -- party spending another's nonce, so this is not about collisions between
    -- members: it is about a member's own nonce being unguessable to whoever
    -- else sees their signature, and about there being one spelling of each
    -- so two cannot both be recorded.
    CONSTRAINT wallet_login_nonce_is_hex_and_unguessable
        CHECK (nonce ~ '^[0-9a-f]{32,128}$'),

    -- The two purposes a login authorises today. Closed, because each is a
    -- distinct authority: a signature obtained to enrol a worker must not
    -- recover one. ADR 0011 leaves no account-recovery purpose to add.
    CONSTRAINT wallet_login_nonce_purpose_known
        CHECK (purpose IN ('WORKER_ENROLLMENT', 'WORKER_RECOVERY'))
);

-- Pruning reads this; every lookup is by primary key.
CREATE INDEX wallet_login_nonce_by_expiry
    ON pool.wallet_login_nonce (network, signature_expires_at);

-- ---------------------------------------------------------------------------
-- What nobody may do
-- ---------------------------------------------------------------------------

-- A consumed nonce is evidence that a signature has already been spent.
-- Editing the row is how a spent signature becomes unspent.
CREATE OR REPLACE FUNCTION pool.wallet_login_nonce_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.wallet_login_nonce records a spent signature (accounting.md §12.2); it is written once'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER wallet_login_nonce_immutable
    BEFORE UPDATE ON pool.wallet_login_nonce
    FOR EACH ROW
    EXECUTE FUNCTION pool.wallet_login_nonce_is_immutable();

-- Forgetting a nonce while the signature that used it is still inside its own
-- expiry makes that signature replayable — which is the single thing this
-- table exists to stop. Pruning is therefore allowed only once the signature
-- could no longer be accepted anyway.
CREATE OR REPLACE FUNCTION pool.wallet_login_nonce_forgets_only_when_spent()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF now() < OLD.signature_expires_at THEN
        RAISE EXCEPTION
            'the signature that spent this nonce is valid until % (accounting.md §12.2)',
            OLD.signature_expires_at
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN OLD;
END;
$$;

CREATE TRIGGER wallet_login_nonce_forget_when_spent
    BEFORE DELETE ON pool.wallet_login_nonce
    FOR EACH ROW
    EXECUTE FUNCTION pool.wallet_login_nonce_forgets_only_when_spent();

-- TRUNCATE fires no row triggers, and emptying this table makes every
-- unexpired signature replayable at once.
CREATE TRIGGER wallet_login_nonce_no_truncate
    BEFORE TRUNCATE ON pool.wallet_login_nonce
    FOR EACH STATEMENT
    EXECUTE FUNCTION pool.wallet_login_nonce_is_immutable();

-- ---------------------------------------------------------------------------
-- Who may touch it
-- ---------------------------------------------------------------------------

-- The API alone. `security.md` §4.1 keeps the ticket HMAC key in the Pool API
-- and B9 keeps the login there with it; no other component verifies a member
-- signature, so no other has a reason to know which nonces are spent.
GRANT SELECT, INSERT, DELETE ON pool.wallet_login_nonce TO pool_api;

GRANT SELECT ON pool.wallet_login_nonce TO pool_readonly;
