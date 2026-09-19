-- Slice 2, criteria D1-D13: capacity offers, their leases, and the FIFO queue.
--
-- `member_protocol.md` §6 owns the offer's disposition — the eight numbered
-- steps from "atomically reject another open offer" to "publish an assignment
-- only after the precommit is confirmed"; §9 owns the member-visible ladder;
-- §16 invariants 1, 3 and 15 are the properties this schema holds:
--
--    1. One open assignment occupies exactly one slot.
--    3. Registration, qualification, and capacity-offer retries cannot create
--       an extra slot generation, qualification result, precommit, or
--       reservation.
--   15. A queued offer consumes neither TIG unverified capacity nor member
--       collateral, and no queued promotion can bypass a fresh atomic
--       global-limit and tier-limit check.
--
-- `architecture.md` §7.6 owns the queue's state and §6 owns who writes it: the
-- Pool API records the offer a member made, and the Controller decides what the
-- pool does about it. That division is the grants at the bottom.

CREATE TABLE pool.capacity_offer (
    network   text NOT NULL,
    worker_id uuid NOT NULL,

    -- §2: the worker chooses this, and it is "never reused for another
    -- availability period". Worker-scoped rather than network-unique for the
    -- reason every member-chosen identifier in this schema is: two workers
    -- picking the same value must not collide, or one member can make another
    -- member's offer fail by guessing.
    offer_id  uuid NOT NULL,

    slot_id    uuid   NOT NULL,
    -- The generation the offer was made for. §6 has the offer repeat the
    -- current compute facts and qualification digest "so unexpected drift fails
    -- closed"; carrying the generation is what lets a later reconfiguration be
    -- visible as drift rather than invisible as a coincidence.
    slot_generation bigint NOT NULL,
    spec_digest     bytea  NOT NULL,

    -- §6's dispositions, plus the two that arrive later:
    --
    --   NO_ACTION / REJECTED  the member is not eligible, and nothing queued
    --   QUEUED                eligible, but the pool-wide limit is full
    --   READY_CHECK           promoted, awaiting the worker's signed echo
    --   PENDING               the slot is reserved, awaiting the precommit
    --   ADMITTED              a precommit intent exists for this offer
    --   EXPIRED / CANCELLED   a lease ran out, or either side stopped it
    --
    -- `NO_ACTION` and `REJECTED` are terminal on arrival: §6 step 4 returns
    -- them "without queuing", so they name an offer that never held anything.
    state text NOT NULL,

    -- §6: the offer is heartbeated against "an explicit `lease_expires_at`".
    -- NULL for the states that hold nothing.
    lease_expires_at timestamptz,

    -- §6's FIFO key is `(queue_accepted_at, offer_id)`, set when the offer
    -- enters the queue and never afterwards: a renewed lease must not move an
    -- offer's place in the line.
    queue_accepted_at timestamptz,

    -- §6's promotion handshake. The controller issues `CONFIRM_AVAILABLE` with
    -- a fresh id; "only that confirmation permits the atomic capacity check,
    -- collateral reservation, and precommit intent".
    ready_check_id         uuid,
    ready_check_expires_at timestamptz,

    -- What the pool decided, for the states that are a decision rather than a
    -- position in a queue. A bounded code, because §15's attribution table and
    -- §6's dispositions are both closed sets.
    terminal_reason text,

    -- §5's idempotency: the canonical body hash of the offer as received, so a
    -- retry with the same `offer_id` and different content is a conflict rather
    -- than a second offer.
    offer_sha256 bytea NOT NULL,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, worker_id, offer_id),

    CONSTRAINT offer_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT offer_state_known
        CHECK (state IN ('NO_ACTION', 'REJECTED', 'QUEUED', 'READY_CHECK',
                         'PENDING', 'ADMITTED', 'EXPIRED', 'CANCELLED')),
    CONSTRAINT offer_digests_are_32_bytes
        CHECK (length(spec_digest) = 32 AND length(offer_sha256) = 32),
    CONSTRAINT offer_terminal_reason_known
        CHECK (terminal_reason IS NULL
               OR terminal_reason IN ('INELIGIBLE', 'UNQUALIFIED', 'INCOMPATIBLE',
                                      'MEMBER_LIMIT', 'POOL_LIMIT', 'DRIFT',
                                      'LEASE_EXPIRED', 'READY_CHECK_EXPIRED',
                                      'MEMBER_CANCELLED', 'POOL_CANCELLED',
                                      'RESERVE_UNAVAILABLE')),
    -- A queue position exists exactly while the offer is in the queue or has
    -- been promoted out of it. §6's FIFO key is meaningless for an offer that
    -- never queued, and losing it on promotion would lose the order the
    -- promotion is supposed to respect.
    CONSTRAINT offer_queue_position_matches_state
        CHECK (
            (state IN ('QUEUED', 'READY_CHECK') AND queue_accepted_at IS NOT NULL)
            OR state NOT IN ('QUEUED', 'READY_CHECK')
        ),
    -- §6: the ready check is what a promotion is waiting on, so it exists in
    -- that state and in no other. An offer carrying one in `PENDING` would be
    -- an admission still quoting a check it already consumed.
    CONSTRAINT offer_ready_check_matches_state
        CHECK ((state = 'READY_CHECK')
               = (ready_check_id IS NOT NULL AND ready_check_expires_at IS NOT NULL)),
    -- A live offer is held by a lease, and the states that hold nothing hold no
    -- lease. §6 gives one to all three live states — step 5 returns `QUEUED`
    -- "with a renewable lease", step 6 returns `PENDING` the same way, and a
    -- promotion keeps it while the ready check runs. What §16 invariant 15
    -- denies a queued offer is TIG capacity and collateral, not a lease.
    --
    -- The terminal states keep whatever lease they had, because the lease
    -- expiring is *why* some of them are terminal (`LEASE_EXPIRED`), and
    -- clearing it would erase the evidence for the reason beside it.
    CONSTRAINT offer_lease_matches_state
        CHECK (
            (state IN ('QUEUED', 'READY_CHECK', 'PENDING') AND lease_expires_at IS NOT NULL)
            OR (state IN ('NO_ACTION', 'REJECTED') AND lease_expires_at IS NULL)
            OR state IN ('ADMITTED', 'EXPIRED', 'CANCELLED')
        ),
    CONSTRAINT offer_has_a_worker
        FOREIGN KEY (network, worker_id)
        REFERENCES pool.worker (network, worker_id),
    -- The offer is for a slot at a generation, and both must exist. §6's drift
    -- check compares what the offer carried against what the slot is now; a
    -- reference to a generation that never existed is not drift, it is a
    -- fabrication.
    CONSTRAINT offer_has_a_slot_generation
        FOREIGN KEY (network, slot_id, slot_generation)
        REFERENCES pool.slot_generation (network, slot_id, generation)
);

-- §6 step 1: "Atomically reject another open offer or assignment for the slot."
-- One partial unique index does it for the offer half: at most one live offer
-- per slot, whatever its stage.
CREATE UNIQUE INDEX offer_one_live_per_slot
    ON pool.capacity_offer (network, slot_id)
    WHERE state IN ('QUEUED', 'READY_CHECK', 'PENDING', 'ADMITTED');

-- §6's FIFO order, `(queue_accepted_at, offer_id)`. The index carries the same
-- pair so "the oldest eligible live offer" is a read rather than a scan.
CREATE INDEX offer_fifo
    ON pool.capacity_offer (network, queue_accepted_at, offer_id)
    WHERE state = 'QUEUED';

CREATE INDEX offer_by_state ON pool.capacity_offer (network, state);

-- What an offer may not change about itself, and where it may go next.
--
-- The ladder is §6's disposition read as states: an offer arrives, is either
-- refused outright, queued, or reserved; a queued one is promoted through a
-- ready check; a reserved one is admitted when its precommit intent exists.
-- Nothing goes backwards, because every backwards step would be the pool
-- forgetting a commitment it already made — `ADMITTED -> PENDING` in particular
-- would be an offer whose precommit exists claiming to be waiting for one.
CREATE OR REPLACE FUNCTION pool.capacity_offer_advances()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.worker_id IS DISTINCT FROM OLD.worker_id
       OR NEW.offer_id IS DISTINCT FROM OLD.offer_id
       OR NEW.slot_id IS DISTINCT FROM OLD.slot_id
       OR NEW.slot_generation IS DISTINCT FROM OLD.slot_generation
       OR NEW.spec_digest IS DISTINCT FROM OLD.spec_digest
       OR NEW.offer_sha256 IS DISTINCT FROM OLD.offer_sha256
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'an offer''s identity and what it offered are fixed (member_protocol.md §5, §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    -- §6: the FIFO key is set on entry to the queue. A renewed lease must not
    -- move an offer's place in the line, and a promotion must not lose it.
    IF OLD.queue_accepted_at IS NOT NULL
       AND NEW.queue_accepted_at IS DISTINCT FROM OLD.queue_accepted_at
    THEN
        RAISE EXCEPTION
            'a queued offer keeps its place in the line (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    -- §6: the worker confirms "by echoing that ID". A check reissued under the
    -- same id would let a stale echo satisfy a later promotion.
    IF OLD.ready_check_id IS NOT NULL
       AND NEW.ready_check_id IS NOT NULL
       AND NEW.ready_check_id IS DISTINCT FROM OLD.ready_check_id
    THEN
        RAISE EXCEPTION
            'a ready check is issued once; promote again with a fresh one (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.state <> OLD.state THEN
        IF OLD.state IN ('NO_ACTION', 'REJECTED', 'EXPIRED', 'CANCELLED', 'ADMITTED') THEN
            RAISE EXCEPTION
                'offer state % is terminal (member_protocol.md §6)', OLD.state
                USING ERRCODE = 'raise_exception';
        END IF;

        IF NOT (
            NEW.state IN ('EXPIRED', 'CANCELLED')
            OR (OLD.state = 'QUEUED'      AND NEW.state = 'READY_CHECK')
            OR (OLD.state = 'READY_CHECK' AND NEW.state = 'PENDING')
            OR (OLD.state = 'PENDING'     AND NEW.state = 'ADMITTED')
        ) THEN
            RAISE EXCEPTION
                'an offer does not go from % to % (member_protocol.md §6)',
                OLD.state, NEW.state
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER capacity_offer_ladder
    BEFORE UPDATE ON pool.capacity_offer
    FOR EACH ROW
    EXECUTE FUNCTION pool.capacity_offer_advances();

-- An offer is for a slot that is qualified at the generation it names.
--
-- §16 invariant 2, at the point it bites: "A slot may offer capacity only with
-- a successful qualification for its exact current generation." The offer
-- carries a generation and a digest; this requires a QUALIFIED row for that
-- exact pair, so a slot whose qualification failed — or whose reconfiguration
-- left the old pass behind — cannot enter the queue at all.
--
-- On INSERT only. An offer already in flight when a requalification fails is
-- §6's business, not a row's: the pool cancels it, which is a state change this
-- check must not block.
CREATE OR REPLACE FUNCTION pool.capacity_offer_is_qualified()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    qualified_digest bytea;
BEGIN
    IF NEW.state IN ('NO_ACTION', 'REJECTED') THEN
        -- §6 step 4 returns these "without queuing", and an ineligible member's
        -- refusal is recorded whatever the slot's standing — including
        -- `UNQUALIFIED`, which could not be recorded if this check refused it.
        RETURN NEW;
    END IF;

    SELECT q.spec_digest INTO qualified_digest
      FROM pool.slot_qualification q
     WHERE q.network = NEW.network
       AND q.slot_id = NEW.slot_id
       AND q.generation = NEW.slot_generation
       AND q.state = 'QUALIFIED';

    IF qualified_digest IS NULL THEN
        RAISE EXCEPTION
            'slot % has no qualification at generation % (member_protocol.md §16 invariant 2)',
            NEW.slot_id, NEW.slot_generation
            USING ERRCODE = 'raise_exception';
    END IF;

    IF qualified_digest IS DISTINCT FROM NEW.spec_digest THEN
        RAISE EXCEPTION
            'the offer''s spec digest is not what qualified (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER capacity_offer_qualified
    BEFORE INSERT ON pool.capacity_offer
    FOR EACH ROW
    EXECUTE FUNCTION pool.capacity_offer_is_qualified();

-- ---------------------------------------------------------------------------
-- Grants (architecture.md §6)
-- ---------------------------------------------------------------------------

-- §6: "Record a member offer, event, or heartbeat command | Pool API", guarded
-- by "Offer/event/heartbeat ID and canonical request hash". Recording is all it
-- does: the API cannot decide the disposition, which is the next row of that
-- table and the controller's.
GRANT SELECT, INSERT ON pool.capacity_offer TO pool_api;

-- The member may withdraw an offer, and that reaches the pool through the API
-- (§8: "A member cancellation is a request"). Before a precommit is submitted
-- §8 lets the pool accept it without trust effect; after, the ladder above
-- refuses it, because `ADMITTED` is terminal here and the workflow owns what
-- happens next.
GRANT UPDATE (state, terminal_reason) ON pool.capacity_offer TO pool_api;

-- §6: "Admit/reject/queue an offer and reserve its slot | Controller" and
-- "Promote a queued offer | Controller". Every column that is a decision.
GRANT SELECT ON pool.capacity_offer TO pool_controller;
GRANT UPDATE (state, lease_expires_at, queue_accepted_at,
              ready_check_id, ready_check_expires_at, terminal_reason)
    ON pool.capacity_offer TO pool_controller;

GRANT SELECT ON pool.capacity_offer TO pool_readonly;
