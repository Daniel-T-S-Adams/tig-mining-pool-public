-- Environment provisioning, part one: the roles and the schema-level
-- privileges that docs/architecture.md §7.1 and §6 require, one role per
-- process boundary.
--
-- Run as a superuser, once per environment, BEFORE `pool-admin migrate`.
-- This is not a migration: a migration connects as `pool_migration` and so
-- cannot create it.
--
-- Deliberately free of psql meta-commands and of any password, so the same
-- file can be executed by psql, by the test suite, and by anything else
-- that speaks plain SQL. Passwords live in
-- `scripts/provision-db-passwords.sql`, which takes them as psql variables
-- from the environment's secret store. Keeping them apart is what lets
-- `crates/pool-admin/tests/migrate.rs` assert this exact file, so a grant
-- added here cannot drift away from what the tests check.

DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'pool_migration') THEN
        CREATE ROLE pool_migration LOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'pool_controller') THEN
        CREATE ROLE pool_controller LOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'pool_gateway') THEN
        CREATE ROLE pool_gateway LOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'pool_api') THEN
        CREATE ROLE pool_api LOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'pool_artifact_worker') THEN
        CREATE ROLE pool_artifact_worker LOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'pool_readonly') THEN
        CREATE ROLE pool_readonly LOGIN;
    END IF;
END
$$;

-- No role may create objects in `public` by default; domain objects live in
-- `pool`, whose privileges the migration owns.
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

-- One exception, narrowly granted: SQLx keeps its `_sqlx_migrations`
-- version/checksum ledger in the connection's default schema, so the
-- migration identity needs CREATE there.
--
-- Stated precisely, because the grant is role-scoped and not object-scoped:
-- `pool_migration` is the only ROLE that may create anything in `public`,
-- and `_sqlx_migrations` is the only object it is *intended* to create.
-- PostgreSQL cannot express the narrower rule; the narrowing is convention,
-- enforced by review of the migrations rather than by this grant.
GRANT CREATE ON SCHEMA public TO pool_migration;
