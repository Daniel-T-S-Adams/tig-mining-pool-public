-- Slice 1 criterion I3: the originating trace id, stored on the intent.
--
-- docs/architecture.md §10.1: "Durable jobs and intents store the originating
-- trace ID so work resumed after a restart remains correlated." An intent is
-- admitted by the controller and transmitted by the gateway, in another
-- process and possibly after a crash, so there is no in-memory context left to
-- correlate by. This column is what makes the two halves one story.

ALTER TABLE pool.tig_write_intent
    ADD COLUMN trace_id text;

-- W3C `trace-id`: 32 lowercase hex characters, and never the all-zero
-- sentinel W3C reserves for "invalid". Constrained here rather than trusted
-- from the application because the column's only value is that a log search
-- for one spelling finds every line: an uppercase or short id would be stored
-- happily and correlate nothing.
ALTER TABLE pool.tig_write_intent
    ADD CONSTRAINT tig_write_intent_trace_id_w3c
        CHECK (
            trace_id IS NULL
            OR (
                trace_id ~ '^[0-9a-f]{32}$'
                AND trace_id <> repeat('0', 32)
            )
        );

-- Nullable on purpose. §10.1 asks that the id be stored, not that work be
-- refused without one, and a NOT NULL here would have two bad consequences:
-- the rows that already exist would need a fabricated id, and a controller
-- whose OS refused randomness would stop admitting precommits — trading a
-- correlation gap for an outage. An absent id reads as "not recorded", which
-- is true, rather than as an id that groups unrelated work.

-- Immutable with the rest of the intent's identity. The originating trace is
-- a historical fact: a later process that "corrected" it would silently
-- re-parent the story, and §7.3's whole point is that an intent's record of
-- what was decided cannot be edited after the fact.
--
-- CREATE OR REPLACE rather than a second trigger: one function owns the rule,
-- so a reader of 0003 is not required to know that 0004..0017 might each have
-- added another.
CREATE OR REPLACE FUNCTION pool.tig_write_intent_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.intent_id IS DISTINCT FROM OLD.intent_id
        OR NEW.network IS DISTINCT FROM OLD.network
        OR NEW.workflow_id IS DISTINCT FROM OLD.workflow_id
        OR NEW.write_kind IS DISTINCT FROM OLD.write_kind
        OR NEW.generation IS DISTINCT FROM OLD.generation
        OR NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
        OR NEW.payload_digest IS DISTINCT FROM OLD.payload_digest
        OR NEW.payload_artifact_id IS DISTINCT FROM OLD.payload_artifact_id
        OR NEW.trace_id IS DISTINCT FROM OLD.trace_id
        OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'tig_write_intent %: identity, key and payload are immutable '
            '(architecture.md §7.3); a changed payload needs a new generation',
            OLD.intent_id;
    END IF;

    IF OLD.state IN ('CONFIRMED', 'REJECTED') AND NEW.state IS DISTINCT FROM OLD.state THEN
        RAISE EXCEPTION
            'tig_write_intent %: % is terminal and cannot become % '
            '(tig_integration.md §7, §10 step 4)',
            OLD.intent_id, OLD.state, NEW.state;
    END IF;

    IF NEW.state = 'PREPARED' AND NEW.state IS DISTINCT FROM OLD.state THEN
        RAISE EXCEPTION
            'tig_write_intent %: cannot return to PREPARED from % '
            '(architecture.md §13 invariant 14)',
            OLD.intent_id, OLD.state;
    END IF;

    IF current_user = 'pool_gateway'
        AND NEW.state IS DISTINCT FROM OLD.state
        AND NOT (OLD.state = 'PREPARED' AND NEW.state = 'OUTCOME_UNKNOWN')
    THEN
        RAISE EXCEPTION
            'tig_write_intent %: the gateway may only record PREPARED -> OUTCOME_UNKNOWN, '
            'not % -> % (architecture.md §6; tig_integration.md §7 — confirmation comes '
            'from reads)',
            OLD.intent_id, OLD.state, NEW.state;
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
