#!/usr/bin/env bash
# architecture.md §1 requires container images pinned by exact version AND
# digest. The PostgreSQL pin appears in three places — the CI service, the
# dev cluster, and the A4 evidence run — and a comment claiming they "must
# all attest to the same image" enforces nothing. If one drifts, the A4
# evidence describes a server CI never ran.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

declare -A pins=(
    [".github/workflows/pr-checks.yml"]="$(sed -n 's/.*image: \(postgres:[^ ]*\).*/\1/p' "$root/.github/workflows/pr-checks.yml" | head -1)"
    ["scripts/dev-db.sh"]="$(sed -n 's/^image="\(.*\)"$/\1/p' "$root/scripts/dev-db.sh" | head -1)"
    ["scripts/a4-scan.sh"]="$(sed -n 's/^image="\(.*\)"$/\1/p' "$root/scripts/a4-scan.sh" | head -1)"
)

reference=""
failed=0
for file in "${!pins[@]}"; do
    pin="${pins[$file]}"
    if [[ -z "$pin" ]]; then
        echo "FAIL: no postgres image pin found in $file" >&2
        failed=1
        continue
    fi
    if [[ "$pin" != *"@sha256:"* ]]; then
        echo "FAIL: $file pins a mutable tag with no digest: $pin" >&2
        failed=1
        continue
    fi
    if [[ -z "$reference" ]]; then
        reference="$pin"
    elif [[ "$pin" != "$reference" ]]; then
        echo "FAIL: image pins disagree" >&2
        echo "  $file: $pin" >&2
        echo "  expected:      $reference" >&2
        failed=1
    fi
done

[[ $failed -eq 0 ]] || exit 1
echo "image pin check: all three postgres pins agree on $reference"
