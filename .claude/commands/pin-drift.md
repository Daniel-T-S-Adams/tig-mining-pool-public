---
description: Check whether TIG has moved away from what this repository is pinned to, and say what it would take to catch up.
---

Run `./scripts/pin-drift.sh` and report what it found.

The script does the detection. Your job is the part it deliberately refuses to
do: say what the differences **mean** for this pool, and what catching up would
actually involve. `docs/tig_integration.md` §15 is the upgrade procedure; §2
is what the pin covers and why.

## How to read the result

- **clean** — say so in one line. Do not manufacture concern.
- **INCOMPLETE** — a check could not run. Say which, and that this is not the
  same as clean. Do not summarise the checks that did pass as though the
  answer were known.
- **DRIFT** — the work below.

## When there is drift

For each difference, establish what it means rather than restating it:

- **The upstream commit moved.** Read the comparison the script links. Most
  commits will be algorithm submissions that touch nothing this pool depends
  on. What matters is changes to protocol contracts, the core data structures
  the pool parses, the benchmarker's behaviour, or configuration meanings.
  Name the ones that matter and say why; say plainly if none do.
- **The API specification changed.** Fetch it and diff against what the pool
  expects. §14 records known discrepancies at the current pin — check whether
  any have been fixed or joined.
- **A container digest moved.** A tag now points at different bytes. This is
  the supply-chain case §2 pins digests against, and it deserves more alarm
  than the others.
- **A live challenge has no pinned runtime.** A CPU one is a challenge this
  pool could have selected and cannot. A GPU one is not, today. Say which.

Then say what catching up would take, in §15's terms, and roughly how much of
it is mechanical versus judgement.

## Rules

- **Change nothing.** Not the pinned config, not the fixtures, not the docs.
  This command reports; §15's upgrade is a separate, reviewed piece of work.
- **Do not guess.** If you cannot tell whether a change matters, say that it
  needs reading rather than assuming it is harmless. A wrong "no action
  needed" is the answer nobody re-checks.
- Be brief when the answer is short. A clean result is one line.
