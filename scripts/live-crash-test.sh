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
# one, so K4's equivalent is a §10 tuple scan of `get-benchmarks`: after the
# crash and reconciliation, **exactly one** confirmed precommit matches the
# reconciliation tuple where the write reached TIG, and exactly zero where it
# did not. A duplicate fails the criterion.
#
# The scan runs here, through `scripts/lib/tuple-scan.py` — the same detector
# `live-run-evidence.sh --selftest` exercises, and a transcription of
# `reconcile.rs`'s `Candidate::matches`. An inline reimplementation stood here
# and matched on three fields of the tuple, so an entry from another track or
# compute type counted as this write's, and an unconfirmed one counted at all.
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

# One extractor for every config value this script reads, the same
# expression `live-run-evidence.sh` pins. Two hand-written copies of it lost
# the `\1` and read as empty, which sent every live read to a URL with no host
# and ended every run in "INCOMPLETE: could not read the window" — a script
# that could never reach a verdict.
value() { grep -E "^${2} *=" "$1" | head -1 | sed -E 's/^[^=]*= *"?([^"]*)"?.*/\1/'; }

db_name="$(value "$config" name)"
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
# `statement_timeout` bounds the loop server-side too. When `timeout` kills the
# client, PostgreSQL may not notice the dropped connection at all —
# `client_connection_check_interval` is 0 by default — and the backend would go
# on spinning at 10 ms intervals after an INCOMPLETE run. A `SET` in the same
# psql session applies to the `DO` that follows it.
if timeout "${CRASH_WAIT_SECONDS:-600}" docker exec -i "$POOL_PG_CONTAINER" \
        psql -U postgres -d "$db_name" -qAt \
        -c "SET statement_timeout = '${CRASH_WAIT_SECONDS:-600}s'" \
        -c "$wait_sql" >/dev/null 2>&1; then
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
#
# A recorded AMBIGUOUS is *not* the sender finishing: it is the sender saying
# it does not know either, and §10's search settles it by the same path. So
# this reads the ledger's "unresolved", which includes it, rather than
# `outcome IS NULL`.
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
# Its own file. Appending to the crashed run's log would let a line the crashed
# gateway wrote *before* the kill satisfy a gate that is asking what the
# restarted one did.
restart_log="$log.restart"
"$GATEWAY_BIN" --config "$config" run > "$restart_log" 2>&1 &
gateway=$!

# Give reconciliation a window. The confirmed read is what settles it, and a
# precommit confirms a block or two after it lands.
#
# Waits on *this* attempt, through the same ledger definition the verdict uses.
# A count over the whole table would never reach zero while an earlier run's
# unresolved attempt sat in it — and this script leaves one behind on every run
# that ends in the refusing-to-guess branch.
for _ in $(seq 1 120); do
    [[ "$(unresolved_now)" == "0" ]] && break
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

# "Recovered by finding the write" is `reconciled_at`, and only `reconciled_at`.
#
# `migrations/0004` defines it as when §10 reconciliation settled an ambiguity,
# and `tig_write_attempt_reconciled_is_settled` constrains it to a settled
# outcome — ACCEPTED **or REJECTED**. So it marks exactly recovery-by-finding,
# and a plain transmit never sets it. The outcome is read alongside because it
# says which count is the right one: an ACCEPTED write is at TIG exactly once,
# a REJECTED one is not there at all.
#
# Adding `outcome = 'ACCEPTED'` to the predicate looked harmless and was not. A
# reconciliation that settled REJECTED would have fallen through to the
# unresolved branch and printed "the pool stopped for an operator rather than
# resending" — an unverified claim about pool reasoning, over a pool that had
# in fact settled from a confirmed read. Nothing produces a reconciled REJECTED
# today (`drive.rs` only reconciles to Accepted), which is exactly why the
# predicate should follow the constraint rather than the current call sites.
#
# The obvious-looking predicate, `ACCEPTED` with a non-null `benchmark_id`, is
# **unsatisfiable here**: `migrations/0003` forces a precommit intent's
# `benchmark_id` to NULL, and `0004`'s trigger copies the intent's value at
# INSERT and freezes it. Slice 1 transmits precommits only, so that branch
# could never fire and a successful recovery would have fallen through to the
# count check below and printed FAIL — inverting K4's verdict on the one path
# it exists to certify.
reconciled_outcome="$(psql "SELECT outcome FROM pool.tig_write_attempt
                             WHERE attempt_id = '$crashed_attempt'::uuid
                               AND reconciled_at IS NOT NULL" | tr -d '[:space:]')"

# The intent this run's attempt belongs to, and the workflow behind it. Both
# are read once and used by every check below, so a check cannot silently be
# about a different write.
read -r crashed_intent crashed_workflow crashed_network <<<"$(psql "
    SELECT i.intent_id::text || ' ' || i.workflow_id || ' ' || i.network
      FROM pool.tig_write_intent i
      JOIN pool.tig_write_attempt a ON a.intent_id = i.intent_id
     WHERE a.attempt_id = '$crashed_attempt'::uuid")"
if [[ -z "${crashed_intent:-}" || -z "${crashed_workflow:-}" ]]; then
    echo "INCOMPLETE: could not identify the intent behind the crashed attempt." >&2
    exit 2
fi

# Before reading the counts: did the restarted gateway actually run §10's
# search? A gateway that never got that far — write gate refused, database
# unreachable, a failing pass — leaves durable state identical to one that
# searched and refused to guess. Recording that as a correct recovery would
# credit the pool for reasoning it never did.
#
# The observable is its own log, and the test is a *decision*, not the id
# appearing somewhere. Three `Acted` values carry this intent's id without the
# search having reached a conclusion about it:
#
# - `Failed` — "the intent could not be evaluated", and `Notability::Operator`,
#   so it is logged at warn exactly like a genuine stop;
# - `LeaseHeldElsewhere` — returned before `decide()` runs. This matters here
#   more than anywhere: the SIGKILLed gateway left its `PrecommitTransmit`
#   lease held, so the restarted one logs this for our intent on every pass
#   until the lease expires;
# - `WriteBlocked` — the decision was `Transmit` and only the write gate
#   stopped it, which is the opposite of refusing to guess.
#
# So this takes the four that do mean a conclusion, and treats anything else as
# INCOMPLETE. A variant added later fails closed — no evidence — rather than
# quietly counting as one.
#
# The two conclusions this test can end on are visible at the dev config's
# `info`: a `StopForOperator` decision is `Notability::Operator` (warn) and
# `AttemptSettled` is `Effect` (info). `AwaitingSender` is `Routine` (debug),
# so a run whose only outcome was "the sender may still be waiting" reads
# INCOMPLETE below debug — again the fail-closed direction, and the answer is
# to re-run or to raise `telemetry.level`.
decided_this_intent() {
    python3 - "$restart_log" "$crashed_intent" <<'SCAN'
import json, sys

path, intent = sys.argv[1], sys.argv[2]
try:
    lines = open(path, encoding="utf-8", errors="replace")
except OSError:
    sys.exit(1)
for line in lines:
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        record = json.loads(line)
    except json.JSONDecodeError:
        continue
    # `pool-telemetry` flattens event fields to the top level; the nested form
    # is read too so this does not depend on that staying true.
    fields = {**record.get("fields", {}), **record}
    if fields.get("event") != "gateway.intent.outcome":
        continue
    if fields.get("intent_id") != intent:
        continue
    # `decision` is `Option<ClaimDecision>` rendered by `Debug`, so an
    # outcome from before the decision renders "None".
    if str(fields.get("decision", "None")) == "None":
        continue
    acted = str(fields.get("acted", ""))
    if not acted.startswith(("Nothing", "AttemptSettled", "AwaitingSender", "Transmitted")):
        continue
    sys.exit(0)
sys.exit(1)
SCAN
}

if [[ -z "$reconciled_outcome" ]] && ! decided_this_intent; then
    echo "INCOMPLETE: no pass of the restarted gateway decided intent $crashed_intent." >&2
    echo "§10's search did not run — or ran and failed — so the state below is the" >&2
    echo "crash's, not a recovery's. Gateway log: $restart_log" >&2
    grep -oE '"event":"[a-z._]+"' "$restart_log" 2>/dev/null | sort | uniq -c | tail -5 >&2
    exit 2
fi

# Whatever the window says, the restarted gateway must not have written again
# for this workflow. This is the regression the whole exercise is about, and
# the window alone cannot report it: a resend that has not confirmed yet is
# absent from §7's confirmed view, so a blind resubmit would otherwise read as
# "refused to guess".
#
# Scoped to the workflow, not to the intent and not to the whole table. A
# resend can arrive as a second attempt on the same intent or as a fresh
# generation with its own intent (§7.3), and both are the duplicate. The table
# as a whole is the wrong scope in the other direction: once the crashed
# attempt settles, §10's lane reopens and a write for some *other* workflow is
# ordinary progress.
resends="$(psql "SELECT count(*) FROM pool.tig_write_attempt a
                   JOIN pool.tig_write_intent i ON i.intent_id = a.intent_id
                  WHERE i.network = '$crashed_network'
                    AND i.workflow_id = '$crashed_workflow'
                    AND a.attempt_id <> '$crashed_attempt'::uuid" | tr -d '[:space:]')"
if [[ "$resends" != "0" ]]; then
    echo >&2
    echo "FAIL: the restarted gateway wrote $resends more attempt(s) for workflow" >&2
    echo "$crashed_workflow. The crash left a record that cannot say whether the" >&2
    echo "request left, and resending on it is the blind resubmit §10 and" >&2
    echo "architecture.md invariant 14 forbid. Gateway log: $restart_log" >&2
    exit 1
fi

# §10's tuple, read off the decision this attempt was made from. Joined on the
# whole key: `migrations/0005` makes a decision unique per
# `(network, workflow_id, generation)`, and §7.3 allows a workflow more than
# one generation — so joining on `workflow_id` alone can pair the attempt with
# another generation's decision and read the tuple off the wrong one.
#
# The whole tuple, not a prefix of it: compute type and the offered track
# settings are in §10's "exact player, decision block, challenge, algorithm,
# compute type and selected-track settings", and `Candidate::matches` compares
# all of them. Matching on (block, challenge, algorithm) alone would count
# another workflow's precommit on the same block as this one's.
decision_file="$(mktemp)"
psql "SELECT json_build_object(
          'anchor_block_id',     d.anchor_block_id,
          'selected_challenge',  d.selected_challenge,
          'selected_algorithm',  d.selected_algorithm,
          'compute_type',        d.compute_type,
          'track_settings',      d.track_settings)::text
        FROM pool.precommit_decision d
        JOIN pool.tig_write_intent i
          ON i.network = d.network
         AND i.workflow_id = d.workflow_id
         AND i.generation = d.generation
        JOIN pool.tig_write_attempt a ON a.intent_id = i.intent_id
       WHERE a.attempt_id = '$crashed_attempt'::uuid" > "$decision_file"

# An empty or partial tuple matches nothing at TIG, which would read as "zero
# precommits" and print a pass. The join can come up empty for reasons that
# have nothing to do with the recovery — so this is INCOMPLETE, not evidence.
if ! python3 -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except (OSError, ValueError):
    sys.exit(1)
missing = [k for k in ("anchor_block_id", "selected_challenge", "selected_algorithm",
                       "compute_type", "track_settings") if not d.get(k)]
sys.exit(1 if missing else 0)' "$decision_file"; then
    echo "INCOMPLETE: could not read the full decision tuple for the crashed attempt." >&2
    echo "A partial tuple matches nothing at TIG and would read as a pass." >&2
    exit 2
fi
python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
print("  tuple: block=%s challenge=%s algorithm=%s compute=%s tracks=%d offered" % (
    d["anchor_block_id"], d["selected_challenge"], d["selected_algorithm"],
    d["compute_type"], len(d["track_settings"])))' "$decision_file"

base_url="$(value "$config" base_url)"
player="$(value "$config" player_id)"
# Named explicitly, because an empty one reads as a failed window fetch — the
# same INCOMPLETE for "the endpoint is unreachable" and "this script cannot
# find the endpoint in the config", which are not the same problem.
if [[ -z "$base_url" || -z "$player" ]]; then
    echo "INCOMPLETE: could not read base_url and player_id from $config." >&2
    echo "Both are read from the top level; a nested or quoted-differently key" >&2
    echo "would come back empty and every TIG read would then fail." >&2
    exit 2
fi
window_file="$(mktemp)"
got_window=no
for _ in 1 2 3 4 5; do
    block_id="$(curl -sS "$base_url/get-block" \
                | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("block",d)["id"])' \
                2>/dev/null)" || true
    [[ -z "$block_id" ]] && { sleep 3; continue; }
    curl -sS "$base_url/get-benchmarks?block_id=$block_id&player_id=$player" > "$window_file" || true
    if python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$window_file" 2>/dev/null; then
        got_window=yes
        break
    fi
    sleep 3
done
if [[ "$got_window" != yes ]]; then
    echo "INCOMPLETE: could not read the window, so the counts cannot be compared." >&2
    exit 2
fi

# The pinned detector, in its single-tuple mode. Exit 2 is its shape error —
# `details` without the numbers the tuple is read from — which is not a count
# to believe in either direction.
scan_status=0
found="$(python3 "$root/scripts/lib/tuple-scan.py" --target "$player" "$window_file" "$decision_file")" \
    || scan_status=$?
if [[ "$scan_status" != "0" ]]; then
    echo "INCOMPLETE: the tuple scan could not read the window (exit $scan_status)." >&2
    exit 2
fi
read -r confirmed unconfirmed _ids <<<"$found"
echo "  TIG holds $confirmed confirmed and $unconfirmed unconfirmed precommit(s) for it"
# `reconcile_precommit` counts matches "across confirmed AND unconfirmed"
# before classifying, because two records matching what the pool submitted
# means it cannot tell which is its own.
matched=$((confirmed + unconfirmed))

logs="gateway logs: $log (crashed run), $restart_log (restart)"

if [[ -n "$reconciled_outcome" ]]; then
    # Recovered by finding the write. This is the branch where a duplicate is
    # possible — the pool sent, crashed, and searched — so G2's "exactly one"
    # is checked here rather than left to a separate run.
    case "$reconciled_outcome" in
        ACCEPTED) expected=1 ;;
        REJECTED) expected=0 ;;
        *)
            echo >&2
            echo "INCOMPLETE: the attempt is reconciled with outcome" >&2
            echo "'$reconciled_outcome', which migrations/0004 does not allow for a" >&2
            echo "reconciled attempt. $logs" >&2
            exit 2
            ;;
    esac
    if [[ "$matched" == "$expected" && "$confirmed" == "$expected" ]]; then
        echo
        echo "recovered by finding the write: §10's search settled the attempt"
        echo "$reconciled_outcome from a confirmed read, and TIG holds exactly"
        echo "$expected precommit(s) for the tuple."
        if [[ "$reconciled_outcome" == "ACCEPTED" ]]; then
            echo "That is G2's 'exactly one where a write reached TIG', and it is K4's"
            echo "scan: one confirmed entry for the reconciliation tuple, no duplicate."
        else
            echo "TIG rejected the write, so nothing landed and there is nothing to"
            echo "duplicate. This run does NOT deliver K4's 'exactly one' evidence."
        fi
        echo
        echo "$logs"
        exit 0
    fi
    echo >&2
    echo "FAIL: the attempt settled $reconciled_outcome but TIG holds $confirmed" >&2
    echo "confirmed and $unconfirmed unconfirmed precommit(s) for the tuple, not" >&2
    echo "$expected confirmed and nothing else. More than one match is the" >&2
    echo "duplicate §10 and invariant 14 exist to prevent; fewer, or one that has" >&2
    echo "not confirmed, means the search settled against something the window" >&2
    echo "does not hold. $logs" >&2
    exit 1
fi

# Not reconciled: the claim this branch makes is that the pool *refused to
# guess*. That claim is about three things together — the attempt is still
# unresolved, no resend was made (checked above), and TIG holds nothing — and
# each is checked, because the durable state of "refused to guess" and of
# "settled it wrongly and resent" differ in exactly these places.
if [[ "$(unresolved_now)" != "1" ]]; then
    echo >&2
    echo "FAIL: the attempt is neither reconciled nor unresolved." >&2
    echo "Something recorded an outcome for it without §10's search settling it," >&2
    echo "so 'refused to guess' is not what happened. $logs" >&2
    exit 1
fi
intent_state="$(psql "SELECT state FROM pool.tig_write_intent
                       WHERE intent_id = '$crashed_intent'::uuid" | tr -d '[:space:]')"
if [[ "$intent_state" == "CONFIRMED" ]]; then
    echo >&2
    echo "FAIL: the intent is CONFIRMED while its attempt is unresolved." >&2
    echo "§7 makes CONFIRMED the record of a write that landed; reaching it" >&2
    echo "without settling the attempt is not refusing to guess. $logs" >&2
    exit 1
fi

# An unconfirmed match is `Reconciliation::PendingConfirmation`, which
# `reconcile.rs` keeps deliberately distinct from `NoCandidate` — a precommit
# that reached TIG and has not confirmed yet is not evidence that nothing was
# sent. Printing "the write never reached TIG" over one would be an unverified
# claim about what TIG holds, and the collapse §7 and §10 forbid acting on.
if [[ "$unconfirmed" != "0" ]]; then
    echo >&2
    echo "INCOMPLETE: $unconfirmed precommit(s) match the tuple but have not" >&2
    echo "confirmed. That is PendingConfirmation, not absence, so this run is not" >&2
    echo "evidence either way. Re-run the scan once they confirm — a precommit" >&2
    echo "confirms a block or two after it lands. $logs" >&2
    exit 2
fi

if [[ "$confirmed" == "0" ]]; then
    echo
    echo "recovered by refusing to guess: the write never reached TIG, the pool"
    echo "stopped for an operator rather than resending, and no duplicate exists."
    echo "That is G2's 'exactly zero where it did not' — the correct recovery for"
    echo "this crash point."
    echo
    echo "NOTE: this run exercised the refusing-to-guess branch only, so on its own"
    echo "it is not K4's 'exactly one' scan. The kill landed before the request left,"
    echo "which means no write was ever at risk of being duplicated here. For the"
    echo "branch where one is — sent, crashed, then found — re-run until the kill"
    echo "lands after the request left; the window is the length of one TIG call."
    echo
    echo "$logs"
    exit 0
fi

echo >&2
echo "FAIL: the write is at TIG ($confirmed confirmed) and the pool did not find it." >&2
echo "§10's search is what settles an ambiguous attempt from confirmed reads;" >&2
echo "leaving it unresolved holds the serialized lane shut behind a write that" >&2
echo "is plainly there. $logs" >&2
exit 1
