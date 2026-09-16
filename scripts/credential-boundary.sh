#!/usr/bin/env bash
# Slice-1 criterion H2: prove the TIG API key-loading path is unreachable
# from any crate other than `tig-gateway`.
#
# `architecture.md` §2.2: "The TIG API key is loaded only by `tig-gateway`
# ... sharing a repository or Rust library does not grant access to the
# secret." That last clause is the one this checks. `tig_gateway::credential`
# keeps `load` and `TigApiKey::expose` crate-private, so the boundary is a
# compile error rather than a convention — but only a probe OUTSIDE the
# workspace demonstrates it, because inside the workspace a test in the same
# crate can reach both legitimately.
#
# Same shape as scripts/feature-gate.sh, which H2 names:
#   1. a probe referencing the key-loading path must FAIL to compile;
#   2. a probe referencing the crate's public API must compile, so a broken
#      probe cannot report success by failing for an unrelated reason.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
probe="$(mktemp -d)"
trap 'rm -rf "$probe"' EXIT
mkdir -p "$probe/src"

cat > "$probe/Cargo.toml" <<TOML
[package]
name = "credential-boundary-probe"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
tig-gateway = { path = "$root/crates/tig-gateway" }

[workspace]
TOML

# The probe's dependencies are a subset of the workspace's, so pinning to
# the same versions keeps the check offline and deterministic.
cp "$root/Cargo.lock" "$probe/Cargo.lock"

run_check() {
    (cd "$root" && CARGO_TARGET_DIR="$root/target/credential-boundary" \
        cargo check --quiet --offline --manifest-path "$probe/Cargo.toml")
}

# 1. Loading a key from outside the gateway must not compile.
cat > "$probe/src/main.rs" <<'RS'
fn main() {
    // Crate-private: no other crate may load the TIG API key.
    let _ = tig_gateway::credential::load;
}
RS
if run_check > /dev/null 2>&1; then
    echo "FAIL: tig_gateway::credential::load is reachable outside the gateway" >&2
    echo "architecture.md §2.2: the TIG API key is loaded only by tig-gateway" >&2
    exit 1
fi

# 2. Nor may the key material be read out of one.
cat > "$probe/src/main.rs" <<'RS'
fn main() {
    let _ = tig_gateway::TigApiKey::expose;
}
RS
if run_check > /dev/null 2>&1; then
    echo "FAIL: TigApiKey::expose is reachable outside the gateway" >&2
    exit 1
fi

# 3. Positive control. Without this, a probe that failed to compile for an
#    unrelated reason — a renamed crate, a broken manifest — would report the
#    boundary as holding while testing nothing.
cat > "$probe/src/main.rs" <<'RS'
fn main() {
    // Public API of the same crate: this MUST compile.
    let _ = tig_gateway::evaluate;
}
RS
if ! run_check; then
    echo "FAIL: the probe cannot compile against tig-gateway's public API; the probe is broken" >&2
    exit 1
fi

# 4. The symbols this gate names must still exist.
#
# Both negative probes read "did not compile" as "the boundary holds", and a
# renamed or deleted item fails to compile for that reason too — the gate
# would keep passing while testing nothing. feature-gate.sh avoids this by
# recompiling the same item with the feature on; a crate-private item cannot
# be reached that way, so existence is asserted directly instead.
credential="$root/crates/tig-gateway/src/credential.rs"
for symbol in 'fn load(' 'fn expose('; do
    if ! grep -q "pub(crate) $symbol" "$credential"; then
        echo "FAIL: '$symbol' is not a crate-private item of tig-gateway::credential;" >&2
        echo "the probes above would pass by failing to resolve it, testing nothing" >&2
        exit 1
    fi
done

# 5. H2 asks that the key-loading PATH be absent from every non-gateway
#    binary, which the probes above do not show: a crate that opens the key
#    file itself never mentions tig-gateway at all.
#
# Two directories are excluded, each for a stated reason:
#
#   * crates/fake-tig is the stand-in TIG SERVER. It CHECKS an inbound
#     x-api-key header; it holds no pool credential.
#   * crates/spike is pre-build code that predates this boundary and does
#     read the testnet key directly. docs/plans/slice-1-gateway.md §7 says
#     slice 1 alone does not clear the spike for deletion, so this is a known
#     and tracked gap rather than a passing check — and it is why the message
#     below says "outside the spike" rather than claiming more.
scan_hits=0
while IFS= read -r file; do
    case "$file" in
        */crates/tig-gateway/*|*/crates/fake-tig/*|*/crates/spike/*) continue ;;
    esac
    body="$(grep -vE '^[[:space:]]*//' "$file" || true)"
    if printf '%s\n' "$body" | grep -qiE 'tig[-_]testnet[-_]api[-_]key|"x-api-key"|api_key_path'; then
        echo "LEAK: $file names the TIG API key path or header"
        scan_hits=$((scan_hits + 1))
    fi
done < <(find "$root/crates" -path '*/src/*' -name '*.rs' -print | sort)

# The scripts too, which this scan used to skip.
#
# It cost something: `scripts/live-run-evidence.sh` read the testnet key and
# passed it to curl on the command line — a second non-gateway reader of the
# credential, and argv is world-readable in /proc for the life of the process,
# which §9 forbids in the same sentence as TOML and logs. The reads did not
# need a key at all. A boundary that only watches one kind of file is a
# boundary with a door in it.
#
# `secret-scan.sh`, `a4-scan.sh` and `dev-db.sh` are the exceptions and each
# earns it: the first two exist to *find* the key and read it only at runtime
# from the untracked file, and `dev-db.sh` provisions local database passwords
# that are not the TIG credential at all.
while IFS= read -r file; do
    case "$(basename "$file")" in
        secret-scan.sh|a4-scan.sh|dev-db.sh|credential-boundary.sh) continue ;;
    esac
    body="$(grep -vE '^[[:space:]]*#' "$file" || true)"
    if printf '%s\n' "$body" | grep -qiE 'tig[-_]testnet[-_]api[-_]key|x-api-key'; then
        echo "LEAK: $file names the TIG API key path or header"
        scan_hits=$((scan_hits + 1))
    fi
done < <(find "$root/scripts" -type f \( -name '*.sh' -o -name '*.py' \) -print | sort)

if [[ $scan_hits -gt 0 ]]; then
    echo "FAIL: the TIG API key is loaded only by tig-gateway (architecture.md §2.2)" >&2
    exit 1
fi

echo "credential boundary holds: the key-loading path is crate-private to tig-gateway,"
echo "and no crate or script outside it (spike, fake-tig and the key-scanners"
echo "excepted, see the script) names the key"
