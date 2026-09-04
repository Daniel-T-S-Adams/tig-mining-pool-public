//! The `Cache-Control` response cache of `docs/tig_integration.md` §11.
//!
//! §11: "The live `Cache-Control` header is respected. Block-addressed
//! responses and immutable confirmed benchmark facts are cached as described
//! above; errors and incomplete snapshots are not cached as successful
//! data."
//!
//! This is the *transport* half of §11's caching rule. §9's per-block cache,
//! which keys each endpoint response by its complete request key within one
//! block, lives in `pool-snapshot` and is a different thing: that one exists
//! so a snapshot cannot mix blocks, this one exists so the pool does not
//! re-ask TIG for something TIG said would not change yet.
//!
//! **Two permissions are needed, not one.** §11 scopes caching to
//! "block-addressed responses and immutable confirmed benchmark facts", and
//! the server must additionally allow it with a `Cache-Control` lifetime. A
//! response with no `Cache-Control` is not cached, because inventing a
//! lifetime for a live protocol value is how §12's rule against compiled
//! constants gets broken by the back door — and a request outside §11's
//! scope is not cached even when the server says it may be.
//!
//! The scope rule is the load-bearing one. `get-block?include_data=true`
//! returns the LATEST block, not a block-addressed response, and §9 issues
//! the identical URL for a snapshot's opening and closing read. Caching it
//! would make the closing read return the opening body, so `closed_id !=
//! opened_id` could never fire and a snapshot spanning a block boundary
//! would be accepted as block-consistent — and the §11 poll would freeze,
//! skipping the heights §10 records as an unrecoverable gap.
//! `pool-snapshot` already says as much at its `get_block`; this is the
//! other half of that claim.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What a response's `Cache-Control` header permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caching {
    /// Store it for this long.
    For(Duration),
    /// Do not store it: `no-store`, `no-cache`, `max-age=0`, an unparseable
    /// directive, or no header at all.
    Never,
}

/// Parse the caching decision out of a `Cache-Control` header value.
///
/// Anything not understood is [`Caching::Never`]. A cache that guessed on a
/// directive it could not parse would serve a stale protocol value, and §12
/// makes a stale live value worse than an extra request.
pub(crate) fn caching_from_header(header: Option<&str>) -> Caching {
    let Some(header) = header else {
        return Caching::Never;
    };
    let lowered = header.to_ascii_lowercase();

    // `no-store` and `no-cache` both mean "do not serve this again without
    // asking". Revalidation is not implemented, so `no-cache` is treated as
    // uncacheable rather than as cacheable-with-revalidation.
    for directive in lowered.split(',').map(str::trim) {
        if directive == "no-store" || directive == "no-cache" {
            return Caching::Never;
        }
    }

    for directive in lowered.split(',').map(str::trim) {
        if let Some(value) = directive.strip_prefix("max-age=") {
            return match value.trim().parse::<u64>() {
                Ok(0) | Err(_) => Caching::Never,
                Ok(seconds) => Caching::For(Duration::from_secs(seconds)),
            };
        }
    }

    Caching::Never
}

/// Whether §11 permits caching this request at all, before the server is
/// consulted.
///
/// An explicit ENDPOINT allow-list, not a test for a `block_id` parameter.
/// Carrying a block anchor does not make a response block-pinned: §5 defines
/// `get-benchmarks` as the latest 120-block window and `get-tracks-data` as
/// records "from the latest 120 blocks", both of which change under a fixed
/// URL inside one block. Only these three are configuration and state AS OF
/// the named block — which is what makes §9's anchored reads
/// block-consistent in the first place.
///
/// Deliberately excluded, each for a reason worth stating:
///
/// * `get-block` — names no block; §9 compares its opening and closing read
///   and a cached body makes step 6 a tautology.
/// * `get-benchmarks` — §5's latest 120-block window, and §7's sole
///   authority for every lifecycle confirmation. A stale hit makes §10's
///   lost-precommit candidate search report no candidate and licenses the
///   resubmission §10 forbids.
/// * `get-player-data` — §12 lists the pool fee balance among the live
///   values; a cached one lets an affordability check pass against a balance
///   an earlier write already spent.
/// * `get-tracks-data`, `get-benchmark-data` — a rolling window and a record
///   that changes as a benchmark progresses.
///
/// A new endpoint is uncacheable until it is added here with a reason. That
/// costs a request; the opposite mistake costs block-consistency or a
/// duplicated write.
pub(crate) fn request_is_cacheable(path_and_query: &str) -> bool {
    const BLOCK_PINNED: &[&str] = &["get-challenges", "get-algorithms", "get-opow"];

    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path_and_query, ""),
    };
    let path = path.trim_start_matches('/');
    if !BLOCK_PINNED.contains(&path) {
        return false;
    }

    // Per parameter, not a substring: `notblock_id=x` names no block, and an
    // empty `block_id=` names none either.
    query.split('&').any(|p| {
        p.strip_prefix("block_id=")
            .is_some_and(|value| !value.is_empty())
    })
}

#[derive(Debug)]
struct Entry {
    body: String,
    expires_at: Instant,
}

/// A bounded per-client cache of successful responses.
///
/// Bounded because the workflow database excludes "an unbounded copy of TIG
/// responses" (`architecture.md` §3) and the same reasoning applies to
/// memory: a per-URL cache with no ceiling grows with every distinct
/// block-anchored request, and those are distinct for every block.
#[derive(Debug)]
pub(crate) struct ResponseCache {
    entries: Mutex<HashMap<String, Entry>>,
    capacity: usize,
}

impl ResponseCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// The stored body for `url`, if one is still live.
    pub(crate) fn get(&self, url: &str, now: Instant) -> Option<String> {
        let mut entries = self.lock();
        match entries.get(url) {
            Some(entry) if entry.expires_at > now => Some(entry.body.clone()),
            Some(_) => {
                // Expired. Dropped on the way past rather than left to the
                // capacity sweep, so a stale body cannot be served by a
                // later clock comparison going the other way.
                entries.remove(url);
                None
            }
            None => None,
        }
    }

    /// Store a successful response for as long as the server allowed.
    pub(crate) fn put(&self, url: &str, body: &str, caching: Caching, now: Instant) {
        let Caching::For(lifetime) = caching else {
            return;
        };
        let mut entries = self.lock();

        // Evict expired entries first, then — if still full — refuse rather
        // than evict a live one. A cache that discarded live entries under
        // pressure would make the hit rate depend on unrelated traffic;
        // declining to store is honest and costs one request.
        if entries.len() >= self.capacity {
            entries.retain(|_, e| e.expires_at > now);
        }
        if entries.len() >= self.capacity && !entries.contains_key(url) {
            return;
        }

        // Checked rather than added: a `max-age` large enough to overflow
        // the clock would panic, and a header value is not this process's to
        // trust. Declining to cache costs one request.
        let Some(expires_at) = now.checked_add(lifetime) else {
            return;
        };
        entries.insert(
            url.to_string(),
            Entry {
                body: body.to_string(),
                expires_at,
            },
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A poisoned cache is recoverable: the worst outcome is a stale-free
        // extra request, and propagating the panic would take down a reader
        // over a cache.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn only_an_explicit_lifetime_is_cacheable() {
        assert_eq!(caching_from_header(None), Caching::Never);
        assert_eq!(caching_from_header(Some("")), Caching::Never);
        assert_eq!(caching_from_header(Some("no-store")), Caching::Never);
        assert_eq!(caching_from_header(Some("no-cache")), Caching::Never);
        assert_eq!(caching_from_header(Some("max-age=0")), Caching::Never);
        // Not understood is not cacheable: guessing would serve a stale
        // protocol value.
        assert_eq!(caching_from_header(Some("max-age=soon")), Caching::Never);
        assert_eq!(caching_from_header(Some("private")), Caching::Never);

        assert_eq!(
            caching_from_header(Some("max-age=30")),
            Caching::For(Duration::from_secs(30))
        );
        assert_eq!(
            caching_from_header(Some("public, max-age=15")),
            Caching::For(Duration::from_secs(15))
        );
        assert_eq!(
            caching_from_header(Some("MAX-AGE=15")),
            Caching::For(Duration::from_secs(15))
        );
    }

    #[test]
    fn no_store_wins_over_a_lifetime_in_the_same_header() {
        // Order must not decide it. A header carrying both is contradictory,
        // and the safe reading is the restrictive one.
        assert_eq!(
            caching_from_header(Some("max-age=60, no-store")),
            Caching::Never
        );
        assert_eq!(
            caching_from_header(Some("no-store, max-age=60")),
            Caching::Never
        );
    }

    #[test]
    fn only_the_three_block_pinned_reads_are_cacheable() {
        assert!(request_is_cacheable("get-challenges?block_id=b1"));
        assert!(request_is_cacheable("get-algorithms?block_id=b1"));
        assert!(request_is_cacheable("get-opow?block_id=b1"));
        assert!(request_is_cacheable("/get-opow?block_id=b1"));
    }

    #[test]
    fn the_latest_block_is_never_cacheable() {
        // §9 issues this identical URL for a snapshot's opening and closing
        // read. Caching it makes step 6's comparison a tautology.
        assert!(!request_is_cacheable("get-block?include_data=true"));
        assert!(!request_is_cacheable("get-block"));
        assert!(!request_is_cacheable("/get-block?include_data=true"));
    }

    #[test]
    fn a_block_anchor_does_not_make_a_rolling_window_cacheable() {
        // The mistake this allow-list exists to prevent. All of these carry
        // a block_id and none of them is block-pinned.
        //
        // get-benchmarks is §5's latest 120-block window and §7's sole
        // authority for lifecycle confirmation: a stale hit makes §10's
        // lost-precommit search find no candidate and licenses the
        // resubmission §10 forbids.
        assert!(!request_is_cacheable(
            "get-benchmarks?block_id=b1&player_id=p1"
        ));
        // §12 lists the pool fee balance among the live values.
        assert!(!request_is_cacheable(
            "get-player-data?block_id=b1&player_id=p1"
        ));
        // Records "from the latest 120 blocks" (§5).
        assert!(!request_is_cacheable(
            "get-tracks-data?block_id=b1&challenge_id=c1"
        ));
        // Changes as the benchmark progresses.
        assert!(!request_is_cacheable("get-benchmark-data?benchmark_id=x"));
        assert!(!request_is_cacheable("get-binary-blob?algorithm_id=a1"));
    }

    #[test]
    fn a_parameter_that_merely_contains_block_id_is_not_a_block_anchor() {
        // Matched per parameter, not as a substring.
        assert!(!request_is_cacheable("get-challenges?notblock_id=b1"));
        assert!(!request_is_cacheable("get-challenges?other_block_id=b1"));
        assert!(!request_is_cacheable("get-challenges?block_id="));
        assert!(!request_is_cacheable("get-challenges"));
    }

    #[test]
    fn an_overflowing_lifetime_declines_rather_than_panics() {
        let cache = ResponseCache::new(4);
        let now = Instant::now();
        cache.put(
            "u",
            "body",
            Caching::For(Duration::from_secs(u64::MAX)),
            now,
        );
        assert_eq!(cache.get("u", now), None);
    }

    #[test]
    fn an_expired_entry_is_not_served() {
        let cache = ResponseCache::new(4);
        let now = Instant::now();
        cache.put("u", "body", Caching::For(Duration::from_secs(10)), now);
        assert_eq!(cache.get("u", now), Some("body".to_string()));
        assert_eq!(cache.get("u", now + Duration::from_secs(11)), None);
        // And it is gone, not merely hidden.
        assert_eq!(cache.get("u", now), None);
    }

    #[test]
    fn an_uncacheable_response_is_not_stored() {
        let cache = ResponseCache::new(4);
        let now = Instant::now();
        cache.put("u", "body", Caching::Never, now);
        assert_eq!(cache.get("u", now), None);
    }

    #[test]
    fn capacity_is_bounded_and_live_entries_are_kept() {
        let cache = ResponseCache::new(2);
        let now = Instant::now();
        let ttl = Caching::For(Duration::from_secs(10));
        cache.put("a", "1", ttl, now);
        cache.put("b", "2", ttl, now);
        cache.put("c", "3", ttl, now);

        // The two live entries survive; the third is declined rather than
        // evicting one of them.
        assert_eq!(cache.get("a", now), Some("1".to_string()));
        assert_eq!(cache.get("b", now), Some("2".to_string()));
        assert_eq!(cache.get("c", now), None);
    }

    #[test]
    fn an_expired_entry_makes_room() {
        let cache = ResponseCache::new(1);
        let now = Instant::now();
        cache.put("a", "1", Caching::For(Duration::from_secs(1)), now);
        let later = now + Duration::from_secs(2);
        cache.put("b", "2", Caching::For(Duration::from_secs(10)), later);
        assert_eq!(cache.get("b", later), Some("2".to_string()));
    }
}
