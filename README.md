# TIG Mining Pool

A mining pool for [The Innovation Game (TIG)](https://tig.foundation). Members
run benchmarks on their own machines via a member agent; the pool owns TIG
credentials, orchestrates precommit/benchmark/proof submission, and settles
rewards through an append-only accounting ledger.

**Status: pre-build.** The design is settled and documented; implementation is
starting with deterministic test fixtures and an end-to-end protocol spike
(see `docs/pre_build_checklist.md`).

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
make check   # fmt --check, clippy -D warnings, test — the definition of a valid change
```

## Contributing

Every change goes through a pull request against `main`; direct pushes are
blocked. `make check` must pass. Changes to settled mining behavior must
update the owning design document in the same PR (see `CLAUDE.md`).

## Layout

```text
CLAUDE.md            operating contract (AGENTS.md symlinks to it)
docs/                authoritative design documents and ADRs
schemas/             versioned member-protocol JSON schemas
config/              pinned TIG integration contract
fixtures/            versioned deterministic test fixtures (see fixtures/tig/v1/README.md)
crates/pool-domain   shared domain types (grows as behavior is implemented)
crates/fake-tig      deterministic local stand-in for the TIG API (`make smoke`)
.github/workflows/   PR checks (fmt, clippy, test from a clean checkout)
```
