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

before="$(attempts_now)"
echo "attempts before: $before"

log="${CRASH_LOG:-$(mktemp)}"
"$GATEWAY_BIN" --config "$config" run > "$log" 2>&1 &
gateway=$!
echo "gateway pid $gateway, log $log"

# Wait for the attempt row, then kill immediately. SIGKILL, not SIGTERM: a
# graceful shutdown would let the sender record its outcome, which is the
# state this crash point exists to avoid producing.
killed=no
for _ in $(seq 1 600); do
    if [[ "$(attempts_now)" -gt "$before" ]]; then
        kill -9 "$gateway" 2>/dev/null || true
        killed=yes
        echo "killed at the attempt row"
        break
    fi
    if ! kill -0 "$gateway" 2>/dev/null; then
        echo "gateway exited before writing an attempt; see $log" >&2
        exit 2
    fi
    sleep 0.2
done
wait "$gateway" 2>/dev/null || true

if [[ "$killed" != yes ]]; then
    echo "INCOMPLETE: no attempt was written within the window." >&2
    echo "Nothing was crashed, so nothing is proved — this is not a pass." >&2
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
echo
echo "gateway log: $log"
echo "run ./scripts/live-run-evidence.sh <controller-config> for the §10 tuple scan (K4)"
