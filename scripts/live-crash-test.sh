#!/usr/bin/env bash
# Slice-1 criterion K4: G2's crash test, repeated against live testnet.
#
# The crash point is G2's second: **after the attempt row is written, before
# the HTTP response is recorded.** That is the one that matters for
# duplicates — the durable record cannot say whether the request left, so
# recovery has to find out rather than guess, and guessing wrong either
# duplicates a paid write or abandons one that landed.
#
# G2's fake-tig tests assert a server-side write count. Live TIG cannot report
# one, so K4's equivalent is the §10 tuple scan in `live-run-evidence.sh`:
# after the crash and reconciliation, **exactly one** confirmed precommit
# matches the reconciliation tuple. A duplicate fails the criterion.
#
# The kill is timed off the database, not a sleep: the attempt row appearing is
# precisely "the request is about to leave", and it is the only observable that
# means that. A fixed delay would hit a different point on a slow call and
# quietly test something else.
#
# Usage:
#   ./scripts/live-crash-test.sh <gateway-config.toml>
set -euo pipefail

config="${1:?usage: live-crash-test.sh <gateway-config.toml>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
: "${POOL_PG_CONTAINER:=pool-pg}"
: "${GATEWAY_BIN:=$root/target/debug/tig-gateway}"

db_name="$(grep -E '^name *=' "$config" | head -1 | sed -E 's/^[^=]*= *"?([^"]*)"?.*/\1/')"
psql() { docker exec -i "$POOL_PG_CONTAINER" psql -U postgres -d "$db_name" -tAc "$1"; }

attempts_now() { psql "SELECT count(*) FROM pool.tig_write_attempt" | tr -d '[:space:]'; }

# "Unresolved" is the ledger's definition, not `outcome IS NULL`:
# `migrations/0004` says "Unresolved includes AMBIGUOUS ... Treating a recorded
# AMBIGUOUS as resolved would reopen the lane", and its partial index and
# `WriteAttempt::is_unresolved` both read it that way. AMBIGUOUS *is* §10's
# stop-for-operator state with the serialized lane still shut, so counting it
# as settled would print "recovered by finding the write" over a pool that is
# stuck.
#
# Scoped to the attempt this run created. The table can hold unresolved
# attempts from earlier runs — the crash this script performs leaves one by
# design — and a verdict taken over the whole table would be about a previous
# run's state.
unresolved_now() {
    psql "SELECT count(*) FROM pool.tig_write_attempt
           WHERE attempt_id = '$crashed_attempt'::uuid
             AND (outcome IS NULL OR outcome = 'AMBIGUOUS')" | tr -d '[:space:]'
}

before="$(attempts_now)"
echo "attempts before: $before"

log="${CRASH_LOG:-$(mktemp)}"
"$GATEWAY_BIN" --config "$config" run > "$log" 2>&1 &
gateway=$!
echo "gateway pid $gateway, log $log"

# Wait for the attempt row **inside the database**, not by polling from here.
#
# The window is narrow: on the live run that motivated this, TIG answered in
# 150 ms. A shell loop that spawns `docker exec psql` each pass takes longer
# than that, so it would routinely wake after the sender had already recorded
# the response — and then kill a process with nothing in flight, while still
# reporting "killed at the attempt row". The evidence would be from a run that
# never exercised the ambiguity path at all.
#
# This blocks server-side and returns the moment the row is visible, so the
# only latency is one 10 ms sleep plus the pipe.
wait_sql="DO \$\$
BEGIN
    WHILE (SELECT count(*) FROM pool.tig_write_attempt) <= $before LOOP
        PERFORM pg_sleep(0.01);
    END LOOP;
END
\$\$;"

killed=no
if timeout "${CRASH_WAIT_SECONDS:-600}" docker exec -i "$POOL_PG_CONTAINER" \
        psql -U postgres -d "$db_name" -qAt -c "$wait_sql" >/dev/null 2>&1; then
    kill -9 "$gateway" 2>/dev/null || true
    killed=yes
    echo "killed at the attempt row"
fi
# Always, including the timeout path: `run` is a long-lived server, so an
# unconditional `wait` on a live one blocks for ever — in precisely the case
# this branch exists to report.
kill -9 "$gateway" 2>/dev/null || true
wait "$gateway" 2>/dev/null || true

if [[ "$killed" != yes ]]; then
    echo "INCOMPLETE: no attempt was written within the window." >&2
    echo "Nothing was crashed, so nothing is proved — this is not a pass." >&2
    exit 2
fi

# This run's attempt: the newest row, which the wait above just watched appear.
crashed_attempt="$(psql "SELECT attempt_id::text FROM pool.tig_write_attempt
                          ORDER BY started_at DESC LIMIT 1")"
if [[ -z "$crashed_attempt" ]]; then
    echo "INCOMPLETE: the attempt row vanished between the wait and the read." >&2
    exit 2
fi
echo "this run's attempt: $crashed_attempt"

# And whether the kill actually landed in the window. G2's crash point is
# defined by the durable state it leaves: an attempt with a NULL outcome,
# because the record cannot say whether the request left. An attempt that
# already carries an outcome means the sender finished first and the run
# exercised the ordinary path, not the ambiguity — which is the one thing K4's
# evidence must not be taken from.
if [[ "$(unresolved_now)" == "0" ]]; then
    echo >&2
    echo "INCOMPLETE: the kill landed after the sender recorded its outcome." >&2
    echo "No attempt is unresolved, so G2's crash point was not reached and this" >&2
    echo "run proves nothing about ambiguity recovery. Re-run; the window is" >&2
    echo "roughly the length of one TIG call." >&2
    exit 2
fi

echo
echo "durable state at the crash point:"
psql "SELECT a.attempt_no, coalesce(a.outcome, 'NULL') AS outcome,
             coalesce(a.http_status::text, '-') AS status, i.state
        FROM pool.tig_write_attempt a
        JOIN pool.tig_write_intent i ON i.intent_id = a.intent_id
       ORDER BY a.started_at"
echo
echo "restarting the gateway; §10's search settles the attempt from confirmed reads"
"$GATEWAY_BIN" --config "$config" run >> "$log" 2>&1 &
gateway=$!

# Give reconciliation a window. The confirmed read is what settles it, and a
# precommit confirms a block or two after it lands.
for _ in $(seq 1 120); do
    unresolved="$(psql "SELECT count(*) FROM pool.tig_write_attempt WHERE outcome IS NULL" | tr -d '[:space:]')"
    [[ "$unresolved" == "0" ]] && break
    sleep 5
done
kill "$gateway" 2>/dev/null || true
wait "$gateway" 2>/dev/null || true

echo
echo "after reconciliation:"
psql "SELECT a.attempt_no, coalesce(a.outcome, 'STILL UNRESOLVED') AS outcome,
             coalesce(a.benchmark_id, '-') AS benchmark_id, i.state
        FROM pool.tig_write_attempt a
        JOIN pool.tig_write_intent i ON i.intent_id = a.intent_id
       ORDER BY a.started_at"

# G2's rule: "exactly one where a write reached TIG, and exactly zero where it
# did not ... one recovers by finding the write and the other by refusing to
# guess, and the counts are what tell them apart."
#
# So an attempt still unresolved is not, on its own, a failure. The crash point
# leaves a record that cannot say whether the request left, and when it did not
# the correct recovery is to stop for an operator rather than resend into a
# possible second fee. What would be a failure is the pool stopping while the
# write *is* there — the search missing evidence that exists.
#
# The first version of this script called every unresolved attempt a failed
# recovery, and this run showed why that is wrong: the kill landed before the
# request left, TIG holds nothing for the tuple, and refusing to guess is
# exactly what §10 asks for.
# "Recovered by finding the write" is `reconciled_at`, not a benchmark id on
# the attempt.
#
# `migrations/0004` defines `reconciled_at` as when §10 reconciliation settled
# an ambiguity, and constrains it to a settled outcome — so it marks exactly
# recovery-by-finding, and a plain transmit never sets it.
#
# The obvious-looking predicate, `ACCEPTED` with a non-null `benchmark_id`, is
# **unsatisfiable here**: `migrations/0003` forces a precommit intent's
# `benchmark_id` to NULL, and `0004`'s trigger copies the intent's value at
# INSERT and freezes it. Slice 1 transmits precommits only, so that branch
# could never fire and a successful recovery would have fallen through to the
# count check below and printed FAIL — inverting K4's verdict on the one path
# it exists to certify.
settled="$(psql "SELECT count(*) FROM pool.tig_write_attempt
                  WHERE attempt_id = '$crashed_attempt'::uuid
                    AND outcome = 'ACCEPTED' AND reconciled_at IS NOT NULL")"

# Before reading the counts: did the restarted gateway actually run §10's
# search? A gateway that never got that far — write gate refused, database
# unreachable, a failing pass — leaves durable state identical to one that
# searched and refused to guess. Recording that as a correct recovery would
# credit the pool for reasoning it never did.
#
# The observable is its own log: a pass that reached this intent emits an
# outcome line for it, whatever it decided.
# Keyed on *this* intent's id, not the event name. The table holds unresolved
# attempts from earlier runs by design — this script's own crash leaves one —
# so an outcome line for any of them would satisfy a bare event-name grep while
# saying nothing about whether the intent under test was ever decided.
crashed_intent="$(psql "SELECT i.intent_id::text
                          FROM pool.tig_write_intent i
                          JOIN pool.tig_write_attempt a ON a.intent_id = i.intent_id
                         WHERE a.attempt_id = '$crashed_attempt'::uuid")"
if [[ "$settled" != "1" ]] \
   && ! grep -q "\"intent_id\":\"$crashed_intent\"" "$log" 2>/dev/null; then
    echo "INCOMPLETE: the restarted gateway never reached intent $crashed_intent." >&2
    echo "No pass decided it, so §10's search did not run and the state below is" >&2
    echo "the crash's, not a recovery's. Gateway log: $log" >&2
    grep -oE '"event":"[a-z._]+"' "$log" 2>/dev/null | sort | uniq -c | tail -5 >&2
    exit 2
fi

# G2: "exactly one where a write reached TIG, and exactly zero where it did
# not". Both branches below read this count — the recovered one to prove there
# is no duplicate, the unresolved one to prove there is nothing to have found.
# Joined on the whole key. `migrations/0005` makes a decision unique per
# `(network, workflow_id, generation)`, and §7.3 allows a workflow more than
# one generation — so joining on `workflow_id` alone can pair the attempt with
# another generation's decision and read the tuple off the wrong one. `LIMIT 1`
# would then pick silently.
tuple="$(psql "SELECT d.anchor_block_id || ' ' || d.selected_challenge || ' ' ||
                      d.selected_algorithm
                 FROM pool.precommit_decision d
                 JOIN pool.tig_write_intent i
                   ON i.network = d.network
                  AND i.workflow_id = d.workflow_id
                  AND i.generation = d.generation
                 JOIN pool.tig_write_attempt a ON a.intent_id = i.intent_id
                WHERE a.attempt_id = '$crashed_attempt'::uuid")"
read -r anchor challenge algorithm <<<"${tuple:-}"
# An empty tuple matches nothing at TIG, which would read as "zero precommits"
# and print a pass. The join can come up empty for reasons that have nothing to
# do with the recovery — so this is INCOMPLETE, not evidence.
if [[ -z "${anchor:-}" || -z "${challenge:-}" || -z "${algorithm:-}" ]]; then
    echo "INCOMPLETE: could not read the decision tuple for the crashed attempt." >&2
    echo "An empty tuple matches nothing at TIG and would read as a pass." >&2
    exit 2
fi
echo "  tuple: block=$anchor challenge=$challenge algorithm=$algorithm"

base_url="$(grep -E '^base_url *=' "$config" | head -1 | sed -E 's/^[^=]*= *"?([^"]*)"?.*/\1/')"
player="$(grep -E '^player_id *=' "$config" | head -1 | sed -E 's/^[^=]*= *"?([^"]*)"?.*/\1/')"
found=""
for _ in 1 2 3 4 5; do
    block_id="$(curl -sS "$base_url/get-block" \
                | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("block",d)["id"])' \
                2>/dev/null)" || true
    [[ -z "$block_id" ]] && { sleep 3; continue; }
    window="$(curl -sS "$base_url/get-benchmarks?block_id=$block_id&player_id=$player")" || true
    [[ "$window" == \{* ]] || { sleep 3; continue; }
    found="$(printf '%s' "$window" | python3 -c '
import json, sys
anchor, challenge, algorithm = sys.argv[1], sys.argv[2], sys.argv[3]
body = json.load(sys.stdin)
hits = [
    p["benchmark_id"]
    for p in body.get("precommits", [])
    if p.get("settings", {}).get("block_id") == anchor
    and p.get("settings", {}).get("challenge_id") == challenge
    and p.get("settings", {}).get("algorithm_id") == algorithm
]
print(len(hits), *hits)' "$anchor" "$challenge" "$algorithm")"
    break
done

if [[ -z "$found" ]]; then
    echo "INCOMPLETE: could not read the window, so the counts cannot be compared." >&2
    exit 2
fi
count="${found%% *}"
echo "  TIG holds $count precommit(s) for it"

if [[ "$settled" == "1" ]]; then
    # Recovered by finding the write. This is the branch where a duplicate is
    # possible — the pool sent, crashed, and searched — so G2's "exactly one"
    # is checked here rather than left to a separate run.
    if [[ "$count" == "1" ]]; then
        echo
        echo "recovered by finding the write: §10's search settled the attempt from"
        echo "a confirmed read, and TIG holds exactly one precommit for the tuple."
        echo "That is G2's 'exactly one where a write reached TIG'."
        echo
        echo "gateway log: $log"
        exit 0
    fi
    echo >&2
    echo "FAIL: the attempt settled but TIG holds $count precommits for the tuple." >&2
    echo "G2 requires exactly one; more is the duplicate §10 and invariant 14" >&2
    echo "exist to prevent, and fewer means the search settled against nothing." >&2
    echo "Gateway log: $log" >&2
    exit 1
fi

if [[ "$count" == "0" ]]; then
    echo
    echo "recovered by refusing to guess: the write never reached TIG, the pool"
    echo "stopped for an operator rather than resending, and no duplicate exists."
    echo "That is G2's 'exactly zero where it did not' — the correct recovery for"
    echo "this crash point."
    echo
    echo "gateway log: $log"
    exit 0
fi

echo >&2
echo "FAIL: the write is at TIG and the pool did not find it." >&2
echo "§10's search is what settles an ambiguous attempt from confirmed reads;" >&2
echo "leaving it unresolved holds the serialized lane shut behind a write that" >&2
echo "is plainly there. Gateway log: $log" >&2
exit 1

echo
echo "gateway log: $log"
echo "run ./scripts/live-run-evidence.sh <controller-config> for the §10 tuple scan (K4)"
