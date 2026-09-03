//! Client-side limits from `docs/tig_integration.md` §11.
//!
//! These are *client policy*, not claims about server capacity. TIG
//! publishes no numerical quota and returned no remaining-limit headers
//! during the review, so the pool is deliberately conservative and records
//! what it observes.

use std::time::Duration;

use serde::Deserialize;

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

    /// The key naming this reader's share in `config/tig_integration.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            TigReader::Controller => "controller",
            TigReader::Gateway => "gateway",
        }
    }
}

/// The §11 limits for one reader.
///
/// Values come from `config/tig_integration.json` via [`ReadPolicy`], the
/// way §8's guardrails already did (criterion E5). There is deliberately no
/// compiled fallback: a default ceiling would be a protocol constant copied
/// into the binary, which §12 forbids, and it would be reached exactly when
/// configuration was missing — the moment least able to notice.
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

/// The §11 client policy, as loaded from `config/tig_integration.json`.
///
/// Criterion E5. The pinned numbers live in configuration rather than in
/// this binary, so changing one is a reviewed config change instead of a
/// code change — and §12's rule that no observed protocol value becomes a
/// compiled constant holds for the read budget too.
///
/// Everything derived from the pool-wide ceiling hangs off a `ReadPolicy`
/// value rather than an associated function. That is what keeps the ceiling
/// un-gettable without configuration: an associated `pool_ceiling()` would
/// have needed a compiled default to return, which is the fallback this type
/// exists to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPolicy {
    ceiling: ReadLimits,
    /// One field per reader rather than a map. `for_reader` is then an
    /// exhaustive match with no fallback arm, so adding a `TigReader`
    /// variant fails to compile until it is given a share — stronger than
    /// a runtime lookup, and with no "unreachable" default that would have
    /// to guess between over- and under-issuing.
    controller: ReadLimits,
    gateway: ReadLimits,
    block_poll_interval: Duration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("cannot parse the TIG integration config: {0}")]
    Unparseable(String),
    #[error("{path} is missing from the TIG integration config")]
    Missing { path: String },
    #[error("{path} is not a {expected}")]
    WrongType { path: String, expected: String },
    /// ADR-0006's sum rule, refused at load rather than at use.
    #[error(
        "the configured reader shares exceed the pool-wide ceiling \
         ({rate} req/s of {ceiling_rate}, burst {burst} of {ceiling_burst}, \
         {concurrent} concurrent of {ceiling_concurrent})"
    )]
    SharesExceedCeiling {
        rate: u32,
        ceiling_rate: u32,
        burst: u32,
        ceiling_burst: u32,
        concurrent: usize,
        ceiling_concurrent: usize,
    },
}

/// The wire shape of `read_limits.pool_ceiling`.
///
/// Every field is required, and unknown ones are refused: a typo would
/// otherwise parse as an unknown key, be dropped in silence, and leave the
/// intended value unset — the config saying one thing and the client doing
/// another.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CeilingWire {
    requests_per_second: u32,
    burst: u32,
    max_concurrent: usize,
    connect_timeout_seconds: u64,
    attempt_timeout_seconds: u64,
    call_deadline_seconds: u64,
    max_backoff_seconds: u64,
    max_attempts: u32,
}

/// The wire shape of one entry in `read_limits.reader_shares`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareWire {
    requests_per_second: u32,
    burst: u32,
    max_concurrent: usize,
}

impl ReadPolicy {
    /// Parse the policy out of the TIG integration config.
    pub fn from_config_json(json: &str) -> Result<Self, PolicyError> {
        let root: serde_json::Value =
            serde_json::from_str(json).map_err(|e| PolicyError::Unparseable(e.to_string()))?;
        let section = root
            .get("read_limits")
            .ok_or_else(|| PolicyError::Missing {
                path: "read_limits".to_string(),
            })?;

        let ceiling_value = section
            .get("pool_ceiling")
            .ok_or_else(|| PolicyError::Missing {
                path: "read_limits.pool_ceiling".to_string(),
            })?;
        let wire: CeilingWire =
            serde_json::from_value(ceiling_value.clone()).map_err(|e| PolicyError::WrongType {
                path: "read_limits.pool_ceiling".to_string(),
                expected: format!("complete ceiling ({e})"),
            })?;
        let ceiling = ReadLimits {
            requests_per_second: wire.requests_per_second,
            burst: wire.burst,
            max_concurrent: wire.max_concurrent,
            connect_timeout: Duration::from_secs(wire.connect_timeout_seconds),
            attempt_timeout: Duration::from_secs(wire.attempt_timeout_seconds),
            call_deadline: Duration::from_secs(wire.call_deadline_seconds),
            max_backoff: Duration::from_secs(wire.max_backoff_seconds),
            max_attempts: wire.max_attempts,
        };

        let shares_value = section
            .get("reader_shares")
            .ok_or_else(|| PolicyError::Missing {
                path: "read_limits.reader_shares".to_string(),
            })?;

        // Every reader must be named in the config. A missing entry is a
        // load failure, never a default share: ADR-0006 exists because a
        // reader that silently received a full allowance would double the
        // pool's rate against a per-IP limit, and §10 makes sustained
        // throttling of the `get-block` poll permanently unrecoverable for
        // per-block attribution.
        let share_of = |reader: TigReader| -> Result<ReadLimits, PolicyError> {
            let path = format!("read_limits.reader_shares.{}", reader.as_str());
            let entry = shares_value
                .get(reader.as_str())
                .ok_or_else(|| PolicyError::Missing { path: path.clone() })?;
            let wire: ShareWire =
                serde_json::from_value(entry.clone()).map_err(|e| PolicyError::WrongType {
                    path,
                    expected: format!("complete reader share ({e})"),
                })?;
            Ok(ReadLimits {
                requests_per_second: wire.requests_per_second,
                burst: wire.burst,
                max_concurrent: wire.max_concurrent,
                ..ceiling
            })
        };
        let controller = share_of(TigReader::Controller)?;
        let gateway = share_of(TigReader::Gateway)?;

        let block_poll_interval = section
            .get("block_poll_interval_seconds")
            .ok_or_else(|| PolicyError::Missing {
                path: "read_limits.block_poll_interval_seconds".to_string(),
            })?
            .as_u64()
            .ok_or_else(|| PolicyError::WrongType {
                path: "read_limits.block_poll_interval_seconds".to_string(),
                expected: "non-negative integer".to_string(),
            })?;

        let policy = Self {
            ceiling,
            controller,
            gateway,
            block_poll_interval: Duration::from_secs(block_poll_interval),
        };

        // ADR-0006's sum rule, enforced at load. Checking it here rather
        // than leaving it to a caller means a config that over-allocates the
        // per-IP budget cannot produce a usable policy at all — §11 makes
        // sustained throttling of the `get-block` poll permanently
        // unrecoverable for per-block attribution (§10), so this is not a
        // warning.
        //
        // Summed over `TigReader::ALL` through the exhaustive accessor, so a
        // new reader is counted here the moment it compiles.
        let allocated: Vec<ReadLimits> = TigReader::ALL
            .iter()
            .copied()
            .map(|r| policy.for_reader(r))
            .collect();
        let rate: u32 = allocated.iter().map(|s| s.requests_per_second).sum();
        let burst: u32 = allocated.iter().map(|s| s.burst).sum();
        let concurrent: usize = allocated.iter().map(|s| s.max_concurrent).sum();
        if rate > ceiling.requests_per_second
            || burst > ceiling.burst
            || concurrent > ceiling.max_concurrent
        {
            return Err(PolicyError::SharesExceedCeiling {
                rate,
                ceiling_rate: ceiling.requests_per_second,
                burst,
                ceiling_burst: ceiling.burst,
                concurrent,
                ceiling_concurrent: ceiling.max_concurrent,
            });
        }

        Ok(policy)
    }

    /// The whole per-IP budget §11 pins.
    ///
    /// Crate-private for the reason ADR-0006 gives: a public accessor
    /// returning the entire allowance is the same hole as a `Default`, with
    /// a different name. Production readers take a share.
    pub(crate) fn ceiling(&self) -> ReadLimits {
        self.ceiling
    }

    /// This reader's configured share of the per-IP budget (ADR-0006).
    ///
    /// Total by construction: the shares are named fields, so there is no
    /// missing-entry case to invent a value for. A fallback here would have
    /// to choose between over-issuing (the fail-open ADR-0006 forbids) and
    /// under-issuing silently; the type system removes the choice.
    pub fn for_reader(&self, reader: TigReader) -> ReadLimits {
        match reader {
            TigReader::Controller => self.controller,
            TigReader::Gateway => self.gateway,
        }
    }

    /// Limits for the `get-block` poll specifically.
    ///
    /// §11 polls on the configured interval and §10 makes a missed height
    /// unrecoverable for per-block attribution, so this path must fail and
    /// be retried rather than stall past the next poll.
    pub fn for_block_poll(&self, reader: TigReader) -> ReadLimits {
        let interval = self.block_poll_interval;
        let base = self.for_reader(reader);
        ReadLimits {
            call_deadline: interval,
            // Clamped too. The whole-call deadline is only consulted
            // between attempts, so a single slow-but-not-failing attempt
            // inheriting the per-attempt budget would occupy twice the poll
            // interval before any bound applied — the opposite of what this
            // constructor exists to guarantee.
            attempt_timeout: base.attempt_timeout.min(interval),
            ..base
        }
    }

    /// Every pinned duration clamped to policy: stricter is allowed, looser
    /// is not.
    ///
    /// Applied to all of them rather than to `max_backoff` alone. §11 pins
    /// the connect timeout, the per-attempt GET timeout and the whole-call
    /// backstop as well, and [`Self::is_entitled_share`] constrains none of
    /// them — so clamping one field left a caller free to widen the other
    /// three, which is the same shape as fixing one route to a guard and
    /// leaving the others open.
    pub(crate) fn clamp(&self, limits: ReadLimits) -> ReadLimits {
        ReadLimits {
            connect_timeout: limits.connect_timeout.min(self.ceiling.connect_timeout),
            attempt_timeout: limits.attempt_timeout.min(self.ceiling.attempt_timeout),
            call_deadline: limits.call_deadline.min(self.ceiling.call_deadline),
            max_backoff: limits.max_backoff.min(self.ceiling.max_backoff),
            ..limits
        }
    }

    /// The backoff ceiling §11 pins, independent of what a caller passed.
    ///
    /// Used where a bound must come from policy rather than from the
    /// client: the process-global `Retry-After` pause is written from a
    /// server response, and `max_backoff` is caller-supplied and outside
    /// the entitlement check, so bounding that pause by the caller's value
    /// alone would let one client widen it arbitrarily.
    pub fn pinned_max_backoff(&self) -> Duration {
        self.ceiling.max_backoff
    }

    /// Whether these limits are a share a reader is entitled to.
    ///
    /// `ReadLimits` has public fields for readability, so loading the
    /// ceiling from configuration and keeping [`Self::ceiling`] crate-private
    /// only closes the named routes to the full allowance — a struct literal
    /// could still spell it out field by field. This is what
    /// `TigReadClient::new` checks, so the entitlement is enforced where a
    /// client is built rather than where limits are described.
    pub(crate) fn is_entitled_share(&self, limits: &ReadLimits) -> bool {
        TigReader::ALL.iter().copied().any(|reader| {
            let allowed = [self.for_reader(reader), self.for_block_poll(reader)];
            allowed.iter().any(|a| {
                a.requests_per_second == limits.requests_per_second
                    && a.burst == limits.burst
                    && a.max_concurrent == limits.max_concurrent
            })
        })
    }

    /// Whether a set of per-reader shares stays within the pinned ceiling.
    pub fn shares_within_ceiling(&self, shares: &[ReadLimits]) -> bool {
        let rate: u32 = shares.iter().map(|s| s.requests_per_second).sum();
        let burst: u32 = shares.iter().map(|s| s.burst).sum();
        let concurrent: usize = shares.iter().map(|s| s.max_concurrent).sum();
        rate <= self.ceiling.requests_per_second
            && burst <= self.ceiling.burst
            && concurrent <= self.ceiling.max_concurrent
    }

    /// The configured `get-block` poll interval (§11).
    pub fn block_poll_interval(&self) -> Duration {
        self.block_poll_interval
    }
}
