#!/usr/bin/env bash
# Evidence for slice-1 criteria K3 and K4, gathered from one live run.
#
# K3 asks for "the run's block heights and intent/attempt rows recorded in the
# PR". K4 asks additionally for a `get-benchmarks` scan of the pool's
# precommits for the run's decision block showing **exactly one** entry
# matching `tig_integration.md` §10's reconciliation tuple — player, decision
# block, challenge, algorithm, compute type, selected-track settings. A
# duplicate there fails the criterion; it is not absorbed as an operator case.
#
# Reads only. It pays no fee, sends no write, and changes nothing: the run has
# already happened by the time this is useful. Safe to re-run.
#
# Usage:
#   ./scripts/live-run-evidence.sh <controller-config.toml>
#   ./scripts/live-run-evidence.sh --selftest
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# K4's assertion is a *detector*, and a detector nothing tests is one that can
# quietly stop detecting — at which point the criterion reads as passing
# because nothing was found. These cases are the ones it has to tell apart,
# in both of its modes: the duplicate must fail the scan, and the single-tuple
# mode `live-crash-test.sh` reads its verdict from must count this decision's
# write and nothing else.
if [[ "${1:-}" == "--selftest" ]]; then
    scratch="$(mktemp -d)"
    trap 'rm -rf "$scratch"' EXIT
    p="0xp00l00000000000000000000000000000000000"
    python3 - "$scratch" "$p" <<'MAKE'
import json, sys
scratch, player = sys.argv[1], sys.argv[2]

FUEL = 5000000000000

def precommit(bid, challenge="c001", confirmed=1, track="t1",
              compute="aws_t4g", num_bundles=4, hyper=2, details_extra=None):
    details = {
        "compute_type": compute,
        "num_bundles": num_bundles,
        "fuel_budget": FUEL,
        "hyperparameters": {} if hyper is None else {"exploration_level": hyper},
    }
    if details_extra is not None:
        details.update(details_extra)
    return {
        "benchmark_id": bid,
        "settings": {
            "player_id": player, "block_id": "b1", "challenge_id": challenge,
            "algorithm_id": f"{challenge}_a001", "track_id": track,
        },
        "details": details,
        "state": {"block_confirmed": confirmed},
    }

other_player = dict(precommit("bench-1"),
                    settings=dict(precommit("bench-1")["settings"],
                                  player_id="0xsomeone-else"))

cases = {
    "one": [precommit("bench-1")],
    "duplicate": [precommit("bench-1"), precommit("bench-2")],
    "two-challenges": [precommit("bench-1"), precommit("bench-2", "c003")],
    "unconfirmed": [precommit("bench-1", confirmed=None)],
    "other-player": [other_player],
    # The single-tuple mode's cases. Each is one entry that differs from the
    # decision below in exactly one part of §10's tuple, so a matcher that
    # dropped that part would count it.
    "wrong-track": [precommit("bench-1", track="t9")],
    "wrong-compute": [precommit("bench-1", compute="nvidia_a10g")],
    "wrong-bundles": [precommit("bench-1", num_bundles=5)],
    "wrong-hyper": [precommit("bench-1", hyper=3)],
    # TIG returns hyperparameters as strings and the pool picks them as
    # values; `reconcile.rs` renders both sides before comparing, so this is
    # the same precommit, not a different one.
    "hyper-as-string": [dict(precommit("bench-1"),
                             details=dict(precommit("bench-1")["details"],
                                          hyperparameters={"exploration_level": "2"}))],
    # `candidate_of` calls a `details` without the numbers a shape error
    # rather than a miss. A miss would read as "the write is not at TIG",
    # which is the direction that licenses a resend.
    "no-bundles": [dict(precommit("bench-1"),
                        details={"compute_type": "aws_t4g", "fuel_budget": FUEL})],
}
for name, precommits in cases.items():
    with open(f"{scratch}/{name}.json", "w") as f:
        json.dump({"precommits": precommits}, f)

with open(f"{scratch}/decision.json", "w") as f:
    json.dump({
        "anchor_block_id": "b1",
        "selected_challenge": "c001",
        "selected_algorithm": "c001_a001",
        "compute_type": "aws_t4g",
        "track_settings": {
            "t1": {"num_bundles": 4, "fuel_budget": FUEL,
                   "hyperparameters": {"exploration_level": 2}},
            "t2": {"num_bundles": 4, "fuel_budget": FUEL,
                   "hyperparameters": {"exploration_level": 2}},
        },
    }, f)
MAKE

    scan="$root/scripts/lib/tuple-scan.py"
    fail() { echo "live-run-evidence selftest FAILED: $1" >&2; exit 1; }

    for name in one two-challenges unconfirmed other-player; do
        python3 "$scan" "$p" "$scratch/$name.json" >/dev/null \
            || fail "$name must not read as a duplicate"
    done
    ! python3 "$scan" "$p" "$scratch/duplicate.json" >/dev/null \
        || fail "a duplicate tuple must fail K4"

    # The single-tuple mode. The count is the verdict `live-crash-test.sh`
    # branches on, so each case names the count it must produce.
    counted() {
        python3 "$scan" --target "$p" "$scratch/$1.json" "$scratch/decision.json" \
            | cut -d' ' -f1
    }
    for case in one:1 duplicate:2 hyper-as-string:1 unconfirmed:0 other-player:0 \
                wrong-track:0 wrong-compute:0 wrong-bundles:0 wrong-hyper:0; do
        got="$(counted "${case%%:*}")" || fail "${case%%:*} could not be scanned"
        [[ "$got" == "${case##*:}" ]] \
            || fail "${case%%:*} counted $got for the target tuple, expected ${case##*:}"
    done
    status=0
    python3 "$scan" --target "$p" "$scratch/no-bundles.json" "$scratch/decision.json" \
        >/dev/null 2>&1 || status=$?
    [[ "$status" == "2" ]] \
        || fail "a details without num_bundles must be a shape error (exit 2), got $status"

    echo "live-run-evidence selftest: detects a duplicate §10 tuple, passes four that" \
         "are not, and counts one decision's tuple across nine cases"
    exit 0
fi

config="${1:?usage: live-run-evidence.sh [--selftest] <controller-config.toml>}"

value() { grep -E "^${2} *=" "$1" | head -1 | sed -E 's/^[^=]*= *"?([^"]*)"?.*/\1/'; }

player="$(value "$config" player_id)"
base_url="$(value "$config" base_url)"
db_name="$(value "$config" name)"
: "${POOL_PG_CONTAINER:=pool-pg}"

psql() { docker exec -i "$POOL_PG_CONTAINER" psql -U postgres -d "$db_name" "$@"; }

echo "# Live run evidence"
echo
echo "- endpoint: \`$base_url\`"
echo "- player: \`$player\`"
echo "- gathered: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo

echo "## Blocks taken in"
echo
echo '```'
psql -c "SELECT height, block_id, reads_complete, active_cache_ready
           FROM pool.block_snapshot
          ORDER BY height DESC LIMIT 10"
echo '```'
echo

echo "## Decisions"
echo
echo '```'
psql -c "SELECT d.decision_id, d.workflow_id, d.anchor_height, d.selected_challenge,
                d.selected_algorithm, d.compute_type, d.precommit_reserve::text
           FROM pool.precommit_decision d
          ORDER BY d.decided_at"
echo '```'
echo

echo "## Write intents (K3)"
echo
echo '```'
psql -c "SELECT intent_id::text, workflow_id, write_kind, generation, state,
                coalesce(trace_id, '-') AS trace_id, encode(payload_digest, 'hex') AS digest
           FROM pool.tig_write_intent
          ORDER BY created_at"
echo '```'
echo

echo "## Attempts (K3)"
echo
echo '```'
psql -c "SELECT a.attempt_no, a.intent_id::text, a.write_kind,
                coalesce(a.outcome, 'UNRESOLVED') AS outcome, a.http_status,
                coalesce(a.benchmark_id, '-') AS benchmark_id,
                a.started_at, a.resolved_at
           FROM pool.tig_write_attempt a
          ORDER BY a.started_at"
echo '```'
echo

echo "## Workflows"
echo
echo '```'
psql -c "SELECT workflow_id, state, coalesce(benchmark_id, '-') AS benchmark_id,
                unverified_from_block, coalesce(unverified_to_block::text, 'open') AS unverified_to
           FROM pool.workflow ORDER BY workflow_id"
echo '```'
echo

# K4's load-bearing assertion. The live API cannot report a server-side write
# count the way `fake-tig` does, so the equivalent is this scan: after a crash
# and reconciliation, exactly one confirmed precommit matches the tuple.
echo "## §10 tuple scan (K4)"
echo
# No credential. `tig_integration.md` §4 makes these reads public — verified
# against testnet, which answers both with HTTP 200 and no `X-Api-Key` — and
# `architecture.md` §2.2 gives the key to `tig-gateway` alone. An earlier
# version of this script read `secrets/tig-testnet-api-key` and passed it on
# curl's command line, which §9 forbids outright and which
# `scripts/credential-boundary.sh` exists to prevent: argv is world-readable
# in /proc for the life of the process. It was not needed for anything.
# `get-benchmarks` is served only for the *latest* block, and testnet advances
# every 15 seconds — so the block can move between reading its id and asking
# for the window. Retried rather than reported as a failure: a race is not
# evidence of anything, and a scan that gave up on one would read as "no
# duplicate found".
scan=""
for attempt in 1 2 3 4 5; do
    block_id="$(curl -sS "$base_url/get-block" \
                | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("block",d)["id"])' \
                2>/dev/null)" || true
    if [[ -z "$block_id" ]]; then sleep 3; continue; fi
    scan="$(curl -sS "$base_url/get-benchmarks?block_id=$block_id&player_id=$player")" || true
    if [[ "$scan" == \{* ]]; then break; fi
    echo "attempt $attempt: ${scan:0:120}" >&2
    scan=""
    sleep 3
done

if [[ -z "$scan" ]]; then
    echo "**INCOMPLETE**: \`get-benchmarks\` could not be read for the latest block."
    echo "The scan proves nothing when it does not run; this is not a pass."
    exit 2
fi

# The window goes to a file, not down a pipe: `python3 -` reads its *program*
# from stdin, so a heredoc and piped data cannot both be there — the data
# silently loses and `json.load` sees the script text.
window="$(mktemp)"
trap 'rm -f "$window"' EXIT
printf '%s' "$scan" > "$window"
python3 "$root/scripts/lib/tuple-scan.py" "$player" "$window"
