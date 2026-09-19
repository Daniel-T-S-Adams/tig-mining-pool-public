-- Slice 2, criteria C1-C8: compute slots, their registration generations, and
-- the qualification that lets one offer capacity.
--
-- `member_protocol.md` §6 owns what a slot records and what qualification
-- proves; §2 owns the identities and which side issues each; §9 owns the slot's
-- state ladder; §16 invariants 2 and 3 are what this schema is for:
--
--   2. A slot may offer capacity only with a successful qualification for its
--      exact current generation, compute facts, and runtime inventory.
--   3. Registration, qualification, and capacity-offer retries cannot create an
--      extra slot generation, qualification result, precommit, or reservation.
--
-- Three tables, because a slot is three different things over time:
--
--   pool.slot               the identity and where it is in §9's ladder
--   pool.slot_generation    what it was configured as, once per registration
--   pool.slot_qualification what a generation proved, once per attempt
--
-- Splitting the generation out is what makes invariant 2 expressible: a
-- qualification references the generation it qualified, so a reconfiguration
-- invalidates every older qualification by pointing somewhere else rather than
-- by anyone remembering to.

-- ---------------------------------------------------------------------------
-- The slot
-- ---------------------------------------------------------------------------

CREATE TABLE pool.slot (
    network          text        NOT NULL,
    slot_id          uuid        NOT NULL,
    worker_id        uuid        NOT NULL,

    -- §2: "Stable member-local identity of one logical slot across
    -- generations. Unique within a worker." The worker chooses it, so it is
    -- member-supplied text and never becomes a path or an object key
    -- (`CLAUDE.md`, `architecture.md` §8.2).
    client_slot_key  text        NOT NULL,

    -- The registration generation this slot is currently configured at. It
    -- names a row in `pool.slot_generation`, and §2 has it increment on every
    -- reconfiguration.
    generation       bigint      NOT NULL,

    -- §9's ladder: `AVAILABLE -> QUEUED -> RESERVED -> COMPUTING -> PACKAGING
    -- -> UPLOADING -> AVAILABLE`, with the direct `AVAILABLE -> RESERVED` when
    -- the pool is not full. The trigger below keeps a slot from skipping a
    -- rung.
    state            text        NOT NULL DEFAULT 'AVAILABLE',

    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, slot_id),

    CONSTRAINT slot_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT slot_generation_positive
        CHECK (generation >= 1),
    CONSTRAINT slot_state_known
        CHECK (state IN ('AVAILABLE', 'QUEUED', 'RESERVED',
                         'COMPUTING', 'PACKAGING', 'UPLOADING')),
    CONSTRAINT slot_key_not_blank
        CHECK (length(trim(client_slot_key)) > 0),
    CONSTRAINT slot_has_a_worker
        FOREIGN KEY (network, worker_id)
        REFERENCES pool.worker (network, worker_id)
);

-- §2: unique within a worker. This is how `(worker_id, client_slot_key)`
-- "locates the logical slot to create or reconfigure", so a second row for one
-- key would make that lookup ambiguous at the moment a reconfiguration decides
-- what it is reconfiguring.
CREATE UNIQUE INDEX slot_key_is_unique_per_worker
    ON pool.slot (network, worker_id, client_slot_key);

CREATE INDEX slot_by_worker ON pool.slot (network, worker_id, state);

-- A foreign-key target for everything that must belong to a slot *at a
-- generation*: a qualification here, an offer and an assignment in the PRs that
-- follow.
CREATE UNIQUE INDEX slot_identity_is_generation_scoped
    ON pool.slot (network, slot_id, generation);

-- ---------------------------------------------------------------------------
-- What it was configured as
-- ---------------------------------------------------------------------------

-- One row per registration or reconfiguration. §2: "Changing a slot's
-- architecture, core count, device, compute type, or qualified runtime
-- inventory uses a new `slot_registration_id`, names the current generation as
-- `prior_slot_generation`, atomically increments the generation, and requires
-- requalification."
CREATE TABLE pool.slot_generation (
    network    text   NOT NULL,
    slot_id    uuid   NOT NULL,
    generation bigint NOT NULL,

    -- §2's idempotency identity of one registration attempt, unique across the
    -- network: a retry of the same attempt cannot land as a second generation,
    -- which is invariant 3's first clause.
    slot_registration_id uuid  NOT NULL,
    registration_sha256  bytea NOT NULL,

    -- The generation this one replaced, NULL for an initial registration. §2
    -- makes a stale value a conflict; that check lives in the trigger below,
    -- because it is a statement about the slot's *current* generation and not
    -- about this row alone.
    prior_slot_generation bigint,

    -- §6's compute facts, as the agent reported them. "The agent must report
    -- its observed facts. A label is not evidence of compatibility": these are
    -- claims, and the qualification is what turns them into evidence.
    compute_kind   text   NOT NULL,
    compute_type   text   NOT NULL,
    cpu_arch       text   NOT NULL,
    -- CPU vendor and fixed logical-core count for a CPU slot; NVIDIA device
    -- identity and driver versions for a GPU one. One column each, NULL for the
    -- kind they do not describe, with the CHECKs below making the pairing
    -- structural rather than conventional.
    cpu_vendor     text,
    logical_cores  integer,
    gpu_device     text,
    nvidia_driver_version     text,
    cuda_driver_version       text,
    container_toolkit_version text,

    agent_version          text  NOT NULL,
    runtime_bundle_version text  NOT NULL,
    -- §6's digest-pinned runtime images, as a JSON array of manifest digests.
    -- Sorted before the digest is taken (§6), so sender order cannot change
    -- what qualified.
    available_image_manifest_digests jsonb NOT NULL,

    -- §6's `qualification_spec_digest`: SHA-256 of the RFC 8785 canonical JSON
    -- of `[worker_id, slot_id, slot_generation, compute, runtime_inventory]`.
    -- Stored because every later step compares against it — the qualification
    -- result, the capacity offer, and the assignment identity all carry it.
    spec_digest bytea NOT NULL,

    created_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, slot_id, generation),

    CONSTRAINT slot_generation_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT slot_generation_is_positive
        CHECK (generation >= 1),
    CONSTRAINT slot_generation_prior_is_lower
        CHECK (prior_slot_generation IS NULL OR prior_slot_generation < generation),
    CONSTRAINT slot_generation_initial_has_no_prior
        CHECK ((generation = 1) = (prior_slot_generation IS NULL)),
    CONSTRAINT slot_generation_compute_kind_known
        CHECK (compute_kind IN ('CPU', 'GPU')),
    CONSTRAINT slot_generation_cpu_arch_known
        CHECK (cpu_arch IN ('amd64', 'arm64')),
    -- §6: a CPU slot is "a fixed group of logical cores"; a v0 GPU slot is "one
    -- NVIDIA device" reporting its driver, CUDA and Container Toolkit versions.
    -- A row describing neither, or both, is a report the pool cannot check
    -- against the supported compute matrix.
    CONSTRAINT slot_generation_cpu_facts
        CHECK ((compute_kind = 'CPU')
               = (cpu_vendor IS NOT NULL AND logical_cores IS NOT NULL)),
    CONSTRAINT slot_generation_gpu_facts
        CHECK ((compute_kind = 'GPU')
               = (gpu_device IS NOT NULL
                  AND nvidia_driver_version IS NOT NULL
                  AND cuda_driver_version IS NOT NULL
                  AND container_toolkit_version IS NOT NULL)),
    CONSTRAINT slot_generation_cores_positive
        CHECK (logical_cores IS NULL OR logical_cores >= 1),
    CONSTRAINT slot_generation_images_are_an_array
        CHECK (jsonb_typeof(available_image_manifest_digests) = 'array'),
    CONSTRAINT slot_generation_digests_are_32_bytes
        CHECK (length(spec_digest) = 32 AND length(registration_sha256) = 32),
    CONSTRAINT slot_generation_has_a_slot
        FOREIGN KEY (network, slot_id)
        REFERENCES pool.slot (network, slot_id)
);

-- Invariant 3: "Registration ... retries cannot create an extra slot
-- generation."
CREATE UNIQUE INDEX slot_registration_is_idempotent
    ON pool.slot_generation (network, slot_registration_id);

-- One spec digest describes one generation. §6 binds the digest to
-- `[worker_id, slot_id, slot_generation, compute, runtime_inventory]`, so two
-- generations sharing one is a contradiction: either the digest did not cover
-- the generation, or identical facts were registered twice.
CREATE UNIQUE INDEX slot_generation_digest_is_unique
    ON pool.slot_generation (network, spec_digest);

-- A generation is a fact about a moment. §2 has a reconfiguration create a new
-- one rather than edit the old, because the qualification that referenced it
-- and the assignments issued under it both described what it said at the time.
CREATE OR REPLACE FUNCTION pool.slot_generation_is_append_only()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'a slot generation records one registration; reconfigure by adding one (member_protocol.md §2)'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER slot_generation_no_update
    BEFORE UPDATE OR DELETE ON pool.slot_generation
    FOR EACH ROW
    EXECUTE FUNCTION pool.slot_generation_is_append_only();

-- What a new generation has to agree with: the slot it belongs to, the
-- generation that slot is currently at, and §2's rule that reconfiguration is
-- "forbidden while an offer, assignment, or upload is open".
--
-- The open-work half is expressed as the slot's own state, which is what §9's
-- ladder already records: anything but `AVAILABLE` means the slot is committed
-- to work. When offers and assignments land they add their own rows; they do
-- not make this weaker.
CREATE OR REPLACE FUNCTION pool.slot_generation_follows_the_slot()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    current_generation bigint;
    current_state      text;
BEGIN
    SELECT generation, state INTO current_generation, current_state
      FROM pool.slot
     WHERE network = NEW.network AND slot_id = NEW.slot_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'slot % does not exist', NEW.slot_id
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.generation = 1 THEN
        RETURN NEW;
    END IF;

    IF NEW.prior_slot_generation IS DISTINCT FROM current_generation THEN
        RAISE EXCEPTION
            'stale prior generation: the slot is at %, the registration names % (member_protocol.md §2)',
            current_generation, NEW.prior_slot_generation
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.generation <> current_generation + 1 THEN
        RAISE EXCEPTION
            'a reconfiguration increments the generation: expected %, got %',
            current_generation + 1, NEW.generation
            USING ERRCODE = 'raise_exception';
    END IF;

    IF current_state <> 'AVAILABLE' THEN
        RAISE EXCEPTION
            'reconfiguration is forbidden while the slot is % (member_protocol.md §2)',
            current_state
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER slot_generation_follows_slot
    BEFORE INSERT ON pool.slot_generation
    FOR EACH ROW
    EXECUTE FUNCTION pool.slot_generation_follows_the_slot();

-- And the slot points at a generation that exists. Declared after
-- `pool.slot_generation` for the obvious reason, and deferrable so an initial
-- registration can insert both rows in one transaction.
ALTER TABLE pool.slot
    ADD CONSTRAINT slot_is_at_a_registered_generation
        FOREIGN KEY (network, slot_id, generation)
        REFERENCES pool.slot_generation (network, slot_id, generation)
        DEFERRABLE INITIALLY DEFERRED;

-- ---------------------------------------------------------------------------
-- What a generation proved
-- ---------------------------------------------------------------------------

-- §6: the pool issues a task for one exact `qualification_spec_digest`, the
-- agent runs the known-output execution, and the pool "independently hashes the
-- canonical result, compares the expected quality and output, and verifies that
-- the observed inventory and compute facts still produce the qualification
-- digest". Success qualifies that exact generation; a mismatch fails it.
CREATE TABLE pool.slot_qualification (
    network          text   NOT NULL,
    qualification_id uuid   NOT NULL,
    slot_id          uuid   NOT NULL,
    generation       bigint NOT NULL,

    -- The digest the task was issued for. Kept here as well as on the
    -- generation so a result can be checked against what was asked for without
    -- trusting that the generation has not moved underneath.
    spec_digest      bytea  NOT NULL,

    -- §2's immutable identity of the known-output execution.
    fixture_id       text   NOT NULL,

    state            text   NOT NULL DEFAULT 'PENDING',

    -- §6: "Tasks expire after 15 minutes."
    task_expires_at  timestamptz NOT NULL,

    -- §2's `qualification_result_id`, the worker's idempotency identity for one
    -- submitted result: "Retrying the same result ID and body returns the same
    -- status." NULL until a result arrives.
    qualification_result_id uuid,
    result_sha256           bytea,

    -- Why it failed, as a bounded code. §6 fails a qualification for reasons
    -- that are not interchangeable: the output did not match, the quality did
    -- not, or the observed facts no longer produce the digest the task was
    -- issued for.
    failure_reason   text,

    created_at       timestamptz NOT NULL DEFAULT now(),
    decided_at       timestamptz,

    PRIMARY KEY (network, qualification_id),

    CONSTRAINT qualification_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT qualification_state_known
        CHECK (state IN ('PENDING', 'QUALIFIED', 'FAILED')),
    CONSTRAINT qualification_failure_reason_known
        CHECK (failure_reason IS NULL
               OR failure_reason IN ('OUTPUT_MISMATCH', 'QUALITY_MISMATCH',
                                     'DIGEST_MISMATCH', 'EXPIRED')),
    CONSTRAINT qualification_decision_is_recorded_whole
        CHECK (
            (state = 'PENDING' AND decided_at IS NULL AND failure_reason IS NULL)
            OR
            (state = 'QUALIFIED' AND decided_at IS NOT NULL AND failure_reason IS NULL)
            OR
            (state = 'FAILED' AND decided_at IS NOT NULL AND failure_reason IS NOT NULL)
        ),
    -- A decided qualification carries the result that decided it, and a pending
    -- one carries none: §6 decides from the submitted result, so a QUALIFIED
    -- row with no result recorded is a pass nobody can re-derive. `EXPIRED` is
    -- the one failure that needs no result, because it is the absence of one.
    CONSTRAINT qualification_result_accompanies_a_decision
        CHECK (
            (state = 'PENDING' AND qualification_result_id IS NULL)
            OR
            (state = 'FAILED' AND failure_reason = 'EXPIRED')
            OR
            (state <> 'PENDING' AND qualification_result_id IS NOT NULL)
        ),
    CONSTRAINT qualification_result_hash_pairs_with_its_id
        CHECK ((qualification_result_id IS NULL) = (result_sha256 IS NULL)),
    CONSTRAINT qualification_digests_are_32_bytes
        CHECK (length(spec_digest) = 32
               AND (result_sha256 IS NULL OR length(result_sha256) = 32)),
    CONSTRAINT qualification_fixture_not_blank
        CHECK (length(trim(fixture_id)) > 0),
    -- §6: "Tasks expire after 15 minutes." Bounded here rather than left to
    -- whoever writes the INSERT, exactly as `migrations/0018` bounds §3.1's
    -- identical fifteen minutes for a ticket.
    CONSTRAINT qualification_task_lifetime_is_bounded
        CHECK (task_expires_at > created_at
               AND task_expires_at <= created_at + interval '15 minutes'),
    -- And a decision comes from a result submitted before the task expired.
    -- `EXPIRED` is the exception it names: that decision is *about* the
    -- deadline, so it is the one that may be recorded after it.
    -- `COALESCE`, not a bare comparison: a CHECK passes when its expression is
    -- TRUE *or NULL*, so `failure_reason = 'EXPIRED'` on a QUALIFIED row —
    -- where the reason is NULL — made the whole disjunction NULL and let a
    -- task be passed after it expired. Three-valued logic turns an omitted
    -- value into permission.
    CONSTRAINT qualification_is_decided_before_it_expires
        CHECK (decided_at IS NULL
               OR decided_at <= task_expires_at
               OR COALESCE(failure_reason, '') = 'EXPIRED'),
    -- The generation is the thing being qualified, so it must exist. §16
    -- invariant 2 is that a slot offers only with "a successful qualification
    -- for its exact current generation".
    CONSTRAINT qualification_has_a_generation
        FOREIGN KEY (network, slot_id, generation)
        REFERENCES pool.slot_generation (network, slot_id, generation)
);

-- Invariant 3: a retried result cannot create a second qualification record.
CREATE UNIQUE INDEX qualification_result_is_idempotent
    ON pool.slot_qualification (network, qualification_result_id)
    WHERE qualification_result_id IS NOT NULL;

-- One generation qualifies once. Without this a slot could hold two QUALIFIED
-- rows for one generation — harmless-looking until a failed requalification
-- leaves an older pass still standing beside it.
CREATE UNIQUE INDEX qualification_is_one_per_generation
    ON pool.slot_qualification (network, slot_id, generation)
    WHERE state = 'QUALIFIED';

CREATE INDEX qualification_by_slot
    ON pool.slot_qualification (network, slot_id, generation, state);

-- A qualification decides once, and what it decided about does not move. §6
-- requalifies by issuing a *new* task; nothing rewrites the verdict on an old
-- one, and a QUALIFIED row that could be flipped back to PENDING is a slot
-- losing its standing without anyone deciding so.
CREATE OR REPLACE FUNCTION pool.qualification_decides_once()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.qualification_id IS DISTINCT FROM OLD.qualification_id
       OR NEW.slot_id IS DISTINCT FROM OLD.slot_id
       OR NEW.generation IS DISTINCT FROM OLD.generation
       OR NEW.spec_digest IS DISTINCT FROM OLD.spec_digest
       OR NEW.fixture_id IS DISTINCT FROM OLD.fixture_id
       OR NEW.task_expires_at IS DISTINCT FROM OLD.task_expires_at
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'a qualification task is fixed when it is issued (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF OLD.state <> 'PENDING' THEN
        RAISE EXCEPTION
            'a qualification decides once; requalify by issuing a new task (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER qualification_decided_once
    BEFORE UPDATE ON pool.slot_qualification
    FOR EACH ROW
    EXECUTE FUNCTION pool.qualification_decides_once();

-- The task is issued for the generation's own digest. §6 binds a task to one
-- exact `qualification_spec_digest`, and a task issued against a digest the
-- generation does not have would qualify facts nobody registered.
CREATE OR REPLACE FUNCTION pool.qualification_matches_its_generation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    registered bytea;
BEGIN
    SELECT spec_digest INTO registered
      FROM pool.slot_generation
     WHERE network = NEW.network
       AND slot_id = NEW.slot_id
       AND generation = NEW.generation;

    IF registered IS DISTINCT FROM NEW.spec_digest THEN
        RAISE EXCEPTION
            'a qualification task carries its generation''s spec digest (member_protocol.md §6)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER qualification_matches_generation
    BEFORE INSERT ON pool.slot_qualification
    FOR EACH ROW
    EXECUTE FUNCTION pool.qualification_matches_its_generation();

-- ---------------------------------------------------------------------------
-- §9's ladder
-- ---------------------------------------------------------------------------

-- The slot's state moves one rung at a time, and back to AVAILABLE from
-- anywhere.
--
-- §9 gives two paths — with and without the queue — and §12 gives the release:
-- the slot "returns to `AVAILABLE` only when the durable acceptance receipt is
-- committed", and a cancellation or expiry releases it too. Skipping forward is
-- what this refuses: a slot that reaches UPLOADING without COMPUTING is a
-- package for work nobody did.
CREATE OR REPLACE FUNCTION pool.slot_state_advances_one_rung()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.slot_id IS DISTINCT FROM OLD.slot_id
       OR NEW.worker_id IS DISTINCT FROM OLD.worker_id
       OR NEW.client_slot_key IS DISTINCT FROM OLD.client_slot_key
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'a slot''s identity and owner are fixed (member_protocol.md §2)'
            USING ERRCODE = 'raise_exception';
    END IF;

    -- The pointer half of a reconfiguration, guarded the same way the row half
    -- is. §2 has a registration "atomically increment the generation" and
    -- forbids reconfiguration "while an offer, assignment, or upload is open" —
    -- and both rules were enforced only on the `pool.slot_generation` INSERT,
    -- so this UPDATE could move the slot to any generation, at any time,
    -- including while it was RESERVED. A slot at a generation its admitted
    -- offer was not qualified under is §16 invariant 2 broken by a single
    -- statement.
    IF NEW.generation IS DISTINCT FROM OLD.generation THEN
        IF NEW.generation <> OLD.generation + 1 THEN
            RAISE EXCEPTION
                'a slot moves to the next generation, not from % to % (member_protocol.md §2)',
                OLD.generation, NEW.generation
                USING ERRCODE = 'raise_exception';
        END IF;

        IF OLD.state <> 'AVAILABLE' OR NEW.state <> 'AVAILABLE' THEN
            RAISE EXCEPTION
                'reconfiguration is forbidden while the slot is % (member_protocol.md §2)',
                CASE WHEN OLD.state <> 'AVAILABLE' THEN OLD.state ELSE NEW.state END
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    IF NEW.state <> OLD.state THEN
        IF NOT (
            NEW.state = 'AVAILABLE'
            OR (OLD.state = 'AVAILABLE' AND NEW.state IN ('QUEUED', 'RESERVED'))
            OR (OLD.state = 'QUEUED'    AND NEW.state = 'RESERVED')
            OR (OLD.state = 'RESERVED'  AND NEW.state = 'COMPUTING')
            OR (OLD.state = 'COMPUTING' AND NEW.state = 'PACKAGING')
            OR (OLD.state = 'PACKAGING' AND NEW.state = 'UPLOADING')
        ) THEN
            RAISE EXCEPTION
                'a slot does not go from % to % (member_protocol.md §9)',
                OLD.state, NEW.state
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER slot_state_ladder
    BEFORE UPDATE ON pool.slot
    FOR EACH ROW
    EXECUTE FUNCTION pool.slot_state_advances_one_rung();

-- ---------------------------------------------------------------------------
-- Grants (architecture.md §6)
-- ---------------------------------------------------------------------------

-- §6: "Register/reconfigure/qualify slot | Pool API". Registration creates the
-- slot and its generation; qualification issues the task and records the
-- verdict.
GRANT SELECT, INSERT ON pool.slot TO pool_api;
GRANT UPDATE (generation) ON pool.slot TO pool_api;
GRANT SELECT, INSERT ON pool.slot_generation TO pool_api;
GRANT SELECT, INSERT ON pool.slot_qualification TO pool_api;
GRANT UPDATE (state, qualification_result_id, result_sha256, failure_reason, decided_at)
    ON pool.slot_qualification TO pool_api;

-- §6: "Admit/reject/queue an offer and reserve its slot | Controller". The
-- operational state of a slot is the controller's alone — the API records what
-- a member said, the controller decides what the pool does about it. That split
-- is what keeps a member from reserving their own slot by asserting a state.
GRANT SELECT ON pool.slot TO pool_controller;
GRANT UPDATE (state) ON pool.slot TO pool_controller;
GRANT SELECT ON pool.slot_generation TO pool_controller;
GRANT SELECT ON pool.slot_qualification TO pool_controller;

GRANT SELECT ON pool.slot TO pool_readonly;
GRANT SELECT ON pool.slot_generation TO pool_readonly;
GRANT SELECT ON pool.slot_qualification TO pool_readonly;
