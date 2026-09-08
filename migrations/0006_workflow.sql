-- Slice 1 criteria F1, F2, F3 and F6: the confirmation-driven workflow row.
--
-- docs/tig_integration.md §7 is the whole point: "the controller advances
-- local state only from confirmed reads". A recorded HTTP 200 is a transport
-- fact, not a protocol fact, and this table is where that distinction is kept
-- — every state below names the confirmed evidence that produces it, and
-- there is no state reachable from a response alone.

CREATE TABLE pool.workflow (
    workflow_id       text        NOT NULL,
    network           text        NOT NULL,

    state             text        NOT NULL DEFAULT 'DECIDED',

    -- §7.5: short transitions lock this row and use a monotonic revision.
    -- Every transition is a compare-and-set on it, so a claimant working from
    -- a stale read cannot commit.
    revision          integer     NOT NULL DEFAULT 1,

    -- F6: the permanent owner, from creation.
    --
    -- `mining_system.md` §2 requires a permanent benchmark_id -> owner
    -- mapping and §7.6 reads it, so it cannot be added once members exist:
    -- a row created without one could only be back-filled by guessing.
    -- Slice 1 has no members, so it writes a pool-owned placeholder that
    -- cannot be mistaken for a member id, and the CHECK below is what makes
    -- "cannot be mistaken" structural rather than a naming convention.
    owner_kind        text        NOT NULL,
    owner_id          text        NOT NULL,

    -- The TIG benchmark this workflow owns, once its precommit confirms.
    -- NULL before that: §7 says a submitted precommit is not a confirmed one,
    -- and TIG assigns the id.
    benchmark_id      text,

    -- F6's unverified interval, and `mining_system.md` §6.1 fixes both ends:
    -- a benchmark is unverified "from creation of its pool precommit intent
    -- until TIG records it as verified or it reaches a terminal stopped,
    -- expired, or failed state".
    --
    -- **From creation**, not from confirmation. §7.6's concurrency counting
    -- reads this, and opening it later would leave the blocks between the
    -- intent and its confirmation uncounted — under-counting concurrency in
    -- exactly the direction §10 invariants 22 and 23 forbid. So it is NOT
    -- NULL: every workflow is unverified the moment it exists.
    unverified_from_block bigint     NOT NULL,
    unverified_to_block   bigint,

    -- The confirmed settings, which REPLACE the proposed ones (F2, §7).
    -- Stored separately from pool.precommit_decision's proposal so the two
    -- can be compared afterwards rather than one overwriting the other.
    confirmed_track_id       text,
    confirmed_settings       jsonb,
    precommit_confirmed_block bigint,

    -- Why a terminal state was reached. F4b bounds what slice 1 may record
    -- here: no member fault and no chargeable member failure, because
    -- attributing fault to a member needs members.
    terminal_reason   text,

    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, workflow_id),

    CONSTRAINT workflow_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT workflow_revision_positive
        CHECK (revision >= 1),

    -- `mining_system.md` §4.5 owns the protocol lifecycle and names the states
    -- the local machine "must distinguish at least". These are the ones slice
    -- 1 can reach.
    --
    -- The *_SUBMITTED states record that a write was sent. §4.5 puts them in
    -- the ladder and says in the same breath that "TIG HTTP acceptance alone
    -- is not protocol confirmation" — which is the point: a submission is a
    -- local fact worth knowing (it is what §10 reconciles against, and what
    -- distinguishes "decided but unsent" from "sent and awaiting an answer"),
    -- and it is emphatically not a confirmation. Every _CONFIRMED state below
    -- names the confirmed TIG evidence `tig_integration.md` §7 requires:
    --
    --   PRECOMMIT_CONFIRMED  get-benchmarks.precommits, block_confirmed set
    --   BENCHMARK_CONFIRMED  get-benchmarks.benchmarks, block_confirmed set
    --   PROOF_CONFIRMED      get-benchmarks.proofs, block_confirmed set
    --   VERIFIED             benchmark id in block.data.confirmed_ids.verified
    --   STOPPED              a confirmed benchmark with details.stopped true;
    --                        §7 sends no proof for one
    --   FRAUDULENT           get-benchmarks.frauds, block_confirmed set
    --   EXPIRED              a local deadline passed (§8), not TIG's word
    --   FAILED               a terminal pool-side failure
    --
    -- §4.5's member- and artifact-facing states — ASSIGNED, COMPUTING,
    -- PACKAGING, the PACKAGE_* ladder, PROOF_BUILDING, PROOF_READY, VERIFYING,
    -- ACTIVE — are absent because slice 1 has neither members nor artifacts to
    -- reach them. They arrive with their own slices; nothing here rules them
    -- out.
    CONSTRAINT workflow_state_known
        CHECK (state IN (
            'DECIDED',
            'PRECOMMIT_SUBMITTED',
            'PRECOMMIT_CONFIRMED',
            'BENCHMARK_SUBMITTED',
            'BENCHMARK_CONFIRMED',
            'PROOF_SUBMITTED',
            'PROOF_CONFIRMED',
            'VERIFIED',
            'STOPPED',
            'FRAUDULENT',
            'EXPIRED',
            'FAILED'
        )),

    -- F6, F4b. Slice 1 writes POOL_BOOTSTRAP only; MEMBER exists so the
    -- registration slice adds rows rather than altering this constraint, and
    -- so the negative test has something to assert against.
    CONSTRAINT workflow_owner_kind_known
        CHECK (owner_kind IN ('POOL_BOOTSTRAP', 'MEMBER')),
    -- A pool-owned row's id is exactly the sentinel. Nothing that looks like a
    -- member id can be recorded under POOL_BOOTSTRAP, and nothing pool-owned
    -- can be quietly renamed into something member-shaped.
    CONSTRAINT workflow_pool_owner_is_the_sentinel
        CHECK (owner_kind <> 'POOL_BOOTSTRAP' OR owner_id = 'pool-bootstrap'),
    -- `mining_system.md` §10 invariant 1's carve-out is bounded to testnet, so
    -- the bound is enforced rather than asserted. A mainnet benchmark has a
    -- member owner or it does not exist: bootstrap rows are the pre-member
    -- exception, and mainnet has no pre-member period to be excepted from.
    CONSTRAINT workflow_bootstrap_is_testnet_only
        CHECK (owner_kind <> 'POOL_BOOTSTRAP' OR network = 'testnet'),
    CONSTRAINT workflow_member_owner_is_not_the_sentinel
        CHECK (owner_kind <> 'MEMBER' OR owner_id <> 'pool-bootstrap'),

    -- A benchmark_id exists exactly when the precommit has confirmed, and the
    -- confirmed settings arrive with it (§7, F2).
    -- A benchmark_id exists once TIG has assigned one, which is at precommit
    -- confirmation. The states before that have none, and the two local
    -- terminal states may be reached from either side.
    CONSTRAINT workflow_benchmark_id_follows_confirmation
        CHECK (
            (state IN ('DECIDED', 'PRECOMMIT_SUBMITTED') AND benchmark_id IS NULL)
            OR (state IN ('EXPIRED', 'FAILED'))
            OR benchmark_id IS NOT NULL
        ),
    CONSTRAINT workflow_confirmed_settings_arrive_together
        CHECK (
            (confirmed_track_id IS NULL
                AND confirmed_settings IS NULL
                AND precommit_confirmed_block IS NULL)
            OR (confirmed_track_id IS NOT NULL
                AND confirmed_settings IS NOT NULL
                AND precommit_confirmed_block IS NOT NULL)
        ),

    -- §6.1's interval opened when the workflow was created; it never closes
    -- before it opened, and never reopens once closed.
    CONSTRAINT workflow_unverified_interval_ordered
        CHECK (
            unverified_to_block IS NULL
            OR unverified_to_block >= unverified_from_block
        ),
    CONSTRAINT workflow_unverified_from_non_negative
        CHECK (unverified_from_block >= 0),

    CONSTRAINT workflow_terminal_reason_only_when_terminal
        CHECK (
            terminal_reason IS NULL
            OR state IN ('STOPPED', 'FRAUDULENT', 'EXPIRED', 'FAILED')
        )
);

-- One workflow owns one TIG benchmark. The partial index rather than a plain
-- unique constraint because benchmark_id is NULL until confirmation, and two
-- undecided workflows are not duplicates of each other.
CREATE UNIQUE INDEX workflow_one_per_benchmark
    ON pool.workflow (network, benchmark_id)
    WHERE benchmark_id IS NOT NULL;

-- F6: the owner mapping is immutable once written.
--
-- `mining_system.md` §8 charges faults to an owner, so a row whose owner could
-- change is a row whose past faults could be re-attributed. Slice-1 rows stay
-- pool-owned for life: the registration slice supplies real member owners for
-- benchmarks created from then on and never rewrites an existing row.
CREATE OR REPLACE FUNCTION pool.workflow_owner_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.owner_kind IS DISTINCT FROM OLD.owner_kind
        OR NEW.owner_id IS DISTINCT FROM OLD.owner_id
    THEN
        RAISE EXCEPTION
            'pool.workflow %: owner is immutable (% % -> % %)',
            OLD.workflow_id, OLD.owner_kind, OLD.owner_id,
            NEW.owner_kind, NEW.owner_id
            USING ERRCODE = 'raise_exception';
    END IF;

    -- Once TIG has assigned a benchmark to this workflow, that binding is
    -- the workflow's for life. `mining_system.md` §2 requires a permanent
    -- benchmark → owner mapping and §8 charges faults through it, so a
    -- workflow that could be re-pointed at a different benchmark would leave
    -- the first one with no owner row and nothing to attribute its faults to.
    IF OLD.benchmark_id IS NOT NULL
        AND NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
    THEN
        RAISE EXCEPTION
            'pool.workflow %: benchmark_id is immutable (% -> %)',
            OLD.workflow_id, OLD.benchmark_id, NEW.benchmark_id
            USING ERRCODE = 'raise_exception';
    END IF;

    -- §7.5: the revision is monotonic. A transition that did not advance it
    -- is a lost update, whatever the caller intended.
    IF NEW.revision <= OLD.revision THEN
        RAISE EXCEPTION
            'pool.workflow %: revision must advance (% -> %)',
            OLD.workflow_id, OLD.revision, NEW.revision
            USING ERRCODE = 'raise_exception';
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER workflow_owner_and_revision
    BEFORE UPDATE ON pool.workflow
    FOR EACH ROW
    EXECUTE FUNCTION pool.workflow_owner_is_immutable();

-- F6, made structural: a write intent cannot exist without the owner mapping
-- for the workflow it belongs to.
--
-- `mining_system.md` §2 requires the benchmark → owner mapping to be
-- permanent and §8 charges faults to it, so an intent whose workflow row was
-- never created is a write the pool could make and then be unable to attribute.
-- The admission transaction (`architecture.md` §7.2) creates the workflow, the
-- decision and the intent together, and this key is what makes that ordering
-- impossible to skip rather than merely conventional.
ALTER TABLE pool.tig_write_intent
    ADD CONSTRAINT tig_write_intent_has_a_workflow
        FOREIGN KEY (network, workflow_id)
        REFERENCES pool.workflow (network, workflow_id);

GRANT SELECT, INSERT, UPDATE ON pool.workflow TO pool_controller;

-- The gateway reads a workflow to know what it is transmitting for and cannot
-- advance one: `architecture.md` invariant 2 says it "cannot decide or
-- manufacture" a write, and D4 tests that it cannot change a workflow.
GRANT SELECT ON pool.workflow TO pool_gateway;

GRANT SELECT ON pool.workflow TO pool_readonly;
