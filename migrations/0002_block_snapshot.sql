-- Slice 1 criterion C2: accepted snapshot records.
--
-- docs/tig_integration.md §9 step 7 requires an accepted snapshot to be
-- persisted atomically with its completeness status before any decision
-- derived from it. This table is that record.
--
-- It is deliberately COMPACT. architecture.md §3 scopes the Workflow
-- Database to "compact authoritative ... snapshot ... facts" and excludes
-- "an unbounded copy of TIG responses", so the response bodies do not live
-- here: a row carries the block anchor, the completeness status, and a
-- digest binding the exact content that was accepted. The §12 values a
-- decision reads are extracted into their own columns by the slice that
-- first needs them, one behavior at a time.
--
-- One block may accumulate SEVERAL rows, one per distinct accepted
-- assembly. §9 step 6 and §5.2 both make re-assembly at the same block a
-- normal path, and an assembly whose reads were incomplete — or whose
-- active-benchmark cache was not yet ready — is legitimately superseded by
-- a later, better one. Keying the table on the block alone would have made
-- the first partial assembly permanent and left the block unable to ever
-- gain a usable record.

CREATE TABLE pool.block_snapshot (
    network            text        NOT NULL,
    block_id           text        NOT NULL,

    -- SHA-256 over the assembled snapshot, and part of the key: an assembly
    -- that recorded more than an earlier one is different content, not an
    -- edit of it. Re-persisting byte-identical content — the crash-retry
    -- path — collides here and is idempotent.
    content_digest     bytea       NOT NULL,

    height             bigint      NOT NULL,

    -- Written in the same row as the content they describe, which is what
    -- makes step 7 atomic: there is no window where a snapshot exists
    -- without its status. Two flags rather than one because criterion C5
    -- gates orchestrator work on both, and a single `complete` column would
    -- be read as the whole gate.
    reads_complete     boolean     NOT NULL,
    active_cache_ready boolean     NOT NULL,

    accepted_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, block_id, content_digest),

    CONSTRAINT block_snapshot_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT block_snapshot_digest_is_sha256
        CHECK (octet_length(content_digest) = 32),
    CONSTRAINT block_snapshot_height_non_negative
        CHECK (height >= 0)
);

-- At most one snapshot per block may be usable for a decision.
--
-- This is the invariant that actually matters, and the one §9 protects: the
-- block's data is immutable, so two *usable* assemblies of one block that
-- disagree are a contradiction, not a supersession. Partial assemblies may
-- coexist — none of them can reach a decision — but the moment one becomes
-- usable it is the only one, and a second, different one is rejected by the
-- database rather than by the code that happens to be writing.
CREATE UNIQUE INDEX block_snapshot_one_usable_per_block
    ON pool.block_snapshot (network, block_id)
    WHERE reads_complete AND active_cache_ready;

-- No UPDATE and no DELETE for the controller, deliberately.
--
-- A recorded assembly is immutable (§9 forbids refetching and substituting a
-- field into an accepted snapshot). Superseding it means INSERTing the
-- better assembly, never editing the recorded one. Withholding the
-- privilege makes that a property of the database rather than of the code:
-- a future component that decided to "correct" a persisted snapshot in
-- place is refused by PostgreSQL, not by a code review.
GRANT SELECT, INSERT ON pool.block_snapshot TO pool_controller;

-- Monitoring reads snapshot age and completeness (architecture.md §10.2).
-- Granted explicitly, per 0001's rule that each slice makes its own
-- monitoring-visibility decision rather than inheriting a blanket default.
GRANT SELECT ON pool.block_snapshot TO pool_readonly;
