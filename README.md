# TIG Mining Pool

A mining pool for [The Innovation Game (TIG)](https://tig.foundation). Members
run benchmarks on their own machines via a member agent; the pool owns TIG
credentials, orchestrates precommit/benchmark/proof submission, and settles
rewards through an append-only accounting ledger.

**Status: slice 1 shipped; slice 2 in progress.** The pool talks to TIG: it
takes block-consistent snapshots, decides what to submit, transmits through the
credential-holding gateway, advances a workflow only on confirmed reads, and
survives a crash at each of the four `architecture.md` §12 points on the TIG
write path without paying twice. That ran live on testnet
(`docs/evidence/slice-1-live-run.md`), and where each of its 61 acceptance
criteria is satisfied is recorded in `docs/evidence/slice-1-criteria.md`. The
earlier end-to-end protocol spike that preceded it is in
`docs/protocol_spike_report.md`.

Slice 2 — the member agent, the member API, and artifact ingestion — is
specified in `docs/plans/slice-2-member-agent.md`. It replaces the two
stand-ins slice 1 shipped with: the pool-owned placeholder where a member
should be, and the feature-gated stub where durable package acceptance should
be. `docs/pre_build_checklist.md` owns the slice order.

## A note on provenance

This repository was developed privately and published from that history.
Two things follow from that, and neither is an oversight:

- **PR and issue numbers cited in `docs/` refer to the private development
  repository** and do not resolve here. They are kept because they record
  when and where a decision was made; rewriting them would remove that
  evidence without adding any.
- **Two retired TIG testnet addresses are deliberately not recorded** — a
  superseded slice-1 candidate and the protocol spike's own identity. Both
  accounts are retired, neither is of use to a reader, and the operator
  holds them. The runs those documents describe are otherwise unchanged.
  See `docs/plans/protocol-spike.md` §4.

The current slice-1 identity *is* named, because `tig_integration.md` §13
check 9 makes the gateway verify that the confirmed player ID matches its
configured identity — a reader needs it to follow that check. A TIG player
address is public by construction: the protocol publishes it in block data.

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
> repository**; restoring one means repeating the current operator procedure
> in `docs/tig_integration.md` §4 with the testnet wallet. To reset only the
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
crates/               one Cargo workspace; the binaries are listed first
  pool-api            member-facing HTTPS service; holds no TIG credential
  pool-controller     the protocol state machine: what to write and when
  tig-gateway         the one holder of the TIG API key; §13's compatibility gate
  pool-admin          operator CLI; `migrate` is the one-shot migration job
  fake-tig            deterministic local stand-in for the TIG API (`make smoke`)
  pool-config         typed fail-closed configuration loading
  pool-decision       the pure decision rules of docs/mining_system.md §6
  pool-domain         shared domain types (grows as behavior is implemented)
  pool-identity       member identity issuance and verification
  pool-snapshot       block-consistent TIG snapshot assembly
  pool-telemetry      structured logging shared by the binaries
  pool-workflow       write intents and their idempotency
  tig-client          rate-limited read client for the TIG API
  pool-test-support   throwaway provisioned databases; never a binary dependency
  spike               disposable protocol-spike binaries (retained until its
                      acceptance tests are ported — docs/plans/slice-1-gateway.md §7)
.github/workflows/   PR checks (fmt, clippy, test from a clean checkout)
```
