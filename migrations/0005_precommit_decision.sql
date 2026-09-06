-- Slice 1 criteria D2, D2a, D2c, D2d and D2e: the precommit decision record.
--
-- docs/architecture.md §5.1 step 4 makes one transaction of the whole
-- admission: recount unverified workflows, check the global limit, reserve
-- the financial exposure, derive and persist the block-specific tie input,
-- call the pure decision engine, and record the decision and one PRECOMMIT
-- intent. This table is the decision half of that transaction.
--
-- It exists now rather than later because it cannot be retrofitted. A
-- decision record is written once, and which challenges were
-- compute-compatible and eligible at that instant is not recoverable
-- afterwards (mining_system.md §6.3, ADR 0005) — so an implementation that
-- recorded only the winner would permanently lose the evidence that makes
-- the draw auditable.

-- Every tied candidate must have a draw rank. A CHECK cannot hold a subquery,
-- so the set comparison lives in an IMMUTABLE function the CHECK calls.
--
-- It answers false rather than raising for a non-array `candidates`: constraint
-- evaluation order is not defined, so this must not depend on the array-shape
-- CHECK having run first.
CREATE OR REPLACE FUNCTION pool.tie_candidates_all_ranked(candidates jsonb, ranks jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT CASE
        WHEN candidates IS NULL THEN true
        WHEN jsonb_typeof(candidates) <> 'array' THEN false
        WHEN jsonb_typeof(ranks) <> 'object' THEN false
        ELSE NOT EXISTS (
            SELECT 1
            FROM jsonb_array_elements_text(candidates) AS candidate(id)
            WHERE NOT (ranks ? candidate.id)
        )
    END
$$;

CREATE TABLE pool.precommit_decision (
    decision_id       uuid        NOT NULL DEFAULT gen_random_uuid(),

    network           text        NOT NULL,
    workflow_id       text        NOT NULL,
    -- Pairs 1:1 with pool.tig_write_intent's (network, workflow_id,
    -- 'precommit', generation). A new generation is a new decision, which is
    -- what §7.3 requires of a changed payload.
    generation        integer     NOT NULL,

    -- D2e: the anchor snapshot, by the same identity pool.block_snapshot
    -- uses. Stored rather than derived, because §6.3's draw is a pure
    -- function of this block and an auditor re-derives it from the record
    -- alone. The digest is here too: a block can carry several assemblies,
    -- and naming only the block would not say which one was read.
    anchor_block_id   text        NOT NULL,
    anchor_digest     bytea       NOT NULL,
    anchor_height     bigint      NOT NULL,

    -- D2d, the §6.3 draw. The domain string is stored, not assumed: it is
    -- versioned ("tig-pool-challenge-tie-v1"), and a record that omitted it
    -- could not be re-derived after the version changed.
    tie_domain        text        NOT NULL,
    -- challenge_id -> lowercase hex of the 32-byte rank, for EVERY
    -- compute-compatible eligible challenge. The full map is the audit
    -- evidence; the tied subset alone would not show what the field was.
    tie_draw_ranks    jsonb       NOT NULL,
    -- The tied set and the winner, NULL when no tie occurred. Two columns
    -- rather than one flag, so "there was a tie" cannot be recorded without
    -- saying who was in it.
    tie_candidates    jsonb,
    tie_winner        text,

    -- What the engine decided. The settings are the pool's own choices, at
    -- the types it chose them (mining_system.md §6.6, tig_integration.md §4):
    -- a numeric hyperparameter is a number here, not its rendering.
    selected_challenge text       NOT NULL,
    selected_algorithm text       NOT NULL,
    track_settings     jsonb      NOT NULL,

    -- D2a: the gate this decision passed, with the count and the limit it
    -- was measured against. A cached metric can never authorize work
    -- (architecture.md §7.6), so the authoritative recount is stored beside
    -- the limit that admitted it.
    pool_unverified            bigint  NOT NULL,
    unverified_limit           bigint  NOT NULL,

    -- D2c: the accounting.md §11.4 reservation INPUTS, with no posting.
    -- Slice 1 has no members, tiers or deposits to run the admission check
    -- against, so it records what the check will read and posts no batch,
    -- which keeps it clear of §8-§10's balanced-batch rules.
    reserve_inputs    jsonb       NOT NULL,
    precommit_reserve numeric(78, 0) NOT NULL,

    -- architecture.md §9: "A digest of decision-affecting configuration is
    -- stored with each decision and accounting batch."
    config_digest     bytea       NOT NULL,

    decided_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (decision_id),

    -- One decision per precommit generation. The intent's own uniqueness
    -- constraint says the same thing about the write; this says it about the
    -- reasoning, so a second decision cannot quietly attach to one intent.
    CONSTRAINT precommit_decision_unique_generation
        UNIQUE (network, workflow_id, generation),

    CONSTRAINT precommit_decision_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT precommit_decision_generation_positive
        CHECK (generation >= 1),
    CONSTRAINT precommit_decision_anchor_digest_is_sha256
        CHECK (octet_length(anchor_digest) = 32),
    CONSTRAINT precommit_decision_config_digest_is_sha256
        CHECK (octet_length(config_digest) = 32),
    CONSTRAINT precommit_decision_anchor_height_non_negative
        CHECK (anchor_height >= 0),

    -- The gate, restated as a constraint. D2a's rule is that the pool never
    -- creates a precommit at or above the limit; asserting it here means a
    -- future admission path that forgot the check is refused by PostgreSQL
    -- rather than by the code that happened to remember.
    CONSTRAINT precommit_decision_under_limit
        CHECK (pool_unverified < unverified_limit),
    CONSTRAINT precommit_decision_limit_positive
        CHECK (unverified_limit >= 1),
    CONSTRAINT precommit_decision_count_non_negative
        CHECK (pool_unverified >= 0),
    -- `>= 0` alone is not enough: PostgreSQL sorts NaN above every numeric, so
    -- NaN satisfies it. Hence the second constraint.
    --
    -- A *fractional* value cannot be caught here at all: this column has
    -- scale 0, so PostgreSQL rounds 1.5 to 2 during coercion, before any CHECK
    -- runs. That rounding is the lossy boundary accounting.md §3 forbids, and
    -- the database cannot refuse it — which is precisely why the Rust side
    -- validates the canonical atom string before binding. A constraint here
    -- asserting integrality would be trivially true and would read as though
    -- it were the defense.
    CONSTRAINT precommit_decision_reserve_non_negative
        CHECK (precommit_reserve >= 0),
    CONSTRAINT precommit_decision_reserve_is_a_number
        CHECK (precommit_reserve <> 'NaN'::numeric),

    -- D2e: a decision can only name a snapshot that was actually persisted.
    -- The transaction additionally refuses any anchor that is not the NEWEST
    -- usable one; this is the half the database can enforce by itself.
    CONSTRAINT precommit_decision_anchor_is_persisted
        FOREIGN KEY (network, anchor_block_id, anchor_digest)
        REFERENCES pool.block_snapshot (network, block_id, content_digest),

    -- A tie is recorded whole or not at all: a winner with no candidate set,
    -- or a candidate set with no winner, is a half-written audit record.
    CONSTRAINT precommit_decision_tie_recorded_whole
        CHECK (
            (tie_candidates IS NULL AND tie_winner IS NULL)
            OR (tie_candidates IS NOT NULL AND tie_winner IS NOT NULL)
        ),
    -- The winner is the selected challenge. They cannot disagree: the tie is
    -- how the selection was made.
    CONSTRAINT precommit_decision_tie_winner_is_the_selection
        CHECK (tie_winner IS NULL OR tie_winner = selected_challenge),
    CONSTRAINT precommit_decision_ranks_are_an_object
        CHECK (jsonb_typeof(tie_draw_ranks) = 'object'),
    CONSTRAINT precommit_decision_candidates_are_an_array
        CHECK (tie_candidates IS NULL OR jsonb_typeof(tie_candidates) = 'array'),

    -- The selection must have a rank, the winner must be one of the tied
    -- candidates, and every candidate must have a rank. Without all three the
    -- draw cannot be re-derived from the record, which is the only reason the
    -- record exists.
    CONSTRAINT precommit_decision_selection_has_a_rank
        CHECK (tie_draw_ranks ? selected_challenge),
    CONSTRAINT precommit_decision_winner_is_a_candidate
        CHECK (tie_candidates IS NULL OR tie_candidates ? tie_winner),
    CONSTRAINT precommit_decision_candidates_are_ranked
        CHECK (pool.tie_candidates_all_ranked(tie_candidates, tie_draw_ranks))
);

-- The decision's own trace of what it read is immutable, like the snapshot it
-- was derived from. Correcting a decision means a new generation, which is a
-- new row; UPDATE is withheld so that stays true whatever a later component
-- decides.
CREATE OR REPLACE FUNCTION pool.precommit_decision_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.precommit_decision % is immutable; a changed decision is a new generation',
        OLD.decision_id
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER precommit_decision_no_update
    BEFORE UPDATE ON pool.precommit_decision
    FOR EACH ROW
    EXECUTE FUNCTION pool.precommit_decision_is_immutable();

GRANT SELECT, INSERT ON pool.precommit_decision TO pool_controller;

-- The gateway reads a decision to build the write body it was told to send,
-- and cannot write one: architecture.md invariant 2 says the gateway "cannot
-- decide or manufacture" a protocol write, and D4 tests exactly this.
GRANT SELECT ON pool.precommit_decision TO pool_gateway;

GRANT SELECT ON pool.precommit_decision TO pool_readonly;
