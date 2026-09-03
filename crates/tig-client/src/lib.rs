//! Rate-limited read client for the TIG API.
//!
//! `docs/tig_integration.md` owns the rules: §5 lists the reads, §11 the
//! limits, timeouts and retry policy.
//!
//! Numerics: §4 requires lossless handling and `accounting.md` §3 forbids
//! floating point. Integers survive this boundary unchanged, because
//! `serde_json` stores them as `u64`/`i64` — and TIG's full-width values
//! (nonce, fuel, counts) are integers while its `PreciseNumber` values are
//! decimal strings.
//!
//! A *non-integral* JSON number would still route through `f64` here. The
//! fix is the pinned models of §1 holding those fields as strings or
//! decimals, which lands with snapshot assembly. It is deliberately NOT
//! `serde_json`'s `arbitrary_precision` feature: cargo unifies features
//! across a workspace build, so enabling it here changes `Value` number
//! round-tripping for every crate — including the RFC 8785 canonical-JSON
//! digests `member_protocol.md` §10.2 and §16 pin for `assignment_digest`
//! and `manifest.json`. Verified: with the feature on, a workspace build
//! re-serialises `1.50` verbatim instead of normalising it to `1.5`.
//!
//! §11's limits are configuration, not compiled constants: [`ReadPolicy`]
//! loads them from `config/tig_integration.json` and there is no default to
//! fall back to (criterion E5).
//!
//! One §11 obligation is still outstanding. §9's per-block caching of each
//! endpoint response by its complete request key landed with snapshot
//! assembly, in `pool-snapshot`; the `Cache-Control` response cache has not.
//! Recorded against the obligation rather than against whichever PR is next,
//! because naming a PR is how this note went stale twice.
//! This crate performs reads only —
//! protocol writes belong to the TIG gateway, which is the sole holder of
//! the API key (`architecture.md` §2.2, invariant 2), and nothing here
//! accepts or stores one.

mod limits;

pub use limits::{PolicyError, ReadLimits, ReadPolicy, TigReader};

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};

/// Every way a read can fail, separated by whether retrying could help.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// Transport failure, 408, 429 or 5xx: retryable, and retried already.
    #[error("{endpoint} failed after {attempts} attempt(s): {reason}")]
    Unavailable {
        endpoint: String,
        attempts: u32,
        reason: String,
    },
    /// 4xx other than 408/429. §11: never retried automatically, because
    /// repeating a request the server has rejected on its contents cannot
    /// succeed and only spends quota.
    #[error("{endpoint} rejected the request ({status}): {body}")]
    Rejected {
        endpoint: String,
        status: u16,
        body: String,
    },
    /// The response was not well-formed JSON.
    ///
    /// Deliberately narrow: at this layer the only check is
    /// well-formedness. Validation against the pinned models of §1 lands
    /// with the snapshot, and until then this variant must not be read as
    /// evidence that a response matched them.
    #[error("{endpoint} returned an unexpected shape: {reason}")]
    Schema { endpoint: String, reason: String },
}

impl ReadError {
    /// Whether a caller may sensibly try again later. Schema and rejection
    /// failures need a human, not a retry.
    pub fn is_transient(&self) -> bool {
        matches!(self, ReadError::Unavailable { .. })
    }
}

/// What one attempt concluded, so the in-flight permit can be released
/// before the decision is acted on.
enum Step {
    Done(String),
    Rejected(u16, String),
    Retry(String),
    /// The call budget ran out while the body was still arriving.
    Expired,
}

/// Token-bucket limiter plus an in-flight ceiling.
#[derive(Debug)]
struct Limiter {
    limits: ReadLimits,
    bucket: Mutex<Bucket>,
    in_flight: Semaphore,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
    /// Set when the server tells us to back off.
    ///
    /// The limiter is shared because the budget is per IP, so a throttle
    /// signal has to pause every reader on the host. Applying `Retry-After`
    /// only to the task that received the 429 leaves the others issuing
    /// GETs at the pinned rate throughout the instructed pause, prolonging
    /// exactly the throttling it asks us to relieve.
    paused_until: Option<Instant>,
}

impl Limiter {
    fn new(limits: ReadLimits) -> Self {
        Self {
            limits,
            bucket: Mutex::new(Bucket {
                tokens: f64::from(limits.burst),
                last_refill: Instant::now(),
                paused_until: None,
            }),
            in_flight: Semaphore::new(limits.max_concurrent),
        }
    }

    /// Record a server-instructed pause for every reader on this host.
    ///
    /// `delay` is bounded by the caller. The receiving call honours the
    /// instruction in full — §11 says to — but the pause this imposes on
    /// OTHER readers is capped, because it is process-global state written
    /// from a server response: an unbounded one lets a single
    /// `Retry-After: 86400`, or a far-future HTTP-date under clock skew,
    /// halt every TIG read for a day, including the `get-block` poll whose
    /// misses §10 makes unrecoverable.
    ///
    /// A reader released early simply meets the throttle again and pauses
    /// again, so the bound costs one wasted request per reader per interval
    /// and cannot compound.
    async fn pause_for(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut bucket = self.bucket.lock().await;
        // Never shorten a pause already in force.
        bucket.paused_until = Some(match bucket.paused_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    }

    /// Wait until a token is available, then take it.
    async fn take_token(&self) {
        loop {
            let wait = {
                let mut bucket = self.bucket.lock().await;
                let now = Instant::now();

                // A server-instructed pause applies to every reader on the
                // host, before any token does.
                if let Some(until) = bucket.paused_until {
                    if until > now {
                        let pause = until - now;
                        drop(bucket);
                        tokio::time::sleep(pause).await;
                        continue;
                    }
                    bucket.paused_until = None;
                }
                let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
                bucket.tokens = (bucket.tokens
                    + elapsed * f64::from(self.limits.requests_per_second))
                .min(f64::from(self.limits.burst));
                bucket.last_refill = now;

                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return;
                }
                let deficit = 1.0 - bucket.tokens;
                Duration::from_secs_f64(deficit / f64::from(self.limits.requests_per_second))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// §11's limiter is shared, so it is keyed by base URL rather than owned per
/// client: two independently constructed clients — the snapshot ingestor and
/// the §5.2 active-benchmark cache filler, say — would otherwise each get a
/// full allowance and together double the pinned rate against TIG's per-IP
/// limit.
///
/// Shared **per process**, not per pool. The controller (§9 snapshots) and
/// the gateway (§13 check 5 pre-write reads, §10 reconciliation) are
/// separate processes, so each holds its own limiter — which is why each is
/// configured with a *share* of the pinned ceiling rather than the whole of
/// it: 1 req/s each against the pool-wide 2 req/s, per ADR-0006. Two clients
/// inside one process share that process's share.
fn shared_limiter(base_url: &str, limits: ReadLimits) -> Result<Arc<Limiter>, String> {
    static REGISTRY: OnceLock<StdMutex<HashMap<String, Arc<Limiter>>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = match registry.lock() {
        Ok(map) => map,
        // A poisoned lock means another thread panicked while registering.
        // The map itself is still consistent, so recover rather than
        // propagate a panic into every subsequent read.
        Err(poisoned) => poisoned.into_inner(),
    };

    // Keyed on the normalised host, not the base-URL string: §11's budget
    // is per IP, so two clients spelling the same host differently — case,
    // an explicit default port, a path suffix — must not each receive a
    // full allowance and silently double the pinned rate.
    let key = limiter_key(base_url);
    if let Some(existing) = map.get(&key) {
        // Fail loudly rather than silently applying someone else's policy.
        // The rate fields live in the shared limiter while the rest live
        // per client, so a mismatch would leave a client reporting limits
        // it is not subject to — with no observable signal.
        let a = existing.limits;
        if a.requests_per_second != limits.requests_per_second
            || a.burst != limits.burst
            || a.max_concurrent != limits.max_concurrent
        {
            return Err(format!(
                "a client for host {key} is already registered with different rate limits \
                 ({} req/s, burst {}, {} concurrent); the limiter is shared per host, so every \
                 client on it must agree",
                a.requests_per_second, a.burst, a.max_concurrent
            ));
        }
        return Ok(Arc::clone(existing));
    }

    let limiter = Arc::new(Limiter::new(limits));
    map.insert(key, Arc::clone(&limiter));
    Ok(limiter)
}

/// Host and port of a base URL, lowercased, with scheme, credentials and
/// path removed and a redundant default port collapsed — the granularity
/// §11's per-IP budget actually applies at.
fn limiter_key(base_url: &str) -> String {
    let (scheme, rest) = base_url
        .split_once("://")
        .map_or(("", base_url), |(s, r)| (s, r));
    let authority = rest
        .split('/')
        .next()
        .unwrap_or(rest)
        .rsplit('@')
        .next()
        .unwrap_or(rest)
        .to_ascii_lowercase();

    // `example.org` and `example.org:443` are the same host to a per-IP
    // limiter. Without this they hash apart and each receive a full
    // allowance — which is what the comment above the call site claimed was
    // already handled.
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" => Some(":443"),
        "http" => Some(":80"),
        _ => None,
    };
    match default_port {
        Some(suffix) => authority
            .strip_suffix(suffix)
            .map(str::to_string)
            .unwrap_or(authority),
        None => authority,
    }
}

/// Deterministic PRNG for backoff jitter.
///
/// Hand-rolled rather than taking a `rand` dependency, but properly: an
/// earlier version used `SystemTime::subsec_nanos() % cap`, which is always
/// under one second, so with §11's pinned 60-second cap every backoff was
/// sub-second and never escalated — turning an escalating retry policy into
/// near-immediate re-hammering of an endpoint that is already failing.
fn jitter_below(cap: Duration, attempt: u32) -> Duration {
    let cap_nanos = cap.as_nanos() as u64;
    if cap_nanos == 0 {
        return Duration::ZERO;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    // xorshift64*, whose output spans the full u64 range so the modulo is
    // uniform over the whole cap rather than a sub-second slice of it.
    let mut x = seed | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let value = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    Duration::from_nanos(value % cap_nanos)
}

/// A read-only TIG API client.
///
/// Holds no credential. Every read is public
/// (`tig_integration.md` §4: "Read endpoints used by the pool are public"),
/// so a client that cannot authenticate is a client that cannot
/// accidentally write.
#[derive(Debug, Clone)]
pub struct TigReadClient {
    base_url: String,
    http: reqwest::Client,
    limiter: Arc<Limiter>,
    limits: ReadLimits,
    /// §11's backoff ceiling, carried from the policy rather than read back
    /// off `limits`. The host-wide `Retry-After` pause is written from a
    /// server response and must be bounded by policy; `limits.max_backoff`
    /// is caller-supplied and outside the entitlement check, so bounding the
    /// pause by it would let one client widen a process-global value.
    pinned_max_backoff: Duration,
}

impl TigReadClient {
    pub fn new(
        base_url: impl Into<String>,
        policy: &ReadPolicy,
        limits: ReadLimits,
    ) -> Result<Self, String> {
        // The rate fields must be a share a reader is entitled to. Without
        // this, a struct literal spelling out the full ceiling bypasses
        // every other guard — removing `Default`, keeping `pool_ceiling`
        // crate-private, gating the testing module — because all of those
        // close named routes while the fields stay public.
        //
        // Under the `testing` feature the ceiling itself is allowed, so the
        // pacing tests can observe the pinned rate rather than one share.
        // Always enforced. Gating this on the `testing` feature made it
        // inert in every test, because the self dev-dependency turns that
        // feature on for the whole test graph — so the one runtime
        // enforcement of ADR-0006 had no coverage and could not have been
        // exercised. Tests that need the full ceiling use
        // `new_unrestricted_for_test` instead, which is an explicit door
        // rather than a disabled guard.
        if !policy.is_entitled_share(&limits) {
            return Err(format!(
                "these limits ({} req/s, burst {}, {} concurrent) are not a reader's share of \
                 the pinned per-IP budget; build them with ReadPolicy::for_reader or \
                 ReadPolicy::for_block_poll (ADR-0006)",
                limits.requests_per_second, limits.burst, limits.max_concurrent
            ));
        }

        Self::build(base_url, policy, limits)
    }

    fn build(
        base_url: impl Into<String>,
        policy: &ReadPolicy,
        limits: ReadLimits,
    ) -> Result<Self, String> {
        // Clamp every pinned duration once, here, rather than at each site
        // that consumes one. These fields are caller-supplied and
        // `is_entitled_share` constrains none of them, so without this a
        // client could sleep past §11's backoff cap, hold a connection past
        // its connect timeout, or run an attempt past the pinned per-attempt
        // budget. A client may be stricter than policy; never looser.
        let limits = policy.clamp(limits);
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(limits.connect_timeout)
            .timeout(limits.attempt_timeout)
            // No redirects. `tig_integration.md` §2.1 requires that failure
            // to reach testnet never redirect elsewhere; following a
            // cross-host redirect would leave that guarantee to the
            // upstream instead of to configuration.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("cannot build HTTP client: {e}"))?;
        Ok(Self {
            limiter: shared_limiter(&base_url, limits)?,
            base_url,
            http,
            limits,
            pinned_max_backoff: policy.pinned_max_backoff(),
        })
    }

    /// Build a client with limits that are not a reader's share.
    ///
    /// For tests that must observe the pinned pool-wide rate rather than one
    /// process's share of it. Behind the `testing` feature, so a production
    /// consumer cannot reach it — and separate from [`Self::new`] so the
    /// entitlement guard there stays live in every build, including test
    /// builds.
    #[cfg(feature = "testing")]
    pub fn new_unrestricted_for_test(
        base_url: impl Into<String>,
        policy: &ReadPolicy,
        limits: ReadLimits,
    ) -> Result<Self, String> {
        Self::build(base_url, policy, limits)
    }

    /// The limits this client actually ended up with, after policy
    /// clamping. Test-only: production code has no reason to read them back.
    #[cfg(feature = "testing")]
    pub fn limits_for_test(&self) -> ReadLimits {
        self.limits
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch and deserialise one read endpoint.
    ///
    /// `path_and_query` is built by the caller from pinned endpoint names;
    /// nothing here interpolates member-supplied input.
    pub async fn get_json(&self, path_and_query: &str) -> Result<serde_json::Value, ReadError> {
        let url = format!(
            "{}/{}",
            self.base_url,
            path_and_query.trim_start_matches('/')
        );
        let endpoint = path_and_query
            .split('?')
            .next()
            .unwrap_or(path_and_query)
            .to_string();

        let started = Instant::now();
        // Every wait in this loop is bounded by what remains of the call
        // budget, not just the HTTP send. Bounding one of them left the
        // others able to carry a call past its deadline — which for the
        // get-block poll means the missed height §10 makes unrecoverable.
        let remaining = |elapsed: Duration| {
            self.limits
                .call_deadline
                .checked_sub(elapsed)
                .unwrap_or(Duration::ZERO)
        };
        let expired = |attempt: u32, endpoint: &str, at: &str| ReadError::Unavailable {
            endpoint: endpoint.to_string(),
            attempts: attempt,
            reason: format!(
                "call deadline of {:?} reached {at} on attempt {attempt}",
                self.limits.call_deadline
            ),
        };
        let mut attempt = 0u32;
        let mut last_reason = String::new();

        while attempt < self.limits.max_attempts {
            attempt += 1;

            // Token first, then the in-flight permit, and the permit is
            // dropped before any wait. §11 caps GETs actually in flight, so
            // a caller waiting on a token or a 60-second Retry-After must
            // not occupy a slot — two such waiters would otherwise stall
            // every other read, including the get-block poll that §9's
            // snapshot assembly and §10's gap detection depend on.
            match tokio::time::timeout(remaining(started.elapsed()), self.limiter.take_token())
                .await
            {
                Ok(()) => {}
                // A shared token bucket under contention — the block poll
                // queued behind the same process's snapshot reads — could
                // otherwise block past the deadline before it was checked.
                Err(_) => {
                    return Err(expired(
                        attempt,
                        &endpoint,
                        "waiting for a rate-limit token",
                    ));
                }
            }

            // The permit spans the send AND the body read: a ceiling on
            // requests "in flight" that released once response headers
            // arrived would let every body — including the large
            // get-benchmark-data payloads §5.2 calls out — stream outside
            // it. It is dropped before any wait, so a caller sitting on a
            // Retry-After still occupies no slot.
            let permit = match tokio::time::timeout(
                remaining(started.elapsed()),
                self.limiter.in_flight.acquire(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(e)) => {
                    return Err(ReadError::Unavailable {
                        endpoint,
                        attempts: attempt,
                        reason: format!("limiter closed: {e}"),
                    });
                }
                // The third wait in this loop, and the one the previous
                // commit missed while claiming to have bounded them all.
                // With a share of one concurrent GET, a large
                // get-benchmark-data body (§5.2) holds the only permit for
                // a whole attempt, and the block poll queued behind it
                // would not consult its deadline until that finished.
                Err(_) => return Err(expired(attempt, &endpoint, "waiting for an in-flight slot")),
            };

            let budget = remaining(started.elapsed());
            if budget.is_zero() {
                drop(permit);
                return Err(expired(attempt, &endpoint, "before the request"));
            }
            let outcome = match tokio::time::timeout(budget, self.http.get(&url).send()).await {
                Ok(result) => result,
                Err(_) => {
                    drop(permit);
                    return Err(expired(attempt, &endpoint, "during the request"));
                }
            };

            // Only consulted on a status that is asking us to back off, and
            // only on one §11 classifies as retryable. "Non-2xx" was too
            // broad: a 404 or a 401 is not a throttle signal, and honouring
            // its header would pause every reader on the host over a
            // request that will never be retried.
            let retry_after = match &outcome {
                Ok(r) if is_retryable_status(r.status()) => parse_retry_after(r.headers()),
                _ => None,
            };
            if let Some(delay) = retry_after {
                // Honoured in full. An earlier version clamped it to the
                // client's own backoff cap, which meant retrying sooner than
                // the server asked — against a per-IP limit that prolongs
                // the throttling the instruction exists to relieve. §11 says
                // to honour it, unqualified.
                //
                // A very long instruction is bounded per CALLER instead: the
                // deadline check below refuses to sleep past a caller's own
                // budget, so a read fails fast and surfaces rather than
                // stalling. Nothing waits longer than it promised to, and
                // nothing retries earlier than it was told to.
                //
                // Pushed into the SHARED limiter, not applied to this call
                // alone: the budget is per IP, so a throttle signal that
                // paused only the receiving task would leave every other
                // reader issuing GETs straight through the instructed wait.
                //
                // Bounded for the others, though. This call obeys `delay` in
                // full via the deadline check below; the host-wide pause is
                // capped so one upstream response cannot disable every read
                // in the process. §11 records the split.
                //
                // Bounded by the POLICY constant rather than this client's
                // own (already clamped) value. The pause is process-global
                // state and two equally entitled clients may hold different
                // backoff ceilings, so taking the receiver's would make the
                // host-wide pause depend on which one happened to see the
                // 429.
                self.limiter
                    .pause_for(delay.min(self.pinned_max_backoff))
                    .await;
            }

            let step = match outcome {
                Ok(r) if r.status().is_success() => {
                    match tokio::time::timeout(remaining(started.elapsed()), r.text()).await {
                        Err(_) => Step::Expired,
                        Ok(Ok(body)) => Step::Done(body),
                        // A body failing mid-transfer is a network failure, and
                        // §11 retries those. Returning here made it a
                        // single-attempt failure that still reported itself as
                        // the retryable variant.
                        Ok(Err(e)) => Step::Retry(format!("cannot read body: {e}")),
                    }
                }
                Ok(r) => {
                    let status = r.status();
                    let body = tokio::time::timeout(remaining(started.elapsed()), r.text())
                        .await
                        .unwrap_or_else(|_| Ok(String::new()))
                        .unwrap_or_default();
                    // §11: retry 408, 429 and 5xx; never other 4xx.
                    if is_retryable_status(status) {
                        Step::Retry(format!("HTTP {status}"))
                    } else {
                        Step::Rejected(status.as_u16(), body.chars().take(200).collect())
                    }
                }
                Err(e) => Step::Retry(e.to_string()),
            };

            drop(permit);

            match step {
                Step::Done(body) => {
                    return serde_json::from_str(&body).map_err(|e| ReadError::Schema {
                        endpoint: endpoint.clone(),
                        reason: format!("not valid JSON: {e}"),
                    });
                }
                Step::Rejected(status, body) => {
                    return Err(ReadError::Rejected {
                        endpoint,
                        status,
                        body,
                    });
                }
                Step::Retry(reason) => last_reason = reason,
                Step::Expired => return Err(expired(attempt, &endpoint, "reading the body")),
            }

            if attempt >= self.limits.max_attempts {
                break;
            }

            // §11: honour Retry-After when present, otherwise full-jitter
            // exponential backoff capped at max_backoff.
            let backoff = retry_after.unwrap_or_else(|| {
                let exponential = Duration::from_millis(
                    250u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1))),
                );
                jitter_below(exponential.min(self.limits.max_backoff), attempt)
            });

            // The backoff itself must fit in what is left.
            if backoff >= remaining(started.elapsed()) {
                return Err(expired(attempt, &endpoint, "before the next backoff"));
            }

            tracing::debug!(
                event = "tig.read.retry",
                endpoint = %endpoint,
                attempt,
                backoff_ms = backoff.as_millis() as u64,
                reason = %last_reason,
            );
            tokio::time::sleep(backoff).await;
        }

        Err(ReadError::Unavailable {
            endpoint,
            attempts: attempt,
            reason: last_reason,
        })
    }
}

/// The statuses §11 classifies as worth retrying: 408, 429 and 5xx.
///
/// Shared by the retry decision and the throttle-signal decision, so the two
/// cannot drift into disagreeing about what counts as "back off".
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

/// `Retry-After` in either legal form: delta-seconds or an HTTP-date.
///
/// The date form is not exotic — a fronting CDN answering a 429 commonly
/// uses it — and silently ignoring it would substitute the client's own,
/// shorter backoff for an instruction the server gave.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .to_string();

    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = httpdate::parse_http_date(&raw).ok()?;
    // Clamp at zero: a date already in the past means "retry now".
    Some(
        when.duration_since(std::time::SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// Helpers exposed for tests that need to observe internal policy directly.
///
/// Behind the `testing` feature, which no production consumer enables:
/// `pool_ceiling_for_test` returns the whole per-IP allowance, and an
/// unconditionally compiled module offering it would undo ADR-0006's
/// requirement that a reader name itself and take a share.
///
/// The backoff distribution cannot be asserted through the public API
/// without waiting out real sleeps, and the range that matters (§11's
/// 60-second cap) is exactly the range a timing test cannot afford.
#[cfg(feature = "testing")]
pub mod testing {
    use std::time::Duration;

    /// See [`super::jitter_below`].
    pub fn jitter_below_for_test(cap: Duration, attempt: u32) -> Duration {
        super::jitter_below(cap, attempt)
    }

    /// See [`super::limiter_key`].
    pub fn limiter_key_for_test(base_url: &str) -> String {
        super::limiter_key(base_url)
    }

    /// The effective backoff ceiling a client ends up with.
    ///
    /// Asserted directly rather than through timing: a test that waits and
    /// checks the clock can be satisfied by an outer timeout and prove
    /// nothing, which is how the first version of this coverage passed
    /// regardless of the clamp.
    pub fn effective_max_backoff_for_test(
        base_url: &str,
        policy: &crate::ReadPolicy,
        limits: crate::ReadLimits,
    ) -> Result<Duration, String> {
        // Returns a Result rather than expecting: the workspace forbids
        // `expect` outside test files, and a helper in `src/` is not one.
        Ok(
            crate::TigReadClient::new_unrestricted_for_test(base_url, policy, limits)?
                .limits
                .max_backoff,
        )
    }

    /// The bound applied to a host-wide `Retry-After` pause.
    ///
    /// Asserted as a value because the real one is 60 seconds — too long to
    /// wait out in a test, and a timing assertion at that scale would be
    /// measuring the test's own patience rather than the bound.
    pub fn host_pause_bound_for_test(policy: &crate::ReadPolicy) -> Duration {
        policy.pinned_max_backoff()
    }

    /// The pinned per-IP ceiling.
    ///
    /// Exposed for tests only: production readers take a share via
    /// `ReadPolicy::for_reader`, and a public accessor returning the whole
    /// allowance would undo ADR-0006's requirement that a reader name
    /// itself.
    pub fn pool_ceiling_for_test(policy: &crate::ReadPolicy) -> crate::ReadLimits {
        policy.ceiling()
    }

    /// The policy as actually shipped in `config/tig_integration.json`.
    ///
    /// Tests run against the real file rather than a hand-written fixture,
    /// so the pinned §11 values are covered as configured. A fixture would
    /// let the shipped config drift to something no test ever loads — which
    /// is the whole failure mode criterion E5 exists to close.
    pub fn shipped_policy_for_test() -> Result<crate::ReadPolicy, crate::PolicyError> {
        crate::ReadPolicy::from_config_json(include_str!("../../../config/tig_integration.json"))
    }
}
