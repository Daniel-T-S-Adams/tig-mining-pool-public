//! Client-side limits from `docs/tig_integration.md` §11.
//!
//! These are *client policy*, not claims about server capacity. TIG
//! publishes no numerical quota and returned no remaining-limit headers
//! during the review, so the pool is deliberately conservative and records
//! what it observes.

use std::time::Duration;

/// Which reader a set of limits is for.
///
/// §11's budget is against a limit TIG applies **per IP**, and the pool runs
/// more than one reading process behind one egress address
/// (`architecture.md` §4, §11.2). So the budget is allocated, not handed
/// out whole to each: see [`ReadLimits::for_reader`] and ADR-0006.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TigReader {
    /// Snapshot assembly (§9), the active-benchmark cache (§5.2) and
    /// lifecycle reconciliation (§10).
    Controller,
    /// Pre-write compatibility checks (§13 check 5) and the reconciliation
    /// a write needs before retry (§10).
    Gateway,
}

impl TigReader {
    /// Every reader that holds a share.
    ///
    /// The ceiling test maps over this rather than a hand-written list, so
    /// a new variant fails the sum instead of silently receiving a further
    /// full share.
    pub const ALL: &'static [TigReader] = &[TigReader::Controller, TigReader::Gateway];
}

/// The §11 limits for one reader.
///
/// Compiled values, not yet wired to configuration. `tig_integration.md` §8's
/// guardrails are read from `config/tig_integration.json`; these are not, so
/// changing one is currently a code change. The struct is shaped to be
/// configured later — the claim that it already is would be false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLimits {
    /// Sustained rate for GETs, shared by every client on the same host
    /// **within this process**.
    pub requests_per_second: u32,
    /// Burst allowance on top of the sustained rate.
    pub burst: u32,
    /// Ceiling on GETs actually in flight — counted while a request or its
    /// body is transferring, not while a caller waits.
    pub max_concurrent: usize,
    pub connect_timeout: Duration,
    /// Budget for a SINGLE attempt, which is what §11's "GET total timeout"
    /// pins. Not the budget for a whole call: that is [`Self::call_deadline`].
    pub attempt_timeout: Duration,
    /// Backstop for a whole call including every retry and backoff.
    ///
    /// A backstop, not a timing policy: its only job is to stop a read
    /// hanging indefinitely, and it stays loose enough that §11's retry
    /// rules remain reachable — an honoured 60-second `Retry-After`
    /// followed by another attempt has to fit inside it.
    ///
    /// Deliberately NOT derived from the block interval. §8 treats observed
    /// block timings as evidence rather than constants, so a caller needing
    /// a read to finish inside a block sets its own bound from live
    /// configuration. [`Self::for_block_poll`] is that bound for the
    /// `get-block` poll, whose misses §10 makes unrecoverable.
    pub call_deadline: Duration,
    /// Full-jitter exponential backoff is capped here.
    pub max_backoff: Duration,
    /// Attempts per call, including the first.
    pub max_attempts: u32,
}

impl ReadLimits {
    /// The whole per-IP budget §11 pins. **Not** for a single process: it
    /// is the ceiling every reader together must stay under, and exists so
    /// [`Self::for_reader`]'s shares can be checked against it.
    ///
    /// Deliberately not public. ADR-0006 removes `Default` so a reader must
    /// name itself rather than take the whole allowance — and a public
    /// constructor handing out exactly that allowance is the same hole with
    /// a different name. Production code reaches the budget only through
    /// [`Self::for_reader`] or [`Self::for_block_poll`]; tests reach it
    /// through the crate's `testing` module.
    pub(crate) fn pool_ceiling() -> Self {
        Self {
            requests_per_second: 2,
            burst: 2,
            max_concurrent: 2,
            connect_timeout: Duration::from_secs(5),
            attempt_timeout: Duration::from_secs(30),
            call_deadline: Duration::from_secs(300),
            max_backoff: Duration::from_secs(60),
            max_attempts: 5,
        }
    }

    /// This reader's share of the pinned per-IP budget (ADR-0006).
    ///
    /// There is deliberately no `Default`: a reader that did not state
    /// which one it is would take the whole allowance, and two such
    /// processes would silently double the pool's rate against a limit
    /// whose whole purpose is to avoid sustained throttling — which §10
    /// turns into permanently unrecoverable per-block attribution when the
    /// `get-block` poll is the request being throttled.
    pub fn for_reader(reader: TigReader) -> Self {
        // Matched exhaustively and per variant, deliberately. Ignoring the
        // argument and returning one fixed share meant a third reader would
        // have received a full share of its own and exceeded the ceiling by
        // construction — the fail-open shape ADR-0006 exists to prevent —
        // while a hand-written test still passed. A new variant now has to
        // be given a share here, and `shares_within_ceiling` over
        // `TigReader::ALL` is what refuses one that does not fit.
        //
        // Equal shares rather than weighted: no measurement yet justifies a
        // weighting, and ADR-0006 records what would change that.
        let (requests_per_second, burst, max_concurrent) = match reader {
            TigReader::Controller => (1, 1, 1),
            TigReader::Gateway => (1, 1, 1),
        };
        Self {
            requests_per_second,
            burst,
            max_concurrent,
            ..Self::pool_ceiling()
        }
    }

    /// Limits for the `get-block` poll specifically.
    ///
    /// §11 polls every 15 seconds and §10 makes a missed height
    /// unrecoverable for per-block attribution, so this path must fail and
    /// be retried rather than stall past the next poll.
    pub fn for_block_poll(reader: TigReader) -> Self {
        let interval = Duration::from_secs(15);
        let base = Self::for_reader(reader);
        Self {
            call_deadline: interval,
            // Clamped too. The whole-call deadline is only consulted
            // between attempts, so a single slow-but-not-failing attempt
            // inheriting the 30-second per-attempt budget would occupy
            // twice the poll interval before any bound applied — the
            // opposite of what this constructor exists to guarantee.
            attempt_timeout: base.attempt_timeout.min(interval),
            ..base
        }
    }

    /// Every pinned duration clamped to policy: stricter is allowed, looser
    /// is not.
    ///
    /// Applied to all of them rather than to `max_backoff` alone. §11 pins
    /// the connect timeout, the per-attempt GET timeout and the whole-call
    /// backstop as well, and `is_entitled_share` constrains none of them —
    /// so clamping one field left a caller free to widen the other three,
    /// which is the same shape as fixing one route to a guard and leaving
    /// the others open.
    pub(crate) fn clamped_to_policy(self) -> Self {
        let pinned = Self::pool_ceiling();
        Self {
            connect_timeout: self.connect_timeout.min(pinned.connect_timeout),
            attempt_timeout: self.attempt_timeout.min(pinned.attempt_timeout),
            call_deadline: self.call_deadline.min(pinned.call_deadline),
            max_backoff: self.max_backoff.min(pinned.max_backoff),
            ..self
        }
    }

    /// The backoff ceiling §11 pins, independent of what a caller passed.
    ///
    /// Used where a bound must come from policy rather than from the
    /// client: the process-global `Retry-After` pause is written from a
    /// server response, and `max_backoff` is caller-supplied and outside
    /// the entitlement check, so bounding that pause by the caller's value
    /// alone would let one client widen it arbitrarily.
    pub(crate) fn pinned_max_backoff() -> Duration {
        Self::pool_ceiling().max_backoff
    }

    /// Whether these limits are a share a reader is entitled to.
    ///
    /// `ReadLimits` has public fields for readability, so removing `Default`
    /// and gating `pool_ceiling` only closed the named routes to the full
    /// allowance — a struct literal could still spell it out field by
    /// field. This is what `TigReadClient::new` checks, so the entitlement
    /// is enforced where a client is built rather than where limits are
    /// described.
    pub(crate) fn is_entitled_share(&self) -> bool {
        TigReader::ALL.iter().copied().any(|reader| {
            let allowed = [Self::for_reader(reader), Self::for_block_poll(reader)];
            allowed.iter().any(|a| {
                a.requests_per_second == self.requests_per_second
                    && a.burst == self.burst
                    && a.max_concurrent == self.max_concurrent
            })
        })
    }

    /// Whether a set of per-reader shares stays within the pinned ceiling.
    pub fn shares_within_ceiling(shares: &[Self]) -> bool {
        let ceiling = Self::pool_ceiling();
        let rate: u32 = shares.iter().map(|s| s.requests_per_second).sum();
        let burst: u32 = shares.iter().map(|s| s.burst).sum();
        let concurrent: usize = shares.iter().map(|s| s.max_concurrent).sum();
        rate <= ceiling.requests_per_second
            && burst <= ceiling.burst
            && concurrent <= ceiling.max_concurrent
    }
}
