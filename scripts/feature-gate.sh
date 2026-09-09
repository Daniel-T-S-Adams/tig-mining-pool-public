#!/usr/bin/env bash
# Issue #32 (AI review round 6): prove the SPIKE/TEST-ONLY qualification
# stand-in (`IdentityService::mark_slot_qualified`) is NOT part of a default
# `pool-identity` build.
#
# This must be a SEPARATE cargo invocation: inside one workspace invocation
# (`cargo test --workspace`), feature unification legitimately enables the
# `spike` feature for the test builds that need the stand-in, which would
# mask the gate. A probe crate outside the workspace resolves features
# fresh:
#   1. referencing the stand-in WITHOUT the feature must FAIL to compile;
#   2. the same probe WITH the feature must compile (positive control, so a
#      broken probe cannot pass silently).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
probe="$(mktemp -d)"
trap 'rm -rf "$probe"' EXIT
mkdir -p "$probe/src"

cat > "$probe/src/main.rs" <<'RS'
fn main() {
    // The spike-only stand-in: absent from a default build.
    let _ = pool_identity::IdentityService::mark_slot_qualified;
}
RS

# Seed the probe with the workspace lockfile. The probe resolves outside the
# workspace on purpose — that is what gives it fresh feature resolution — but
# a fresh resolution under --offline needs every candidate version in the
# local registry, which CI's cache does not have for a crate with a large
# dependency tree. The probe's dependencies are a subset of the workspace's,
# so pinning to the same versions keeps the check offline and deterministic.
seed_lockfile() {
    cp "$root/Cargo.lock" "$probe/Cargo.lock"
}

write_manifest() {
    cat > "$probe/Cargo.toml" <<TOML
[package]
name = "feature-gate-probe"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
pool-identity = { path = "$root/crates/pool-identity"$1 }

[workspace]
TOML
}

# Run cargo from the repository root so the pinned toolchain applies; keep
# the probe's artifacts in a dedicated target dir; offline so this check
# never needs the network beyond what the workspace build already cached.
run_check() {
    (cd "$root" && CARGO_TARGET_DIR="$root/target/feature-gate" \
        cargo check --quiet --offline --manifest-path "$probe/Cargo.toml")
}

write_manifest ""
seed_lockfile
if run_check > /dev/null 2>&1; then
    echo "FAIL: mark_slot_qualified is reachable in a DEFAULT pool-identity build" >&2
    exit 1
fi

write_manifest ', features = ["spike"]'
seed_lockfile
if ! run_check; then
    echo "FAIL: probe did not compile even with the spike feature; the probe itself is broken" >&2
    exit 1
fi

echo "feature gate holds: the spike qualification stand-in is absent from a default pool-identity build"

# Second gate, same shape: tig-client's `testing` module exposes the whole
# per-IP read allowance that ADR-0006 requires readers to take a share of.
# A production consumer must not be able to reach it.
cat > "$probe/src/main.rs" <<'RS'
fn main() {
    // Behind the `testing` feature: absent from a default build.
    let _ = tig_client::testing::pool_ceiling_for_test;
}
RS

write_tig_manifest() {
    cat > "$probe/Cargo.toml" <<TOML
[package]
name = "feature-gate-probe"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
tig-client = { path = "$root/crates/tig-client"$1 }

[workspace]
TOML
}

write_tig_manifest ""
seed_lockfile
if run_check > /dev/null 2>&1; then
    echo "FAIL: tig_client::testing is reachable in a DEFAULT build; a production reader could take the whole per-IP allowance" >&2
    exit 1
fi

write_tig_manifest ', features = ["testing"]'
seed_lockfile
if ! run_check; then
    echo "FAIL: the tig-client probe did not compile even with the testing feature; the probe itself is broken" >&2
    exit 1
fi

echo "feature gate holds: tig-client's testing module is absent from a default build"

# Third gate, and the one with teeth: the F4a stub acceptance record. It can
# assert "this package was durably accepted" for a package that never existed,
# which is the precondition `architecture.md` §13 invariant 4 exists to
# require. Criterion K3 runs the production binaries against live testnet, so
# this must be ABSENT from that build rather than refused inside it.
#
# F4d's runtime endpoint check is defence in depth behind this, not a
# substitute: the two fail independently, and a test binary is exactly the
# thing someone eventually points at a real endpoint.
cat > "$probe/src/main.rs" <<'RS'
fn main() {
    // Behind the `stub-acceptance` feature: absent from a default build.
    let _ = pool_controller::stub::stub_acceptance::<&sqlx::PgPool>;
}
RS

write_controller_manifest() {
    cat > "$probe/Cargo.toml" <<TOML
[package]
name = "feature-gate-probe"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
pool-controller = { path = "$root/crates/pool-controller"$1 }
sqlx = { version = "0.9", default-features = false, features = [
    "postgres",
    "runtime-tokio",
    "tls-rustls-ring",
] }

[workspace]
TOML
}

write_controller_manifest ""
seed_lockfile
if run_check > /dev/null 2>&1; then
    echo "FAIL: the stub acceptance record is reachable in a DEFAULT pool-controller build; a live-testnet binary could fabricate a durable package acceptance" >&2
    exit 1
fi

write_controller_manifest ', features = ["stub-acceptance"]'
seed_lockfile
if ! run_check; then
    echo "FAIL: the pool-controller probe did not compile even with the stub-acceptance feature; the probe itself is broken" >&2
    exit 1
fi

echo "feature gate holds: the stub acceptance record is absent from a default pool-controller build"
