//! Persisting an accepted snapshot (`docs/tig_integration.md` §9 step 7).
//!
//! Step 7 is an *ordering* rule: the accepted snapshot and its completeness
//! status reach durable storage before anything derived from them. A comment
//! cannot enforce an ordering, so the order is expressed in the types —
//! [`PersistedSnapshot`] has no public constructor, only a
//! [`BlockSnapshotStore`] produces one, and it is the only thing that hands
//! out a [`Snapshot`] for a decision. Code that skipped persistence has
//! nothing to pass.
//!
//! What is stored is compact. `architecture.md` §3 excludes "an unbounded
//! copy of TIG responses" from the Workflow Database, so the row is the
//! block anchor, the completeness status and a digest — not the response
//! bodies. The assembled snapshot stays in memory for the decision that
//! follows it.

use std::future::Future;

use pool_domain::Network;
use sha2::{Digest, Sha256};

use crate::Snapshot;

/// The compact facts a persisted snapshot leaves behind.
///
/// One accepted assembly. A block may have several — a partial assembly is
/// superseded by a later, better one rather than edited — of which at most
/// one is usable for a decision.
///
/// Deliberately not enough to make a decision from. §10 restart
/// reconciliation re-assembles at the current block rather than reviving a
/// stale one, so a record that could be mistaken for a decision input would
/// invite exactly the stale read it exists to rule out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub network: Network,
    pub block_id: String,
    pub height: u64,
    pub reads_complete: bool,
    pub active_cache_ready: bool,
    pub content_digest: [u8; 32],
}

/// An accepted snapshot that has reached durable storage together with its
/// completeness status.
///
/// Constructed only by a [`BlockSnapshotStore`] implementation.
#[derive(Debug, Clone)]
pub struct PersistedSnapshot {
    snapshot: Snapshot,
    record: SnapshotRecord,
}

impl PersistedSnapshot {
    /// Only callable from inside this crate, so persistence cannot be
    /// asserted by a caller that did not perform it.
    pub(crate) fn new(snapshot: Snapshot, record: SnapshotRecord) -> Self {
        Self { snapshot, record }
    }

    pub fn record(&self) -> &SnapshotRecord {
        &self.record
    }

    /// The §9 / criterion C5 gate: a snapshot may be read for a decision
    /// only when its reads are complete *and* the active-benchmark cache is
    /// ready.
    ///
    /// Returning `Result` rather than exposing the snapshot with the flags
    /// alongside it keeps "is this usable" from being a check a caller can
    /// forget: there is no path to the data that does not go through it.
    pub fn for_decision(&self) -> Result<&Snapshot, NotUsable> {
        if !self.snapshot.reads_complete {
            return Err(NotUsable::ReadsIncomplete);
        }
        if !self.snapshot.active_cache_ready {
            return Err(NotUsable::ActiveCacheUnavailable);
        }
        Ok(&self.snapshot)
    }
}

/// Why a persisted snapshot cannot be used for a decision.
///
/// The two cases stay separate because they call for different operator
/// action: incomplete reads clear on the next block, an unavailable active
/// cache does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NotUsable {
    #[error("the snapshot's reads are incomplete")]
    ReadsIncomplete,
    #[error("the active-benchmark metadata cache is not ready")]
    ActiveCacheUnavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store could not be reached or the statement failed. Retryable.
    #[error("snapshot store unavailable: {0}")]
    Unavailable(String),
    /// A *different* snapshot is already accepted for this block.
    ///
    /// The block's data is immutable, so two *usable* assemblies of one
    /// block that disagree are a contradiction — an operator question, not a
    /// retry. This is deliberately narrow: re-persisting identical content
    /// after a crash is idempotent, and a partial assembly superseded by a
    /// better one is the normal path §9 step 6 and §5.2 describe, not a
    /// divergence. Treating supersession as divergence would leave a block
    /// that was first assembled incompletely unable to ever gain a usable
    /// record.
    #[error("a different snapshot is already accepted for {network} block {block_id}")]
    Divergent { network: Network, block_id: String },
    /// A stored row could not be read back into a record.
    #[error("stored snapshot for block {block_id} is unreadable: {reason}")]
    Corrupt { block_id: String, reason: String },
}

/// The `BlockSnapshotStore` port of `architecture.md` §4.
pub trait BlockSnapshotStore {
    /// Persist the accepted snapshot and its completeness status as one
    /// atomic act, returning the proof a decision needs.
    fn persist(
        &self,
        network: Network,
        snapshot: Snapshot,
    ) -> impl Future<Output = Result<PersistedSnapshot, StoreError>> + Send;

    /// The record for the block's decision-usable snapshot, if one exists.
    ///
    /// Partial assemblies of the same block are recorded but never returned
    /// here: none of them may reach a decision, and at most one usable
    /// snapshot per block exists by construction.
    fn load_usable_record(
        &self,
        network: Network,
        block_id: &str,
    ) -> impl Future<Output = Result<Option<SnapshotRecord>, StoreError>> + Send;
}

/// SHA-256 over the assembled snapshot, binding every field an accepted
/// snapshot is made of.
///
/// Determinism rests on `serde_json::Map` being a `BTreeMap` — key order is
/// sorted, not insertion order — which holds while serde_json's
/// `preserve_order` feature is off. It is off, and
/// `digest_ignores_key_insertion_order` below fails if that ever changes,
/// because the symptom otherwise is every re-persist reporting `Divergent`
/// against its own earlier write.
pub fn content_digest(snapshot: &Snapshot) -> Result<[u8; 32], StoreError> {
    let payload = serde_json::json!({
        "block_id": snapshot.block_id,
        "height": snapshot.height,
        "block": snapshot.block,
        "reads": snapshot.reads,
        "tracks": snapshot.tracks,
        "reads_complete": snapshot.reads_complete,
        "active_cache_ready": snapshot.active_cache_ready,
    });
    // Serializing a `Value` cannot fail in practice — its numbers exclude
    // NaN and its keys are strings — but the error is mapped rather than
    // unwrapped so a future payload change cannot turn into a panic in the
    // persistence path.
    let bytes = serde_json::to_vec(&payload).map_err(|e| StoreError::Corrupt {
        block_id: snapshot.block_id.clone(),
        reason: format!("cannot canonicalize for digest: {e}"),
    })?;
    Ok(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::BTreeMap;

    fn snapshot(reads_complete: bool, active_cache_ready: bool) -> Snapshot {
        Snapshot {
            block_id: "block-a".to_string(),
            height: 100,
            block: serde_json::json!({ "id": "block-a" }),
            reads: BTreeMap::new(),
            tracks: BTreeMap::new(),
            reads_complete,
            active_cache_ready,
        }
    }

    fn persisted(snapshot: Snapshot) -> PersistedSnapshot {
        let record = SnapshotRecord {
            network: Network::Testnet,
            block_id: snapshot.block_id.clone(),
            height: snapshot.height,
            reads_complete: snapshot.reads_complete,
            active_cache_ready: snapshot.active_cache_ready,
            content_digest: content_digest(&snapshot).expect("digest"),
        };
        PersistedSnapshot::new(snapshot, record)
    }

    #[test]
    fn an_incomplete_snapshot_yields_no_decision_input() {
        // C5: the orchestrator does no work while the snapshot is incomplete
        // or the active cache is unavailable. Each half is asserted on its
        // own, because a gate that only tested the pair would pass while
        // reading just one of them.
        assert_eq!(
            persisted(snapshot(false, true)).for_decision().unwrap_err(),
            NotUsable::ReadsIncomplete
        );
        assert_eq!(
            persisted(snapshot(true, false)).for_decision().unwrap_err(),
            NotUsable::ActiveCacheUnavailable
        );
        assert_eq!(
            persisted(snapshot(false, false))
                .for_decision()
                .unwrap_err(),
            NotUsable::ReadsIncomplete
        );
        assert!(persisted(snapshot(true, true)).for_decision().is_ok());
    }

    #[test]
    fn the_digest_covers_the_completeness_status_and_the_content() {
        // If the digest ignored a field, a snapshot differing only in that
        // field would re-persist as "the same snapshot" and the accepted row
        // would silently describe content that was never accepted.
        let base = content_digest(&snapshot(true, true)).expect("digest");
        assert_ne!(
            base,
            content_digest(&snapshot(false, true)).expect("digest")
        );
        assert_ne!(
            base,
            content_digest(&snapshot(true, false)).expect("digest")
        );

        let mut different_reads = snapshot(true, true);
        different_reads
            .reads
            .insert("get-opow".to_string(), serde_json::json!({ "a": 1 }));
        assert_ne!(base, content_digest(&different_reads).expect("digest"));

        let mut different_tracks = snapshot(true, true);
        different_tracks
            .tracks
            .insert("c001".to_string(), serde_json::json!({ "b": 2 }));
        assert_ne!(base, content_digest(&different_tracks).expect("digest"));

        let mut different_height = snapshot(true, true);
        different_height.height = 101;
        assert_ne!(base, content_digest(&different_height).expect("digest"));
    }

    #[test]
    fn digest_ignores_key_insertion_order() {
        // Guards the assumption content_digest documents. With serde_json's
        // `preserve_order` on, these two parse to different key orders and
        // digest differently — which would make every crash-retry re-persist
        // look like a divergent snapshot rather than an idempotent one.
        let mut first = snapshot(true, true);
        first.block = serde_json::from_str(r#"{"a":1,"b":2,"c":3}"#).expect("json");
        let mut second = snapshot(true, true);
        second.block = serde_json::from_str(r#"{"c":3,"b":2,"a":1}"#).expect("json");
        assert_eq!(
            content_digest(&first).expect("digest"),
            content_digest(&second).expect("digest")
        );
    }
}
