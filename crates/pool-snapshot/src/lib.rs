//! Block-consistent snapshot assembly.
//!
//! `docs/tig_integration.md` §9 owns the algorithm; this implements it and
//! does not restate the rules. The shape that matters: every read in one
//! snapshot is anchored to the same block, and a snapshot whose closing
//! `get-block` disagrees with its opening one is **discarded, not patched**.
//!
//! Assembly is written against the [`SnapshotSource`] port rather than the
//! HTTP client directly (`architecture.md` §4), so the algorithm can be
//! driven through the cases that matter — a block advancing mid-assembly
//! above all, which cannot be provoked against live testnet on demand.

mod cache;
mod tig_source;

pub use cache::BlockCache;
pub use tig_source::TigSnapshotSource;

use std::collections::BTreeMap;
use std::future::Future;

/// The reads §9 step 3 anchors to the opening block.
///
/// Named rather than free-form so a caller cannot invent an endpoint that
/// bypasses the per-block cache, and so the set a snapshot depends on is
/// visible in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnchoredRead {
    Challenges,
    Algorithms,
    Opow,
    PlayerData,
    Benchmarks,
}

impl AnchoredRead {
    pub const ALL: &'static [AnchoredRead] = &[
        AnchoredRead::Challenges,
        AnchoredRead::Algorithms,
        AnchoredRead::Opow,
        AnchoredRead::PlayerData,
        AnchoredRead::Benchmarks,
    ];

    pub fn endpoint(self) -> &'static str {
        match self {
            AnchoredRead::Challenges => "get-challenges",
            AnchoredRead::Algorithms => "get-algorithms",
            AnchoredRead::Opow => "get-opow",
            AnchoredRead::PlayerData => "get-player-data",
            AnchoredRead::Benchmarks => "get-benchmarks",
        }
    }
}

/// Everything assembly needs from TIG.
///
/// A port, not the client: §9's interesting case is a block advancing
/// between the opening and closing reads, which a test can stage here and
/// cannot stage against a real server.
pub trait SnapshotSource {
    /// `GET /get-block?include_data=true`, returning the block object.
    fn get_block(&self) -> impl Future<Output = Result<serde_json::Value, SnapshotError>> + Send;

    /// One anchored read, for the block the caller opened with.
    fn get_anchored(
        &self,
        read: AnchoredRead,
        block_id: &str,
    ) -> impl Future<Output = Result<serde_json::Value, SnapshotError>> + Send;

    /// Track data for one challenge.
    ///
    /// Separate from [`Self::get_anchored`] because §9 step 3 fetches track
    /// data *per challenge*, so the set of calls depends on the challenges
    /// response taken earlier in the same pass — one more reason every read
    /// has to be anchored to the same block.
    fn get_tracks(
        &self,
        challenge_id: &str,
        block_id: &str,
    ) -> impl Future<Output = Result<serde_json::Value, SnapshotError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A read failed in a way the caller may retry.
    #[error("{endpoint} unavailable: {reason}")]
    Unavailable { endpoint: String, reason: String },
    /// A response did not carry what §9 step 2 requires.
    #[error("{endpoint} returned an unusable shape: {reason}")]
    Shape { endpoint: String, reason: String },
    /// The block moved during assembly. §9 step 6: discard and restart at
    /// the new block. Not an error the caller should paper over — it is the
    /// algorithm working.
    #[error("block changed during assembly: opened at {opened}, closed at {closed}")]
    BlockAdvanced { opened: String, closed: String },
    /// A read was refused because the block it named is no longer latest.
    ///
    /// §9 calls this a refresh conflict, "not partial success", and handles
    /// it exactly as step 6 handles a changed closing block. It matters
    /// because it is how a real advance actually surfaces: §8 requires
    /// anchored reads to name the latest block, so TIG rejects the
    /// *remaining* reads mid-assembly, long before the closing `get-block`
    /// would notice. Treating it as a schema failure aborts the very
    /// scenario the algorithm exists for.
    #[error("{endpoint} reported a refresh conflict: {reason}")]
    RefreshConflict { endpoint: String, reason: String },
    /// Every attempt was outrun by a block advance.
    #[error(
        "could not assemble a snapshot in {attempts} attempts; the chain is advancing faster than assembly"
    )]
    Outpaced { attempts: u32 },
}

/// One accepted, block-consistent snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The block every read in this snapshot is anchored to.
    pub block_id: String,
    pub height: u64,
    pub block: serde_json::Value,
    pub reads: BTreeMap<String, serde_json::Value>,
    /// Track data per challenge, keyed by challenge id.
    pub tracks: BTreeMap<String, serde_json::Value>,
    /// Whether every step-3 read and per-challenge track read succeeded.
    ///
    /// Named for what it covers. §9 step 7 persists a completeness status so
    /// an incomplete snapshot cannot reach a decision, but slice-1 criterion
    /// C5 gates orchestrator work on *both* this and the active-benchmark
    /// cache — so a single `complete` flag would be read as the whole gate
    /// and let an orchestrator proceed without the cache.
    pub reads_complete: bool,
    /// Whether §9 step 4's incremental active-benchmark metadata cache is
    /// ready.
    ///
    /// Always `false` here: step 4 is not implemented yet. Present rather
    /// than absent so C5's gate has both halves to read from the moment an
    /// orchestrator exists, instead of being retrofitted once something has
    /// already been written against `reads_complete` alone.
    pub active_cache_ready: bool,
}

impl Snapshot {
    pub fn read(&self, read: AnchoredRead) -> Option<&serde_json::Value> {
        self.reads.get(read.endpoint())
    }
}

/// Assemble one block-consistent snapshot, retrying at the new block when
/// the chain moves under us.
///
/// `max_attempts` bounds the retry: a chain advancing faster than assembly
/// completes would otherwise loop forever, and §9 wants that surfaced rather
/// than hidden in a spin.
pub async fn assemble<S: SnapshotSource>(
    source: &S,
    max_attempts: u32,
) -> Result<Snapshot, SnapshotError> {
    for attempt in 1..=max_attempts {
        match assemble_once(source).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(SnapshotError::RefreshConflict { endpoint, reason }) => {
                tracing::debug!(
                    event = "snapshot.discarded",
                    attempt,
                    endpoint = %endpoint,
                    reason = %reason,
                    "a read reported a refresh conflict; discarding and restarting"
                );
            }
            Err(SnapshotError::BlockAdvanced { opened, closed }) => {
                // Not a failure: the chain moved, so the assembled reads are
                // a mixture of two blocks and the whole thing goes. §9 step 6
                // says restart at the new block, never patch this one.
                tracing::debug!(
                    event = "snapshot.discarded",
                    attempt,
                    opened_block = %opened,
                    closed_block = %closed,
                    "block advanced during assembly; discarding and restarting"
                );
            }
            Err(other) => return Err(other),
        }
    }
    Err(SnapshotError::Outpaced {
        attempts: max_attempts,
    })
}

/// One pass of §9 steps 1-6.
async fn assemble_once<S: SnapshotSource>(source: &S) -> Result<Snapshot, SnapshotError> {
    // Step 1: open, and record the block id.
    let opening = source.get_block().await?;
    let (opened_id, height) = block_identity(&opening)?;

    // Step 3: every anchored read, against that block.
    let mut reads = BTreeMap::new();
    let mut complete = true;
    for read in AnchoredRead::ALL {
        match source.get_anchored(*read, &opened_id).await {
            Ok(value) => {
                reads.insert(read.endpoint().to_string(), value);
            }
            Err(SnapshotError::Unavailable { .. }) => {
                // Recorded as incomplete rather than abandoned: §9 step 7
                // persists completeness, and step 5's consistency check is
                // still worth performing so a partial snapshot is not also
                // a mixed-block one.
                complete = false;
            }
            Err(other) => return Err(other),
        }
    }

    // Still step 3: track data per challenge, from the challenges response
    // taken in this same pass. Derived rather than configured, so a
    // challenge appearing or disappearing between blocks changes the call
    // set — which is why it cannot be hoisted out of the anchored section.
    let mut tracks = BTreeMap::new();
    for challenge_id in challenge_ids(reads.get(AnchoredRead::Challenges.endpoint()))? {
        match source.get_tracks(&challenge_id, &opened_id).await {
            Ok(value) => {
                tracks.insert(challenge_id, value);
            }
            Err(SnapshotError::Unavailable { .. }) => complete = false,
            Err(other) => return Err(other),
        }
    }

    // Step 5-6: close, and accept only if the block has not moved.
    let closing = source.get_block().await?;
    let (closed_id, _) = block_identity(&closing)?;
    if closed_id != opened_id {
        return Err(SnapshotError::BlockAdvanced {
            opened: opened_id,
            closed: closed_id,
        });
    }

    Ok(Snapshot {
        block_id: opened_id,
        height,
        block: opening,
        reads,
        tracks,
        reads_complete: complete,
        active_cache_ready: false,
    })
}

/// Challenge ids from the challenges response.
///
/// Absent means the read failed and `reads_complete` already carries that.
/// **Present but unparseable is different**: the read succeeded, so nothing
/// else records a problem, and returning no ids would produce a snapshot
/// marked complete with no per-challenge track data at all — §14's rule is
/// that recorded discrepancies do not authorise accepting new ones.
fn challenge_ids(challenges: Option<&serde_json::Value>) -> Result<Vec<String>, SnapshotError> {
    let Some(value) = challenges else {
        return Ok(Vec::new());
    };
    let list = value
        .get("challenges")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| SnapshotError::Shape {
            endpoint: "get-challenges".to_string(),
            reason: "response carried no challenge list".to_string(),
        })?;
    // Every entry must carry the pinned `id`. Skipping the ones that do not
    // would yield fewer track reads while `reads_complete` still said true —
    // the same fail-open shape as an unparseable envelope, one level down,
    // and §1 makes a missing or type-incompatible required field fatal.
    let mut ids = Vec::with_capacity(list.len());
    for (index, challenge) in list.iter().enumerate() {
        match challenge.get("id").and_then(|v| v.as_str()) {
            Some(id) => ids.push(id.to_string()),
            None => {
                return Err(SnapshotError::Shape {
                    endpoint: "get-challenges".to_string(),
                    reason: format!("challenge at index {index} carries no string id"),
                });
            }
        }
    }
    Ok(ids)
}

/// §9 step 2: the block must carry the identity everything else anchors to,
/// and the sets and configuration later steps read from it.
///
/// `data.active_ids` and `data.confirmed_ids` are validated here because §7
/// makes them authoritative for "Active" and "Verification event" — a block
/// missing them would otherwise be accepted as a complete snapshot and read
/// downstream as "nothing is active", which is indistinguishable from the
/// truthful answer.
///
/// `details.prev_block_id`, `details.round` and `details.timestamp` are
/// checked for the same reason, and by type rather than mere presence. §5
/// lists all three as required `get-block` data and §12 lists them as live
/// values. `round` is the load-bearing one: §5.1 decides which challenges
/// are active with `challenge.state.round_active <= block.details.round`, so
/// a block that reached here without a numeric round would either fault that
/// comparison downstream — past the completeness status that already
/// declared the snapshot sound — or silently compare against a default and
/// mis-scope the active set. That is the fail-open this function already
/// rejects for the ID sets.
///
/// Only the anchor is returned; everything else stays in `block`, which the
/// snapshot carries, so nothing needs re-fetching to read them (§9's rule
/// against substituting a field into an accepted snapshot).
fn block_identity(block: &serde_json::Value) -> Result<(String, u64), SnapshotError> {
    let inner = block.get("block").unwrap_or(block);
    for (path, value) in [
        ("config", inner.get("config")),
        (
            "data.confirmed_ids",
            inner.get("data").and_then(|d| d.get("confirmed_ids")),
        ),
        (
            "data.active_ids",
            inner.get("data").and_then(|d| d.get("active_ids")),
        ),
    ] {
        if value.is_none_or(serde_json::Value::is_null) {
            return Err(SnapshotError::Shape {
                endpoint: "get-block".to_string(),
                reason: format!("missing {path}; §12 forbids substituting a compiled default"),
            });
        }
    }
    let id = inner
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| SnapshotError::Shape {
            endpoint: "get-block".to_string(),
            reason: "no block id".to_string(),
        })?
        .to_string();
    let details = inner.get("details");
    let height = details
        .and_then(|d| d.get("height"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SnapshotError::Shape {
            endpoint: "get-block".to_string(),
            reason: "no block height".to_string(),
        })?;

    // Typed, not merely present: `round: "834"` is as unusable to §5.1's
    // comparison as no round at all, and a string that parses nowhere is the
    // harder failure to trace once a decision has already been made on it.
    for (path, well_formed) in [
        (
            "details.prev_block_id",
            details
                .and_then(|d| d.get("prev_block_id"))
                .is_some_and(serde_json::Value::is_string),
        ),
        (
            "details.round",
            details
                .and_then(|d| d.get("round"))
                .is_some_and(serde_json::Value::is_u64),
        ),
        (
            "details.timestamp",
            details
                .and_then(|d| d.get("timestamp"))
                .is_some_and(serde_json::Value::is_u64),
        ),
    ] {
        if !well_formed {
            return Err(SnapshotError::Shape {
                endpoint: "get-block".to_string(),
                reason: format!(
                    "missing or type-incompatible {path}; §5 requires it and §12 forbids \
                     substituting a compiled default"
                ),
            });
        }
    }

    Ok((id, height))
}
