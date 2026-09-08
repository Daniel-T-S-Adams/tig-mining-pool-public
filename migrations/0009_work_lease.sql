-- Slice 1 criterion G3: leases and fence tokens for long work.
--
-- docs/architecture.md §7.5 splits work in two. A **short transition** locks
-- the workflow row and uses its monotonic revision. **Long work** — network
-- calls, bulk artifact handling — cannot hold a transaction open (§7.2), so it
-- uses this claim row instead:
--
--   1. a process claims in a short transaction, increments the fence, commits;
--   2. it does the work with no transaction open;
--   3. it commits the result only with a compare-and-set on the workflow
--      revision AND the exact fence token; and
--   4. after lease expiry another process may reclaim with a higher fence,
--      making a late result from the old owner unable to commit.
--
-- Step 4 is the whole point, and invariant 8 states it as a rule: "a lease
-- claimant that has lost its fence cannot commit a late result." A lease with
-- an expiry but no fence would let a process that stalled past its lease come
-- back and commit over the work of whoever took over — the expiry alone
-- proves the old owner *should* have stopped, not that it *did*.

CREATE TABLE pool.work_lease (
    network       text        NOT NULL,
    workflow_id   text        NOT NULL,
    -- One workflow can have several kinds of long work outstanding over its
    -- life; the kind is part of the key so a proof job's lease is not the same
    -- row as an ingestion job's.
    lease_kind    text        NOT NULL,

    -- Monotonic per row, and the thing a late writer fails on. It is NOT
    -- reset by expiry or by a change of owner: a fence that could go
    -- backwards would let a stale token match again.
    fence_token   bigint      NOT NULL DEFAULT 1,

    lease_owner   text        NOT NULL,
    lease_until   timestamptz NOT NULL,

    claimed_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, workflow_id, lease_kind),

    CONSTRAINT work_lease_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT work_lease_fence_positive
        CHECK (fence_token >= 1),
    CONSTRAINT work_lease_owner_not_empty
        CHECK (length(lease_owner) > 0),
    CONSTRAINT work_lease_kind_known
        CHECK (lease_kind IN ('PRECOMMIT_TRANSMIT', 'PROOF_BUILD', 'PACKAGE_INGEST')),

    CONSTRAINT work_lease_has_a_workflow
        FOREIGN KEY (network, workflow_id)
        REFERENCES pool.workflow (network, workflow_id)
);

-- Two rules, and the difference between them matters.
--
-- **The fence never goes backwards.** This is the one property a late writer's
-- safety rests on: if a fence could be reset — by a reclaim, an operator
-- repair, or a future code path that rebuilt the row — a token from a lost
-- lease would match again, and §7.5 step 3's compare-and-set would admit
-- exactly the write invariant 8 forbids.
--
-- **A change of owner advances it.** Handing the work to someone else without
-- a new fence would leave the previous owner's token valid, which is the same
-- hole by a different route.
--
-- Deliberately *not* required: that every update advance the fence. Shortening
-- a lease — an operator forcing an expiry, a holder relinquishing time — does
-- not hand the work to anyone, so demanding a new fence for it would either
-- block the operation or spend a fence for nothing.
CREATE OR REPLACE FUNCTION pool.work_lease_fence_is_monotonic()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.fence_token < OLD.fence_token THEN
        RAISE EXCEPTION
            'pool.work_lease %/%: fence must never go backwards (% -> %)',
            OLD.workflow_id, OLD.lease_kind, OLD.fence_token, NEW.fence_token
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.lease_owner IS DISTINCT FROM OLD.lease_owner
        AND NEW.fence_token <= OLD.fence_token
    THEN
        RAISE EXCEPTION
            'pool.work_lease %/%: handing work from % to % must advance the fence (% -> %)',
            OLD.workflow_id, OLD.lease_kind, OLD.lease_owner, NEW.lease_owner,
            OLD.fence_token, NEW.fence_token
            USING ERRCODE = 'raise_exception';
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER work_lease_fence_monotonic
    BEFORE UPDATE ON pool.work_lease
    FOR EACH ROW
    EXECUTE FUNCTION pool.work_lease_fence_is_monotonic();

GRANT SELECT, INSERT, UPDATE ON pool.work_lease TO pool_controller;

-- The gateway takes the precommit transmission lease: §10's serialized
-- submission lane is its work, and §7.5 names that lane as one of the
-- singleton activities that uses a lease.
GRANT SELECT, INSERT, UPDATE ON pool.work_lease TO pool_gateway;

GRANT SELECT ON pool.work_lease TO pool_readonly;
