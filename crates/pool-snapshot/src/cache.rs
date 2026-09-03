//! Per-block response cache.
//!
//! `docs/tig_integration.md` §9: "Within one block, each endpoint response is
//! cached by its complete request key. No component may independently
//! refetch and substitute one field into an already accepted snapshot."
//!
//! Both halves matter, and the second is the reason this is a cache keyed by
//! block rather than a time-based one: substituting a fresher value into an
//! accepted snapshot is precisely how a block-consistent view stops being
//! consistent, and a TTL cache would make that the normal case rather than a
//! mistake.

use std::collections::HashMap;
use std::sync::Mutex;

/// Responses for one block, dropped whole when the block changes.
///
/// Dropped whole rather than evicted per entry: a cache holding some entries
/// from the previous block and some from the current one is the mixed-block
/// state §9 exists to prevent.
#[derive(Debug, Default)]
pub struct BlockCache {
    inner: Mutex<Option<Entry>>,
}

#[derive(Debug)]
struct Entry {
    block_id: String,
    responses: HashMap<String, serde_json::Value>,
}

impl BlockCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached response for `request_key` at `block_id`, if any.
    ///
    /// The key is the complete request — endpoint and parameters — because
    /// two calls to the same endpoint with different parameters are
    /// different responses, and keying on the endpoint alone would serve one
    /// for the other.
    pub fn get(&self, block_id: &str, request_key: &str) -> Option<serde_json::Value> {
        let guard = self.lock();
        let entry = guard.as_ref()?;
        if entry.block_id != block_id {
            return None;
        }
        entry.responses.get(request_key).cloned()
    }

    pub fn put(&self, block_id: &str, request_key: &str, value: serde_json::Value) {
        let mut guard = self.lock();
        match guard.as_mut() {
            Some(entry) if entry.block_id == block_id => {
                entry.responses.insert(request_key.to_string(), value);
            }
            // A different block: the previous block's responses go entirely.
            _ => {
                let mut responses = HashMap::new();
                responses.insert(request_key.to_string(), value);
                *guard = Some(Entry {
                    block_id: block_id.to_string(),
                    responses,
                });
            }
        }
    }

    /// How many responses are held for `block_id`. Zero for any other block.
    pub fn len_for(&self, block_id: &str) -> usize {
        let guard = self.lock();
        match guard.as_ref() {
            Some(entry) if entry.block_id == block_id => entry.responses.len(),
            _ => 0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Entry>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            // The cache holds no invariant a panic could corrupt: recovering
            // beats poisoning every later read.
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
