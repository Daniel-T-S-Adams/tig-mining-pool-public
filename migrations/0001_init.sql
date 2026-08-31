-- Slice 1 foundations: the schema, and the grants that make the process
-- boundary real.
--
-- Forward-only (docs/architecture.md §7.1). This migration deliberately
-- creates NO domain tables: the schema stays incremental, and a table is
-- added only with the first query, invariant, recovery action, or audit
-- requirement that needs it.
--
-- Roles are NOT created here. A migration runs as `pool_migration`, so it
-- cannot be what brings `pool_migration` into existence; role creation is
-- environment provisioning and needs a superuser. See
-- `scripts/provision-db-roles.sql` and `scripts/dev-db.sh`. Passwords are
-- supplied by the environment's secret store and never appear in this
-- repository (CLAUDE.md "Security and secrets").

CREATE SCHEMA IF NOT EXISTS pool;

-- Every service role may resolve names in the schema. Object-level grants
-- are what actually separate them, and they arrive with the objects
-- themselves, one slice at a time.
GRANT USAGE ON SCHEMA pool TO
    pool_controller, pool_gateway, pool_api, pool_artifact_worker, pool_readonly;

-- DDL belongs to the one-shot migration identity alone; no service role
-- gets CREATE here (architecture.md §6: "services do not own DDL
-- privileges or auto-migrate at startup").
GRANT CREATE ON SCHEMA pool TO pool_migration;

-- Deliberately NO blanket default privilege for pool_readonly.
--
-- `ALTER DEFAULT PRIVILEGES ... GRANT SELECT ON TABLES` would make every
-- table any future slice creates automatically readable by the monitoring
-- credential — a fail-open default for exactly the credential
-- architecture.md §9 scopes. The migration that creates a table grants
-- SELECT on it explicitly, so each slice makes the monitoring-visibility
-- decision rather than inheriting it.
