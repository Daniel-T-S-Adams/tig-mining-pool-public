# One command defines a valid change (CLAUDE.md "Mandatory workflow").
# Every target propagates failure; masking a required check is forbidden.

.PHONY: check fmt-check fmt lint test feature-gate credential-boundary secret-scan image-pin no-observed-constants smoke db-up db-test db-scan provisioning-selftest

check: fmt-check lint test feature-gate credential-boundary secret-scan image-pin no-observed-constants

fmt-check:
	cargo fmt --all --check

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

# Issue #32: the spike-only qualification stand-in must be absent from a
# default pool-identity build. Needs a separate cargo invocation because
# in-workspace feature unification would mask the gate (see the script).
feature-gate:
	./scripts/feature-gate.sh

# Criterion A4, part one: the leak scanner must be able to find a planted
# secret before a clean result means anything. Self-contained, so it runs on
# a fresh checkout with no secrets/ directory. Scanning a real run is
# `make db-scan`, which needs a database.
secret-scan:
	./scripts/secret-scan.sh --selftest

# architecture.md §1 pins images by version and digest. The postgres pin
# lives in three places; this proves they have not drifted apart, so the A4
# evidence run cannot describe a server CI never used.
image-pin:
	./scripts/image-pin-check.sh

# Canonical fake-TIG smoke scenario: precommit -> confirmed -> benchmark
# -> sampled nonces -> proof -> ACTIVE, plus failure injection.
smoke:
	cargo test -p fake-tig --test smoke

# Local PostgreSQL 18 plus least-privilege role provisioning.
db-up:
	./scripts/dev-db.sh

# The database-backed half of the slice-1 checks. In CI these run inside
# `make check` because the job provides POOL_TEST_SUPERUSER_URL, and the
# workflow runs the A4 scan as its own step; locally the cargo tests skip
# unless pointed at a database, which this target does.
#
# `--workspace`, not one package. Every database-backed test in the
# repository skips itself when POOL_TEST_SUPERUSER_URL is unset, so a plain
# `make check` reports a green suite while running none of them. Naming a
# single package here left the migrations, the workflow state machine and
# the restart pass with no local target that runs them at all — passing
# locally and only being tested on the CI machine. POOL_REQUIRE_DB_TESTS=1
# turns the skip into a failure, so this target cannot quietly become the
# same thing again.
db-test: db-up provisioning-selftest db-scan
	POOL_TEST_SUPERUSER_URL="postgres://postgres@127.0.0.1:5433/postgres" \
	POOL_REQUIRE_DB_TESTS=1 \
	cargo test --workspace

# Proves the psql \set quoting convention provisioning depends on: a
# password containing a quote or a backslash must round-trip verbatim, or the
# role's password stops matching the file the A4 scan reads its needle from.
provisioning-selftest: db-up
	./scripts/provisioning-selftest.sh

# Criterion A4, part two: run the real binary, capture everything it emits,
# dump every value the database holds, and prove no secret is in either.
# The selftest above proves the scanner can detect; this proves there is
# nothing to detect.
db-scan: db-up
	./scripts/a4-scan.sh

# Slice-1 criterion H2: the TIG API key-loading path must be unreachable
# from any crate but tig-gateway (architecture.md §2.2).
credential-boundary:
	./scripts/credential-boundary.sh

# Slice-1 criterion C4: no observed TIG constant compiled into slice-1
# source. Runs its own positive control first — a check that cannot detect
# anything reports "clean" for the same reason a clean tree does.
no-observed-constants:
	./scripts/no-observed-constants.sh --selftest
	./scripts/no-observed-constants.sh
