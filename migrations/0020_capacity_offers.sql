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

-- The foreign-key target for "this worker's slot". `migrations/0019` gave
-- `pool.slot` a generation-scoped unique index for the same reason; this is the
-- ownership-scoped one, and it is what lets the offer below reference the pair
-- rather than each half independently.
CREATE UNIQUE INDEX slot_identity_is_worker_scoped
    ON pool.slot (network, worker_id, slot_id);

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

    -- What the offer is, which is two different kinds of fact:
    --
    --   RECEIVED              the member made an offer, and nothing is decided
    --
    --   NO_ACTION / REJECTED  the member is not eligible, and nothing queued
    --   QUEUED                eligible, but the pool-wide limit is full
    --   READY_CHECK           promoted, awaiting the worker's signed echo
    --   PENDING               the slot is reserved, awaiting the precommit
    --   ADMITTED              a precommit intent exists for this offer
    --   CLOSED                the work it led to is over; the slot is free
    --   EXPIRED / CANCELLED   a lease ran out, or either side stopped it
    --
    -- The first is the Pool API's, and the rest are the Controller's.
    -- `architecture.md` §6 splits them — "Record a member offer ... | Pool API"
    -- against "Admit/reject/queue an offer and reserve its slot | Controller" —
    -- and §5.1 step 1 says the API "does not decide whether the member may
    -- receive work". An offer that could arrive already `PENDING` would be the
    -- member deciding, whatever the grants on UPDATE said afterwards, so
    -- `RECEIVED` exists to be the only thing an INSERT may say.
    --
    -- `NO_ACTION` and `REJECTED` are terminal: §6 step 4 returns them "without
    -- queuing", so they name an offer that never held anything.
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
        CHECK (state IN ('RECEIVED', 'NO_ACTION', 'REJECTED', 'QUEUED',
                         'READY_CHECK', 'PENDING', 'ADMITTED', 'CLOSED',
                         'EXPIRED', 'CANCELLED')),
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
            OR (state IN ('RECEIVED', 'NO_ACTION', 'REJECTED') AND lease_expires_at IS NULL)
            OR state IN ('ADMITTED', 'CLOSED', 'EXPIRED', 'CANCELLED')
        ),
    -- The worker that made the offer owns the slot it offered. Two independent
    -- references — one to the worker, one to the slot — would each be satisfied
    -- by a row offering somebody else's slot, and `migrations/0018`'s
    -- `ticket_worker_belongs_to_its_member` is the same chain expressed the
    -- same way.
    CONSTRAINT offer_slot_belongs_to_its_worker
        FOREIGN KEY (network, worker_id, slot_id)
        REFERENCES pool.slot (network, worker_id, slot_id),
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
    WHERE state IN ('RECEIVED', 'QUEUED', 'READY_CHECK', 'PENDING', 'ADMITTED');

-- `ADMITTED` is in that set because an admitted offer still occupies its slot —
-- §16 invariant 1, and §9's ladder, which keeps the slot busy until the receipt.
-- It leaves the set by being `CLOSED`, which is §6's "after durable acceptance
-- the slot may offer again" and §12's receipt releasing it. Without that exit
-- an admitted offer would seal its slot for ever, which is what the first draft
-- of this migration did: `ADMITTED` was terminal *and* counted as live.

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

    -- §6: the worker confirms "by echoing that ID", and the id has to be
    -- *fresh* — not issued once.
    --
    -- This first read "issued once", which the pinned fixture contradicts:
    -- `fixtures/queue-lifecycle/v1/availability-queue.json`'s
    -- `ready_check_replay_stale_id_ignored` has a controller restart re-issue a
    -- second id for the same offer, and the offer is promoted on that one while
    -- the pre-restart echo is ignored. A rule that forbade the re-issue would
    -- leave a restarted controller with two bad choices: honour an id the
    -- fixture calls stale, or expire the offer and cost it the FIFO place §6
    -- protects.
    --
    -- What must hold is that a re-issue is a *new window*: the id changes and
    -- the deadline moves with it, so an echo of the older id cannot satisfy the
    -- current check and the current check cannot inherit an expired deadline.
    IF OLD.ready_check_id IS NOT NULL
       AND NEW.ready_check_id IS NOT NULL
       AND NEW.ready_check_id IS DISTINCT FROM OLD.ready_check_id
       AND NOT (NEW.ready_check_expires_at > OLD.ready_check_expires_at)
    THEN
        RAISE EXCEPTION
            'a re-issued ready check carries a later deadline (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.state <> OLD.state THEN
        IF OLD.state IN ('NO_ACTION', 'REJECTED', 'EXPIRED', 'CANCELLED', 'CLOSED') THEN
            RAISE EXCEPTION
                'offer state % is terminal (member_protocol.md §6)', OLD.state
                USING ERRCODE = 'raise_exception';
        END IF;

        IF NOT (
            -- An admitted offer is past the point where either side may drop
            -- it: §6 makes the member's commitment "irrevocable" once
            -- `PRECOMMIT_SUBMITTED` is recorded, and what ends it is the work
            -- ending, which is `CLOSED`.
            (NEW.state IN ('EXPIRED', 'CANCELLED') AND OLD.state <> 'ADMITTED')
            OR (OLD.state = 'RECEIVED'
                AND NEW.state IN ('NO_ACTION', 'REJECTED', 'QUEUED', 'PENDING'))
            OR (OLD.state = 'QUEUED'      AND NEW.state = 'READY_CHECK')
            OR (OLD.state = 'READY_CHECK' AND NEW.state = 'PENDING')
            OR (OLD.state = 'PENDING'     AND NEW.state = 'ADMITTED')
            OR (OLD.state = 'ADMITTED'    AND NEW.state = 'CLOSED')
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

-- An offer arrives undecided.
--
-- `architecture.md` §5.1 step 1: the Pool API "authenticates a capacity offer
-- and records its idempotent command. It does not decide whether the member may
-- receive work." A column grant cannot express that, because the API legitimately
-- writes the row — so the value is what is constrained. Every disposition, every
-- lease, every queue position and every ready check is therefore an UPDATE, and
-- the UPDATE grants are the Controller's.
--
-- This refuses the insert whoever makes it, including the controller and the
-- table owner: an offer that appeared already admitted would have no record of
-- the decision that admitted it.
CREATE OR REPLACE FUNCTION pool.capacity_offer_arrives_undecided()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.state <> 'RECEIVED' THEN
        RAISE EXCEPTION
            'an offer is recorded as RECEIVED and disposed of afterwards (architecture.md §5.1)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.lease_expires_at IS NOT NULL
       OR NEW.queue_accepted_at IS NOT NULL
       OR NEW.ready_check_id IS NOT NULL
       OR NEW.ready_check_expires_at IS NOT NULL
       OR NEW.terminal_reason IS NOT NULL
    THEN
        RAISE EXCEPTION
            'a lease, a queue position and a ready check are the pool''s to grant (architecture.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER capacity_offer_undecided
    BEFORE INSERT ON pool.capacity_offer
    FOR EACH ROW
    EXECUTE FUNCTION pool.capacity_offer_arrives_undecided();

-- An offer reaches a live state only from a slot qualified at its *current*
-- generation.
--
-- §16 invariant 2, at the point it bites: "A slot may offer capacity only with
-- a successful qualification for its exact current generation." A slot whose
-- qualification failed, or whose reconfiguration left an old pass behind,
-- cannot be queued or reserved.
--
-- On the **disposition**, not the arrival. §6 applies these checks during
-- admission, and the API's job before that is to record what the member sent —
-- including an offer from a slot that is not qualified, which is how the member
-- gets told `REJECTED` with reason `UNQUALIFIED` and how the retry of that
-- request returns the same answer. Refusing the INSERT would replace a recorded
-- refusal with an error nobody can look up.
--
-- It does not run on the way *out* either: an offer already live when a
-- requalification fails is cancelled by the pool, and this must not stand in
-- the way of recording that.
CREATE OR REPLACE FUNCTION pool.capacity_offer_is_qualified()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    qualified_digest   bytea;
    current_generation bigint;
BEGIN
    IF NEW.state NOT IN ('QUEUED', 'READY_CHECK', 'PENDING', 'ADMITTED') THEN
        RETURN NEW;
    END IF;

    IF TG_OP = 'UPDATE' AND OLD.state = NEW.state THEN
        -- A lease renewal or a ready-check issue, not an admission.
        RETURN NEW;
    END IF;

    -- §16 invariant 2 is about the slot's *exact current* generation, and §6
    -- says "a new registration generation invalidates every older
    -- qualification". Checking only that the named generation has a pass would
    -- let a reconfigured slot offer under the pass its previous configuration
    -- earned — which is the thing the invariant names.
    SELECT generation INTO current_generation
      FROM pool.slot
     WHERE network = NEW.network AND slot_id = NEW.slot_id;

    IF current_generation IS DISTINCT FROM NEW.slot_generation THEN
        RAISE EXCEPTION
            'slot % is at generation %, the offer names % (member_protocol.md §16 invariant 2)',
            NEW.slot_id, current_generation, NEW.slot_generation
            USING ERRCODE = 'raise_exception';
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
    BEFORE INSERT OR UPDATE ON pool.capacity_offer
    FOR EACH ROW
    EXECUTE FUNCTION pool.capacity_offer_is_qualified();

-- ---------------------------------------------------------------------------
-- Grants (architecture.md §6)
-- ---------------------------------------------------------------------------

-- §6: "Record a member offer, event, or heartbeat command | Pool API", guarded
-- by "Offer/event/heartbeat ID and canonical request hash". Recording is all it
-- does, and the columns it may write are the ones a member's offer consists of.
-- Everything a decision is made of is absent from this grant *and* refused at
-- INSERT by the trigger above, because a grant alone would still let the API
-- record an offer that had already decided itself.
GRANT SELECT ON pool.capacity_offer TO pool_api;
GRANT INSERT (network, worker_id, offer_id, slot_id, slot_generation,
              spec_digest, state, offer_sha256)
    ON pool.capacity_offer TO pool_api;

-- The API has **no** UPDATE. §8 makes a member cancellation "a request, not an
-- immediate local rewrite", so the API records the request and the Controller
-- applies it — and the request is a member command, which arrives with the
-- events table in the next migration. Granting the API `state` here would let
-- the member-facing role write `ADMITTED`, which §16 invariant 15 requires a
-- fresh atomic capacity check to reach.

-- §6: "Admit/reject/queue an offer and reserve its slot | Controller" and
-- "Promote a queued offer | Controller". Every column that is a decision.
GRANT SELECT ON pool.capacity_offer TO pool_controller;
GRANT UPDATE (state, lease_expires_at, queue_accepted_at,
              ready_check_id, ready_check_expires_at, terminal_reason)
    ON pool.capacity_offer TO pool_controller;

GRANT SELECT ON pool.capacity_offer TO pool_readonly;
