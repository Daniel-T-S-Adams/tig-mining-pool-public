-- Slice 1: the one decision input the record did not keep.
--
-- The gateway must transmit the exact bytes an intent's `payload_digest` was
-- taken over — `tig-gateway`'s claim path refuses anything else, on the
-- reconcile path as well as the send. So it has to rebuild the §6.1 body
-- from what the pool recorded, and `pool.precommit_decision` held every
-- input to that body except `compute_type`.
--
-- It is a decision input and not configuration: `tig_integration.md` §3 has
-- enrollment detect it per worker and restrict it to the compatible set, so
-- it varies per offer once members exist. The pool-owned bootstrap of slice 1
-- declares one; a later slice records the member's.
--
-- Nullable column plus an insert trigger, the `0007` pattern. A NOT NULL
-- column would need a default to apply against existing rows, and a default
-- compute type would be a value the pool never decided — exactly what a
-- decision record must not contain. The trigger binds the rule where it can
-- be true: at the moment a decision is recorded.

ALTER TABLE pool.precommit_decision
    ADD COLUMN compute_type text;

CREATE OR REPLACE FUNCTION pool.precommit_decision_carries_compute_type()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.compute_type IS NULL OR length(trim(NEW.compute_type)) = 0 THEN
        RAISE EXCEPTION
            'precommit_decision %/%/%: a decision names the compute type it '
            'was made for; the §6.1 body cannot be rebuilt without it',
            NEW.network, NEW.workflow_id, NEW.generation
            USING ERRCODE = 'raise_exception';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER precommit_decision_insert_compute_type
    BEFORE INSERT ON pool.precommit_decision
    FOR EACH ROW
    EXECUTE FUNCTION pool.precommit_decision_carries_compute_type();
