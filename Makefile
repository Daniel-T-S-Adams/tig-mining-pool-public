# One command defines a valid change (CLAUDE.md "Mandatory workflow").
# Every target propagates failure; masking a required check is forbidden.

.PHONY: check fmt-check fmt lint test feature-gate smoke

check: fmt-check lint test feature-gate

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

# Canonical fake-TIG smoke scenario: precommit -> confirmed -> benchmark
# -> sampled nonces -> proof -> ACTIVE, plus failure injection.
smoke:
	cargo test -p fake-tig --test smoke
