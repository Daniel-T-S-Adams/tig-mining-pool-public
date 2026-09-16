"""`tig_integration.md` §10's reconciliation tuple, counted over one window.

Slice-1 criterion K4: after a crash and reconciliation, a `get-benchmarks` scan
of the pool's precommits must show **exactly one** confirmed entry per tuple. A
duplicate fails the criterion — it is not absorbed as an operator-resolution
case, because a duplicate confirmed precommit is precisely what §10 and
`architecture.md` invariant 14 exist to prevent.

Its own file rather than a heredoc so `--selftest` can run the same code the
live run does; a detector tested by a copy of itself tests nothing.
"""

import collections
import json
import sys

player = sys.argv[1]
with open(sys.argv[2]) as f:
    body = json.load(f)
# §10's tuple. `num_bundles` is not in it: TIG derives `num_nonces` and selects
# the track, so the pool's proposal and TIG's record differ by construction on
# those, and matching on them would turn a real duplicate into two misses.
by_tuple = collections.defaultdict(list)
for p in body.get("precommits", []):
    if p.get("state", {}).get("block_confirmed") is None:
        continue
    s = p.get("settings", {})
    if not s.get("player_id", "").lower() == player.lower():
        continue
    by_tuple[(
        s.get("block_id"),
        s.get("challenge_id"),
        s.get("algorithm_id"),
        s.get("track_id"),
        p.get("details", {}).get("compute_type"),
    )].append(p["benchmark_id"])

if not by_tuple:
    print("no confirmed precommit for this player in the window")
    sys.exit(0)

duplicates = 0
print("```")
for tup, ids in sorted(by_tuple.items(), key=lambda kv: str(kv[0])):
    block, challenge, algorithm, track, compute = tup
    mark = "DUPLICATE" if len(ids) > 1 else "ok"
    if len(ids) > 1:
        duplicates += 1
    print(f"{mark:9} block={block} challenge={challenge} algorithm={algorithm} "
          f"track={track} compute={compute} -> {len(ids)}: {', '.join(ids)}")
print("```")
if duplicates:
    print()
    print(f"**K4 FAILS**: {duplicates} tuple(s) with more than one confirmed "
          "precommit. A duplicate here is not absorbed as an operator case.")
    sys.exit(1)
