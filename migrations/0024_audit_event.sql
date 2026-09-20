-- Slice 2, criterion A4: the durable audit fact.
--
-- `security.md` §9 makes security and financial audit events "append-only
-- facts", lists the fields each records, and requires that "ordinary
-- application roles cannot update or delete audit rows". `architecture.md` §3
-- already places audit facts in the workflow database, so this table is that
-- decision implemented rather than a new one — no ADR.
--
-- It arrives now because §7.1 says a table is added "only with the first
-- implemented query, invariant, recovery action, or audit requirement that
-- needs it", and this is that first requirement: `member_protocol.md` §3.2
-- says reusing a request ID with different signed bytes "is rejected and
-- audited". The rejection is `pool.request_replay`'s; the audit is here.
--
-- Deliberately not a complete rendering of §9. Its "where applicable" fields
-- that no current writer sets — the TIG block and round, command and intent
-- identifiers, operator reason and approval identities — arrive with the
-- audited actions that have them, which is what incremental means.

CREATE TABLE pool.audit_event (
    audit_event_id  uuid        NOT NULL DEFAULT gen_random_uuid(),
    occurred_at     timestamptz NOT NULL DEFAULT now(),

    -- §9 records both. `deployment` distinguishes two environments that share
    -- a network, which is the case an operator reading a row most needs told
    -- apart; it is the same value `[telemetry].deployment` names.
    deployment      text        NOT NULL,
    network         text        NOT NULL,

    -- Who acted, as the pool understood them at the time.
    --
    -- `UNKNOWN` is a real answer and not a gap: §9 asks for "gateway
    -- authentication failure" to be audited, and a caller who failed to
    -- authenticate has no identity the pool is entitled to assert. Recording
    -- `UNKNOWN` says that, where a NULL would read as a writer that forgot.
    actor_type      text        NOT NULL,
    actor_id        text,

    -- What was attempted and how it ended.
    --
    -- `action` is checked for shape rather than against a closed list. §9's
    -- own catalogue spans every component in the system, and enumerating it
    -- here would make each newly audited action a migration — while a closed
    -- list assembled ahead of its writers would mostly be guesses. The cost is
    -- that a typo produces a new action name rather than an error, which is
    -- why the constant belongs in one place in code per component.
    action          text        NOT NULL,
    outcome         text        NOT NULL,

    -- What it was about. NULL for an action with no single subject.
    resource_type   text,
    resource_id     text,

    -- §3.2's per-attempt identifier, when the action had one.
    request_id      uuid,

    -- §9's "prior_state and resulting_state where applicable".
    prior_state     text,
    resulting_state text,

    -- §9's "typed reason": a short machine token, not a sentence. The human
    -- explanation belongs to the operator-command fields, which this table
    -- does not carry yet.
    reason          text,

    -- §9's "compact evidence hashes". Bounded and shallow on purpose: "raw
    -- secrets and solutions are never audit evidence", and the way a blob
    -- reaches an audit row is someone attaching "just this once" a value that
    -- did not fit anywhere else. A size cap makes that fail at the INSERT.
    evidence        jsonb       NOT NULL DEFAULT '{}'::jsonb,

    -- §9's "originating trace_id", so a row and the logs around it can be put
    -- back together.
    trace_id        text,

    PRIMARY KEY (audit_event_id),

    CONSTRAINT audit_event_network_known
        CHECK (network IN ('testnet', 'mainnet')),

    CONSTRAINT audit_event_deployment_is_named
        CHECK (length(deployment) BETWEEN 1 AND 64),

    -- Who can act on this system: a member or one of their workers, the pool
    -- itself, an operator, TIG, or nobody the pool can name.
    CONSTRAINT audit_event_actor_type_known
        CHECK (actor_type IN ('MEMBER', 'WORKER', 'POOL', 'OPERATOR', 'TIG', 'UNKNOWN')),

    -- An identified actor has an identifier and an unknown one does not.
    -- Without this, `UNKNOWN` with an `actor_id` would read as an
    -- identification the pool did not actually make.
    CONSTRAINT audit_event_unknown_actor_is_anonymous
        CHECK ((actor_type = 'UNKNOWN') = (actor_id IS NULL)),

    CONSTRAINT audit_event_action_is_a_token
        CHECK (action ~ '^[A-Z][A-Z0-9_]{2,63}$'),

    -- Every audited attempt ended one of these ways. Unlike `action`, this is
    -- closed: an outcome outside it would make "what happened" unanswerable
    -- by query, which is the one thing every audit read starts from.
    CONSTRAINT audit_event_outcome_known
        CHECK (outcome IN ('ALLOWED', 'REJECTED', 'APPLIED', 'FAILED')),

    -- A resource is named whole or not at all.
    CONSTRAINT audit_event_resource_is_named_whole
        CHECK ((resource_type IS NULL) = (resource_id IS NULL)),

    CONSTRAINT audit_event_reason_is_a_token
        CHECK (reason IS NULL OR reason ~ '^[A-Z][A-Z0-9_]{2,63}$'),

    CONSTRAINT audit_event_evidence_is_a_compact_object
        CHECK (jsonb_typeof(evidence) = 'object'
               AND length(evidence::text) <= 2048)
);

-- The two reads an operator actually makes: everything about one resource,
-- and everything that happened in a window.
CREATE INDEX audit_event_by_resource
    ON pool.audit_event (network, resource_type, resource_id, occurred_at DESC)
    WHERE resource_type IS NOT NULL;

CREATE INDEX audit_event_by_time
    ON pool.audit_event (network, occurred_at DESC);

-- ---------------------------------------------------------------------------
-- What nobody may do
-- ---------------------------------------------------------------------------

-- §9: "append-only facts". A row that could be edited or removed is not
-- evidence of anything, and the case that matters is the one where the party
-- with the most reason to change it is the party that wrote it.
--
-- DELETE is refused outright rather than bounded the way `request_replay`'s
-- is, because §9 says retention "is finalized before public membership" — the
-- policy does not exist yet, and a table that already permitted deletion
-- would be answering a question nobody has decided. The migration that
-- settles retention adds the bounded path.
CREATE OR REPLACE FUNCTION pool.audit_event_is_append_only()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.audit_event is append-only (security.md §9); correct a record by appending another'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER audit_event_append_only
    BEFORE UPDATE OR DELETE ON pool.audit_event
    FOR EACH ROW
    EXECUTE FUNCTION pool.audit_event_is_append_only();

-- ---------------------------------------------------------------------------
-- Who may write one
-- ---------------------------------------------------------------------------

-- INSERT and SELECT, and nothing else, for the component that audits today.
-- §9's "ordinary application roles cannot update or delete audit rows" is two
-- things here: the trigger, which stops everyone, and the absent privilege,
-- which stops the statement before a trigger is consulted.
--
-- Only `pool_api`, because only `pool_api` audits yet. The controller, the
-- gateway and the artifact worker each gain INSERT with their own first
-- audited action — the same rule that put this table in this migration rather
-- than in an earlier one.
GRANT SELECT, INSERT ON pool.audit_event TO pool_api;

-- `security.md` §8: access to audit logs is read-only and role restricted.
GRANT SELECT ON pool.audit_event TO pool_readonly;
