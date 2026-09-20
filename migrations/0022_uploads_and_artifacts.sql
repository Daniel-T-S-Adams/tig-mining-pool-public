-- Slice 2, criteria G1-G7 and I1-I7: upload sessions, the quarantine chunk
-- ledger, the acceptance receipt, and the artifact rows behind them.
--
-- `member_protocol.md` §11 owns the resumable upload, §12 owns what `RECEIVED`,
-- `STRUCTURALLY_ACCEPTED` and `DURABLY_ACCEPTED` each mean, and §16 invariants
-- 8, 9, 10 and 12 are what this schema holds:
--
--    8. A package retry cannot create a second accepted artifact.
--    9. `RECEIVED` and `STRUCTURALLY_ACCEPTED` release nothing.
--   10. A durable receipt is immutable, recoverable, and releases both local
--       retention and the compute slot.
--   12. No request retry can cause a duplicate assignment, event, upload byte
--       range, durable acceptance, or TIG submission.
--
-- `architecture.md` §8.2 owns quarantine and publication, §8.3 the artifact
-- row's fields, and §6 who writes each of them.

-- ---------------------------------------------------------------------------
-- The session
-- ---------------------------------------------------------------------------

CREATE TABLE pool.upload_session (
    network       text NOT NULL,
    upload_id     uuid NOT NULL,
    assignment_id uuid NOT NULL,

    -- §2: `package_id` is the worker's identity "of one package-generation
    -- attempt for an assignment", so it is scoped to the assignment rather than
    -- trusted to be unique on its own. §11: "Starting the same `package_id`
    -- with identical declarations returns the same live upload. Changed
    -- declarations conflict."
    package_id    uuid  NOT NULL,
    declaration_sha256 bytea NOT NULL,

    -- What the member declared before a byte moved (§11, `security.md` §5.1).
    -- The pool checks these against the assignment's limits at admission and
    -- against the bytes themselves at finalization.
    media_type        text   NOT NULL,
    compressed_size   bigint NOT NULL,
    uncompressed_size bigint NOT NULL,
    manifest_sha256   bytea  NOT NULL,
    package_sha256    bytea  NOT NULL,

    -- The chunk size the server chose (§11), and the offset it has durably
    -- committed. §5.2: "The deterministic quarantine object is made durable
    -- before the database advances the committed offset" — so this column is
    -- the answer to "resume from where?", and it may never run ahead of the
    -- ledger below.
    chunk_size       integer NOT NULL,
    committed_offset bigint  NOT NULL DEFAULT 0,

    -- §12's three meanings, plus the ones that end a session:
    --
    --   OPEN                     bytes are arriving
    --   FINALIZED                every declared byte is present; ingestion owed
    --   RECEIVED                 size and whole-package SHA-256 verified
    --   STRUCTURALLY_ACCEPTED    the archive parsed and every check passed
    --   DURABLY_ACCEPTED        the artifact is published and recorded
    --   REJECTED / EXPIRED / ABORTED
    --
    -- §16 invariant 9 is why `RECEIVED` and `STRUCTURALLY_ACCEPTED` are
    -- separate states rather than flags: they release nothing, and a schema
    -- that collapsed them into "accepted" would make that impossible to say.
    state text NOT NULL DEFAULT 'OPEN',

    -- §11: an expiry "no later than the assignment deadline". Block deadlines
    -- are authoritative (§13) and this is the wall-clock bound the upload's own
    -- session carries.
    expires_at timestamptz NOT NULL,

    rejection_reason text,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, upload_id),

    CONSTRAINT upload_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT upload_state_known
        CHECK (state IN ('OPEN', 'FINALIZED', 'RECEIVED', 'STRUCTURALLY_ACCEPTED',
                         'DURABLY_ACCEPTED', 'REJECTED', 'EXPIRED', 'ABORTED')),
    CONSTRAINT upload_digests_are_32_bytes
        CHECK (length(manifest_sha256) = 32
               AND length(package_sha256) = 32
               AND length(declaration_sha256) = 32),
    -- `member_protocol.md` §10.3's ceilings. The assignment may advertise
    -- lower, and does; these are the ones no assignment may exceed, so a
    -- declaration beyond them is refused before quota is reserved
    -- (`security.md` §5.1).
    CONSTRAINT upload_sizes_within_the_protocol_ceilings
        CHECK (compressed_size > 0
               AND compressed_size <= 68719476736
               AND uncompressed_size > 0
               AND uncompressed_size <= 137438953472
               AND uncompressed_size >= compressed_size),
    CONSTRAINT upload_chunk_size_within_bounds
        CHECK (chunk_size BETWEEN 1048576 AND 67108864),
    -- §11: "Bytes beyond the declared package size are never accepted."
    --
    -- Defence in depth: the ledger trigger below already refuses any offset
    -- that is not the contiguous prefix of committed ranges, and a range past
    -- the declaration cannot be committed under it. No test here trips this
    -- without removing that trigger first.
    CONSTRAINT upload_offset_within_the_declaration
        CHECK (committed_offset >= 0 AND committed_offset <= compressed_size),
    CONSTRAINT upload_rejection_is_recorded
        CHECK ((state = 'REJECTED') = (rejection_reason IS NOT NULL)),
    CONSTRAINT upload_has_an_assignment
        FOREIGN KEY (network, assignment_id)
        REFERENCES pool.assignment (network, assignment_id)
);

-- §11: one `package_id` is one upload session for an assignment. Without this a
-- retried declaration opens a second session, and two sessions for one package
-- generation are two committed offsets for one set of bytes.
CREATE UNIQUE INDEX upload_package_is_one_session
    ON pool.upload_session (network, assignment_id, package_id);

-- §16 invariant 8: "A package retry cannot create a second accepted artifact",
-- and §11: "Only one package can become durably accepted for an assignment;
-- rejected package generations remain auditable but may be replaced with a new
-- `package_id` before the deadline." Both halves in one index: many sessions
-- per assignment, one that reaches durable acceptance.
CREATE UNIQUE INDEX upload_one_durable_acceptance_per_assignment
    ON pool.upload_session (network, assignment_id)
    WHERE state = 'DURABLY_ACCEPTED';

-- And one live session at a time, so a member cannot open ten and race them.
CREATE UNIQUE INDEX upload_one_live_session_per_assignment
    ON pool.upload_session (network, assignment_id)
    WHERE state IN ('OPEN', 'FINALIZED', 'RECEIVED', 'STRUCTURALLY_ACCEPTED');

CREATE INDEX upload_by_state ON pool.upload_session (network, state);

-- ---------------------------------------------------------------------------
-- The chunk ledger
-- ---------------------------------------------------------------------------

-- `architecture.md` §8.2: "Each quarantine chunk is first written under a
-- deterministic upload ID, offset, length, and checksum, then verified in the
-- store, and only then added to the contiguous committed-range ledger in a
-- database transaction."
--
-- This is that ledger. One row per committed range, keyed by its offset, so
-- §11's rules are arithmetic rather than judgement: an identical retry finds
-- its row, a different one at the same offset conflicts, and a higher offset
-- has nothing before it.
CREATE TABLE pool.upload_chunk (
    network      text   NOT NULL,
    upload_id    uuid   NOT NULL,
    chunk_offset bigint NOT NULL,
    chunk_length integer NOT NULL,
    chunk_sha256 bytea  NOT NULL,

    -- The quarantine object this range is in. Pool-generated from validated
    -- IDs (`architecture.md` §8.2, `CLAUDE.md`): no member-provided name ever
    -- reaches a key.
    object_key   text   NOT NULL,

    committed_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, upload_id, chunk_offset),

    CONSTRAINT chunk_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT chunk_offset_not_negative
        CHECK (chunk_offset >= 0),
    CONSTRAINT chunk_length_positive
        CHECK (chunk_length > 0),
    CONSTRAINT chunk_sha256_is_32_bytes
        CHECK (length(chunk_sha256) = 32),
    CONSTRAINT chunk_key_is_not_a_member_path
        CHECK (object_key ~ '^quarantine/[a-z]+/[0-9a-f-]+/[0-9]+$'),
    CONSTRAINT chunk_has_a_session
        FOREIGN KEY (network, upload_id)
        REFERENCES pool.upload_session (network, upload_id)
);

-- A committed range is a fact about bytes that are durable. §11 makes an
-- identical retry idempotent and a different one at the same offset a terminal
-- `CHUNK_CONFLICT` — neither rewrites what is there, because the bytes in the
-- store do not change when a row does.
CREATE OR REPLACE FUNCTION pool.upload_chunk_is_committed()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'a committed chunk records bytes already durable; it is not rewritten (member_protocol.md §11)'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER upload_chunk_append_only
    BEFORE UPDATE OR DELETE ON pool.upload_chunk
    FOR EACH ROW
    EXECUTE FUNCTION pool.upload_chunk_is_committed();

-- The committed offset is the ledger's, not the caller's.
--
-- §8.2: "The Pool API never advances the returned offset before the object is
-- durable." A column the caller may set to anything is a resume point that can
-- skip bytes nobody stored — so the trigger recomputes it from the contiguous
-- prefix of committed ranges and refuses anything else.
CREATE OR REPLACE FUNCTION pool.upload_offset_follows_the_ledger()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    contiguous bigint := 0;
    chunk      record;
BEGIN
    IF NEW.committed_offset = OLD.committed_offset THEN
        RETURN NEW;
    END IF;

    FOR chunk IN
        SELECT chunk_offset, chunk_length
          FROM pool.upload_chunk
         WHERE network = NEW.network AND upload_id = NEW.upload_id
         ORDER BY chunk_offset
    LOOP
        EXIT WHEN chunk.chunk_offset <> contiguous;
        contiguous := contiguous + chunk.chunk_length;
    END LOOP;

    IF NEW.committed_offset <> contiguous THEN
        RAISE EXCEPTION
            'the committed offset is the ledger''s contiguous prefix (% bytes), not % (architecture.md §8.2)',
            contiguous, NEW.committed_offset
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

-- What a session may not change about itself, and where it may go next.
--
-- §12's ladder is one-way: bytes arrive, they are complete, they are verified,
-- they parse, they are published. A session that could go back to `OPEN` after
-- `RECEIVED` would accept bytes into a package the pool has already hashed.
CREATE OR REPLACE FUNCTION pool.upload_session_advances()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.upload_id IS DISTINCT FROM OLD.upload_id
       OR NEW.assignment_id IS DISTINCT FROM OLD.assignment_id
       OR NEW.package_id IS DISTINCT FROM OLD.package_id
       OR NEW.declaration_sha256 IS DISTINCT FROM OLD.declaration_sha256
       OR NEW.media_type IS DISTINCT FROM OLD.media_type
       OR NEW.compressed_size IS DISTINCT FROM OLD.compressed_size
       OR NEW.uncompressed_size IS DISTINCT FROM OLD.uncompressed_size
       OR NEW.manifest_sha256 IS DISTINCT FROM OLD.manifest_sha256
       OR NEW.package_sha256 IS DISTINCT FROM OLD.package_sha256
       OR NEW.chunk_size IS DISTINCT FROM OLD.chunk_size
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'an upload''s declaration is what the member committed to; it does not change (member_protocol.md §11)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.committed_offset < OLD.committed_offset THEN
        RAISE EXCEPTION
            'a committed offset never goes backwards (member_protocol.md §11)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.state <> OLD.state THEN
        IF OLD.state IN ('DURABLY_ACCEPTED', 'REJECTED', 'EXPIRED', 'ABORTED') THEN
            RAISE EXCEPTION
                'upload state % is terminal (member_protocol.md §12)', OLD.state
                USING ERRCODE = 'raise_exception';
        END IF;

        IF NOT (
            NEW.state IN ('REJECTED', 'EXPIRED', 'ABORTED')
            OR (OLD.state = 'OPEN'      AND NEW.state = 'FINALIZED')
            OR (OLD.state = 'FINALIZED' AND NEW.state = 'RECEIVED')
            OR (OLD.state = 'RECEIVED'  AND NEW.state = 'STRUCTURALLY_ACCEPTED')
            OR (OLD.state = 'STRUCTURALLY_ACCEPTED' AND NEW.state = 'DURABLY_ACCEPTED')
        ) THEN
            RAISE EXCEPTION
                'an upload does not go from % to % (member_protocol.md §12)',
                OLD.state, NEW.state
                USING ERRCODE = 'raise_exception';
        END IF;

        -- §5.2 step 2: "Finalization verifies that the durable chunk ledger
        -- covers the declaration exactly." Not approximately, and not later.
        IF NEW.state = 'FINALIZED' AND NEW.committed_offset <> NEW.compressed_size THEN
            RAISE EXCEPTION
                'finalization needs every declared byte: % of % (architecture.md §5.2)',
                NEW.committed_offset, NEW.compressed_size
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER upload_session_ladder
    BEFORE UPDATE ON pool.upload_session
    FOR EACH ROW
    EXECUTE FUNCTION pool.upload_session_advances();

-- Each rung belongs to the component `architecture.md` §6 gives it.
--
-- A column grant cannot say this. All three roles legitimately write `state` —
-- the API finalizes, the Artifact Worker reports what it verified, the
-- Controller records durable acceptance — so `GRANT UPDATE (state)` to each of
-- them lets any of them write *any* state, and the API could mark a package
-- durably accepted without a byte being read. §6's table is about operations,
-- not columns, so the value is where the ownership has to be expressed.
--
-- This names database roles in a trigger, which the schema otherwise does only
-- in grants. That is the coupling it costs, and it buys the one thing grants
-- cannot: `DURABLY_ACCEPTED` is the Controller's word and nobody else's, which
-- is what releases a member's slot and retention obligation (§16 invariant 10).
CREATE OR REPLACE FUNCTION pool.upload_rung_belongs_to_its_owner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    owner text;
BEGIN
    IF NEW.state = OLD.state THEN
        RETURN NEW;
    END IF;

    owner := CASE
        -- §6: "Create/resume/finalize upload session | Pool API".
        WHEN NEW.state = 'FINALIZED' THEN 'pool_api'
        -- §6: "Verify complete package bytes and report RECEIVED" and
        -- "Validate and report structural package result | Artifact Worker".
        WHEN NEW.state IN ('RECEIVED', 'STRUCTURALLY_ACCEPTED') THEN 'pool_artifact_worker'
        -- §6: "Record durable acceptance, receipt, and slot release |
        -- Controller".
        WHEN NEW.state = 'DURABLY_ACCEPTED' THEN 'pool_controller'
        -- An abandoned or refused session is ended by whichever of them found
        -- out, so these name no single owner.
        ELSE NULL
    END;

    IF owner IS NOT NULL
       AND current_user <> owner
       AND current_user NOT IN ('pool_migration', 'postgres')
    THEN
        RAISE EXCEPTION
            '% is %''s to record, not %''s (architecture.md §6)',
            NEW.state, owner, current_user
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER upload_session_rung_owner
    BEFORE UPDATE ON pool.upload_session
    FOR EACH ROW
    EXECUTE FUNCTION pool.upload_rung_belongs_to_its_owner();

-- A session arrives empty.
--
-- Everything above is `BEFORE UPDATE`, so the row's *first* state was
-- unguarded: a session could be created already `DURABLY_ACCEPTED`, or with a
-- committed offset covering the whole declaration, and every rule about how it
-- got there would simply not have run. The same shape as an offer arriving
-- already admitted, and refused the same way — at the value, because the API
-- legitimately writes the row.
CREATE OR REPLACE FUNCTION pool.upload_session_arrives_empty()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.state <> 'OPEN' THEN
        RAISE EXCEPTION
            'an upload session is created OPEN and climbs from there (member_protocol.md §11, §12)'
            USING ERRCODE = 'raise_exception';
    END IF;

    IF NEW.committed_offset <> 0 OR NEW.rejection_reason IS NOT NULL THEN
        RAISE EXCEPTION
            'a new session has no committed bytes and no verdict (architecture.md §8.2)'
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER upload_session_arrives_open
    BEFORE INSERT ON pool.upload_session
    FOR EACH ROW
    EXECUTE FUNCTION pool.upload_session_arrives_empty();

CREATE TRIGGER upload_session_offset_follows_ledger
    BEFORE UPDATE ON pool.upload_session
    FOR EACH ROW
    EXECUTE FUNCTION pool.upload_offset_follows_the_ledger();

-- ---------------------------------------------------------------------------
-- Artifacts
-- ---------------------------------------------------------------------------

-- `architecture.md` §8.3's compact row, carrying "only fields required by
-- implemented behavior". Slice 2 implements the package and the commitment
-- payload; proof payloads arrive with step 3 and the `kind` already names them
-- so that slice adds rows rather than altering this constraint.
CREATE TABLE pool.artifact (
    network     text NOT NULL,
    artifact_id uuid NOT NULL,

    assignment_id uuid NOT NULL,
    benchmark_id  text NOT NULL,

    kind    text NOT NULL,
    backend text NOT NULL,

    -- Where it is. The key is pool-generated from validated IDs, and
    -- `architecture.md` §8.2 gives its shape; a member-provided name never
    -- reaches it.
    container   text NOT NULL,
    object_key  text NOT NULL,
    provider_version text,

    media_type     text NOT NULL,
    format_version text NOT NULL,

    -- §8.3: "An S3 ETag may be stored for diagnostics but is never used as the
    -- package integrity check." `sha256` is the protocol digest and the only
    -- thing anything compares against.
    sha256            bytea  NOT NULL,
    provider_checksum text,
    compressed_size   bigint NOT NULL,
    uncompressed_size bigint,
    manifest_sha256   bytea,

    lifecycle_state text NOT NULL DEFAULT 'PUBLISHING',

    accepted_at  timestamptz,
    deletable_at timestamptz,
    deleted_at   timestamptz,

    created_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, artifact_id),

    CONSTRAINT artifact_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT artifact_kind_known
        CHECK (kind IN ('PACKAGE', 'COMMITMENT_PAYLOAD', 'PROOF_PAYLOAD')),
    CONSTRAINT artifact_backend_known
        CHECK (backend IN ('FILESYSTEM', 'S3')),
    CONSTRAINT artifact_lifecycle_known
        CHECK (lifecycle_state IN ('QUARANTINE', 'PUBLISHING', 'ACCEPTED',
                                   'DELETABLE', 'DELETED', 'DELETE_FAILED')),
    CONSTRAINT artifact_sha256_is_32_bytes
        CHECK (length(sha256) = 32
               AND (manifest_sha256 IS NULL OR length(manifest_sha256) = 32)),
    CONSTRAINT artifact_sizes_positive
        CHECK (compressed_size > 0
               AND (uncompressed_size IS NULL OR uncompressed_size > 0)),
    -- A package carries the two fields only a package has. §8.3 lists them as
    -- "package only", and a derived payload claiming an uncompressed size or a
    -- manifest digest would be describing something it is not.
    CONSTRAINT artifact_package_fields_belong_to_packages
        CHECK ((kind = 'PACKAGE')
               = (uncompressed_size IS NOT NULL AND manifest_sha256 IS NOT NULL)),
    CONSTRAINT artifact_acceptance_is_dated
        CHECK ((lifecycle_state IN ('ACCEPTED', 'DELETABLE', 'DELETED', 'DELETE_FAILED'))
               = (accepted_at IS NOT NULL)),
    CONSTRAINT artifact_deletion_is_dated
        CHECK ((lifecycle_state = 'DELETED') = (deleted_at IS NOT NULL)),
    CONSTRAINT artifact_key_is_pool_generated
        CHECK (object_key ~ '^(accepted|derived)/[a-z]+/[A-Za-z0-9_-]+/'),
    CONSTRAINT artifact_has_an_assignment
        FOREIGN KEY (network, assignment_id)
        REFERENCES pool.assignment (network, assignment_id)
);

-- §8.2: "There is exactly one authoritative full package: the accepted object
-- named by the current database artifact row."
CREATE UNIQUE INDEX artifact_one_accepted_package_per_assignment
    ON pool.artifact (network, assignment_id)
    WHERE kind = 'PACKAGE'
      AND lifecycle_state IN ('ACCEPTED', 'DELETABLE', 'DELETED', 'DELETE_FAILED');

-- Deterministic keys (§8.2), so a retried publication finds the object it
-- already wrote rather than writing a second one beside it.
CREATE UNIQUE INDEX artifact_key_is_unique
    ON pool.artifact (network, backend, container, object_key);

CREATE INDEX artifact_by_lifecycle ON pool.artifact (network, lifecycle_state);

-- What an artifact is does not change once it is published.
--
-- §8.2 publishes to a deterministic immutable key and verifies size and hash
-- before the fenced result is committed; proof construction later "recomputes
-- that SHA-256 before using the package". A row whose digest or location could
-- be edited would make that check a comparison against whatever was written
-- last.
CREATE OR REPLACE FUNCTION pool.artifact_location_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.network IS DISTINCT FROM OLD.network
       OR NEW.artifact_id IS DISTINCT FROM OLD.artifact_id
       OR NEW.assignment_id IS DISTINCT FROM OLD.assignment_id
       OR NEW.benchmark_id IS DISTINCT FROM OLD.benchmark_id
       OR NEW.kind IS DISTINCT FROM OLD.kind
       OR NEW.backend IS DISTINCT FROM OLD.backend
       OR NEW.container IS DISTINCT FROM OLD.container
       OR NEW.object_key IS DISTINCT FROM OLD.object_key
       OR NEW.sha256 IS DISTINCT FROM OLD.sha256
       OR NEW.compressed_size IS DISTINCT FROM OLD.compressed_size
       OR NEW.uncompressed_size IS DISTINCT FROM OLD.uncompressed_size
       OR NEW.manifest_sha256 IS DISTINCT FROM OLD.manifest_sha256
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION
            'an artifact''s identity, location and digest are fixed at publication (architecture.md §8.2)'
            USING ERRCODE = 'raise_exception';
    END IF;

    -- §8.4: "The controller alone decides that confirmed TIG state and pending
    -- work make an artifact deletable. The Artifact Worker alone performs
    -- physical deletion." Two components, two different decisions — and a
    -- column grant cannot tell them apart, because both write
    -- `lifecycle_state`. A worker that could mark its own output deletable
    -- could then delete the one authoritative accepted package (§8.2) before
    -- the retention condition the controller is there to check.
    IF NEW.lifecycle_state <> OLD.lifecycle_state
       AND current_user NOT IN ('pool_migration', 'postgres')
    THEN
        IF NEW.lifecycle_state = 'DELETABLE' AND current_user <> 'pool_controller' THEN
            RAISE EXCEPTION
                'retention eligibility is the controller''s, not %''s (architecture.md §8.4)',
                current_user
                USING ERRCODE = 'raise_exception';
        END IF;

        -- The mirror of the rule above. Today the controller is already stopped
        -- short of this by its grants — it holds no `accepted_at` or
        -- `deleted_at` — so this branch is unreachable for the roles that
        -- exist, and it is here for the role a later slice grants the column
        -- to.
        IF NEW.lifecycle_state IN ('ACCEPTED', 'DELETED', 'DELETE_FAILED')
           AND current_user <> 'pool_artifact_worker'
        THEN
            RAISE EXCEPTION
                'publication and deletion are the artifact worker''s, not %''s (architecture.md §8.4)',
                current_user
                USING ERRCODE = 'raise_exception';
        END IF;
    END IF;

    -- Neither component un-deletes anything, and nothing walks back to
    -- `PUBLISHING`.
    IF NOT (
        NEW.lifecycle_state = OLD.lifecycle_state
        OR (OLD.lifecycle_state = 'PUBLISHING' AND NEW.lifecycle_state = 'ACCEPTED')
        OR (OLD.lifecycle_state = 'ACCEPTED'   AND NEW.lifecycle_state = 'DELETABLE')
        OR (OLD.lifecycle_state = 'DELETABLE'
            AND NEW.lifecycle_state IN ('DELETED', 'DELETE_FAILED'))
        OR (OLD.lifecycle_state = 'DELETE_FAILED'
            AND NEW.lifecycle_state IN ('DELETED', 'DELETABLE'))
    ) THEN
        RAISE EXCEPTION
            'an artifact does not go from % to % (architecture.md §8.4)',
            OLD.lifecycle_state, NEW.lifecycle_state
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER artifact_location_immutable
    BEFORE UPDATE ON pool.artifact
    FOR EACH ROW
    EXECUTE FUNCTION pool.artifact_location_is_immutable();

-- ---------------------------------------------------------------------------
-- The receipt
-- ---------------------------------------------------------------------------

-- §12: `DURABLY_ACCEPTED` means "one database transaction recorded its
-- location, hashes, size, format, retention state, assignment state, slot
-- release, and immutable acceptance receipt". §16 invariant 10: the receipt is
-- "immutable, recoverable, and releases both local retention and the compute
-- slot".
--
-- One row per assignment, because §9 has the receipt set
-- `member_may_delete_package` and `slot_released` "at that instant" — a second
-- receipt would be a second instant for one release.
CREATE TABLE pool.acceptance_receipt (
    network       text NOT NULL,
    assignment_id uuid NOT NULL,

    receipt_id    uuid NOT NULL,
    artifact_id   uuid NOT NULL,
    upload_id     uuid NOT NULL,

    -- The digest the member's own package declared and the pool verified. §12
    -- has a lost response "retrieve the same receipt from status", so what the
    -- member reads back has to be the same bytes, which means the receipt
    -- carries its own facts rather than joining for them.
    package_sha256 bytea NOT NULL,

    accepted_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (network, assignment_id),

    CONSTRAINT receipt_network_known
        CHECK (network IN ('testnet', 'mainnet')),
    CONSTRAINT receipt_sha256_is_32_bytes
        CHECK (length(package_sha256) = 32),
    CONSTRAINT receipt_has_an_assignment
        FOREIGN KEY (network, assignment_id)
        REFERENCES pool.assignment (network, assignment_id),
    CONSTRAINT receipt_has_an_artifact
        FOREIGN KEY (network, artifact_id)
        REFERENCES pool.artifact (network, artifact_id),
    CONSTRAINT receipt_has_an_upload
        FOREIGN KEY (network, upload_id)
        REFERENCES pool.upload_session (network, upload_id)
);

CREATE UNIQUE INDEX receipt_id_is_unique ON pool.acceptance_receipt (network, receipt_id);

-- §16 invariant 10: immutable. Recoverable means a member can read it again,
-- not that it can be reissued — a second version of a receipt is a second
-- answer to "may I delete my package?".
CREATE OR REPLACE FUNCTION pool.receipt_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'a durable-acceptance receipt is immutable (member_protocol.md §16 invariant 10)'
        USING ERRCODE = 'raise_exception';
END;
$$;

CREATE TRIGGER receipt_immutable
    BEFORE UPDATE OR DELETE ON pool.acceptance_receipt
    FOR EACH ROW
    EXECUTE FUNCTION pool.receipt_is_immutable();

-- A receipt exists only behind a durably accepted upload and an accepted
-- artifact. §16 invariant 9 — "`RECEIVED` and `STRUCTURALLY_ACCEPTED` release
-- nothing" — is exactly this: the receipt is what releases, so it may not exist
-- while either of those is the furthest the package got.
CREATE OR REPLACE FUNCTION pool.receipt_follows_durable_acceptance()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    upload_state   text;
    artifact_state text;
    artifact_kind  text;
BEGIN
    SELECT state INTO upload_state
      FROM pool.upload_session
     WHERE network = NEW.network AND upload_id = NEW.upload_id;

    IF upload_state IS DISTINCT FROM 'DURABLY_ACCEPTED' THEN
        RAISE EXCEPTION
            'a receipt follows durable acceptance, not an upload in % (member_protocol.md §16 invariant 9)',
            upload_state
            USING ERRCODE = 'raise_exception';
    END IF;

    SELECT lifecycle_state, kind INTO artifact_state, artifact_kind
      FROM pool.artifact
     WHERE network = NEW.network AND artifact_id = NEW.artifact_id;

    IF artifact_kind IS DISTINCT FROM 'PACKAGE'
       OR artifact_state NOT IN ('ACCEPTED', 'DELETABLE', 'DELETED', 'DELETE_FAILED')
    THEN
        RAISE EXCEPTION
            'a receipt names an accepted package, not a % in % (architecture.md §8.2)',
            artifact_kind, artifact_state
            USING ERRCODE = 'raise_exception';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER receipt_from_durable_acceptance
    BEFORE INSERT ON pool.acceptance_receipt
    FOR EACH ROW
    EXECUTE FUNCTION pool.receipt_follows_durable_acceptance();

-- ---------------------------------------------------------------------------
-- Grants (architecture.md §6)
-- ---------------------------------------------------------------------------

-- §6: "Create/resume/finalize upload session | Pool API" and "Commit upload
-- bytes/ranges to quarantine | Pool API". The API owns the session and the
-- ledger, and owns them alone — the offset it returns is the member's resume
-- point.
GRANT SELECT, INSERT ON pool.upload_session TO pool_api;
GRANT UPDATE (committed_offset, state, rejection_reason) ON pool.upload_session TO pool_api;
GRANT SELECT, INSERT ON pool.upload_chunk TO pool_api;

-- §6: "Verify complete package bytes and report `RECEIVED` | Artifact Worker",
-- "Validate and report structural package result", "Publish accepted artifact
-- and derived payload". The worker reads the declaration it is checking
-- against, writes the artifact it published, and moves the session through the
-- two states that mean it checked something.
GRANT SELECT ON pool.upload_session TO pool_artifact_worker;
GRANT UPDATE (state, rejection_reason) ON pool.upload_session TO pool_artifact_worker;
GRANT SELECT ON pool.upload_chunk TO pool_artifact_worker;
GRANT SELECT, INSERT ON pool.artifact TO pool_artifact_worker;
GRANT UPDATE (lifecycle_state, accepted_at, deleted_at, provider_version,
              provider_checksum)
    ON pool.artifact TO pool_artifact_worker;

-- §6: "Record durable acceptance, receipt, and slot release | Controller", in
-- one transaction. The controller is the only writer of the receipt, and the
-- only role that can move a session to `DURABLY_ACCEPTED` — the worker reports,
-- the controller decides.
GRANT SELECT ON pool.upload_session TO pool_controller;
GRANT UPDATE (state) ON pool.upload_session TO pool_controller;
GRANT SELECT ON pool.upload_chunk TO pool_controller;
GRANT SELECT ON pool.artifact TO pool_controller;
-- §8.4: "The controller alone decides that confirmed TIG state and pending work
-- make an artifact deletable."
GRANT UPDATE (lifecycle_state, deletable_at) ON pool.artifact TO pool_controller;
GRANT SELECT, INSERT ON pool.acceptance_receipt TO pool_controller;

-- The member reads its own receipt back (§12: a lost response is recovered from
-- status), so the API reads and never writes one.
GRANT SELECT ON pool.acceptance_receipt TO pool_api;

GRANT SELECT ON pool.upload_session TO pool_readonly;
GRANT SELECT ON pool.artifact TO pool_readonly;
GRANT SELECT ON pool.acceptance_receipt TO pool_readonly;
