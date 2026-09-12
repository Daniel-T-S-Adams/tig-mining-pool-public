-- Slice 1 criterion C5: the active-benchmark metadata cache.
--
-- docs/tig_integration.md §5.2: the compact track endpoint exposes neither a
-- benchmark id nor source hyperparameters, so the controller reads
-- `get-benchmark-data` for every id in the block's active set it has not
-- seen, and retains "only confirmed settings, selected hyperparameters and
-- fuel, algorithm and track, counts, and average_quality_by_bundle". This
-- table is what it retains. §9 step 4 advances it as part of accepting a
-- block, and a snapshot is usable for a decision only once every active id
-- in its block is here (`pool.block_snapshot.active_cache_ready`).
--
-- Durable rather than in-memory because §5.2 says "an initial cache warm-up
-- may span several blocks": the active set is hundreds of benchmarks, each
-- one read under the controller's share of the read budget, and a restart
-- that forgot them all would spend that warm-up again while the pool
-- decided nothing. mining_system.md §5.2 lists "active bundle qualities
-- needed for qualifier attribution" among the facts the first slice
-- persists, and these are those.
--
-- Keyed by benchmark id, not by block. §5.2: "confirmed precommit and
-- benchmark facts are immutable enough to cache by benchmark_id; active
-- membership is always taken from the current block." Which benchmarks are
-- active is the block's to say every time; what an active benchmark IS was
-- fixed when TIG confirmed it. A row is therefore written once. The
-- controller holds no UPDATE, so a component that decided to "refresh" a
-- confirmed fact is refused by the database.
--
-- Compact by construction (architecture.md §3): the solution-quality and
-- proof payloads §5.2 says to discard are never columns here.

CREATE TABLE pool.active_benchmark_meta (
    network           text        NOT NULL,
    benchmark_id      text        NOT NULL,

    -- The confirmed precommit: who, what, and the settings TIG fixed.
    player_id         text        NOT NULL,
    challenge_id      text        NOT NULL,
    algorithm_id      text        NOT NULL,
    track_id          text        NOT NULL,
    compute_type      text,
    num_bundles       bigint      NOT NULL,
    fuel_budget       bigint,
    -- The source hyperparameters mining_system.md §6.6 copies. JSON because
    -- their shape is the challenge's, not the pool's.
    hyperparameters   jsonb,
    precommit_block_confirmed bigint NOT NULL,

    -- The confirmed benchmark: the per-bundle qualities §7 attributes from,
    -- and the counts §5.2 names.
    num_active_bundles bigint,
    average_quality_by_bundle jsonb NOT NULL,
    stopped           boolean     NOT NULL,
    benchmark_block_confirmed bigint NOT NULL,

    -- The block the pool read this at. The facts are immutable; this is
    -- when the pool learned them, for an auditor asking why a decision at
    -- height H did or did not see this benchmark.
    fetched_at_height bigint      NOT NULL,
    fetched_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, benchmark_id),

    CONSTRAINT active_benchmark_meta_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT active_benchmark_meta_counts_non_negative
        CHECK (num_bundles >= 0
               AND (num_active_bundles IS NULL OR num_active_bundles >= 0)
               AND (fuel_budget IS NULL OR fuel_budget >= 0)),
    CONSTRAINT active_benchmark_meta_heights_non_negative
        CHECK (precommit_block_confirmed >= 0
               AND benchmark_block_confirmed >= 0
               AND fetched_at_height >= 0),
    -- The qualities are what §7 expands "into local bundle entries"; a
    -- non-array here would be a row §7 cannot read.
    CONSTRAINT active_benchmark_meta_qualities_are_a_list
        CHECK (jsonb_typeof(average_quality_by_bundle) = 'array')
);

-- The lookup §5.2 step 2 makes on every block: which of these ids are
-- absent. The primary key serves it.

GRANT SELECT, INSERT ON pool.active_benchmark_meta TO pool_controller;

-- Monitoring reads cache coverage (architecture.md §10.2, "snapshot age").
GRANT SELECT ON pool.active_benchmark_meta TO pool_readonly;
