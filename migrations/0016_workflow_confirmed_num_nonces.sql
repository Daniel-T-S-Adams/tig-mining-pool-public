-- The nonce count TIG fixed for a confirmed precommit.
--
-- `tig_integration.md` §6.2 fixes a benchmark commitment's `solution_quality`
-- at exactly `precommit.details.num_nonces` entries, and TIG refuses a body
-- of any other length **after** the per-bundle fee is paid and the write has
-- occupied its lane. So the pool has to know that number before it builds the
-- body — and it is a precommit *detail* TIG assigns, not one of the settings
-- the pool proposed, so 0006's `confirmed_settings` does not carry it, for
-- exactly the reason 0007 gives about `block_started`.
--
-- Without this column the check had nowhere to read from. It was written
-- against `confirmed_settings`, where the value never appears in production,
-- so it silently never ran: every commitment would have been built unchecked
-- and a wrong length would have surfaced as a paid-for rejection.
--
-- Nullable, like `block_started` before it: rows confirmed before this column
-- existed have no value, and a forward-only migration cannot invent one. What
-- the code must not do is treat absence as permission — `create_commitment_
-- intent` refuses rather than skipping the check.

ALTER TABLE pool.workflow
    ADD COLUMN confirmed_num_nonces bigint;

ALTER TABLE pool.workflow
    ADD CONSTRAINT workflow_confirmed_num_nonces_positive
        CHECK (confirmed_num_nonces IS NULL OR confirmed_num_nonces > 0);

-- 0007's immutability trigger, extended.
--
-- Replaced whole rather than patched, for the reason 0015 replaced 0011's:
-- the function is one statement of what a workflow row may not change, and a
-- second function guarding "the rest" would be two places to read. Every
-- other rule in it is carried across unchanged — including the two at the
-- end, the revision compare-and-set and the `updated_at` stamp, which are
-- easy to lose precisely because they come after the immutability checks
-- this migration is here to extend. A `CREATE OR REPLACE` that drops a rule
-- silently succeeds; `the_revision_can_never_go_backwards` is what catches
-- it, and it did.
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

    -- And the same for the nonce count. §6.2's body is built to this length;
    -- a value that could move would let a commitment be built against one
    -- number and judged against another.
    IF OLD.confirmed_num_nonces IS NOT NULL
        AND NEW.confirmed_num_nonces IS DISTINCT FROM OLD.confirmed_num_nonces
    THEN
        RAISE EXCEPTION
            'pool.workflow %: confirmed_num_nonces is immutable (% -> %)',
            OLD.workflow_id, OLD.confirmed_num_nonces, NEW.confirmed_num_nonces
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

-- No new grant: 0006 gives the controller table-level UPDATE on
-- `pool.workflow`, so a new column is covered. Named here only so the
-- absence is deliberate rather than forgotten.
