"""`tig_integration.md` §10's reconciliation tuple, over one `get-benchmarks` window.

Two modes, one detector:

  tuple-scan.py <player> <window.json>
      K4's duplicate scan. Groups the player's confirmed precommits by tuple
      and fails if any tuple holds more than one.

  tuple-scan.py --target <player> <window.json> <decision.json>
      Counts the confirmed precommits that are **one particular decision's**
      write, for G2's live crash test. Prints "<count> <benchmark_id>..." and
      leaves the verdict to the caller, because both counts are meaningful
      there: one where the write reached TIG, zero where it did not.

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


def confirmed_for_player(body, player):
    """§7's confirmation test, and whose precommit it is.

    `state.block_confirmed` is the sole lifecycle authority: an entry can be
    present in the window and unconfirmed, and counting one as a hit would
    call an unconfirmed write "at TIG" — the reading §10 forbids acting on.
    """
    for p in body.get("precommits", []):
        if p.get("state", {}).get("block_confirmed") is None:
            continue
        if p.get("settings", {}).get("player_id", "").lower() != player.lower():
            continue
        yield p


def rendered(hyperparameters):
    """`reconcile.rs`'s `hyperparameters_match`, in Python.

    Both sides render to text before comparing, so `250` and `"250"` are the
    same hyperparameter: TIG returns these as strings and the pool selects
    them as values, and a type difference is not a different method.
    """
    return {
        k: v if isinstance(v, str) else json.dumps(v, sort_keys=True, separators=(",", ":"))
        for k, v in (hyperparameters or {}).items()
    }


def matches(p, decision):
    """`Candidate::matches` (crates/pool-workflow/src/reconcile.rs), in Python.

    The whole §10 tuple — block, challenge, algorithm, compute type and the
    selected track's settings — not the three-field prefix. Another workflow's
    decision can land on the same (block, challenge, algorithm) with a
    different compute type or track, and counting it here would invent a
    duplicate that does not exist.

    The track comes from TIG, so the test is that its selection is one of the
    tracks the pool offered AND that what it confirmed for that track is what
    the pool submitted for it.
    """
    s, d = p.get("settings", {}), p.get("details", {})
    if (s.get("block_id"), s.get("challenge_id"), s.get("algorithm_id")) != (
        decision["anchor_block_id"],
        decision["selected_challenge"],
        decision["selected_algorithm"],
    ):
        return False
    if d.get("compute_type") != decision["compute_type"]:
        return False
    offered = decision["track_settings"].get(s.get("track_id"))
    if offered is None:
        return False
    for key in ("num_bundles", "fuel_budget"):
        if not isinstance(d.get(key), int) or isinstance(d.get(key), bool):
            # `candidate_of` calls a non-numeric `details.<key>` a shape error
            # rather than a miss, and so does this: a miss would read as "the
            # write is not at TIG", which is the direction that licenses a
            # resend.
            raise ValueError(
                f"{p.get('benchmark_id')}: details.{key} is missing or not a number"
            )
        if offered.get(key) != d[key]:
            return False
    return rendered(offered.get("hyperparameters")) == rendered(d.get("hyperparameters"))


def scan_target(player, body, decision):
    hits = [p["benchmark_id"] for p in confirmed_for_player(body, player) if matches(p, decision)]
    print(len(hits), *hits)
    return 0


def scan_duplicates(player, body):
    # §10's tuple. `num_bundles` is not in it: TIG derives `num_nonces` and
    # selects the track, so the pool's proposal and TIG's record differ by
    # construction on those, and matching on them would turn a real duplicate
    # into two misses.
    by_tuple = collections.defaultdict(list)
    for p in confirmed_for_player(body, player):
        s = p.get("settings", {})
        by_tuple[(
            s.get("block_id"),
            s.get("challenge_id"),
            s.get("algorithm_id"),
            s.get("track_id"),
            p.get("details", {}).get("compute_type"),
        )].append(p["benchmark_id"])

    if not by_tuple:
        print("no confirmed precommit for this player in the window")
        return 0

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
        return 1
    return 0


def main(argv):
    if argv[1:2] == ["--target"]:
        player, window, target = argv[2], argv[3], argv[4]
        with open(window) as f:
            body = json.load(f)
        with open(target) as f:
            decision = json.load(f)
        return scan_target(player, body, decision)
    player, window = argv[1], argv[2]
    with open(window) as f:
        body = json.load(f)
    return scan_duplicates(player, body)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except ValueError as e:
        # Exit 2, distinct from a duplicate's 1: the window did not have the
        # shape the tuple is read from, so there is no count to believe either
        # way.
        print(f"tuple-scan: {e}", file=sys.stderr)
        sys.exit(2)
