-- Slice 1 criteria D1 and D1a: TIG write intents.
--
-- docs/architecture.md §7.3 owns write idempotency. Each intent has an
-- immutable identity, a canonical payload digest and a state, and the unique
-- constraint below is what makes a duplicate write impossible rather than
-- merely unlikely.

CREATE TABLE pool.tig_write_intent (
    intent_id            uuid        NOT NULL DEFAULT gen_random_uuid(),

    network              text        NOT NULL,
    workflow_id          text        NOT NULL,
    write_kind           text        NOT NULL,
    generation           integer     NOT NULL,

    -- The TIG benchmark a benchmark/proof generation is bound to (§7.3).
    -- NULL for precommits, which is what the CHECK below enforces: a
    -- benchmark write whose benchmark_id was optional could be recorded
    -- without saying which benchmark it belongs to, and the binding §7.3
    -- requires would exist only in prose.
    benchmark_id         text,

    -- SHA-256 over the canonical payload. §7.3: changing a canonical payload
    -- requires an explicit new generation, so this is immutable and a
    -- differing digest for the same key is a conflict, never an update.
    payload_digest       bytea       NOT NULL,
    payload_artifact_id  text,

    state                text        NOT NULL DEFAULT 'PREPARED',

    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (intent_id),

    -- §7.3, verbatim. One intent per (network, workflow, kind, generation);
    -- a second write for the same generation cannot exist.
    CONSTRAINT tig_write_intent_unique_generation
        UNIQUE (network, workflow_id, write_kind, generation),

    CONSTRAINT tig_write_intent_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT tig_write_intent_kind_known
        CHECK (write_kind IN ('precommit', 'benchmark', 'proof')),
    CONSTRAINT tig_write_intent_generation_positive
        CHECK (generation >= 1),
    CONSTRAINT tig_write_intent_digest_is_sha256
        CHECK (octet_length(payload_digest) = 32),

    -- D1a: a benchmark or proof generation is bound to a benchmark_id, and a
    -- precommit has none to be bound to.
    CONSTRAINT tig_write_intent_benchmark_binding
        CHECK (
            (write_kind = 'precommit' AND benchmark_id IS NULL)
            OR (write_kind IN ('benchmark', 'proof') AND benchmark_id IS NOT NULL)
        ),

    -- The states slice 1 defines. PREPARED is created-but-unattempted;
    -- OUTCOME_UNKNOWN is §7.3's ambiguous outcome, which reconciles against
    -- confirmed TIG state rather than resending; CONFIRMED and REJECTED are
    -- set only from confirmed TIG reads, never from an HTTP status
    -- (tig_integration.md §7).
    CONSTRAINT tig_write_intent_state_known
        CHECK (state IN ('PREPARED', 'OUTCOME_UNKNOWN', 'CONFIRMED', 'REJECTED'))
);

-- State has to change — the gateway records an ambiguous outcome, and
-- confirmation arrives later from reads — so UPDATE cannot be withheld the
-- way it is for an accepted snapshot. This trigger is what keeps "immutable"
-- in §7.3 true anyway: identity, key, payload digest and benchmark binding
-- cannot be edited by anything holding UPDATE, so repointing an intent at a
-- different payload is impossible rather than merely against the rules.
CREATE FUNCTION pool.tig_write_intent_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.intent_id IS DISTINCT FROM OLD.intent_id
        OR NEW.network IS DISTINCT FROM OLD.network
        OR NEW.workflow_id IS DISTINCT FROM OLD.workflow_id
        OR NEW.write_kind IS DISTINCT FROM OLD.write_kind
        OR NEW.generation IS DISTINCT FROM OLD.generation
        OR NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
        OR NEW.payload_digest IS DISTINCT FROM OLD.payload_digest
        -- The artifact pointer too. §7.3 lists it as part of the intent, so
        -- repointing it changes what is actually sent without the new
        -- generation §7.3 requires — and the role best placed to do that is
        -- the gateway, which §13 invariant 2 keeps out of deciding what a
        -- write contains. If a later slice needs to attach an artifact after
        -- the intent exists, that is an explicit decision, not a gap.
        OR NEW.payload_artifact_id IS DISTINCT FROM OLD.payload_artifact_id
        OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'tig_write_intent %: identity, key and payload are immutable '
            '(architecture.md §7.3); a changed payload needs a new generation',
            OLD.intent_id;
    END IF;
    -- CONFIRMED and REJECTED are terminal. tig_integration.md §7 makes them
    -- the product of confirmed reads and §10 step 4 requires local state to
    -- advance monotonically, so retracting one re-enters reconciliation for
    -- a write that in fact settled — and fires §10.3's "ambiguous for more
    -- than two target blocks" page for it.
    IF OLD.state IN ('CONFIRMED', 'REJECTED') AND NEW.state IS DISTINCT FROM OLD.state THEN
        RAISE EXCEPTION
            'tig_write_intent %: % is terminal and cannot become % '
            '(tig_integration.md §7, §10 step 4)',
            OLD.intent_id, OLD.state, NEW.state;
    END IF;

    -- Nothing returns to PREPARED. Re-arming a settled write is how a
    -- restart silently duplicates a TIG write (architecture.md §13
    -- invariant 14).
    IF NEW.state = 'PREPARED' AND NEW.state IS DISTINCT FROM OLD.state THEN
        RAISE EXCEPTION
            'tig_write_intent %: cannot return to PREPARED from % '
            '(architecture.md §13 invariant 14)',
            OLD.intent_id, OLD.state;
    END IF;

    -- architecture.md §6 assigns confirmation to the controller's
    -- reconciler, and tig_integration.md §7 makes a transport status no
    -- evidence at all: "a recorded HTTP 200 never advances a workflow".
    -- Row-wide UPDATE would let the transmitting component mark its own
    -- write confirmed from the response it just received, which is the one
    -- thing §7 exists to prevent. The gateway records the ambiguous outcome
    -- and nothing else.
    --
    -- Constrained by transition, not only by target state: allowing any
    -- write of OUTCOME_UNKNOWN would let the ambiguity path (criterion E3),
    -- firing on a slow response for an intent the reconciler had already
    -- confirmed, drive a confirmed write back to ambiguous.
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

CREATE TRIGGER tig_write_intent_immutable
    BEFORE UPDATE ON pool.tig_write_intent
    FOR EACH ROW EXECUTE FUNCTION pool.tig_write_intent_immutable();

-- The controller creates intents (§5.1 step 4) and records confirmation from
-- reads. The gateway never creates one — it transmits what already exists
-- and records the outcome — so it holds UPDATE without INSERT, which is what
-- stops a transmission path from inventing a write nobody decided on.
GRANT SELECT, INSERT, UPDATE ON pool.tig_write_intent TO pool_controller;
GRANT SELECT, UPDATE ON pool.tig_write_intent TO pool_gateway;

-- Neither may delete: an intent is the record that a write was decided on,
-- and §7.3's reconciliation depends on it still being there afterwards.
GRANT SELECT ON pool.tig_write_intent TO pool_readonly;
