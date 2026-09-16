"""`tig_integration.md` §10's reconciliation tuple, over one `get-benchmarks` window.

Two modes, one detector:

  tuple-scan.py <player> <window.json>
      K4's duplicate scan. Groups the player's confirmed precommits by tuple
      and fails if any tuple holds more than one.

  tuple-scan.py --target <player> <window.json> <decision.json>
      Counts the precommits that are **one particular decision's** write, for
      G2's live crash test. Prints "<confirmed> <unconfirmed> <benchmark_id>..."
      and leaves the verdict to the caller, because both counts are meaningful
      there: one confirmed where the write reached TIG, none at all where it did
      not, and an unconfirmed match is neither — it is a write that arrived and
      has not confirmed yet, which §7 and §10 keep distinct from absence.

      Either mode exits 2 on a record it cannot read, which is what
      `candidate_of` does with one: a record that cannot be parsed might be the
      pool's own.

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


REQUIRED_SETTINGS = ("player_id", "block_id", "challenge_id", "algorithm_id", "track_id")


def candidate_of(index, p):
    """`candidate_of` (crates/pool-workflow/src/reconcile.rs), in Python.

    Every record in the window is read, and one that cannot be read is a shape
    error rather than a miss — `reconcile_precommit` calls `candidate_of` on
    all of them before matching any, "precisely because a record that cannot be
    parsed might be the pool's own". A miss would read as "the write is not at
    TIG", the direction that licenses a resend.
    """
    def shape(reason):
        return ValueError(f"precommit {index}: {reason}")

    if not isinstance(p.get("benchmark_id"), str):
        raise shape("missing precommit.benchmark_id")
    settings, details = p.get("settings"), p.get("details")
    if not isinstance(settings, dict):
        raise shape("missing settings")
    if not isinstance(details, dict):
        raise shape("missing details")
    for key in REQUIRED_SETTINGS:
        if not isinstance(settings.get(key), str):
            raise shape(f"missing settings.{key}")
    if not isinstance(details.get("compute_type"), str):
        raise shape("missing details.compute_type")
    for key in ("num_bundles", "fuel_budget"):
        value = details.get(key)
        # `as_u64`, so a negative is as unreadable as a string. TIG never
        # returns one; the point is that this file claims to be a
        # transcription, and a claim that is true of the easy cases only is
        # the kind a reader inherits as fact.
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise shape(f"missing or non-numeric details.{key}")
    # `candidate_of` takes null or an object and calls anything else a shape
    # error. Coercing here would send a string into `rendered`, which raises
    # `AttributeError` — exit 1, the duplicate's code, with a traceback.
    hyperparameters = details.get("hyperparameters")
    if hyperparameters is not None and not isinstance(hyperparameters, dict):
        raise shape("details.hyperparameters is not an object")

    return {
        "benchmark_id": p["benchmark_id"],
        "player_id": settings["player_id"],
        "block_id": settings["block_id"],
        "challenge_id": settings["challenge_id"],
        "algorithm_id": settings["algorithm_id"],
        "track_id": settings["track_id"],
        "compute_type": details["compute_type"],
        "num_bundles": details["num_bundles"],
        "fuel_budget": details["fuel_budget"],
        "hyperparameters": hyperparameters or {},
        # §7's confirmation test: `state.block_confirmed` is the sole
        # lifecycle authority. An entry can be present in the window and
        # unconfirmed, which is a different fact from being absent.
        "confirmed": p.get("state", {}).get("block_confirmed") is not None,
    }


def for_player(body, player):
    """Every readable record in the window that is this player's.

    Case-insensitive, unlike `Candidate::matches`, because the pool's
    configured id and TIG's rendering of it are both hex and need not agree on
    case; they do on testnet today, and a scan that silently matched nothing
    because of case would read as "no write at TIG".
    """
    for index, p in enumerate(body.get("precommits", [])):
        candidate = candidate_of(index, p)
        if candidate["player_id"].lower() == player.lower():
            yield candidate


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


def matches(c, decision):
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
    if (c["block_id"], c["challenge_id"], c["algorithm_id"], c["compute_type"]) != (
        decision["anchor_block_id"],
        decision["selected_challenge"],
        decision["selected_algorithm"],
        decision["compute_type"],
    ):
        return False
    offered = decision["track_settings"].get(c["track_id"])
    if offered is None:
        return False
    if (offered.get("num_bundles"), offered.get("fuel_budget")) != (
        c["num_bundles"],
        c["fuel_budget"],
    ):
        return False
    return rendered(offered.get("hyperparameters")) == rendered(c["hyperparameters"])


def scan_target(player, body, decision):
    """Count this decision's write, confirmed and unconfirmed separately.

    Both counts, because `reconcile_precommit` keeps `NoCandidate` and
    `PendingConfirmation` deliberately distinct: a precommit that reached TIG
    and has not confirmed yet is not evidence that nothing was sent, and
    collapsing the two is the reading §7 and §10 forbid acting on. The caller
    reads "0 0" as "nothing is there" and "0 1" as "something is, but it has
    not confirmed".
    """
    confirmed, unconfirmed = [], []
    for c in for_player(body, player):
        if matches(c, decision):
            (confirmed if c["confirmed"] else unconfirmed).append(c["benchmark_id"])
    print(len(confirmed), len(unconfirmed), *(confirmed + unconfirmed))
    return 0


def scan_duplicates(player, body):
    # §10's tuple. `num_bundles` is not in it: TIG derives `num_nonces` and
    # selects the track, so the pool's proposal and TIG's record differ by
    # construction on those, and matching on them would turn a real duplicate
    # into two misses.
    by_tuple = collections.defaultdict(list)
    for c in for_player(body, player):
        if not c["confirmed"]:
            continue
        by_tuple[(
            c["block_id"],
            c["challenge_id"],
            c["algorithm_id"],
            c["track_id"],
            c["compute_type"],
        )].append(c["benchmark_id"])

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
