//! The production [`SnapshotSource`]: the TIG read client plus the per-block
//! cache §9 requires.
//!
//! Kept separate from the algorithm so assembly can be driven through cases
//! a real server will not produce on demand, and so the cache sits on the
//! one path that talks to TIG rather than being a discipline each caller has
//! to remember.

use crate::{AnchoredRead, BlockCache, SnapshotError, SnapshotSource};

use tig_client::{ReadError, TigReadClient};

/// Reads TIG through the rate-limited client, caching each response by its
/// complete request key for the life of one block.
#[derive(Debug)]
pub struct TigSnapshotSource {
    client: TigReadClient,
    cache: BlockCache,
    /// The pool's own address, needed by the player-data read.
    player_id: String,
}

impl TigSnapshotSource {
    pub fn new(client: TigReadClient, player_id: impl Into<String>) -> Self {
        Self {
            client,
            cache: BlockCache::new(),
            player_id: player_id.into(),
        }
    }

    /// Entries currently cached for `block_id`. Test-facing, so a test can
    /// assert the cache is used rather than inferring it from timing.
    pub fn cached_for(&self, block_id: &str) -> usize {
        self.cache.len_for(block_id)
    }

    /// The complete request key: endpoint plus every parameter that changes
    /// the response. Keying on the endpoint alone would let one player's
    /// data serve for another's.
    fn request_key(&self, read: AnchoredRead, block_id: &str) -> String {
        match read {
            // Both are player-scoped. §5 pins get-benchmarks as
            // `?block_id=…&player_id=…`, and it is the sole authority for
            // every §7 confirmation — issued unscoped it returns either
            // nothing (so workflows never advance) or another player's
            // records, which §8 makes a fault-attribution hazard.
            AnchoredRead::PlayerData | AnchoredRead::Benchmarks => format!(
                "{}?player_id={}&block_id={}",
                read.endpoint(),
                self.player_id,
                block_id
            ),
            _ => format!("{}?block_id={}", read.endpoint(), block_id),
        }
    }

    fn tracks_key(&self, challenge_id: &str, block_id: &str) -> String {
        format!("get-tracks-data?challenge_id={challenge_id}&block_id={block_id}")
    }
}

fn to_snapshot_error(endpoint: &str, e: ReadError) -> SnapshotError {
    // A transport failure is retryable and becomes an incomplete read.
    if e.is_transient() {
        return SnapshotError::Unavailable {
            endpoint: endpoint.to_string(),
            reason: e.to_string(),
        };
    }
    // A 4xx that says the named block is no longer latest is §9's refresh
    // conflict — "not partial success" — and is how a real mid-assembly
    // advance actually surfaces, since §8 requires anchored reads to name
    // the latest block. Classifying it as a schema fault aborts the exact
    // case the algorithm exists for, before the closing get-block could
    // ever notice.
    if is_refresh_conflict(&e) {
        return SnapshotError::RefreshConflict {
            endpoint: endpoint.to_string(),
            reason: e.to_string(),
        };
    }
    SnapshotError::Shape {
        endpoint: endpoint.to_string(),
        reason: e.to_string(),
    }
}

/// Whether a rejection is §9's refresh conflict rather than a real fault.
///
/// Matched on the message because TIG returns 400 for both, and §9 names
/// the three shapes it treats this way: a stale block, inconsistent
/// required IDs, and a different snapshot.
fn is_refresh_conflict(e: &ReadError) -> bool {
    let ReadError::Rejected { body, .. } = e else {
        return false;
    };
    let body = body.to_ascii_lowercase();
    // Deliberately narrow. §9 names three shapes, and anything else stays a
    // fatal Shape error: misclassifying an unrelated validation failure as a
    // refresh conflict would make assembly retry a request that can never
    // succeed, burning the attempt budget and reporting Outpaced — which
    // reads as a fast-moving chain rather than the malformed request it is.
    body.contains("block must be latest")
        || body.contains("inconsistent required ids")
        || body.contains("different snapshot")
}

impl SnapshotSource for TigSnapshotSource {
    async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
        // Never cached: it is the thing being checked for change. Serving a
        // cached block would make the closing read agree with the opening
        // one by construction, turning §9 step 6 into a tautology.
        self.client
            .get_json("get-block?include_data=true")
            .await
            .map_err(|e| to_snapshot_error("get-block", e))
    }

    async fn get_anchored(
        &self,
        read: AnchoredRead,
        block_id: &str,
    ) -> Result<serde_json::Value, SnapshotError> {
        let key = self.request_key(read, block_id);
        if let Some(hit) = self.cache.get(block_id, &key) {
            return Ok(hit);
        }
        let value = self
            .client
            .get_json(&key)
            .await
            .map_err(|e| to_snapshot_error(read.endpoint(), e))?;
        self.cache.put(block_id, &key, value.clone());
        Ok(value)
    }

    async fn get_tracks(
        &self,
        challenge_id: &str,
        block_id: &str,
    ) -> Result<serde_json::Value, SnapshotError> {
        let key = self.tracks_key(challenge_id, block_id);
        if let Some(hit) = self.cache.get(block_id, &key) {
            return Ok(hit);
        }
        let value = self
            .client
            .get_json(&key)
            .await
            .map_err(|e| to_snapshot_error("get-tracks-data", e))?;
        self.cache.put(block_id, &key, value.clone());
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(body: &str) -> ReadError {
        ReadError::Rejected {
            endpoint: "get-challenges".to_string(),
            status: 400,
            body: body.to_string(),
        }
    }

    #[test]
    fn a_stale_block_rejection_is_a_refresh_conflict() {
        let mapped = to_snapshot_error("get-challenges", rejected("block must be latest"));
        assert!(
            matches!(mapped, SnapshotError::RefreshConflict { .. }),
            "got {mapped}"
        );
    }

    #[test]
    fn an_unrelated_rejection_stays_fatal() {
        // Retrying a request the server refused on its contents cannot
        // succeed; it would burn the attempt budget and surface as Outpaced,
        // which reads as a fast chain rather than a malformed request.
        let mapped = to_snapshot_error("get-challenges", rejected("missing challenge_id"));
        assert!(
            matches!(mapped, SnapshotError::Shape { .. }),
            "got {mapped}"
        );
    }

    #[test]
    fn a_transport_failure_is_unavailable_not_a_conflict() {
        let mapped = to_snapshot_error(
            "get-opow",
            ReadError::Unavailable {
                endpoint: "get-opow".to_string(),
                attempts: 3,
                reason: "connection reset".to_string(),
            },
        );
        assert!(
            matches!(mapped, SnapshotError::Unavailable { .. }),
            "got {mapped}"
        );
    }

    #[test]
    fn a_schema_failure_is_not_a_conflict() {
        let mapped = to_snapshot_error(
            "get-block",
            ReadError::Schema {
                endpoint: "get-block".to_string(),
                reason: "not valid JSON".to_string(),
            },
        );
        assert!(
            matches!(mapped, SnapshotError::Shape { .. }),
            "got {mapped}"
        );
    }
}
