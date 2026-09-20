-- Corrections to `0023` and `0024`, from the review of the PR that added
-- them.
--
-- Two of them: a column nothing fills, and a statement both tables' triggers
-- do not see.

-- ---------------------------------------------------------------------------
-- A column nothing fills
-- ---------------------------------------------------------------------------

-- `security.md` §9 lists "originating trace_id" among the fields an audit
-- event records, and `0024` created the column citing that sentence. Nothing
-- in this codebase produces a trace id: `pool-telemetry` builds `tracing`
-- spans with fields, and there is no propagated identifier to put here. Every
-- row therefore landed with NULL.
--
-- `architecture.md` §7.1 adds a field "only with the first implemented query,
-- invariant, recovery action, or audit requirement that needs it", and the
-- same reasoning removes one that nothing needs yet: a column that is always
-- NULL does not record a trace id, it only looks as though it might. It
-- returns with the work that produces trace ids.
--
-- What the column was for — putting a row back together with the logs around
-- it — is served meanwhile by `audit_event_id`, which the writer now logs at
-- the moment it inserts the row.
ALTER TABLE pool.audit_event DROP COLUMN trace_id;

-- ---------------------------------------------------------------------------
-- The statement the triggers did not see
-- ---------------------------------------------------------------------------

-- `0024`'s append-only trigger is `FOR EACH ROW`, and TRUNCATE fires no row
-- triggers. The table owner could therefore empty an append-only table in one
-- statement, and the test asserting it "cannot be edited or removed by
-- anyone" would not have noticed.
--
-- A statement-level trigger is the only kind TRUNCATE fires. It reuses the
-- same function, because the answer is the same one.
CREATE TRIGGER audit_event_no_truncate
    BEFORE TRUNCATE ON pool.audit_event
    FOR EACH STATEMENT
    EXECUTE FUNCTION pool.audit_event_is_append_only();

-- `0023`'s memory has the same hole, and losing it is worse than losing an
-- audit row: every request id in the forgotten window becomes reusable, which
-- is the replay `member_protocol.md` §3.2 exists to refuse.
--
-- Its own function refuses UPDATE and is not reused here, because its message
-- says "append-only" and this one has to say why a *bounded* deletion does
-- not license an unbounded one.
CREATE OR REPLACE FUNCTION pool.request_replay_is_not_truncated()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.request_replay is pruned row by row once each record is due (member_protocol.md §3.2); TRUNCATE would forget the window entire'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER request_replay_no_truncate
    BEFORE TRUNCATE ON pool.request_replay
    FOR EACH STATEMENT
    EXECUTE FUNCTION pool.request_replay_is_not_truncated();
