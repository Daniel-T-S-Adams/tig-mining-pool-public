# One command defines a valid change (CLAUDE.md "Mandatory workflow").
# Every target propagates failure; masking a required check is forbidden.

.PHONY: check fmt-check fmt lint test smoke

check: fmt-check lint test

fmt-check:
	cargo fmt --all --check

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

# Canonical fake-TIG smoke scenario: precommit -> confirmed -> benchmark
# -> sampled nonces -> proof -> ACTIVE, plus failure injection.
smoke:
	cargo test -p fake-tig --test smoke
