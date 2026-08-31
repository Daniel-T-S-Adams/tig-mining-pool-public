-- Environment provisioning, part two: set each role's password.
--
-- Split from `provision-db-roles.sql` so that file stays plain SQL and can
-- be asserted directly by the test suite. This one needs psql, because it
-- takes its values as variables:
--
--   { for each role: printf "\\set pw_<role> '<value>'\n"; cat this file; } \
--     | psql -v ON_ERROR_STOP=1 "$URL"
--
-- No password appears in this file, and none may ever be added to it
-- (CLAUDE.md "Security and secrets").
--
-- Quoting is a SHARED responsibility, and only half of it lives here. psql
-- substitutes :'name' as a correctly quoted SQL literal, so the ALTER ROLE
-- statements below are safe. The `\set` line itself is built by the caller,
-- and psql does NOT escape it.
--
-- **The caller must double every single quote in the value** before
-- interpolating it (`''`). Verified against psql: `\set x 'a''b'` sets
-- `a'b`. The failure this prevents is a corrupted value, not SQL injection —
-- an unescaped quote produces an "unterminated quoted string" warning and a
-- mangled password, executing nothing. A mangled password is still serious:
-- the role stops matching its password file, and the A4 scan's needle is
-- read from that file, so a clean scan stops being evidence for that role.
-- `scripts/provisioning-selftest.sh` proves the convention end to end.

ALTER ROLE pool_migration       WITH PASSWORD :'pw_migration';
ALTER ROLE pool_controller      WITH PASSWORD :'pw_controller';
ALTER ROLE pool_gateway         WITH PASSWORD :'pw_gateway';
ALTER ROLE pool_api             WITH PASSWORD :'pw_api';
ALTER ROLE pool_artifact_worker WITH PASSWORD :'pw_artifact_worker';
ALTER ROLE pool_readonly        WITH PASSWORD :'pw_readonly';
