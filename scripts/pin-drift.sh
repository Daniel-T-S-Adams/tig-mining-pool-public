#!/usr/bin/env bash
# Has TIG moved away from what this repository is pinned to?
#
# `tig_integration.md` §2 pins an exact upstream commit, an API-spec checksum
# and ten container digests; §15 makes changing any of them a reviewed human
# upgrade. Nothing watched whether the world had moved in the meantime, so a
# pin went seven weeks and 45 commits stale, and a new CPU challenge went
# live, without anything noticing.
#
# This is the watching half, and only that half. It reports what differs. It
# never decides whether a difference matters — that judgement is what §15's
# procedure is for, and a script that guessed at it would be worse than none,
# because a wrong "no action needed" is the answer nobody re-checks.
#
# **Deliberately not part of `make check`.** A green build must not depend on
# TIG standing still: CI turning red the morning TIG lands a commit would
# train everyone to ignore it. Run it when you want to know.
#
# Exit codes: 0 clean, 1 drift found, 2 a check could not run.
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
pinned="$root/config/tig_integration.json"
repo="tig-foundation/tig-monorepo"

drift=0
unchecked=0

note()    { printf '    %s\n' "$*"; }
ok()      { printf '  [ok]        %s\n' "$*"; }
moved()   { printf '  [DRIFT]     %s\n' "$*"; drift=1; }
unknown() { printf '  [unchecked] %s\n' "$*"; unchecked=1; }

# Exits non-zero on a missing key. Without that a renamed field yields an
# empty string, and the caller reports "pinned " with no value as DRIFT —
# blaming TIG for a local config error.
pin() { python3 -c "
import json,sys
d=json.load(open('$pinned'))
for k in sys.argv[1].split('.'):
    d=d[k]
print(d)" "$1" 2>/dev/null; }

printf '\nTIG pin drift — %s\n\n' "$(date -u +%Y-%m-%dT%H:%MZ)"

# ---- 1. the upstream source commit ----------------------------------------
printf 'Upstream source commit\n'
if ! pinned_commit="$(pin upstream.commit)" || [[ -z "$pinned_commit" ]]; then
    unknown "the pinned file has no upstream.commit"
    pinned_commit=""
fi
head_json="$(curl -sf --max-time 25 "https://api.github.com/repos/$repo/commits/main" 2>/dev/null)"
if [[ -z "$head_json" ]]; then
    unknown "could not reach GitHub for $repo"
else
    head_commit="$(printf '%s' "$head_json" | python3 -c 'import sys,json;print(json.load(sys.stdin)["sha"])')"
    if [[ -z "$pinned_commit" ]]; then
        : # already reported as unchecked
    elif [[ "$head_commit" == "$pinned_commit" ]]; then
        ok "pinned commit is the current head"
    else
        cmp_json="$(curl -sf --max-time 25 \
            "https://api.github.com/repos/$repo/compare/$pinned_commit...$head_commit" 2>/dev/null)"
        ahead="$(printf '%s' "${cmp_json:-}" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("ahead_by","?"))' 2>/dev/null || echo '?')"
        moved "pinned $pinned_commit"
        note  "head    $head_commit"
        note  "$ahead commit(s) since the pin"
        note  "diff: https://github.com/$repo/compare/${pinned_commit:0:12}...${head_commit:0:12}"
    fi
fi

# ---- 2. the published API specification -----------------------------------
printf '\nAPI specification\n'
spec_url="$(pin upstream.openapi.url)"
pinned_sha="$(pin upstream.openapi.sha256)"
live_sha="$(curl -sf --max-time 30 "$spec_url" 2>/dev/null | sha256sum | cut -d' ' -f1)"
if [[ -z "$live_sha" || "$live_sha" == "$(printf '' | sha256sum | cut -d' ' -f1)" ]]; then
    unknown "could not fetch $spec_url"
elif [[ "$live_sha" == "$pinned_sha" ]]; then
    ok "published spec still matches the pinned checksum"
else
    moved "spec checksum changed"
    note  "pinned $pinned_sha"
    note  "live   $live_sha"
fi

# ---- 3. the pinned container images ---------------------------------------
printf '\nContainer images\n'
# mktemp, not a fixed name in a shared directory: a predictable path in /tmp
# lets anyone who can write there pre-plant a symlink and have this redirect
# overwrite whatever it points at.
image_list="$(mktemp)"
# `2>&1 >file` and not `>file 2>&1`: the second sends stderr to where stdout
# already points — the file — so a traceback lands in the image list and the
# loop below parses it as image entries.
extract_err="$(python3 - "$pinned" 2>&1 >"$image_list" <<'IMG'
import json, sys
images = json.load(open(sys.argv[1]))["images"]
if not images:
    raise SystemExit("the pinned file lists no images")
for name, image in images.items():
    print(name, image["reference"], image["manifest_digest"])
IMG
)"
extract_status=$?
# The rule section 4 follows and this section did not: a read that failed
# leaves an empty list, the loop below runs zero times, no ok/DRIFT line is
# printed for any image, and the run finishes *clean*. That is the
# silent-clean answer this whole script exists to refuse, reached inside the
# script itself.
if (( extract_status != 0 )); then
    unknown "could not read the image pins: ${extract_err//$'\n'/ }"
    : > "$image_list"
elif [[ ! -s "$image_list" ]]; then
    unknown "the image pins read as empty"
fi
while read -r name reference pinned_digest; do
    [[ -z "${name:-}" ]] && continue
    path="${reference#ghcr.io/}"; path="${path%%:*}"
    tag="${reference##*:}"
    token="$(curl -sf --max-time 20 "https://ghcr.io/token?scope=repository:${path}:pull" 2>/dev/null \
        | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])' 2>/dev/null)"
    if [[ -z "$token" ]]; then
        unknown "$name: no registry token"
        continue
    fi
    live_digest="$(curl -sfI --max-time 25 -H "Authorization: Bearer $token" \
        -H 'Accept: application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json' \
        "https://ghcr.io/v2/${path}/manifests/${tag}" 2>/dev/null \
        | tr -d '\r' | awk -F': ' 'tolower($1)=="docker-content-digest"{print $2}')"
    if [[ -z "$live_digest" ]]; then
        unknown "$name: could not resolve $reference"
    elif [[ "$live_digest" == "$pinned_digest" ]]; then
        ok "$name"
    else
        moved "$name: the tag now points somewhere else"
        note  "pinned $pinned_digest"
        note  "live   $live_digest"
    fi
done < "$image_list"
rm -f "$image_list"

# ---- 4. live challenges against the pinned runtimes ------------------------
printf '\nLive challenges without a pinned runtime\n'
base="$(pin network.api_base_url)"
block="$(curl -sf --max-time 25 "$base/get-block" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["block"]["id"])' 2>/dev/null)"
if [[ -z "$block" ]]; then
    unknown "could not read the latest block from $base"
else
    challenges="$(curl -sf --max-time 30 "$base/get-challenges?block_id=$block" 2>/dev/null)"
    if [[ -z "$challenges" ]]; then
        unknown "could not read challenges from $base"
    else
        # The comparison runs from a file, not a heredoc: a heredoc here takes
        # over stdin from the pipe, so python read its own source as the
        # challenge data, failed, and printed nothing — and an empty answer
        # then read as "nothing missing". A check that could not run reporting
        # itself clean is the whole failure this script exists against.
        cmp_py="$(mktemp)"
        cat > "$cmp_py" <<'CMP'
import json, sys
pinned = set(json.load(open(sys.argv[1]))["images"])
challenges = json.load(sys.stdin).get("challenges")
if challenges is None:
    raise SystemExit("no challenges array in the response")
if not challenges:
    raise SystemExit("the challenges array is empty")
for c in challenges:
    cfg = c.get("config", {})
    name, kind = cfg.get("name"), cfg.get("type")
    if not name:
        raise SystemExit("a challenge carries no config.name")
    if f"{name}_runtime" not in pinned:
        print(f"{name}\t{kind}")
CMP
        report="$(printf '%s' "$challenges" | python3 "$cmp_py" "$pinned" 2>&1)"
        status=$?
        rm -f "$cmp_py"
        if (( status != 0 )); then
            unknown "could not compare challenges against the pins: $report"
        elif [[ -z "$report" ]]; then
            ok "every live challenge has a pinned runtime"
        else
            while IFS=$'\t' read -r name kind; do
                [[ -z "${name:-}" ]] && continue
                if [[ "$kind" == "cpu" ]]; then
                    # The ones that matter first: slice 1 offers no GPU, so a
                    # GPU challenge is one the pool would not select anyway.
                    moved "$name ($kind) — no pinned runtime, and the pool's compute path could select it"
                else
                    printf '  [note]      %s (%s) — no pinned runtime; outside the pool'"'"'s compute path today\n' "$name" "$kind"
                fi
            done <<< "$report"
        fi
    fi
fi

printf '\n'
if (( unchecked )); then
    printf 'Result: INCOMPLETE — a check could not run. Not the same as clean.\n\n'
    exit 2
elif (( drift )); then
    printf 'Result: DRIFT. What it means is a judgement, not a diff:\n'
    printf '  tig_integration.md §15 is the upgrade procedure, and a pin is only\n'
    printf '  worth what the review behind it established. Nothing here has been\n'
    printf '  changed; nothing should be until that review is done.\n\n'
    exit 1
else
    printf 'Result: clean — TIG is where this repository expects it to be.\n\n'
    exit 0
fi
