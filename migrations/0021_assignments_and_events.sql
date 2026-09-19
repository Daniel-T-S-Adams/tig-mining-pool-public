-- Slice 2, criteria E1-E6 and F1-F6: the confirmed assignment, and the events a
-- member reports against it.
--
-- `member_protocol.md` §7 owns what an assignment is — "the assignment embeds
-- the confirmed TIG precommit, not the pool's proposed settings" — §8 owns
-- events, §9 owns the member-visible ladder, and §16 invariants 1, 4, 5 and 6
-- are what this schema holds:
--
--   1. One open assignment occupies exactly one slot, and one TIG benchmark has
--      exactly one member owner.
--   4. An assignment contains only confirmed TIG precommit facts.
--   5. The assignment digest binds the qualified slot generation and every fact
--      capable of changing benchmark output or proof construction.
--   6. The spike package deadline preserves ten complete blocks before local
--      workflow expiry for pool-owned protocol completion.
--
-- The workflow this belongs to already exists: slice 1 built `pool.workflow`,
-- and an assignment is its member-facing face. One row per workflow, published
-- only once TIG has confirmed the precommit that workflow sent.

CREATE TABLE pool.assignment (
    network       text NOT NULL,
    assignment_id uuid NOT NULL,

    -- Who it belongs to, in the chain §2 makes permanent:
    -- `benchmark_id -> assignment_id -> slot_id -> worker_id -> member_id`.
    -- Every link is a reference, and the composite ones are what stop a row
    -- from naming a slot of one worker and a worker of another member.
    member_id       uuid   NOT NULL,
    worker_id       uuid   NOT NULL,
    slot_id         uuid   NOT NULL,
    slot_generation bigint NOT NULL,
    offer_id        uuid   NOT NULL,

    -- The workflow whose precommit this assignment publishes. Slice 1's row:
    -- the decision, the intent and the confirmation all hang off it, and this
    -- is the member's view of the same thing.
    workflow_id     text   NOT NULL,

    -- §7, §16 invariant 4: the confirmed facts, not the proposal. TIG assigns
    -- the benchmark id at confirmation and selects the track, so an assignment
    -- cannot exist before either is known — which is why both are NOT NULL
    -- here while `pool.workflow` leaves them nullable until confirmation.
    benchmark_id       text   NOT NULL,
    confirmed_track_id text   NOT NULL,
    compute_type       text   NOT NULL,
    num_nonces         bigint NOT NULL,

    -- §7: `assignment_digest = SHA-256(RFC 8785 canonical JSON(assignment_identity))`,
    -- and the identity object itself, as published. The agent recomputes the
    -- digest and "must refuse to start if any identity field is inconsistent",
    -- so the pool has to keep exactly what it sent rather than something it can
    -- rebuild later from parts that may have moved.
    assignment_digest bytea NOT NULL,
    identity          jsonb NOT NULL,

    -- §7's deadlines. `package_due_before_block = workflow_expiry_block -
    -- proof_reserve_blocks`, and the ten reserved blocks are "pool-owned time
    -- for benchmark commitment confirmation, TIG sampling, proof construction,
    -- proof submission, and confirmation before local expiry".
    workflow_expiry_block   bigint NOT NULL,
    proof_reserve_blocks    bigint NOT NULL,
    package_due_before_block bigint NOT NULL,
    ack_by                  timestamptz NOT NULL,

    -- §9's member-visible ladder.
    state text NOT NULL DEFAULT 'AVAILABLE',

    -- §15's classification, recorded and read by nobody in this slice. ADR 0013
    -- detached it from money: `accounting.md` §11.6 charges the owning member
    -- whatever the cause, so this is operational reporting and not an input to
    -- a charge.
    attribution     text,
    terminal_reason text,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, assignment_id),

    CONSTRAINT assignment_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT assignment_state_known
        CHECK (state IN ('AVAILABLE', 'ACKNOWLEDGED', 'COMPUTING', 'PACKAGING',
                         'UPLOADING', 'PACKAGE_RECEIVED',
                         'PACKAGE_STRUCTURALLY_ACCEPTED',
                         'PACKAGE_DURABLY_ACCEPTED',
                         'CANCELLED', 'EXPIRED', 'FAILED')),
    CONSTRAINT assignment_attribution_known
        CHECK (attribution IS NULL
               OR attribution IN ('MEMBER', 'POOL', 'TIG', 'UNRESOLVED')),
    -- §15: "Every terminal outcome records one of MEMBER, POOL, TIG, or
    -- UNRESOLVED plus a machine reason and evidence." A terminal row without
    -- one is an outcome nobody can report; a live row with one is a
    -- classification of something that has not happened.
    CONSTRAINT assignment_attribution_is_terminal
        CHECK (
            (state IN ('CANCELLED', 'EXPIRED', 'FAILED')
                AND attribution IS NOT NULL AND terminal_reason IS NOT NULL)
            OR
            (state NOT IN ('CANCELLED', 'EXPIRED', 'FAILED')
                AND attribution IS NULL AND terminal_reason IS NULL)
        ),
    CONSTRAINT assignment_digest_is_32_bytes
        CHECK (length(assignment_digest) = 32),
    CONSTRAINT assignment_identity_is_an_object
        CHECK (jsonb_typeof(identity) = 'object'),
    CONSTRAINT assignment_nonces_positive
        CHECK (num_nonces >= 1),
    -- §7's derivation, exactly. The pool owns the reserved blocks; an
    -- assignment whose package deadline did not leave them would be promising
    -- the member time the pool needs.
    CONSTRAINT assignment_deadline_reserves_its_blocks
        CHECK (package_due_before_block = workflow_expiry_block - proof_reserve_blocks
               AND proof_reserve_blocks >= 1
               AND package_due_before_block >= 1),

    -- The chain, one composite reference per link.
    CONSTRAINT assignment_slot_belongs_to_its_worker
        FOREIGN KEY (network, worker_id, slot_id)
        REFERENCES pool.slot (network, worker_id, slot_id),
    CONSTRAINT assignment_worker_belongs_to_its_member
        FOREIGN KEY (network, member_id, worker_id)
        REFERENCES pool.worker (network, member_id, worker_id),
    CONSTRAINT assignment_has_its_offer
        FOREIGN KEY (network, worker_id, offer_id)
        REFERENCES pool.capacity_offer (network, worker_id, offer_id),
    CONSTRAINT assignment_has_its_workflow
        FOREIGN KEY (network, workflow_id)
        REFERENCES pool.workflow (network, workflow_id)
);

-- §16 invariant 1: "one TIG benchmark has exactly one member owner".
--
-- A second lock on a door `migrations/0006` already locks: `pool.workflow` has
-- its own `workflow_one_per_benchmark`, and every assignment names a workflow,
-- so two assignments for one benchmark would need two workflows for it first.
-- That makes this index unreachable on its own — no test here can trip it
-- without removing slice 1's — and it is kept anyway, because the invariant is
-- about the *owner* and this table is where owners live. It costs an index and
-- states the rule where a reader of this table will look for it.
CREATE UNIQUE INDEX assignment_benchmark_has_one_owner
    ON pool.assignment (network, benchmark_id);

-- One workflow publishes one assignment. Two would be two members owning one
-- precommit, which is the same invariant approached from the pool's side.
CREATE UNIQUE INDEX assignment_workflow_has_one_assignment
    ON pool.assignment (network, workflow_id);

-- One offer leads to one assignment: §6's disposition ends at `ADMITTED`, and
-- an offer that produced two assignments would have reserved one slot for two
-- benchmarks.
CREATE UNIQUE INDEX assignment_offer_has_one_assignment
    ON pool.assignment (network, worker_id, offer_id);

-- §7: the agent recomputes the digest, so two assignments sharing one would be
-- two different pieces of work a member cannot tell apart.
CREATE UNIQUE INDEX assignment_digest_is_unique
    ON pool.assignment (network, assignment_digest);

CREATE INDEX assignment_by_slot ON pool.assignment (network, slot_id, state);
CREATE INDEX assignment_by_member ON pool.assignment (network, member_id, state);

-- What an assignment is does not change, and where it may go next.
--
-- §7's identity is what the agent verified before starting: "the pool ... never
-- substitutes a different algorithm, track, binary, runtime, or benchmark under
-- the same assignment" (§14). So every identity-bearing column is fixed, and
-- the digest with them.
--
-- §9's ladder runs `ASSIGNMENT_AVAILABLE -> ACKNOWLEDGED -> COMPUTING ->
-- PACKAGING -> UPLOADING -> PACKAGE_RECEIVED -> PACKAGE_STRUCTURALLY_ACCEPTED
-- -> PACKAGE_DURABLY_ACCEPTED`, with `CANCELLED`, `EXPIRED` and `FAILED` as its
-- terminal branches. `PACKAGE_DURABLY_ACCEPTED` is terminal too: §12 says the
-- receipt releases the slot and the retention obligation, and §9 says the
-- pool-side TIG states after it "do not occupy the slot" — they are the
-- workflow's, not the assignment's.
CREATE OR REPLACE FUNCTION pool.assignment_advances()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.assignment_id IS DISTINCT FROM OLD.assignment_id
       OR NEW.member_id IS DISTINCT FROM OLD.member_id
       OR NEW.worker_id IS DISTINCT FROM OLD.worker_id
       OR NEW.slot_id IS DISTINCT FROM OLD.slot_id
       OR NEW.slot_generation IS DISTINCT FROM OLD.slot_generation
       OR NEW.offer_id IS DISTINCT FROM OLD.offer_id
       OR NEW.workflow_id IS DISTINCT FROM OLD.workflow_id
       OR NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
       OR NEW.confirmed_track_id IS DISTINCT FROM OLD.confirmed_track_id
       OR NEW.compute_type IS DISTINCT FROM OLD.compute_type
       OR NEW.num_nonces IS DISTINCT FROM OLD.num_nonces
       OR NEW.assignment_digest IS DISTINCT FROM OLD.assignment_digest
       OR NEW.identity IS DISTINCT FROM OLD.identity
       OR NEW.workflow_expiry_block IS DISTINCT FROM OLD.workflow_expiry_block
       OR NEW.proof_reserve_blocks IS DISTINCT FROM OLD.proof_reserve_blocks
       OR NEW.package_due_before_block IS DISTINCT FROM OLD.package_due_before_block
       OR NEW.ack_by IS DISTINCT FROM OLD.ack_by
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'an assignment''s identity is what the agent verified; it does not change (member_protocol.md §7, §14)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.state <> OLD.state THEN
        IF OLD.state IN ('PACKAGE_DURABLY_ACCEPTED', 'CANCELLED', 'EXPIRED', 'FAILED') THEN
            RAISE EXCEPTION
                'assignment state % is terminal (member_protocol.md §9, §12)', OLD.state
                USING ERRCODE = 'raise_exception';
        END IF;

        IF NOT (
            NEW.state IN ('CANCELLED', 'EXPIRED', 'FAILED')
            OR (OLD.state = 'AVAILABLE'        AND NEW.state = 'ACKNOWLEDGED')
            OR (OLD.state = 'ACKNOWLEDGED'     AND NEW.state = 'COMPUTING')
            OR (OLD.state = 'COMPUTING'        AND NEW.state = 'PACKAGING')
            OR (OLD.state = 'PACKAGING'        AND NEW.state = 'UPLOADING')
            OR (OLD.state = 'UPLOADING'        AND NEW.state = 'PACKAGE_RECEIVED')
            OR (OLD.state = 'PACKAGE_RECEIVED' AND NEW.state = 'PACKAGE_STRUCTURALLY_ACCEPTED')
            OR (OLD.state = 'PACKAGE_STRUCTURALLY_ACCEPTED'
                AND NEW.state = 'PACKAGE_DURABLY_ACCEPTED')
        ) THEN
            RAISE EXCEPTION
                'an assignment does not go from % to % (member_protocol.md §9)',
                OLD.state, NEW.state
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER assignment_ladder
    BEFORE UPDATE ON pool.assignment
    FOR EACH ROW
    EXECUTE FUNCTION pool.assignment_advances();

-- An assignment exists only for a confirmed precommit, and only for the offer
-- that was admitted.
--
-- §6 step 8: "Publish an assignment only after the precommit is confirmed and
-- the selected track is known." §16 invariant 4 says the same from the other
-- side. The workflow's state is where that confirmation lives, and the offer's
-- is where the admission does.
CREATE OR REPLACE FUNCTION pool.assignment_follows_confirmation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    workflow_state   text;
    workflow_benchmark text;
    offer_state      text;
BEGIN
    SELECT w.state, w.benchmark_id INTO workflow_state, workflow_benchmark
      FROM pool.workflow w
     WHERE w.network = NEW.network AND w.workflow_id = NEW.workflow_id;

    IF workflow_state IS NULL THEN
        RAISE EXCEPTION 'workflow % does not exist', NEW.workflow_id
            USING ERRCODE = 'raise_exception';
    END IF;

    -- Everything at or past `PRECOMMIT_CONFIRMED` has a confirmed precommit
    -- behind it; the states before it do not, and the local terminal ones never
    -- will. Naming the states that *do* rather than the ones that do not is
    -- what keeps a later state from being admitted here by omission.
    IF workflow_state NOT IN ('PRECOMMIT_CONFIRMED', 'BENCHMARK_SUBMITTED',
                              'BENCHMARK_CONFIRMED', 'PROOF_SUBMITTED',
                              'PROOF_CONFIRMED', 'VERIFIED')
    THEN
        RAISE EXCEPTION
            'an assignment is published from a confirmed precommit, not from % (member_protocol.md §6 step 8)',
            workflow_state
            USING ERRCODE = 'raise_exception';
    END IF;

    IF workflow_benchmark IS DISTINCT FROM NEW.benchmark_id THEN
        RAISE EXCEPTION
            'the assignment names benchmark %, the workflow holds % (member_protocol.md §16 invariant 4)',
            NEW.benchmark_id, workflow_benchmark
            USING ERRCODE = 'raise_exception';
    END IF;

    SELECT o.state INTO offer_state
      FROM pool.capacity_offer o
     WHERE o.network = NEW.network
       AND o.worker_id = NEW.worker_id
       AND o.offer_id = NEW.offer_id;

    IF offer_state IS DISTINCT FROM 'ADMITTED' THEN
        RAISE EXCEPTION
            'an assignment follows an admitted offer, not one in % (member_protocol.md §6)',
            offer_state
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER assignment_from_confirmation
    BEFORE INSERT ON pool.assignment
    FOR EACH ROW
    EXECUTE FUNCTION pool.assignment_follows_confirmation();

-- ---------------------------------------------------------------------------
-- Events
-- ---------------------------------------------------------------------------

-- §8: "Assignment events have a strictly increasing `event_seq` starting at 1
-- and a unique `event_id`. They are facts from the agent, not authority to
-- advance TIG state."
CREATE TABLE pool.assignment_event (
    network       text   NOT NULL,
    assignment_id uuid   NOT NULL,

    -- §2: the worker chooses `event_id`, so it is scoped to the assignment it
    -- belongs to rather than trusted to be unique on its own.
    event_id      uuid   NOT NULL,
    event_seq     bigint NOT NULL,

    event_type    text   NOT NULL,

    -- The event's own fields, as received. Bounded by the API's request limits
    -- rather than by this column, and never re-rendered: §5's idempotency
    -- compares the canonical hash, and a payload the pool rewrote would not
    -- match the hash it stored.
    payload       jsonb  NOT NULL,
    event_sha256  bytea  NOT NULL,

    received_at   timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, assignment_id, event_id),

    CONSTRAINT event_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT event_type_known
        CHECK (event_type IN ('PROGRESS', 'COMPUTE_COMPLETED', 'PACKAGE_STARTED',
                              'PACKAGE_READY', 'RETRYABLE_ERROR', 'TERMINAL_ERROR')),
    CONSTRAINT event_seq_starts_at_one
        CHECK (event_seq >= 1),
    CONSTRAINT event_payload_is_an_object
        CHECK (jsonb_typeof(payload) = 'object'),
    CONSTRAINT event_sha256_is_32_bytes
        CHECK (length(event_sha256) = 32),
    CONSTRAINT event_has_an_assignment
        FOREIGN KEY (network, assignment_id)
        REFERENCES pool.assignment (network, assignment_id)
);

-- §8: one sequence number, one event. A gap "is rejected with the expected
-- sequence so the worker can replay missing events", and two events at one
-- sequence would make that expectation meaningless.
CREATE UNIQUE INDEX event_seq_is_unique_per_assignment
    ON pool.assignment_event (network, assignment_id, event_seq);

-- Events are facts. §8 has a duplicate id "return the original acceptance", so
-- nothing rewrites one — the original is the answer a retry gets.
CREATE OR REPLACE FUNCTION pool.assignment_event_is_a_fact()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'an assignment event is a fact the member reported; it is not edited (member_protocol.md §8)'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER assignment_event_append_only
    BEFORE UPDATE OR DELETE ON pool.assignment_event
    FOR EACH ROW
    EXECUTE FUNCTION pool.assignment_event_is_a_fact();

-- ---------------------------------------------------------------------------
-- Grants (architecture.md §6)
-- ---------------------------------------------------------------------------

-- §6: "Create a confirmed assignment | Controller", guarded by "Confirmed TIG
-- benchmark ID and permanent ownership constraint"; and "Apply member-reported
-- assignment progress | Controller". The API publishes nothing and advances
-- nothing — it reads an assignment to answer a status request.
GRANT SELECT ON pool.assignment TO pool_api;
GRANT SELECT, INSERT ON pool.assignment TO pool_controller;
GRANT UPDATE (state, attribution, terminal_reason) ON pool.assignment TO pool_controller;

-- §6: "Record a member offer, event, or heartbeat command | Pool API". An event
-- is the member's report, so the API writes it and nobody edits it.
GRANT SELECT, INSERT ON pool.assignment_event TO pool_api;
GRANT SELECT ON pool.assignment_event TO pool_controller;

GRANT SELECT ON pool.assignment TO pool_readonly;
GRANT SELECT ON pool.assignment_event TO pool_readonly;
