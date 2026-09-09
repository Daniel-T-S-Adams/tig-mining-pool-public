-- Slice 1 criterion F4a: the two facts a benchmark or proof write may not
-- exist without.
--
-- `architecture.md` §13 invariants 4 and 5:
--
--   4. No benchmark commitment intent exists before durable package
--      acceptance.
--   5. No proof intent exists before a canonical proof payload for the
--      confirmed sample.
--
-- Both were unenforced. `pool.tig_write_intent` accepts a `benchmark` or
-- `proof` row today with nothing behind it, and `payload_artifact_id` is free
-- text pointing at nothing. That is not a gap the write path can be trusted to
-- close by convention: §13 calls these invariants, and an invariant a caller
-- can forget is a rule that holds until the first caller forgets it.
--
-- These tables are **minimal on purpose**. The real durable-acceptance record
-- belongs to the member and upload slice (`pre_build_checklist.md` §10 step 2)
-- — it carries the assignment, the receipt, the slot release and the accepted
-- object's provenance. Slice 1 has no members, so inventing that shape here
-- would mean guessing at a schema step 2 must then live with, or migrate away
-- from, for no benefit today. What slice 1 needs is exactly the *precondition*:
-- a durable row that says acceptance happened, that the write path can be
-- required to find. Step 2 extends these; it does not replace the check.
--
-- `architecture.md` §7.1: tables are added as an invariant needs them.

-- ---------------------------------------------------------------------------
-- Invariant 4's subject: the package for a benchmark was durably accepted.
-- ---------------------------------------------------------------------------
CREATE TABLE pool.package_acceptance (
    network        text        NOT NULL,
    workflow_id    text        NOT NULL,
    -- The benchmark the accepted package is for. Part of the key rather than a
    -- payload column: acceptance is a fact about *this* package for *this*
    -- benchmark, and a row that could be re-pointed at another benchmark would
    -- satisfy invariant 4 for a package nobody accepted.
    benchmark_id   text        NOT NULL,

    -- Whole-package SHA-256, as the acceptance recorded it. Slice 1 does not
    -- re-verify it — the artifact worker does that in step 2 — but the write
    -- path must be able to say *which* bytes were accepted, or "acceptance
    -- happened" names nothing.
    package_sha256 bytea       NOT NULL,

    accepted_at    timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, workflow_id, benchmark_id),

    CONSTRAINT package_acceptance_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT package_acceptance_sha256_is_a_digest
        CHECK (octet_length(package_sha256) = 32),

    CONSTRAINT package_acceptance_has_a_workflow
        FOREIGN KEY (network, workflow_id)
        REFERENCES pool.workflow (network, workflow_id)
);

-- ---------------------------------------------------------------------------
-- Invariant 5's subject: a canonical proof payload exists for the sample TIG
-- confirmed.
-- ---------------------------------------------------------------------------
CREATE TABLE pool.canonical_payload (
    network        text        NOT NULL,
    -- Deterministic, immutable, content-addressed: `architecture.md` §6
    -- publishes derived payloads under a "deterministic immutable key and
    -- content hash". This is that key, and it is what an intent points at.
    artifact_id    text        NOT NULL,

    workflow_id    text        NOT NULL,
    benchmark_id   text        NOT NULL,

    -- The confirmed sample this payload answers. §7 makes the sampled nonces a
    -- confirmed read, and invariant 5 is specifically about the payload being
    -- for *the confirmed sample* — a payload built for a different sample is
    -- as absent as none at all.
    sample_digest  bytea       NOT NULL,

    -- SHA-256 over the canonical payload bytes. The write intent carries the
    -- same value, and the trigger below requires them to agree, so an intent
    -- cannot point at a payload while transmitting different bytes.
    payload_digest bytea       NOT NULL,

    built_at       timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, artifact_id),

    CONSTRAINT canonical_payload_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT canonical_payload_sample_is_a_digest
        CHECK (octet_length(sample_digest) = 32),
    CONSTRAINT canonical_payload_digest_is_a_digest
        CHECK (octet_length(payload_digest) = 32),

    CONSTRAINT canonical_payload_has_a_workflow
        FOREIGN KEY (network, workflow_id)
        REFERENCES pool.workflow (network, workflow_id)
);

-- The lookup the trigger below performs on every benchmark/proof intent.
CREATE INDEX canonical_payload_by_workflow
    ON pool.canonical_payload (network, workflow_id, benchmark_id);

-- ---------------------------------------------------------------------------
-- The enforcement.
-- ---------------------------------------------------------------------------
--
-- A trigger, not a foreign key. The rule is conditional on `write_kind` —
-- a precommit has neither precondition and must stay insertable with neither —
-- and a foreign key cannot be conditional. It is also not a CHECK: both
-- preconditions are facts in *other* tables, which a row constraint cannot
-- see.
--
-- BEFORE INSERT only. §7.3 already makes an intent's kind, key and payload
-- immutable (`0003`'s `tig_write_intent_identity_is_immutable`), so the
-- columns this reads cannot change afterwards, and re-checking on UPDATE
-- would make an ordinary state transition fail if step 2 ever archived an
-- acceptance row — punishing a live intent for a change to history.
CREATE OR REPLACE FUNCTION pool.tig_write_intent_preconditions_hold()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- §13 invariant 4.
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

CREATE TRIGGER tig_write_intent_preconditions
    BEFORE INSERT ON pool.tig_write_intent
    FOR EACH ROW
    EXECUTE FUNCTION pool.tig_write_intent_preconditions_hold();

-- The controller records both facts; §6 gives it "record durable acceptance,
-- receipt, and slot release" and "publish accepted artifact and derived
-- payload" is the artifact worker's, whose result the controller applies. No
-- UPDATE and no DELETE: both rows are the evidence a write was permitted, and
-- a rewritable precondition is not one.
GRANT SELECT, INSERT ON pool.package_acceptance TO pool_controller;
GRANT SELECT, INSERT ON pool.canonical_payload TO pool_controller;

-- The gateway reads them for no reason today and is granted nothing. §13
-- invariant 2 keeps it out of deciding what a write contains, and these tables
-- are exactly that decision.
GRANT SELECT ON pool.package_acceptance TO pool_readonly;
GRANT SELECT ON pool.canonical_payload TO pool_readonly;
