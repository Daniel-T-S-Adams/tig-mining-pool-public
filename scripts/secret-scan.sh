#!/usr/bin/env bash
# Slice-1 criterion A4: prove no secret reached a log, a trace, or a
# database value (docs/architecture.md §2.2, §9).
#
# The scanner is itself a place a secret could leak, so:
#   * needles are read ONLY from the untracked files under secrets/ at run
#     time — never embedded here, never passed on a command line, never
#     written to CI configuration;
#   * a hit reports the FILE and BYTE OFFSET only, never the matched bytes
#     and never the needle;
#   * needles shorter than 8 bytes are skipped, because a short value
#     produces meaningless coincidental matches.
#
# Usage: scripts/secret-scan.sh <file-or-directory> [...]
# Exit:  0 = clean, 1 = a secret was found, 2 = usage/setup error.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# Overridable so --selftest can point the scanner at a synthetic needle
# directory. Nothing else should set it.
secrets_dir="${POOL_SECRET_SCAN_SECRETS_DIR:-$root/secrets}"

if [[ $# -eq 0 ]]; then
    echo "usage: $0 [--selftest] <file-or-directory> [...]" >&2
    exit 2
fi

# Positive control. A scanner that cannot find anything reports "clean" for
# exactly the same reason a clean tree does, so prove it detects a planted
# secret before trusting a clean result.
if [[ "${1:-}" == "--selftest" ]]; then
    # The needle is SYNTHETIC and generated here. Using a real secret would
    # make the control depend on a provisioned developer machine — which is
    # exactly how this check first failed in CI, where no secrets/ directory
    # exists — and would write a real secret into a temporary file for no
    # reason.
    probe="$(mktemp -d)"
    fake_secrets="$(mktemp -d)"
    trap 'rm -rf "$probe" "$fake_secrets"' EXIT

    control="secret-scan-selftest-needle-4f3a91c6d2b70e58"
    printf '%s' "$control" > "$fake_secrets/synthetic-needle"

    printf 'harmless prefix %s harmless suffix\n' "$control" > "$probe/planted.log"
    if POOL_SECRET_SCAN_SECRETS_DIR="$fake_secrets" "$0" "$probe" > /dev/null 2>&1; then
        echo "FAIL: secret-scan did not detect a planted secret; the scanner is broken" >&2
        exit 1
    fi

    rm -f "$probe/planted.log"
    printf 'nothing to see here\n' > "$probe/clean.log"
    if ! POOL_SECRET_SCAN_SECRETS_DIR="$fake_secrets" "$0" "$probe" > /dev/null 2>&1; then
        echo "FAIL: secret-scan reported a finding on a clean file" >&2
        exit 1
    fi

    echo "secret-scan selftest: detects a planted secret and passes a clean file"
    exit 0
fi

if [[ ! -d "$secrets_dir" ]]; then
    echo "secret-scan: no secrets/ directory; nothing to scan for" >&2
    exit 2
fi

shopt -s nullglob
needle_files=("$secrets_dir"/*)
shopt -u nullglob

if [[ ${#needle_files[@]} -eq 0 ]]; then
    echo "secret-scan: secrets/ is empty; nothing to scan for" >&2
    exit 2
fi

# Collect the haystack up front so the report can name a count.
targets=()
for path in "$@"; do
    if [[ -d "$path" ]]; then
        while IFS= read -r -d '' file; do targets+=("$file"); done \
            < <(find "$path" -type f -print0)
    elif [[ -f "$path" ]]; then
        targets+=("$path")
    fi
done

if [[ ${#targets[@]} -eq 0 ]]; then
    echo "secret-scan: no files to scan under: $*" >&2
    exit 2
fi

findings=0
scanned_needles=0

for needle_file in "${needle_files[@]}"; do
    [[ -f "$needle_file" ]] || continue
    # Read the needle into a variable that is never printed.
    needle="$(cat "$needle_file")"
    needle="${needle%$'\n'}"
    if [[ ${#needle} -lt 8 ]]; then
        echo "secret-scan: skipping $(basename "$needle_file") (under 8 bytes)" >&2
        continue
    fi
    # One file, one secret. grep splits a multi-line pattern into several
    # independent patterns, so a file like an env file would contribute its
    # non-secret lines as needles — and a blank interior line becomes an
    # empty pattern that matches every byte, reporting the whole run as
    # leaking. Skip loudly rather than scan wrongly in either direction.
    if [[ "$needle" == *$'\n'* ]]; then
        echo "secret-scan: skipping $(basename "$needle_file") (multi-line; one file must hold one secret)" >&2
        continue
    fi
    scanned_needles=$((scanned_needles + 1))

    for target in "${targets[@]}"; do
        # -F: literal, -b: byte offset, -a: treat binary as text so a
        # pg_dump custom-format file is still searched. Only the offset
        # column is kept; the matched line never reaches stdout.
        #
        # grep's status is checked rather than discarded. This is a required
        # check, and CLAUDE.md forbids masking one: an unreadable target
        # must fail the scan, not silently contribute zero findings and let
        # the run report "clean".
        # The needle goes in on STDIN via `-f -`, never on argv. Passing it
        # as an argument would publish every secret in secrets/ — the TIG
        # API key included — in /proc/<pid>/cmdline for the life of each
        # grep, which is the exposure architecture.md §9 forbids and which
        # this script's own header promises not to do. Multi-line needles
        # are skipped above, so a single-pattern `-f -` is equivalent.
        set +e
        matches="$(printf '%s\n' "$needle" | grep -abo -F -f - -- "$target")"
        status=$?
        set -e
        if [[ $status -ge 2 ]]; then
            echo "secret-scan: cannot scan $target (grep exit $status)" >&2
            exit 2
        fi
        while IFS= read -r match; do
            [[ -n "$match" ]] || continue
            echo "LEAK: $(basename "$needle_file") appears in $target at byte offset ${match%%:*}"
            findings=$((findings + 1))
        done <<< "$matches"
    done
done

if [[ $scanned_needles -eq 0 ]]; then
    echo "secret-scan: no usable needles" >&2
    exit 2
fi

if [[ $findings -gt 0 ]]; then
    echo "secret-scan: FAILED with $findings finding(s)" >&2
    exit 1
fi

echo "secret-scan: clean — ${#targets[@]} file(s) scanned against $scanned_needles secret(s)"
