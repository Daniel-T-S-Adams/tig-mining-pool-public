-- Slice 1 criterion F4d, the half that must exist before the rows do.
--
-- F4a's stub fabricates a package acceptance and a canonical payload so the
-- fake-tig lifecycle drive can satisfy `architecture.md` §13 invariants 4 and
-- 5 in a slice with no members. F4d then says creating **or accepting** such a
-- record is refused unless the TIG endpoint is a local fake-tig.
--
-- Accepting requires telling a fabricated row from a real one, and that is a
-- fact about the row's origin, not about its contents. Recording it has to
-- happen when the row is written: a stub row inserted without this column is
-- indistinguishable from a real acceptance for the rest of the database's
-- life, and no later migration can recover what it was. So the column lands
-- with the stub, in the same slice, even though the guard that reads it lands
-- with the intent-creation path it guards.
--
-- Deliberately not a sentinel digest. The stub uses fixed obviously-synthetic
-- bytes, and matching on those would conflate what a row *contains* with where
-- it *came from* — a real package whose bytes happened to collide would be
-- refused, and a fabricated row rewritten with plausible bytes would not be.
-- Origin is its own fact and gets its own column.
--
-- `DEFAULT false` so every existing row is what it says: those were written by
-- tests through the real recording path, which is exactly what a non-stub row
-- means.

ALTER TABLE pool.package_acceptance
    ADD COLUMN stub_origin boolean NOT NULL DEFAULT false;

ALTER TABLE pool.canonical_payload
    ADD COLUMN stub_origin boolean NOT NULL DEFAULT false;

-- Immutable, like the rest of both rows. A row that could be relabelled as
-- genuine after the fact would defeat the guard by the simplest possible
-- route, and `migrations/0011` already withholds UPDATE from every granted
-- role — this trigger is what holds if a later slice grants UPDATE for some
-- legitimate column, which is exactly when a withheld grant stops being the
-- protection.
CREATE OR REPLACE FUNCTION pool.acceptance_origin_is_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.stub_origin IS DISTINCT FROM OLD.stub_origin THEN
        RAISE EXCEPTION
            'stub_origin is immutable: a fabricated precondition cannot be '
            'relabelled as a real one (slice-1 F4d)'
            USING ERRCODE = 'raise_exception';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER package_acceptance_origin_immutable
    BEFORE UPDATE ON pool.package_acceptance
    FOR EACH ROW
    EXECUTE FUNCTION pool.acceptance_origin_is_immutable();

CREATE TRIGGER canonical_payload_origin_immutable
    BEFORE UPDATE ON pool.canonical_payload
    FOR EACH ROW
    EXECUTE FUNCTION pool.acceptance_origin_is_immutable();

-- The question the F4d guard asks, as one index rather than a scan: does this
-- database hold any fabricated precondition at all?
CREATE INDEX package_acceptance_stub_rows
    ON pool.package_acceptance (network)
    WHERE stub_origin;

CREATE INDEX canonical_payload_stub_rows
    ON pool.canonical_payload (network)
    WHERE stub_origin;
