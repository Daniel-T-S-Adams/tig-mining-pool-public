-- Slice 1 criterion G4: recorded gaps in observed block history.
--
-- `tig_integration.md` §10: "If the newest accepted height is more than one
-- above the last local height, the pool records every missing height as a data
-- gap and alerts. It may resume current mining after reconciliation, but it
-- must not invent per-block qualifier attribution or payouts for the gap."
--
-- The row exists because the loss is permanent and silent otherwise. TIG's
-- public API does not serve arbitrary historical snapshots, so a height the
-- pool did not observe cannot be fetched later: `get-round-emissions` can
-- audit a round's total but cannot recover the per-block member weights that
-- total was made of. A gap that is not recorded is therefore indistinguishable
-- afterwards from a block in which nobody earned anything — and
-- `accounting.md` §4 would go on to reconcile the round as if it were.

CREATE TABLE pool.block_data_gap (
    network        text        NOT NULL,
    -- One row per missing height, not one per range. The range is what was
    -- observed; the heights are what was lost, and attribution is per block.
    height         bigint      NOT NULL,

    -- The observation that revealed the gap, kept so an auditor can see how
    -- it was found rather than only that it exists.
    after_height   bigint      NOT NULL,
    observed_height bigint     NOT NULL,

    -- Set when an operator has accepted the gap as unrecoverable, or supplied
    -- it from the approved recovery source §10 says public operation will
    -- need. NULL means still open.
    resolved_at    timestamptz,
    resolution     text,
    -- Who resolved it. `architecture.md` §6 gives "apply an administrative
    -- override" an actor and a reason, and §13 invariant 6 wants the record
    -- auditable; a free-text resolution with nobody's name on it says a gap
    -- was settled without saying by whom, which is the half an audit needs.
    -- Text rather than a foreign key to the operator-command table: that
    -- table does not exist yet, and a column that cannot be filled in is not
    -- an audit trail either. When it lands, `resolved_by` becomes the
    -- command's actor and gains its reference.
    resolved_by    text,

    recorded_at    timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, height),

    CONSTRAINT block_data_gap_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT block_data_gap_heights_non_negative
        CHECK (height >= 0 AND after_height >= 0 AND observed_height >= 0),
    -- The missing height lies strictly between what the pool had and what it
    -- then saw. A "gap" outside that interval is a bookkeeping error, not a
    -- gap.
    CONSTRAINT block_data_gap_height_is_between
        CHECK (height > after_height AND height < observed_height),
    -- A resolution, its timestamp and its actor arrive together, so a
    -- resolved gap always says how it was resolved and by whom. All three or
    -- none: two of the three is a record that cannot be audited, and the
    -- CHECK is where that is enforced because the column grant below cannot
    -- see it.
    CONSTRAINT block_data_gap_resolution_is_whole
        CHECK (
            (resolved_at IS NULL AND resolution IS NULL AND resolved_by IS NULL)
            OR (
                resolved_at IS NOT NULL
                AND resolution IS NOT NULL
                AND length(trim(resolution)) > 0
                AND resolved_by IS NOT NULL
                AND length(trim(resolved_by)) > 0
            )
        )
);

-- Open gaps are what the §10.3 alert and the readiness check read.
CREATE INDEX block_data_gap_open
    ON pool.block_data_gap (network, height)
    WHERE resolved_at IS NULL;

-- A recorded gap is never deleted: it is the evidence that a round's per-block
-- attribution is incomplete, and deleting it would make the round look whole.
--
-- Resolving one is the *only* permitted edit, and both halves of that are
-- structural rather than conventional. A table-level UPDATE grant would let
-- the evidence columns be rewritten — or a resolution be undone — which the
-- CHECK above cannot see, and which would leave §10's record saying whatever
-- was most convenient.
CREATE OR REPLACE FUNCTION pool.block_data_gap_evidence_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
        OR NEW.height IS DISTINCT FROM OLD.height
        OR NEW.after_height IS DISTINCT FROM OLD.after_height
        OR NEW.observed_height IS DISTINCT FROM OLD.observed_height
        OR NEW.recorded_at IS DISTINCT FROM OLD.recorded_at
    THEN
        RAISE EXCEPTION
            'pool.block_data_gap %/%: the gap evidence is immutable',
            OLD.network, OLD.height
            USING ERRCODE = 'raise_exception';
    END IF;

    -- A settled gap stays settled. Un-resolving one would re-fire §10.3's
    -- alert for something an operator had already dealt with, and erase the
    -- record of how they dealt with it. A settled gap's resolution and actor
    -- are equally fixed: rewriting either turns the audit record into
    -- whatever the last writer preferred, and the CHECK cannot tell an edit
    -- from the original write.
    IF OLD.resolved_at IS NOT NULL AND (
        NEW.resolved_at IS DISTINCT FROM OLD.resolved_at
        OR NEW.resolution IS DISTINCT FROM OLD.resolution
        OR NEW.resolved_by IS DISTINCT FROM OLD.resolved_by
    ) THEN
        RAISE EXCEPTION
            'pool.block_data_gap %/%: a resolved gap cannot be reopened or rewritten',
            OLD.network, OLD.height
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER block_data_gap_evidence_immutable
    BEFORE UPDATE ON pool.block_data_gap
    FOR EACH ROW
    EXECUTE FUNCTION pool.block_data_gap_evidence_is_immutable();

GRANT SELECT, INSERT ON pool.block_data_gap TO pool_controller;
GRANT UPDATE (resolved_at, resolution, resolved_by) ON pool.block_data_gap TO pool_controller;
GRANT SELECT ON pool.block_data_gap TO pool_readonly;
