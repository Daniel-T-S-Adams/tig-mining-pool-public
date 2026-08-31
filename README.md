# TIG Mining Pool

A mining pool for [The Innovation Game (TIG)](https://tig.foundation). Members
run benchmarks on their own machines via a member agent; the pool owns TIG
credentials, orchestrates precommit/benchmark/proof submission, and settles
rewards through an append-only accounting ledger.

**Status: pre-build complete.** The design is settled, the deterministic
fixture sets exist, and the end-to-end protocol spike ran twice on TIG testnet
(`docs/protocol_spike_report.md`). The first production vertical slice — TIG
gateway and restart-safe protocol state machine — is specified in
`docs/plans/slice-1-gateway.md`; `docs/pre_build_checklist.md` owns the
remaining gates and the slice order.

## Document authority

- Protocol meaning is owned by `docs/mining_system.md`,
  `docs/tig_integration.md`, `docs/member_protocol.md`, and
  `docs/accounting.md`.
- Component, process, storage, and deployment boundaries are owned by
  `docs/architecture.md`.
- Durable decisions and their reasons live in `docs/adr/`.
- Once code exists, tests and current code outrank prose.

`CLAUDE.md` (symlinked as `AGENTS.md`) is the operating contract for both
human and AI contributors: workflow, secrets policy, and human-only actions.

## Local setup

Requires [rustup](https://rustup.rs); the toolchain is pinned by
`rust-toolchain.toml` and installs automatically on first `cargo` invocation.

```bash
make check   # fmt --check, clippy -D warnings, test, feature gate, secret-scan selftest
```

Work that touches the database also needs a local PostgreSQL 18. One command
starts it, generates dev-only passwords into the untracked `secrets/`
directory, and provisions the least-privilege login roles:

```bash
make db-up     # container + roles
make db-test   # the database-backed migration and grant tests
cargo run -p pool-admin -- --config config/pool-admin.dev.toml migrate
```

`make check` skips the database tests when no database is configured, so it
works on a fresh checkout. CI always provides one and sets
`POOL_REQUIRE_DB_TESTS=1`, which turns a skip into a failure there.

> **Never delete `secrets/` wholesale.** It mixes two kinds of file. The
> `db-*-password` files are local dev passwords that `scripts/dev-db.sh`
> regenerates on demand. Others — notably `tig-testnet-api-key` — are
> **provisioned credentials that cannot be regenerated from this
> repository**; restoring one means re-running the issuance procedure in
> `docs/plans/protocol-spike.md` §4 with the account key. To reset only the
> database side, remove `secrets/db-*` and leave everything else alone.

## Contributing

Every change goes through a pull request against `main`; direct pushes are
blocked. `make check` must pass. Changes to settled mining behavior must
update the owning design document in the same PR (see `CLAUDE.md`).

## Layout

```text
CLAUDE.md            operating contract (AGENTS.md symlinks to it)
docs/                authoritative design documents and ADRs
schemas/             versioned member-protocol JSON schemas
config/              pinned TIG integration contract; per-binary dev configs
fixtures/            versioned deterministic test fixtures (see fixtures/tig/v1/README.md)
migrations/          forward-only SQL, applied only by `pool-admin migrate`
scripts/             dev database, secret scan, feature gate
crates/pool-domain   shared domain types (grows as behavior is implemented)
crates/pool-config   typed fail-closed configuration loading
crates/pool-telemetry structured logging shared by the binaries
crates/pool-identity member identity issuance and verification
crates/pool-admin    operator CLI; `migrate` is the one-shot migration job
crates/fake-tig      deterministic local stand-in for the TIG API (`make smoke`)
crates/spike         disposable protocol-spike binaries (retained until its
                     acceptance tests are ported — docs/plans/slice-1-gateway.md §7)
.github/workflows/   PR checks (fmt, clippy, test from a clean checkout)
```
