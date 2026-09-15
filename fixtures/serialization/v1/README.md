# Serialization fixture v1

Provenance: **constructed**. These are not captured TIG responses; they are
the two properties `tig_integration.md` §13 check 7 requires a build to
demonstrate before it may write, expressed as data so the demonstration can
run rather than be asserted.

Files here are immutable once merged — corrections create a `v2`, per the
convention in `fixtures/tig/v1/README.md`.

## Why these run at startup and not only in CI

§13 is a gate on a *running process*: "at startup and after any deployment,
the TIG gateway must pass all of these checks". A property proven in CI is a
property of a tree, not of the binary in front of you — and a binary built
from a tree whose tests never ran is exactly the deployment §13 exists to
stop. Both files are compiled into the gateway with `include_str!`, so the
process carries what it verifies.

## `lossless.json`

§4 requires lossless numeric handling, and `accounting.md` §3 forbids floating
point. The values here are chosen so that a parser routing integers through
`f64` gives a different answer: each is above 2^53, where a double can no
longer represent every integer. A round-trip that returns them unchanged
demonstrates the property; one that returns a neighbouring value demonstrates
the bug.

`fuel_consumed` and `nonce` are the fields §6.3 carries at full width, which
is why they are the ones recorded.

## `precommit-body.json`

§6.1's body, rendered by `pool_workflow::payload::precommit_body`, with the
exact bytes it must produce.

Pinned for reasons the design documents actually establish, not for a claim
about how TIG hashes requests — nothing here records that, and
`tig_integration.md` §10 identifies a precommit by its semantic fields rather
than by a body digest:

- `architecture.md` §7.3 binds an admitted intent to its canonical payload
  digest, and the gateway refuses to transmit bytes that do not reproduce it.
  A change to the rendering therefore invalidates intents already recorded —
  silently, and after the decision that created them.
- `mining_system.md` §6.6 copies the source benchmark's hyperparameters, so a
  re-typed one is a different method rather than a different encoding of the
  same one.

`input` is the submission; `expected_bytes` is the serialization it must
produce.

**What this establishes, and what it does not.** `expected_bytes` was taken
from what the current code renders, so it cannot prove the rendering is what
TIG accepts — that would be the code agreeing with itself.

What the protocol spike established is narrower than it is tempting to claim.
It submitted §6.1 bodies to live testnet and had them confirmed
(`docs/protocol_spike_report.md`), which verifies the envelope, the key order
and the empty `track_id`. But the spike always sent
`"hyperparameters": null` (`crates/spike/src/lib.rs`), so **no body carrying a
hyperparameters object has ever been confirmed by TIG**, let alone one mixing
an integer and a float.

That part of this fixture is pinned from current code and is *unverified
live*. It stays that way until slice 1's live run submits a body with typed
hyperparameters, which is the step that closes it. Recording it as verified
would be the fixture-integrity failure this file exists to prevent. The hyperparameters deliberately mix an integer and a float, because
§6.6 copies the source benchmark's values at their own types and re-typing
either is the failure this pins.
