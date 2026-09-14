-- The second thing a benchmark commitment intent may not exist without.
--
-- `architecture.md` §13 invariant 4 names one precondition — durable package
-- acceptance — and `migrations/0011` enforces it. This adds the other one the
-- *write path* requires, which is not a matter of policy but of mechanism.
--
-- §7.3 records an intent's `payload_digest` before the send, and the gateway
-- refuses to transmit bytes that do not reproduce it. For a precommit the
-- gateway rebuilds those bytes from `pool.precommit_decision`. A commitment's
-- bytes are the ordered quality vector and the Merkle root
-- (`tig_integration.md` §6.2) — built from the accepted package by the
-- artifact worker, which `architecture.md` §3 lists as its own output:
-- "canonical commitment payload construction". Without a durable record of
-- that construction there is nothing for the gateway to check the bytes
-- against, and the digest guard is a check on a value nobody can reproduce.
--
-- So this table is that record, and the trigger requires it. That makes
-- invariant 4 strictly stronger — acceptance *and* a built commitment — which
-- is the direction §13 invariants may move.
--
-- A sibling of `pool.canonical_payload`, not a column on it. That table says
-- what it means: a proof payload for *the confirmed sample*, with the sample
-- as a NOT NULL part of the key question. A commitment answers no sample, so
-- sharing the table would have meant making `sample_digest` nullable — a
-- weakening of the row that carries invariant 5 — to accommodate a row that
-- is not the same kind of thing.
--
-- **No payload bytes here.** `architecture.md` §3 excludes large payloads
-- from the Workflow Database and §8.2 publishes derived payloads to the
-- artifact store under a deterministic immutable key; §9 grants the gateway
-- read-only access to them. What is durable here is that key and the digest.
-- Slice 1 has no artifact store and no members, so its bytes come from F4a's
-- stub and go straight to the gateway in memory; the digest is what makes
-- that safe, and the row is what makes the digest checkable.

CREATE TABLE pool.commitment_payload (
    network        text        NOT NULL,
    -- The deterministic immutable key the payload is published under, the
    -- same role `canonical_payload.artifact_id` plays for a proof.
    artifact_id    text        NOT NULL,

    workflow_id    text        NOT NULL,
    benchmark_id   text        NOT NULL,

    -- SHA-256 over the §6.2 body. The intent carries the same value and the
    -- trigger requires them to agree, so an intent cannot cite this payload
    -- while transmitting different bytes.
    payload_digest bytea       NOT NULL,

    -- Whether F4a's stub built this rather than the artifact worker. Recorded
    -- here for the reason `package_acceptance.stub_origin` is: a fabricated
    -- row written without it is indistinguishable from a real one for the
    -- rest of the database's life, and no later migration recovers what it
    -- was.
    stub_origin    boolean     NOT NULL DEFAULT false,

    built_at       timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, artifact_id),

    CONSTRAINT commitment_payload_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT commitment_payload_digest_is_a_digest
        CHECK (octet_length(payload_digest) = 32),
    CONSTRAINT commitment_payload_has_a_workflow
        FOREIGN KEY (network, workflow_id)
        REFERENCES pool.workflow (network, workflow_id)
);

-- The lookup the trigger performs on every benchmark intent.
CREATE INDEX commitment_payload_by_workflow
    ON pool.commitment_payload (network, workflow_id, benchmark_id);

-- One commitment per benchmark, whatever key it was published under.
--
-- Two payloads for one benchmark would mean two different quality vectors
-- were built from one accepted package, which is either a bug or a second
-- package — and §6.2's write is once per benchmark either way. Enforced here
-- rather than left to the caller, since the caller that gets it wrong is the
-- one that submits the wrong commitment.
CREATE UNIQUE INDEX commitment_payload_one_per_benchmark
    ON pool.commitment_payload (network, benchmark_id);

-- Immutable, like every other precondition row: it is the evidence a write
-- was permitted.
CREATE OR REPLACE FUNCTION pool.commitment_payload_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'pool.commitment_payload %/%: a built payload is immutable',
        OLD.network, OLD.artifact_id
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER commitment_payload_no_update
    BEFORE UPDATE ON pool.commitment_payload
    FOR EACH ROW
    EXECUTE FUNCTION pool.commitment_payload_is_immutable();

-- ---------------------------------------------------------------------------
-- 0011's trigger, extended.
-- ---------------------------------------------------------------------------
--
-- Replaced whole rather than patched: the function is one definition of what
-- a write intent's preconditions are, and a second function checking "the
-- other half" would be two places to read. The proof branch is unchanged.
CREATE OR REPLACE FUNCTION pool.tig_write_intent_preconditions_hold()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- §13 invariant 4, both halves.
    IF NEW.write_kind = 'benchmark' THEN
        IF NOT EXISTS (
            SELECT 1 FROM pool.package_acceptance a
             WHERE a.network = NEW.network
               AND a.workflow_id = NEW.workflow_id
               AND a.benchmark_id = NEW.benchmark_id
        ) THEN
            RAISE EXCEPTION
                'tig_write_intent %/%: no durable package acceptance for '
                'benchmark % (architecture.md §13 invariant 4)',
                NEW.network, NEW.workflow_id, NEW.benchmark_id
                USING ERRCODE = 'raise_exception';
        END IF;
        IF NEW.payload_artifact_id IS NULL THEN
            RAISE EXCEPTION
                'tig_write_intent %/%: a benchmark commitment names no built '
                'payload (architecture.md §13 invariant 4; §7.3)',
                NEW.network, NEW.workflow_id
                USING ERRCODE = 'raise_exception';
        END IF;
        -- Artifact, benchmark and digest together, for the reason the proof
        -- branch checks all three: a real payload built for another
        -- benchmark, or bytes the payload does not contain, satisfies the
        -- letter and not the point.
        IF NOT EXISTS (
            SELECT 1 FROM pool.commitment_payload c
             WHERE c.network = NEW.network
               AND c.artifact_id = NEW.payload_artifact_id
               AND c.workflow_id = NEW.workflow_id
               AND c.benchmark_id = NEW.benchmark_id
               AND c.payload_digest = NEW.payload_digest
        ) THEN
            RAISE EXCEPTION
                'tig_write_intent %/%: no commitment payload % for benchmark % '
                'with this digest (architecture.md §13 invariant 4; §7.3)',
                NEW.network, NEW.workflow_id,
                NEW.payload_artifact_id, NEW.benchmark_id
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    -- §13 invariant 5. All three of artifact, benchmark and payload digest
    -- must line up: an intent pointing at a real payload built for another
    -- benchmark, or transmitting bytes the payload does not contain, satisfies
    -- the invariant's letter and not its point.
    IF NEW.write_kind = 'proof' THEN
        IF NEW.payload_artifact_id IS NULL THEN
            RAISE EXCEPTION
                'tig_write_intent %/%: a proof write names no canonical '
                'payload (architecture.md §13 invariant 5)',
                NEW.network, NEW.workflow_id
                USING ERRCODE = 'raise_exception';
        END IF;
        IF NOT EXISTS (
            SELECT 1 FROM pool.canonical_payload p
             WHERE p.network = NEW.network
               AND p.artifact_id = NEW.payload_artifact_id
               AND p.workflow_id = NEW.workflow_id
               AND p.benchmark_id = NEW.benchmark_id
               AND p.payload_digest = NEW.payload_digest
        ) THEN
            RAISE EXCEPTION
                'tig_write_intent %/%: no canonical payload % for benchmark % '
                'with this digest (architecture.md §13 invariant 5)',
                NEW.network, NEW.workflow_id,
                NEW.payload_artifact_id, NEW.benchmark_id
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    RETURN NEW;
END;
$$;

GRANT SELECT, INSERT ON pool.commitment_payload TO pool_controller;
-- Nothing for the gateway, matching `0011`'s treatment of the sibling
-- `canonical_payload`: §13 invariant 2 keeps the gateway out of deciding what
-- a write contains, and these tables are exactly that decision. The intent
-- row already names the payload it cites and carries the digest the gateway
-- checks. A grant with no reader is a grant nobody can justify later.
GRANT SELECT ON pool.commitment_payload TO pool_readonly;
