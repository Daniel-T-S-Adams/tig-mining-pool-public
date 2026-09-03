//! §9's assembly algorithm, driven through the cases that matter.
//!
//! The important one — a block advancing between the opening and closing
//! reads — cannot be provoked against a real server on demand, which is why
//! assembly is written against a port rather than the HTTP client.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use pool_snapshot::{AnchoredRead, BlockCache, Snapshot, SnapshotError, SnapshotSource, assemble};
use serde_json::json;

/// A well-shaped block: §9 step 2 requires the config and the ID sets, not
/// only the identity, because §7 makes `active_ids` and `confirmed_ids`
/// authoritative and an absent set reads downstream as "nothing is active".
fn block(id: &str, height: u64) -> serde_json::Value {
    json!({
        "block": {
            "id": id,
            // §5's required get-block data in full: a helper that omitted
            // round or timestamp would make every test in this file assert
            // against a block TIG never sends.
            "details": {
                "height": height,
                "prev_block_id": "block-prev",
                "round": 834,
                "timestamp": 1_753_900_000u64,
            },
            "config": { "era": 1 },
            "data": {
                "confirmed_ids": { "benchmark": [] },
                "active_ids": { "benchmark": [] },
            },
        }
    })
}

/// A source whose block can be made to advance at a chosen point.
struct StagedSource {
    /// Block ids handed out by successive `get_block` calls.
    blocks: Mutex<Vec<String>>,
    block_calls: AtomicU32,
    anchored_calls: Mutex<Vec<(AnchoredRead, String)>>,
    tracks_calls: Mutex<Vec<(String, String)>>,
    /// Reads that should report themselves unavailable.
    unavailable: Vec<AnchoredRead>,
}

impl StagedSource {
    fn new(blocks: &[&str]) -> Self {
        Self {
            blocks: Mutex::new(blocks.iter().map(|s| s.to_string()).collect()),
            block_calls: AtomicU32::new(0),
            anchored_calls: Mutex::new(Vec::new()),
            tracks_calls: Mutex::new(Vec::new()),
            unavailable: Vec::new(),
        }
    }

    fn with_unavailable(mut self, reads: &[AnchoredRead]) -> Self {
        self.unavailable = reads.to_vec();
        self
    }

    fn anchored_calls(&self) -> Vec<(AnchoredRead, String)> {
        self.anchored_calls.lock().unwrap().clone()
    }
}

impl SnapshotSource for StagedSource {
    async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
        let n = self.block_calls.fetch_add(1, Ordering::SeqCst) as usize;
        let blocks = self.blocks.lock().unwrap();
        // The last entry repeats, so a stable chain needs only one id.
        let id = blocks.get(n).or_else(|| blocks.last()).unwrap().clone();
        Ok(block(&id, 100 + n as u64))
    }

    async fn get_anchored(
        &self,
        read: AnchoredRead,
        block_id: &str,
    ) -> Result<serde_json::Value, SnapshotError> {
        self.anchored_calls
            .lock()
            .unwrap()
            .push((read, block_id.to_string()));
        if self.unavailable.contains(&read) {
            return Err(SnapshotError::Unavailable {
                endpoint: read.endpoint().to_string(),
                reason: "staged outage".to_string(),
            });
        }
        if read == AnchoredRead::Challenges {
            // Two challenges, so the per-challenge track reads have
            // something to derive from.
            return Ok(json!({
                "challenges": [{ "id": "c001" }, { "id": "c002" }],
                "anchored_to": block_id,
            }));
        }
        Ok(json!({ "endpoint": read.endpoint(), "anchored_to": block_id }))
    }

    async fn get_tracks(
        &self,
        challenge_id: &str,
        block_id: &str,
    ) -> Result<serde_json::Value, SnapshotError> {
        self.tracks_calls
            .lock()
            .unwrap()
            .push((challenge_id.to_string(), block_id.to_string()));
        Ok(json!({ "challenge_id": challenge_id, "anchored_to": block_id }))
    }
}

#[tokio::test]
async fn a_stable_block_produces_a_complete_snapshot() {
    let source = StagedSource::new(&["block-a"]);
    let snapshot: Snapshot = assemble(&source, 3).await.expect("assembles");

    assert_eq!(snapshot.block_id, "block-a");
    assert!(snapshot.reads_complete, "every read succeeded");
    assert!(
        !snapshot.active_cache_ready,
        "§9 step 4 is not implemented, so the cache must not claim readiness"
    );
    for read in AnchoredRead::ALL {
        assert!(
            snapshot.read(*read).is_some(),
            "{} missing from the snapshot",
            read.endpoint()
        );
    }
}

#[tokio::test]
async fn every_read_is_anchored_to_the_block_that_opened_the_snapshot() {
    // The point of §9: not merely that the reads succeed, but that they all
    // describe the same block.
    let source = StagedSource::new(&["block-a"]);
    let snapshot = assemble(&source, 3).await.unwrap();

    for (read, anchored_to) in source.anchored_calls() {
        assert_eq!(
            anchored_to,
            snapshot.block_id,
            "{} was fetched against {anchored_to}, not the snapshot's block",
            read.endpoint()
        );
    }
}

#[tokio::test]
async fn a_block_advancing_mid_assembly_discards_the_whole_snapshot() {
    // §9 step 6. The opening read sees block-a, the closing read sees
    // block-b, so every read taken in between describes a block that is no
    // longer current. Patching the changed field would leave a snapshot
    // mixing two blocks, which is the state the algorithm exists to prevent.
    //
    // Third call returns block-b again, so the retry succeeds there.
    let source = StagedSource::new(&["block-a", "block-b", "block-b", "block-b"]);
    let snapshot = assemble(&source, 3)
        .await
        .expect("retries at the new block");

    assert_eq!(
        snapshot.block_id, "block-b",
        "the retry must anchor to the block that won, not the one abandoned"
    );
    // And nothing from the abandoned pass survived into it.
    for read in AnchoredRead::ALL {
        let value = snapshot.read(*read).unwrap();
        assert_eq!(
            value["anchored_to"],
            "block-b",
            "{} carried over from the discarded pass",
            read.endpoint()
        );
    }
}

#[tokio::test]
async fn a_chain_advancing_every_pass_is_reported_rather_than_spun_on() {
    // Every closing read disagrees with its opening one, so no pass can be
    // accepted. §9 wants that surfaced; a silent spin would look like the
    // snapshot merely being slow.
    let source = StagedSource::new(&["b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8"]);
    let err = assemble(&source, 3).await.expect_err("cannot converge");
    match err {
        SnapshotError::Outpaced { attempts } => assert_eq!(attempts, 3),
        other => panic!("expected Outpaced, got {other}"),
    }
}

#[tokio::test]
async fn a_failed_read_marks_the_snapshot_incomplete_rather_than_hiding_it() {
    // §9 step 7 persists completeness precisely so an incomplete snapshot
    // cannot be mistaken for a whole one later by checking which fields
    // happen to be present.
    let source = StagedSource::new(&["block-a"]).with_unavailable(&[AnchoredRead::Opow]);
    let snapshot = assemble(&source, 3).await.expect("still assembles");

    assert!(
        !snapshot.reads_complete,
        "a missing read must show as incomplete"
    );
    assert!(snapshot.read(AnchoredRead::Opow).is_none());
    // The rest is still block-consistent: partial is not the same as mixed.
    assert_eq!(snapshot.block_id, "block-a");
    assert!(snapshot.read(AnchoredRead::Challenges).is_some());
}

#[tokio::test]
async fn a_block_without_an_id_is_a_shape_error_not_a_retry() {
    struct Shapeless;
    impl SnapshotSource for Shapeless {
        async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({ "block": { "details": { "height": 1 } } }))
        }
        async fn get_anchored(
            &self,
            _read: AnchoredRead,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
        async fn get_tracks(
            &self,
            _challenge_id: &str,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
    }
    let err = assemble(&Shapeless, 3).await.expect_err("no block id");
    match err {
        SnapshotError::Shape { .. } => {}
        other => panic!("expected Shape, got {other}"),
    }
}

#[test]
fn the_cache_is_dropped_whole_when_the_block_changes() {
    // Not evicted per entry: a cache holding some entries from the previous
    // block and some from the current one is exactly the mixed-block state
    // §9 exists to prevent.
    let cache = BlockCache::new();
    cache.put("block-a", "get-challenges", json!({"a": 1}));
    cache.put("block-a", "get-opow?player_id=x", json!({"a": 2}));
    assert_eq!(cache.len_for("block-a"), 2);

    cache.put("block-b", "get-challenges", json!({"b": 1}));
    assert_eq!(cache.len_for("block-b"), 1, "only the new block's entry");
    assert_eq!(
        cache.len_for("block-a"),
        0,
        "the old block is gone entirely"
    );
    assert!(cache.get("block-a", "get-opow?player_id=x").is_none());
}

#[test]
fn the_cache_keys_on_the_complete_request_not_the_endpoint() {
    // Two calls to one endpoint with different parameters are different
    // responses; keying on the endpoint alone would serve one for the other.
    let cache = BlockCache::new();
    cache.put(
        "block-a",
        "get-tracks-data?challenge_id=c001",
        json!({"c": 1}),
    );
    assert!(
        cache
            .get("block-a", "get-tracks-data?challenge_id=c002")
            .is_none(),
        "a different request key must not hit"
    );
    assert_eq!(
        cache.get("block-a", "get-tracks-data?challenge_id=c001"),
        Some(json!({"c": 1}))
    );
}

#[tokio::test]
async fn track_data_is_fetched_per_challenge_at_the_same_block() {
    // §9 step 3 fetches track data per challenge, derived from the
    // challenges response taken in the same pass — so the call set depends
    // on that response and cannot be hoisted out of the anchored section.
    let source = StagedSource::new(&["block-a"]);
    let snapshot = assemble(&source, 3).await.unwrap();

    let calls = source.tracks_calls.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        2,
        "one track read per challenge, got {calls:?}"
    );
    for (challenge, anchored_to) in &calls {
        assert_eq!(
            anchored_to, &snapshot.block_id,
            "track data for {challenge} was fetched against another block"
        );
    }
    assert!(snapshot.tracks.contains_key("c001"));
    assert!(snapshot.tracks.contains_key("c002"));
}

#[tokio::test]
async fn a_refresh_conflict_mid_assembly_restarts_rather_than_aborting() {
    // How a real advance actually surfaces. §8 requires anchored reads to
    // name the latest block, so TIG rejects the *remaining* reads with
    // "block must be latest" long before the closing get-block would notice
    // the change. Treating that 4xx as a schema fault aborts the exact
    // scenario §9 step 6 exists for — and the staged-block test cannot see
    // it, because there the reads always succeed.
    struct ConflictingSource {
        reads: std::sync::atomic::AtomicU32,
        block_calls: AtomicU32,
    }
    impl SnapshotSource for ConflictingSource {
        async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
            let n = self.block_calls.fetch_add(1, Ordering::SeqCst);
            // First pass opens at block-a; everything after is block-b.
            Ok(block(if n == 0 { "block-a" } else { "block-b" }, 100))
        }
        async fn get_anchored(
            &self,
            read: AnchoredRead,
            block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            // The third read of the first pass is refused because the block
            // it names has moved on.
            let n = self.reads.fetch_add(1, Ordering::SeqCst);
            if n == 2 {
                return Err(SnapshotError::RefreshConflict {
                    endpoint: read.endpoint().to_string(),
                    reason: "block must be latest".to_string(),
                });
            }
            if read == AnchoredRead::Challenges {
                return Ok(json!({ "challenges": [], "anchored_to": block_id }));
            }
            Ok(json!({ "endpoint": read.endpoint(), "anchored_to": block_id }))
        }
        async fn get_tracks(
            &self,
            _challenge_id: &str,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
    }

    let source = ConflictingSource {
        reads: AtomicU32::new(0),
        block_calls: AtomicU32::new(0),
    };
    let snapshot = assemble(&source, 3)
        .await
        .expect("a refresh conflict must restart assembly, not abort it");
    assert_eq!(
        snapshot.block_id, "block-b",
        "the retry must anchor to the block that superseded the abandoned one"
    );
}

#[tokio::test]
async fn a_successful_but_unparseable_challenges_response_is_not_silently_empty() {
    // Absent because the read failed is already carried by reads_complete.
    // Present but unparseable is different: nothing else records it, and
    // returning no ids would produce a snapshot marked complete with no
    // per-challenge track data at all.
    struct OddChallenges;
    impl SnapshotSource for OddChallenges {
        async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
            Ok(block("block-a", 100))
        }
        async fn get_anchored(
            &self,
            read: AnchoredRead,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            if read == AnchoredRead::Challenges {
                // A 200 whose envelope is not what §5 pins.
                return Ok(json!({ "unexpected": "envelope" }));
            }
            Ok(json!({}))
        }
        async fn get_tracks(
            &self,
            _challenge_id: &str,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
    }

    let err = assemble(&OddChallenges, 3)
        .await
        .expect_err("an unparseable challenges envelope must not pass as complete");
    match err {
        SnapshotError::Shape { endpoint, .. } => assert_eq!(endpoint, "get-challenges"),
        other => panic!("expected Shape for get-challenges, got {other}"),
    }
}

#[tokio::test]
async fn a_block_missing_its_id_sets_is_refused() {
    // §7 makes active_ids and confirmed_ids authoritative; §12 forbids
    // compiling a default. A block without them accepted as complete would
    // read downstream as "nothing is active", indistinguishable from the
    // truthful answer.
    struct Bare;
    impl SnapshotSource for Bare {
        async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({ "block": { "id": "b", "details": { "height": 1 }, "config": {} } }))
        }
        async fn get_anchored(
            &self,
            _read: AnchoredRead,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
        async fn get_tracks(
            &self,
            _challenge_id: &str,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
    }
    let err = assemble(&Bare, 3).await.expect_err("no id sets");
    assert!(
        format!("{err}").contains("confirmed_ids"),
        "the error should name the missing set, got: {err}"
    );
}

#[tokio::test]
async fn a_challenge_without_an_id_is_refused_rather_than_skipped() {
    // Skipping it would produce fewer per-challenge track reads while
    // reads_complete still said true — the same fail-open shape as an
    // unparseable envelope, one level down.
    struct MissingId;
    impl SnapshotSource for MissingId {
        async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
            Ok(block("block-a", 100))
        }
        async fn get_anchored(
            &self,
            read: AnchoredRead,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            if read == AnchoredRead::Challenges {
                return Ok(json!({ "challenges": [{ "id": "c001" }, { "name": "no id here" }] }));
            }
            Ok(json!({}))
        }
        async fn get_tracks(
            &self,
            _challenge_id: &str,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
    }
    let err = assemble(&MissingId, 3)
        .await
        .expect_err("entry without an id");
    assert!(
        format!("{err}").contains("index 1"),
        "the error should name which entry, got: {err}"
    );
}

#[tokio::test]
async fn a_block_missing_round_or_timestamp_or_prev_id_is_refused() {
    // §5 requires all three; §5.1 compares `details.round` to decide which
    // challenges are active. Accepting a block without one and reporting
    // reads_complete: true puts the fail-open one level below the
    // completeness status — the decision is made before anything can notice.
    async fn assemble_without(field: &str) -> SnapshotError {
        struct Missing {
            field: String,
        }
        impl SnapshotSource for Missing {
            async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
                let mut b = block("block-a", 100);
                b["block"]["details"]
                    .as_object_mut()
                    .expect("details is an object")
                    .remove(&self.field);
                Ok(b)
            }
            async fn get_anchored(
                &self,
                _read: AnchoredRead,
                _block_id: &str,
            ) -> Result<serde_json::Value, SnapshotError> {
                Ok(json!({}))
            }
            async fn get_tracks(
                &self,
                _challenge_id: &str,
                _block_id: &str,
            ) -> Result<serde_json::Value, SnapshotError> {
                Ok(json!({}))
            }
        }
        assemble(
            &Missing {
                field: field.to_string(),
            },
            3,
        )
        .await
        .expect_err("a required block field was absent")
    }

    for field in ["prev_block_id", "round", "timestamp"] {
        let err = assemble_without(field).await;
        assert!(
            format!("{err}").contains(field),
            "the error should name the missing field {field}, got: {err}"
        );
    }
}

#[tokio::test]
async fn a_round_that_is_not_a_number_is_refused() {
    // Presence alone would pass this block through to §5.1's
    // `round_active <= round` comparison, which cannot evaluate a string.
    struct StringRound;
    impl SnapshotSource for StringRound {
        async fn get_block(&self) -> Result<serde_json::Value, SnapshotError> {
            let mut b = block("block-a", 100);
            b["block"]["details"]["round"] = json!("834");
            Ok(b)
        }
        async fn get_anchored(
            &self,
            _read: AnchoredRead,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
        async fn get_tracks(
            &self,
            _challenge_id: &str,
            _block_id: &str,
        ) -> Result<serde_json::Value, SnapshotError> {
            Ok(json!({}))
        }
    }
    let err = assemble(&StringRound, 3)
        .await
        .expect_err("round was a string");
    assert!(
        format!("{err}").contains("details.round"),
        "the error should name the offending field, got: {err}"
    );
}
