-- Slice 1 criteria E1, E2 and E6: the gateway's write-attempt ledger.
--
-- architecture.md §3 gives the write-attempt ledger to the TIG Gateway, and
-- §7.3 fixes its shape: "records every attempt before sending, then records
-- the response separately". The response columns are therefore nullable and
-- a NULL outcome means unresolved — which is also what the two lane
-- constraints below key on.

CREATE TABLE pool.tig_write_attempt (
    attempt_id   uuid        NOT NULL DEFAULT gen_random_uuid(),
    intent_id    uuid        NOT NULL REFERENCES pool.tig_write_intent (intent_id),
    attempt_no   integer     NOT NULL,

    -- Copied from the intent by the trigger below, never supplied by the
    -- caller. A partial unique index cannot join, so the lane rules need
    -- these columns here; taking them from the intent rather than trusting
    -- the writer is what stops a wrong value from silently disabling the
    -- lane it is supposed to enforce.
    network      text        NOT NULL,
    write_kind   text        NOT NULL,
    benchmark_id text,

    started_at   timestamptz NOT NULL DEFAULT now(),

    -- The response, recorded separately (§7.3). NULL means unresolved: the
    -- request may or may not have reached TIG.
    outcome      text,
    http_status  integer,
    -- A short CLASSIFICATION of what went wrong, never response bytes.
    --
    -- The only process that writes this row is the one holding the TIG API
    -- key, so an unbounded free-text column here is the shape architecture.md
    -- §9 forbids (a secret reaching a database value) and §3 excludes (an
    -- unbounded copy of a TIG response). Bounded and stated rather than left
    -- to the caller's judgement; the A4 scan is the backstop, not the rule.
    detail       text,
    -- When the outcome was recorded. For an ambiguity this is when it BECAME
    -- ambiguous, which is the fact architecture.md §10.3's "ambiguous for
    -- more than two target blocks" page reads — so settling it later must
    -- not overwrite this.
    resolved_at  timestamptz,
    -- When §10 reconciliation settled an ambiguity, if it did.
    reconciled_at timestamptz,

    PRIMARY KEY (attempt_id),

    CONSTRAINT tig_write_attempt_no_positive CHECK (attempt_no >= 1),
    CONSTRAINT tig_write_attempt_unique_no UNIQUE (intent_id, attempt_no),
    CONSTRAINT tig_write_attempt_outcome_known
        CHECK (outcome IS NULL OR outcome IN ('ACCEPTED', 'REJECTED', 'AMBIGUOUS')),
    -- Resolved and unresolved are one fact, not two that can disagree.
    CONSTRAINT tig_write_attempt_resolution_consistent
        CHECK ((outcome IS NULL) = (resolved_at IS NULL)),
    CONSTRAINT tig_write_attempt_detail_bounded
        CHECK (detail IS NULL OR length(detail) <= 200),
    -- Only a settled ambiguity carries a reconciliation time.
    CONSTRAINT tig_write_attempt_reconciled_is_settled
        CHECK (reconciled_at IS NULL OR outcome IN ('ACCEPTED', 'REJECTED'))
);

-- E2: `tig_integration.md` §10 — "the gateway permits only one unresolved
-- precommit request per serialized submission lane". A lost precommit
-- response cannot be matched by id, so a second one entering the lane makes
-- the §10 reconciliation search ambiguous by construction.
-- "Unresolved" includes AMBIGUOUS. A lost precommit response is the case
-- §10 defines this lane for — "the client may not know the generated ID" —
-- and §11 forbids a replacement precommit while the previous outcome is
-- ambiguous. Treating a recorded AMBIGUOUS as resolved would reopen the lane
-- at exactly the moment a second precommit makes §10's confirmed-precommit
-- search multi-candidate, which is the failure the lane exists to prevent.
-- The lane reopens when reconciliation settles the ambiguity into a
-- definitive outcome.
CREATE UNIQUE INDEX tig_write_attempt_one_unresolved_precommit
    ON pool.tig_write_attempt (network)
    WHERE write_kind = 'precommit' AND (outcome IS NULL OR outcome = 'AMBIGUOUS');

-- E6: never two concurrent writes for one benchmark (§11). Benchmark and
-- proof writes reconcile by benchmark_id, so two in flight for the same
-- benchmark cannot be told apart afterwards.
CREATE UNIQUE INDEX tig_write_attempt_one_unresolved_per_benchmark
    ON pool.tig_write_attempt (network, benchmark_id)
    WHERE benchmark_id IS NOT NULL AND (outcome IS NULL OR outcome = 'AMBIGUOUS');

-- Fill the lane columns from the intent, and keep the record append-only
-- once resolved.
CREATE FUNCTION pool.tig_write_attempt_guard() RETURNS trigger AS $$
DECLARE
    intent RECORD;
BEGIN
    IF TG_OP = 'INSERT' THEN
        SELECT network, write_kind, benchmark_id
          INTO intent
          FROM pool.tig_write_intent
         WHERE intent_id = NEW.intent_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'tig_write_attempt: no intent %', NEW.intent_id;
        END IF;
        NEW.network := intent.network;
        NEW.write_kind := intent.write_kind;
        NEW.benchmark_id := intent.benchmark_id;

        -- Born unresolved, always. §7.3 records the attempt BEFORE sending
        -- and the response separately, and an INSERT that arrived already
        -- carrying an outcome would never enter either partial index — it
        -- would sail past the serialized precommit lane and the
        -- per-benchmark write lock that this migration claims to enforce by
        -- construction.
        IF NEW.outcome IS NOT NULL OR NEW.resolved_at IS NOT NULL THEN
            RAISE EXCEPTION
                'tig_write_attempt: an attempt is recorded before the request is sent and '
                'cannot be created already resolved (architecture.md §7.3)';
        END IF;
        RETURN NEW;
    END IF;

    -- A DEFINITIVE outcome is final. §7.3 makes the ledger the evidence
    -- reconciliation reads; rewriting a settled result would let a later
    -- attempt restate an earlier one's.
    IF OLD.outcome IN ('ACCEPTED', 'REJECTED') AND (
        NEW.outcome IS DISTINCT FROM OLD.outcome
        OR NEW.http_status IS DISTINCT FROM OLD.http_status
        OR NEW.detail IS DISTINCT FROM OLD.detail
        OR NEW.resolved_at IS DISTINCT FROM OLD.resolved_at
    ) THEN
        RAISE EXCEPTION
            'tig_write_attempt %: a definitive outcome is recorded once '
            '(architecture.md §7.3)',
            OLD.attempt_id;
    END IF;

    -- An AMBIGUOUS outcome is settled by §10 reconciliation, and only into a
    -- definitive one. That transition is what reopens the lane, so it cannot
    -- go back to unresolved or restate the ambiguity — either would leave
    -- the lane closed with nothing left to settle it.
    -- Settling an ambiguity establishes what happened at TIG; it does not
    -- restate the transport evidence of the lost response. Freezing these
    -- across the settling transition as well as outside it is what keeps
    -- §10's reconciliation record intact — otherwise any UPDATE holder
    -- could rewrite what the failed request looked like while resolving it.
    IF OLD.outcome = 'AMBIGUOUS' AND NEW.outcome IN ('ACCEPTED', 'REJECTED') AND (
        NEW.http_status IS DISTINCT FROM OLD.http_status
        OR NEW.detail IS DISTINCT FROM OLD.detail
        OR NEW.resolved_at IS DISTINCT FROM OLD.resolved_at
    ) THEN
        RAISE EXCEPTION
            'tig_write_attempt %: reconciliation settles the outcome and cannot rewrite the '
            'recorded response (architecture.md §7.3; tig_integration.md §10)',
            OLD.attempt_id;
    END IF;

    IF OLD.outcome = 'AMBIGUOUS'
        AND (NEW.outcome IS NULL OR NEW.outcome = 'AMBIGUOUS')
        AND (
            NEW.outcome IS DISTINCT FROM OLD.outcome
            OR NEW.http_status IS DISTINCT FROM OLD.http_status
            OR NEW.detail IS DISTINCT FROM OLD.detail
            OR NEW.resolved_at IS DISTINCT FROM OLD.resolved_at
        )
    THEN
        RAISE EXCEPTION
            'tig_write_attempt %: an ambiguous outcome is settled by reconciliation into '
            'ACCEPTED or REJECTED, not %  (tig_integration.md §10)',
            OLD.attempt_id, COALESCE(NEW.outcome, 'unresolved');
    END IF;

    IF NEW.attempt_id IS DISTINCT FROM OLD.attempt_id
        OR NEW.intent_id IS DISTINCT FROM OLD.intent_id
        OR NEW.attempt_no IS DISTINCT FROM OLD.attempt_no
        OR NEW.network IS DISTINCT FROM OLD.network
        OR NEW.write_kind IS DISTINCT FROM OLD.write_kind
        OR NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
        OR NEW.started_at IS DISTINCT FROM OLD.started_at
    THEN
        RAISE EXCEPTION
            'tig_write_attempt %: an attempt records what was sent and cannot be edited',
            OLD.attempt_id;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER tig_write_attempt_guard
    BEFORE INSERT OR UPDATE ON pool.tig_write_attempt
    FOR EACH ROW EXECUTE FUNCTION pool.tig_write_attempt_guard();

-- The ledger belongs to the gateway (architecture.md §3). The controller
-- reads it — §10 reconciles a write before retrying it — and writes none of
-- it, so the component that chooses work cannot fabricate evidence that a
-- write was attempted.
GRANT SELECT, INSERT, UPDATE ON pool.tig_write_attempt TO pool_gateway;
GRANT SELECT ON pool.tig_write_attempt TO pool_controller;
GRANT SELECT ON pool.tig_write_attempt TO pool_readonly;
