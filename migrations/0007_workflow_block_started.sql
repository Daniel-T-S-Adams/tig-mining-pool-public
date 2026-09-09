-- Slice 1 criterion F5: the block a workflow's benchmark started at.
--
-- `tig_integration.md` §8's guardrails are all ages measured from
-- `block_started` — a precommit *detail* TIG assigns, not one of the settings
-- the pool proposed, so 0006's `confirmed_settings` does not carry it. Without
-- it the deadline check would have to re-read TIG to learn how old its own
-- workflow is, which turns a local monitoring decision into a network call and
-- leaves the pool unable to expire anything while TIG is unreachable.
--
-- A separate migration because 0006 was already merged when this was found.
-- Migrations are forward-only (`pre_build_checklist.md` §8), so this is the
-- mechanism for exactly that.

ALTER TABLE pool.workflow
    ADD COLUMN block_started bigint;

-- The pairing is enforced by triggers, not by a table CHECK.
--
-- `NOT VALID` was the obvious answer and it is the wrong one. It skips the
-- initial table scan, but PostgreSQL still evaluates the CHECK against the
-- full NEW row on **every subsequent UPDATE** — so a row written before this
-- column existed, with `confirmed_track_id` set and `block_started` NULL,
-- would pass the migration and then fail its next transition. Every
-- confirmation, expiry and failure on that workflow would be rejected, and a
-- forward-only migration has no repair path once that is live.
--
-- The trigger enforces the rule where it belongs: at the moment
-- `confirmed_track_id` is *newly set*. New rows are constrained exactly as a
-- CHECK would constrain them; pre-existing ones keep transitioning.
-- Validated immediately: the column is new, so every existing value is NULL
-- and already satisfies it. `NOT VALID` here would leave the constraint
-- permanently unvalidated in the catalog for no benefit — the reasoning above
-- is about the *pairing*, which depends on a column that predates this
-- migration, not about this one.
ALTER TABLE pool.workflow
    ADD CONSTRAINT workflow_block_started_non_negative
        CHECK (block_started IS NULL OR block_started >= 0);

-- 0006's trigger, with `block_started` added to what it protects.
-- The pairing, on INSERT. The trigger below is BEFORE UPDATE and reads OLD,
-- so it cannot see a row created wrong in the first place; without this an
-- INSERT naming `confirmed_track_id` with no `block_started` would be accepted
-- and §8's deadlines would have nothing to measure that workflow from.
CREATE OR REPLACE FUNCTION pool.workflow_insert_carries_block_started()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.confirmed_track_id IS NOT NULL AND NEW.block_started IS NULL THEN
        RAISE EXCEPTION
            'pool.workflow %: a confirmed precommit must carry block_started',
            NEW.workflow_id
            USING ERRCODE = 'raise_exception';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER workflow_insert_block_started
    BEFORE INSERT ON pool.workflow
    FOR EACH ROW
    EXECUTE FUNCTION pool.workflow_insert_carries_block_started();

CREATE OR REPLACE FUNCTION pool.workflow_owner_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.owner_kind IS DISTINCT FROM OLD.owner_kind
        OR NEW.owner_id IS DISTINCT FROM OLD.owner_id
    THEN
        RAISE EXCEPTION
            'pool.workflow %: owner is immutable (% % -> % %)',
            OLD.workflow_id, OLD.owner_kind, OLD.owner_id,
            NEW.owner_kind, NEW.owner_id
            USING ERRCODE = 'raise_exception';
    END IF;

    IF OLD.benchmark_id IS NOT NULL
        AND NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
    THEN
        RAISE EXCEPTION
            'pool.workflow %: benchmark_id is immutable (% -> %)',
            OLD.workflow_id, OLD.benchmark_id, NEW.benchmark_id
            USING ERRCODE = 'raise_exception';
    END IF;

    -- A confirmation must bring `block_started` with it. Enforced here rather
    -- than as a table CHECK so that a row predating the column can still
    -- transition: the rule binds the moment the confirmation is recorded, not
    -- every update thereafter.
    IF OLD.confirmed_track_id IS NULL
        AND NEW.confirmed_track_id IS NOT NULL
        AND NEW.block_started IS NULL
    THEN
        RAISE EXCEPTION
            'pool.workflow %: a confirmed precommit must carry block_started',
            OLD.workflow_id
            USING ERRCODE = 'raise_exception';
    END IF;

    -- Once TIG has told the pool when the benchmark started, that is when it
    -- started. Allowing it to move would let a workflow approaching expiry be
    -- handed a fresh deadline, which is the one edit §8's guardrails cannot
    -- survive.
    IF OLD.block_started IS NOT NULL
        AND NEW.block_started IS DISTINCT FROM OLD.block_started
    THEN
        RAISE EXCEPTION
            'pool.workflow %: block_started is immutable (% -> %)',
            OLD.workflow_id, OLD.block_started, NEW.block_started
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.revision <= OLD.revision THEN
        RAISE EXCEPTION
            'pool.workflow %: revision must advance (% -> %)',
            OLD.workflow_id, OLD.revision, NEW.revision
            USING ERRCODE = 'raise_exception';
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;
