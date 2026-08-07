//! Spike pool: resumable chunked upload into quarantine, durable acceptance
//! saga, immutable receipt, and slot re-offer.
//!
//! Contracts under test: `docs/member_protocol.md` §11–§12 (resumable and
//! idempotent upload; received vs durably accepted), `docs/architecture.md`
//! §5.2 (package acceptance flow), §8.2 (quarantine + ordered publication
//! saga and its crash matrix), §8.3 (artifact reference), §12 (restart
//! guarantees). Plan: `docs/plans/protocol-spike.md` phase S3 (issue #12).
//!
//! Disposable spike code (plan §3), but the guarantees under test are
//! honestly durable: every chunk object and every state document is fsynced
//! before it is acknowledged; multi-field state commits are single atomic
//! renames of fsynced documents; recovery is a pure fold over what survived.
//!
//! Filesystem artifact-store adapter (architecture §8.1 local row): two
//! separate directories, `quarantine/` and `accepted/`, under one pool root
//! on the same durable volume, plus `state/` for the durable pool state that
//! stands in for the relational database.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::member::{derived_uuid, rfc3339_utc, sha256_hex};

// ---------------------------------------------------------------------------
// Typed errors (member_protocol §12: "status returns a stable typed reason")
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Chunk offset is ahead of the committed offset (`OFFSET_MISMATCH`).
    OffsetMismatch,
    /// A retried older range differs from what was committed (`CHUNK_CONFLICT`).
    ChunkConflict,
    /// Chunk bytes do not match the declared chunk SHA-256.
    ChunkChecksumMismatch,
    /// Bytes beyond the declared package size are never accepted.
    BeyondDeclaredSize,
    /// Same `package_id`, different declaration.
    DeclarationConflict,
    UnknownUpload,
    UnknownAssignment,
    /// Finalize called before the durable ledger covers the declaration.
    LedgerIncomplete,
    /// Streamed whole-package SHA-256 differs from the declaration.
    PackageSha256Mismatch,
    /// Bounded decompression disagrees with declared uncompressed size or
    /// manifest SHA-256.
    StructuralMismatch,
    /// A deterministic accepted object exists but fails verification.
    PublicationCorrupt,
    SlotOccupied,
    SlotUnknown,
    /// Another package is already durably accepted for this assignment
    /// (member_protocol §11: only one package can become durably accepted
    /// for an assignment; §16.8).
    AssignmentAlreadyAccepted,
    /// Test hook: simulated crash at an injected point.
    SimulatedCrash,
    Storage,
}

#[derive(Debug)]
pub struct PoolError {
    pub code: ErrorCode,
    pub detail: String,
    /// For `OffsetMismatch`: the authoritative committed offset.
    pub committed_offset: Option<u64>,
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.detail)
    }
}

impl std::error::Error for PoolError {}

fn err(code: ErrorCode, detail: impl Into<String>) -> PoolError {
    PoolError {
        code,
        detail: detail.into(),
        committed_offset: None,
    }
}

fn storage(detail: impl fmt::Display) -> PoolError {
    err(ErrorCode::Storage, detail.to_string())
}

pub type PResult<T> = Result<T, PoolError>;

/// Crash-injection points for the §8.2 crash-matrix tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the chunk object is durable in quarantine but before the range
    /// is committed to the durable ledger.
    AfterChunkObjectWrite,
    /// After the accepted object is published (renamed + parent fsynced) but
    /// before the durable-acceptance state commit.
    AfterPublication,
}

// ---------------------------------------------------------------------------
// Durable filesystem primitives
// ---------------------------------------------------------------------------

fn fsync_dir(dir: &Path) -> PResult<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| storage(format!("fsync dir {}: {e}", dir.display())))
}

/// Write `bytes` at `path` atomically: temp file in the same directory,
/// fsync file, rename to the final name, fsync the parent directory
/// (architecture §8.2 filesystem publication rule; used for every state
/// document so a multi-field commit is all-or-nothing).
fn write_atomic(path: &Path, bytes: &[u8]) -> PResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| storage(format!("no parent for {}", path.display())))?;
    fs::create_dir_all(parent).map_err(storage)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| storage(format!("bad file name {}", path.display())))?;
    let tmp = parent.join(format!(".tmp-{file_name}"));
    let mut f = File::create(&tmp).map_err(storage)?;
    f.write_all(bytes).map_err(storage)?;
    f.sync_all().map_err(storage)?;
    drop(f);
    fs::rename(&tmp, path).map_err(storage)?;
    fsync_dir(parent)
}

fn write_doc_atomic(path: &Path, doc: &Value) -> PResult<()> {
    let bytes = serde_json::to_vec_pretty(doc).map_err(storage)?;
    write_atomic(path, &bytes)
}

fn read_doc(path: &Path) -> PResult<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| storage(format!("corrupt doc {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(storage(format!("reading {}: {e}", path.display()))),
    }
}

/// Total bytes of regular files under `dir` (used for the peak
/// temporary-disk measurement; tolerant of concurrent renames).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Member-provided identifiers never become paths unvalidated
/// (architecture §8.2, CLAUDE.md). Accept only lowercase alphanumerics and
/// dashes, bounded length — covers pool-issued UUIDs, hex digests, and
/// network names, and admits no path metacharacters.
fn validate_id(kind: &str, id: &str) -> PResult<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(err(
            ErrorCode::Storage,
            format!("invalid {kind} identifier {id:?}"),
        ))
    }
}

fn field_str<'v>(v: &'v Value, key: &str) -> PResult<&'v str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| storage(format!("missing string field '{key}'")))
}

fn field_u64(v: &Value, key: &str) -> PResult<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| storage(format!("missing u64 field '{key}'")))
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

pub struct Pool {
    root: PathBuf,
    /// SPIKE-ONLY serialization of the acceptance saga (issue #32): the
    /// single-acceptance check and the durable-acceptance commit must be one
    /// critical section, or two concurrent finalizes of different packages
    /// for the same assignment could both pass the check and both commit —
    /// two accepted artifacts and a double slot release (member_protocol
    /// §11/§16.8/§16.10). The production guarantee is NOT a lock held across
    /// verification and publication (a transaction spanning bulk artifact
    /// work is forbidden, architecture §13 invariant 7): the Artifact Worker
    /// publishes first, and the Controller then enforces the §6 "one
    /// accepted package per assignment" unique constraint inside its short
    /// acceptance transaction.
    accept: std::sync::Mutex<()>,
}

/// One committed contiguous range from the durable ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RangeRec {
    offset: u64,
    length: u64,
    sha256: String,
}

#[derive(Debug, Clone)]
pub struct UploadSession {
    pub upload_id: String,
    pub committed_offset: u64,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    pub slot_id: String,
    pub generation: u64,
    pub occupied: bool,
    pub assignment_id: String,
}

/// Result of `finalize`. `fresh` is false when a stored receipt was
/// returned; measurement fields are present only on the fresh path.
#[derive(Debug, Clone)]
pub struct FinalizeOutcome {
    /// Canonical JSON receipt bytes — byte-identical across retries.
    pub receipt_json: String,
    pub fresh: bool,
    pub verify_us: Option<u128>,
    pub publish_us: Option<u128>,
    pub commit_us: Option<u128>,
    /// Bytes under the pool root sampled just before the atomic rename —
    /// the true peak of temporary disk use during the saga.
    pub peak_bytes_under_root: Option<u64>,
}

impl Pool {
    pub fn open(root: impl Into<PathBuf>) -> PResult<Pool> {
        let root = root.into();
        for sub in ["quarantine", "accepted", "state"] {
            fs::create_dir_all(root.join(sub)).map_err(storage)?;
        }
        Ok(Pool {
            root,
            accept: std::sync::Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // -- assignment + slot registration (stand-in for §5.1 state) ----------

    /// Record the confirmed assignment context an upload will bind to, and
    /// occupy its slot at the given generation. Idempotent for identical
    /// documents.
    pub fn register_assignment(&self, a: &Value) -> PResult<()> {
        let digest = field_str(a, "assignment_digest")?;
        validate_id("assignment_digest", digest)?;
        for key in ["assignment_id", "benchmark_id", "slot_id", "network"] {
            validate_id(key, field_str(a, key)?)?;
        }
        // Issued-identity fields (issue #32): validated when present so the
        // authed path can bind the §2 ownership chain into the receipt.
        for key in ["member_id", "worker_id"] {
            if a.get(key).is_some() {
                validate_id(key, field_str(a, key)?)?;
            }
        }
        let generation = field_u64(a, "slot_generation")?;
        // One digest per assignment_id: an assignment context, once
        // registered, can never be re-registered under a different digest
        // (member_protocol §14: never a different track/binary/benchmark
        // under the same assignment; §16.5).
        let assignment_id = field_str(a, "assignment_id")?;
        let index_path = self.state_path(&format!("assignments-by-id/{assignment_id}.json"));
        match read_doc(&index_path)? {
            Some(index) if field_str(&index, "assignment_digest")? != digest => {
                return Err(err(
                    ErrorCode::DeclarationConflict,
                    format!("assignment {assignment_id} already registered with another digest"),
                ));
            }
            Some(_) => {}
            None => write_doc_atomic(&index_path, &json!({ "assignment_digest": digest }))?,
        }
        let path = self.state_path(&format!("assignments/{digest}.json"));
        if let Some(existing) = read_doc(&path)? {
            if existing != *a {
                return Err(err(
                    ErrorCode::DeclarationConflict,
                    format!("assignment {digest} already registered with different content"),
                ));
            }
        } else {
            write_doc_atomic(&path, a)?;
        }
        let slot_id = field_str(a, "slot_id")?;
        let slot_path = self.state_path(&format!("slots/{slot_id}.json"));
        let slot_doc = json!({
            "slot_id": slot_id,
            "generation": generation,
            "state": "UPLOADING",
            "assignment_id": field_str(a, "assignment_id")?,
        });
        match read_doc(&slot_path)? {
            Some(existing) if existing != slot_doc => {
                let existing_gen = field_u64(&existing, "generation")?;
                // A slot reserved by `offer_slot` for this same assignment
                // and generation may proceed to UPLOADING (issue #32: the
                // authed path reserves before registering the assignment).
                let same_reservation = existing.get("state").and_then(Value::as_str)
                    == Some("RESERVED")
                    && existing_gen == generation
                    && existing.get("assignment_id") == slot_doc.get("assignment_id");
                if !same_reservation && existing_gen >= generation {
                    return Err(err(
                        ErrorCode::SlotOccupied,
                        format!("slot {slot_id} already at generation {existing_gen}"),
                    ));
                }
                write_doc_atomic(&slot_path, &slot_doc)
            }
            Some(_) => Ok(()),
            None => write_doc_atomic(&slot_path, &slot_doc),
        }
    }

    /// The registered assignment document for a digest, if any (read-only;
    /// used by the authed path to bind a declaration's digest to its issued
    /// `assignment_id`).
    pub fn assignment_by_digest(&self, digest: &str) -> PResult<Option<Value>> {
        validate_id("assignment_digest", digest)?;
        read_doc(&self.state_path(&format!("assignments/{digest}.json")))
    }

    // -- upload session ----------------------------------------------------

    /// Create (or resume) the upload for a declaration
    /// (member_protocol §11). Starting the same `package_id` with an
    /// identical declaration returns the same live upload; a changed
    /// declaration conflicts.
    pub fn create_upload(&self, declaration: &Value) -> PResult<UploadSession> {
        let package_id = field_str(declaration, "package_id")?;
        validate_id("package_id", package_id)?;
        let digest = field_str(declaration, "assignment_digest")?;
        validate_id("assignment_digest", digest)?;
        if read_doc(&self.state_path(&format!("assignments/{digest}.json")))?.is_none() {
            return Err(err(
                ErrorCode::UnknownAssignment,
                format!("no registered assignment for digest {digest}"),
            ));
        }
        validate_id("package_sha256", field_str(declaration, "package_sha256")?)?;
        validate_id(
            "manifest_sha256",
            field_str(declaration, "manifest_sha256")?,
        )?;
        if field_u64(declaration, "compressed_size_bytes")? == 0 {
            return Err(storage("declared compressed size is zero"));
        }

        let canonical = serde_json::to_string(declaration).map_err(storage)?;
        let upload_id = sha256_hex(canonical.as_bytes())[..32].to_owned();

        let index_path = self.state_path(&format!("uploads-by-package/{package_id}.json"));
        if let Some(index) = read_doc(&index_path)? {
            let existing_declaration = index.get("declaration").cloned().unwrap_or(Value::Null);
            if existing_declaration != *declaration {
                return Err(err(
                    ErrorCode::DeclarationConflict,
                    format!("package {package_id} already declared differently"),
                ));
            }
            let ranges = self.read_ranges(&upload_id)?;
            return Ok(UploadSession {
                upload_id,
                committed_offset: committed_offset(&ranges),
                resumed: true,
            });
        }

        let decl_path = self.state_path(&format!("uploads/{upload_id}/declaration.json"));
        write_doc_atomic(&decl_path, declaration)?;
        write_doc_atomic(
            &index_path,
            &json!({ "upload_id": upload_id, "declaration": declaration }),
        )?;
        fs::create_dir_all(self.root.join("quarantine").join(&upload_id)).map_err(storage)?;
        Ok(UploadSession {
            upload_id,
            committed_offset: 0,
            resumed: false,
        })
    }

    pub(crate) fn declaration(&self, upload_id: &str) -> PResult<Value> {
        read_doc(&self.state_path(&format!("uploads/{upload_id}/declaration.json")))?
            .ok_or_else(|| err(ErrorCode::UnknownUpload, format!("no upload {upload_id}")))
    }

    /// Durable committed ranges. A torn final line (crash mid-append,
    /// before fsync completed) is not committed and is ignored; any other
    /// malformed line is storage corruption.
    fn read_ranges(&self, upload_id: &str) -> PResult<Vec<RangeRec>> {
        let path = self.state_path(&format!("uploads/{upload_id}/ranges.jsonl"));
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(storage(format!("reading {}: {e}", path.display()))),
        };
        let text = String::from_utf8_lossy(&bytes);
        let complete_end = text.rfind('\n').map_or(0, |i| i + 1);
        let mut out = Vec::new();
        for line in text[..complete_end].lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .map_err(|e| storage(format!("corrupt range ledger line: {e}")))?;
            let rec = RangeRec {
                offset: field_u64(&v, "offset")?,
                length: field_u64(&v, "length")?,
                sha256: field_str(&v, "sha256")?.to_owned(),
            };
            let expected = out.last().map_or(0, |r: &RangeRec| r.offset + r.length);
            if rec.offset != expected {
                return Err(storage(format!(
                    "range ledger not contiguous: offset {} after {expected}",
                    rec.offset
                )));
            }
            out.push(rec);
        }
        Ok(out)
    }

    fn append_range(&self, upload_id: &str, rec: &RangeRec) -> PResult<()> {
        let path = self.state_path(&format!("uploads/{upload_id}/ranges.jsonl"));
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(storage)?;
        let mut line = serde_json::to_string(&json!({
            "offset": rec.offset,
            "length": rec.length,
            "sha256": rec.sha256,
        }))
        .map_err(storage)?;
        line.push('\n');
        f.write_all(line.as_bytes()).map_err(storage)?;
        f.sync_all().map_err(storage)
    }

    fn chunk_path(&self, upload_id: &str, rec: &RangeRec) -> PathBuf {
        // Deterministic key per (upload_id, offset, length, checksum)
        // (architecture §8.2 quarantine rule).
        self.root
            .join("quarantine")
            .join(upload_id)
            .join(format!("{:020}-{}-{}", rec.offset, rec.length, rec.sha256))
    }

    pub fn put_chunk(
        &self,
        upload_id: &str,
        offset: u64,
        declared_sha256: &str,
        bytes: &[u8],
    ) -> PResult<u64> {
        self.put_chunk_crash(upload_id, offset, declared_sha256, bytes, None)
    }

    /// Accept one chunk. The acknowledged offset never runs ahead of durable
    /// state: the chunk object is written and fsynced under its
    /// deterministic key, verified in the store, and only then added to the
    /// contiguous committed-range ledger; only that ledger determines the
    /// returned offset.
    pub fn put_chunk_crash(
        &self,
        upload_id: &str,
        offset: u64,
        declared_sha256: &str,
        bytes: &[u8],
        crash: Option<CrashPoint>,
    ) -> PResult<u64> {
        validate_id("chunk sha256", declared_sha256)?;
        let declaration = self.declaration(upload_id)?;
        let declared_size = field_u64(&declaration, "compressed_size_bytes")?;
        let ranges = self.read_ranges(upload_id)?;
        let committed = committed_offset(&ranges);

        if offset > committed {
            return Err(PoolError {
                code: ErrorCode::OffsetMismatch,
                detail: format!("offset {offset} ahead of committed {committed}"),
                committed_offset: Some(committed),
            });
        }
        if offset < committed {
            // Idempotent retry of an already-committed range
            // (member_protocol §11): exact range and checksum → adopt
            // without writing; anything else conflicts.
            let matches = ranges.iter().any(|r| {
                r.offset == offset && r.length == bytes.len() as u64 && r.sha256 == declared_sha256
            });
            return if matches && sha256_hex(bytes) == declared_sha256 {
                Ok(committed)
            } else {
                Err(err(
                    ErrorCode::ChunkConflict,
                    format!("retried range at {offset} differs from committed ledger"),
                ))
            };
        }
        if bytes.is_empty() {
            return Err(err(ErrorCode::ChunkChecksumMismatch, "empty chunk"));
        }
        if sha256_hex(bytes) != declared_sha256 {
            return Err(err(
                ErrorCode::ChunkChecksumMismatch,
                "chunk bytes do not match declared chunk SHA-256",
            ));
        }
        if offset + bytes.len() as u64 > declared_size {
            return Err(err(
                ErrorCode::BeyondDeclaredSize,
                format!(
                    "chunk end {} beyond declared size {declared_size}",
                    offset + bytes.len() as u64
                ),
            ));
        }

        let rec = RangeRec {
            offset,
            length: bytes.len() as u64,
            sha256: declared_sha256.to_owned(),
        };
        let path = self.chunk_path(upload_id, &rec);
        if path.exists() {
            // Crash happened after a previous chunk write but before the
            // range commit: an identical retry verifies and adopts the
            // orphan chunk; a conflicting one fails (§8.2).
            let existing = fs::read(&path).map_err(storage)?;
            if sha256_hex(&existing) != declared_sha256 {
                return Err(err(
                    ErrorCode::ChunkConflict,
                    "existing quarantine chunk under this key has different content",
                ));
            }
        } else {
            write_atomic(&path, bytes)?;
        }
        // Verify in the store before committing the range.
        let stored = fs::read(&path).map_err(storage)?;
        if stored.len() as u64 != rec.length || sha256_hex(&stored) != declared_sha256 {
            return Err(err(
                ErrorCode::Storage,
                "store verification failed after chunk write",
            ));
        }
        if crash == Some(CrashPoint::AfterChunkObjectWrite) {
            return Err(err(
                ErrorCode::SimulatedCrash,
                "simulated crash after chunk object write, before range commit",
            ));
        }
        self.append_range(upload_id, &rec)?;
        Ok(offset + rec.length)
    }

    /// Resumable status (member_protocol §11): the committed offset comes
    /// only from the durable ledger.
    pub fn upload_status(&self, upload_id: &str) -> PResult<Value> {
        let declaration = self.declaration(upload_id)?;
        let ranges = self.read_ranges(upload_id)?;
        let package_id = field_str(&declaration, "package_id")?;
        let acceptance =
            read_doc(&self.state_path(&format!("acceptance/{package_id}.json")))?.is_some();
        Ok(json!({
            "upload_id": upload_id,
            "package_id": package_id,
            "committed_offset": committed_offset(&ranges),
            "declared_compressed_size_bytes": field_u64(&declaration, "compressed_size_bytes")?,
            "durably_accepted": acceptance,
        }))
    }

    // -- finalization: ordered acceptance saga ------------------------------

    pub fn finalize(&self, package_id: &str) -> PResult<FinalizeOutcome> {
        self.finalize_crash(package_id, None)
    }

    /// Idempotent finalization (member_protocol §11, architecture §5.2/§8.2).
    ///
    /// Order: verify the durable ledger covers the declaration exactly →
    /// stream-verify the whole-package SHA-256 → bounded structural touch
    /// (uncompressed size + manifest SHA-256) → publish to the accepted
    /// store (temp file, verify, fsync, atomic rename to the deterministic
    /// key, fsync parent) → THEN commit the artifact reference + receipt +
    /// assignment state + slot release in one atomic state document.
    /// A retry after the commit returns the same immutable receipt.
    pub fn finalize_crash(
        &self,
        package_id: &str,
        crash: Option<CrashPoint>,
    ) -> PResult<FinalizeOutcome> {
        // One finalize at a time: the single-acceptance check below and the
        // acceptance commit at the end form one critical section.
        let _accepting = self
            .accept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        validate_id("package_id", package_id)?;
        let acceptance_path = self.state_path(&format!("acceptance/{package_id}.json"));
        if let Some(doc) = read_doc(&acceptance_path)? {
            // Crash after commit, before response — or any later retry:
            // return the stored immutable receipt, byte-identical.
            let receipt = doc
                .get("receipt")
                .ok_or_else(|| storage("acceptance doc missing receipt"))?;
            return Ok(FinalizeOutcome {
                receipt_json: serde_json::to_string(receipt).map_err(storage)?,
                fresh: false,
                verify_us: None,
                publish_us: None,
                commit_us: None,
                peak_bytes_under_root: None,
            });
        }

        let index = read_doc(&self.state_path(&format!("uploads-by-package/{package_id}.json")))?
            .ok_or_else(|| {
            err(
                ErrorCode::UnknownUpload,
                format!("no upload for package {package_id}"),
            )
        })?;
        let upload_id = field_str(&index, "upload_id")?.to_owned();
        let declaration = index
            .get("declaration")
            .cloned()
            .ok_or_else(|| storage("upload index missing declaration"))?;
        let declared_size = field_u64(&declaration, "compressed_size_bytes")?;
        let declared_sha = field_str(&declaration, "package_sha256")?;
        let digest = field_str(&declaration, "assignment_digest")?;
        let assignment = read_doc(&self.state_path(&format!("assignments/{digest}.json")))?
            .ok_or_else(|| err(ErrorCode::UnknownAssignment, format!("digest {digest}")))?;

        // 0. Only one package can become durably accepted for an assignment
        //    (member_protocol §11, invariant §16.8; architecture §6 "one
        //    accepted package per assignment"). A retry of the accepted
        //    package returned its receipt above; any other package for the
        //    same assignment is terminally refused before the saga runs.
        let this_assignment = field_str(&assignment, "assignment_id")?;
        if let Some(accepted) = self.accepted_package_for_assignment(this_assignment)?
            && accepted != package_id
        {
            return Err(err(
                ErrorCode::AssignmentAlreadyAccepted,
                format!("assignment {this_assignment} already durably accepted package"),
            ));
        }

        // 1. The durable chunk ledger must cover the declaration exactly.
        let ranges = self.read_ranges(&upload_id)?;
        let committed = committed_offset(&ranges);
        if committed != declared_size {
            return Err(err(
                ErrorCode::LedgerIncomplete,
                format!("committed {committed} of declared {declared_size} bytes"),
            ));
        }

        // 2. Stream-verify the whole-package SHA-256 against the declaration.
        let t_verify = std::time::Instant::now();
        let (streamed_len, streamed_sha) =
            stream_sha256(ChunkStream::new(self, &upload_id, &ranges)).map_err(storage)?;
        if streamed_len != declared_size {
            return Err(storage("quarantine chunk length changed"));
        }
        if streamed_sha != declared_sha {
            return Err(err(
                ErrorCode::PackageSha256Mismatch,
                format!("streamed {streamed_sha}, declared {declared_sha}"),
            ));
        }
        self.structural_touch(&declaration, ChunkStream::new(self, &upload_id, &ranges))?;
        let verify_us = t_verify.elapsed().as_micros();

        // 3. Publish to the accepted store at the deterministic key.
        let t_publish = std::time::Instant::now();
        let network = field_str(&assignment, "network")?;
        let benchmark_id = field_str(&assignment, "benchmark_id")?;
        let object_key =
            format!("accepted/{network}/{benchmark_id}/{package_id}/{declared_sha}.tar.zst");
        let final_path = self.root.join(&object_key);
        let mut peak_bytes = None;
        if final_path.exists() {
            // Crash after publication, before the state commit: discover
            // the deterministic object, verify it, and complete the same
            // transaction (§8.2 crash matrix).
            let existing = File::open(&final_path).map_err(storage)?;
            let (len, sha) = stream_sha256(existing).map_err(storage)?;
            if len != declared_size || sha != declared_sha {
                return Err(err(
                    ErrorCode::PublicationCorrupt,
                    "accepted object at deterministic key fails verification",
                ));
            }
        } else {
            let parent = final_path
                .parent()
                .ok_or_else(|| storage("accepted key has no parent"))?;
            fs::create_dir_all(parent).map_err(storage)?;
            let tmp = parent.join(format!(".tmp-{declared_sha}.tar.zst"));
            let mut f = File::create(&tmp).map_err(storage)?;
            std::io::copy(&mut ChunkStream::new(self, &upload_id, &ranges), &mut f)
                .map_err(storage)?;
            f.sync_all().map_err(storage)?;
            drop(f);
            // Verify size and hash of the temporary file before rename.
            let (len, sha) = stream_sha256(File::open(&tmp).map_err(storage)?).map_err(storage)?;
            if len != declared_size || sha != declared_sha {
                return Err(err(
                    ErrorCode::Storage,
                    "temp accepted file verification failed",
                ));
            }
            // Peak temporary disk use: quarantine + state + full temp copy.
            peak_bytes = Some(dir_size(&self.root));
            fs::rename(&tmp, &final_path).map_err(storage)?;
            fsync_dir(parent)?;
        }
        let publish_us = t_publish.elapsed().as_micros();

        if crash == Some(CrashPoint::AfterPublication) {
            return Err(err(
                ErrorCode::SimulatedCrash,
                "simulated crash after publication, before state commit",
            ));
        }

        // 4. Atomic durable-acceptance commit, strictly after publication:
        //    artifact reference (§8.3) + assignment state + slot release +
        //    immutable receipt, one atomically renamed document.
        let t_commit = std::time::Instant::now();
        let accepted_at = rfc3339_utc(unix_now());
        let slot_id = field_str(&assignment, "slot_id")?;
        let slot_generation = field_u64(&assignment, "slot_generation")?;
        let assignment_id = field_str(&assignment, "assignment_id")?;
        let artifact = json!({
            "artifact_id": derived_uuid(&format!("spike-pool:artifact:{package_id}")),
            "assignment_id": assignment_id,
            "benchmark_id": benchmark_id,
            "kind": "PACKAGE",
            "backend": "FILESYSTEM",
            "container": self.root.display().to_string(),
            "object_key": object_key,
            "provider_version": Value::Null,
            "format_version": "proof-material-v1",
            "media_type": field_str(&declaration, "media_type")?,
            "sha256": declared_sha,
            "provider_checksum": Value::Null,
            "compressed_size": declared_size,
            "uncompressed_size": field_u64(&declaration, "uncompressed_size_bytes")?,
            "manifest_sha256": field_str(&declaration, "manifest_sha256")?,
            "lifecycle_state": "ACCEPTED",
            "accepted_at": accepted_at,
        });
        let mut receipt = json!({
            "receipt_id": derived_uuid(&format!("spike-pool:receipt:{package_id}")),
            "package_id": package_id,
            "upload_id": upload_id,
            "assignment_id": assignment_id,
            "benchmark_id": benchmark_id,
            "package_sha256": declared_sha,
            "accepted_at": accepted_at,
            "member_may_delete_package": true,
            "slot_released": true,
            "slot_id": slot_id,
            "slot_generation": slot_generation,
        });
        // Issued-identity binding (issue #32, member_protocol §2): when the
        // registered assignment carries pool-issued member/worker identities,
        // the immutable receipt records the full ownership chain.
        for key in ["member_id", "worker_id"] {
            if let Some(v) = assignment.get(key) {
                receipt[key] = v.clone();
            }
        }
        let acceptance = json!({
            "artifact": artifact,
            "receipt": receipt,
            "assignment_state": "PACKAGE_DURABLY_ACCEPTED",
            "slot_release": { "slot_id": slot_id, "generation": slot_generation },
        });
        write_doc_atomic(&acceptance_path, &acceptance)?;
        let commit_us = t_commit.elapsed().as_micros();

        let receipt_json = serde_json::to_string(&receipt).map_err(storage)?;
        Ok(FinalizeOutcome {
            receipt_json,
            fresh: true,
            verify_us: Some(verify_us),
            publish_us: Some(publish_us),
            commit_us: Some(commit_us),
            peak_bytes_under_root: peak_bytes,
        })
    }

    /// Bounded structural touch on the verified compressed stream: declared
    /// uncompressed size and manifest SHA-256 must hold. (Full structural
    /// acceptance per member_protocol §12 is S4 scope.)
    fn structural_touch(&self, declaration: &Value, package: impl std::io::Read) -> PResult<()> {
        let declared_uncompressed = field_u64(declaration, "uncompressed_size_bytes")?;
        let declared_manifest_sha = field_str(declaration, "manifest_sha256")?;
        let mut decoder = zstd::stream::read::Decoder::new(std::io::BufReader::new(package))
            .map_err(|e| err(ErrorCode::StructuralMismatch, format!("zstd: {e}")))?;
        // Spike shortcut: the decompressed tar is buffered (bounded by the
        // declared size + 1); production streams entry-by-entry (S4 scope).
        let mut tar_bytes = Vec::new();
        let cap = declared_uncompressed + 1;
        std::io::Read::take(&mut decoder, cap)
            .read_to_end(&mut tar_bytes)
            .map_err(|e| err(ErrorCode::StructuralMismatch, format!("decompress: {e}")))?;
        if tar_bytes.len() as u64 != declared_uncompressed {
            return Err(err(
                ErrorCode::StructuralMismatch,
                format!(
                    "uncompressed {} bytes vs declared {declared_uncompressed}",
                    tar_bytes.len()
                ),
            ));
        }
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let entries = archive
            .entries()
            .map_err(|e| err(ErrorCode::StructuralMismatch, format!("tar: {e}")))?;
        for entry in entries {
            let mut entry =
                entry.map_err(|e| err(ErrorCode::StructuralMismatch, format!("tar entry: {e}")))?;
            let is_manifest = entry
                .path()
                .ok()
                .is_some_and(|p| p.as_ref() == Path::new("manifest.json"));
            if is_manifest {
                let mut manifest = Vec::new();
                entry.read_to_end(&mut manifest).map_err(|e| {
                    err(ErrorCode::StructuralMismatch, format!("manifest read: {e}"))
                })?;
                if sha256_hex(&manifest) != declared_manifest_sha {
                    return Err(err(
                        ErrorCode::StructuralMismatch,
                        "manifest SHA-256 does not match declaration",
                    ));
                }
                return Ok(());
            }
        }
        Err(err(
            ErrorCode::StructuralMismatch,
            "no manifest.json in archive",
        ))
    }

    // -- slot fold and re-offer ---------------------------------------------

    /// The package already durably accepted for an assignment, if any — a
    /// fold over the durable acceptance records, like `release_count`
    /// (member_protocol §11: only one package can become durably accepted
    /// for an assignment).
    pub fn accepted_package_for_assignment(&self, assignment_id: &str) -> PResult<Option<String>> {
        validate_id("assignment_id", assignment_id)?;
        let dir = self.state_path("acceptance");
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(storage(e)),
        };
        for entry in entries {
            let entry = entry.map_err(storage)?;
            let Some(doc) = read_doc(&entry.path())? else {
                continue;
            };
            let receipt = doc.get("receipt").cloned().unwrap_or(Value::Null);
            if receipt.get("assignment_id").and_then(Value::as_str) == Some(assignment_id)
                && let Some(package_id) = receipt.get("package_id").and_then(Value::as_str)
            {
                return Ok(Some(package_id.to_owned()));
            }
        }
        Ok(None)
    }

    /// Number of durable-acceptance records releasing (slot, generation) —
    /// the "released exactly once" evidence.
    pub fn release_count(&self, slot_id: &str, generation: u64) -> PResult<usize> {
        let dir = self.state_path("acceptance");
        let mut n = 0;
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(storage(e)),
        };
        for entry in entries {
            let entry = entry.map_err(storage)?;
            let Some(doc) = read_doc(&entry.path())? else {
                continue;
            };
            let release = doc.get("slot_release").cloned().unwrap_or(Value::Null);
            if release.get("slot_id").and_then(Value::as_str) == Some(slot_id)
                && release.get("generation").and_then(Value::as_u64) == Some(generation)
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Authoritative slot view: a fold over the durable slot document and
    /// the durable-acceptance records — released iff an acceptance record
    /// exists for the slot's current generation.
    pub fn slot_view(&self, slot_id: &str) -> PResult<SlotView> {
        validate_id("slot_id", slot_id)?;
        let doc = read_doc(&self.state_path(&format!("slots/{slot_id}.json")))?
            .ok_or_else(|| err(ErrorCode::SlotUnknown, format!("no slot {slot_id}")))?;
        let generation = field_u64(&doc, "generation")?;
        let released = self.release_count(slot_id, generation)? > 0;
        Ok(SlotView {
            slot_id: slot_id.to_owned(),
            generation,
            occupied: !released,
            assignment_id: field_str(&doc, "assignment_id")?.to_owned(),
        })
    }

    /// Offer the slot again. Admissible only when the current generation has
    /// been released by a durable acceptance; bumps the generation.
    pub fn offer_slot(&self, slot_id: &str, assignment_id: &str) -> PResult<u64> {
        validate_id("assignment_id", assignment_id)?;
        let view = self.slot_view(slot_id)?;
        if view.occupied {
            return Err(err(
                ErrorCode::SlotOccupied,
                format!(
                    "slot {slot_id} generation {} not released by a durable acceptance",
                    view.generation
                ),
            ));
        }
        let next = view.generation + 1;
        write_doc_atomic(
            &self.state_path(&format!("slots/{slot_id}.json")),
            &json!({
                "slot_id": slot_id,
                "generation": next,
                "state": "RESERVED",
                "assignment_id": assignment_id,
            }),
        )?;
        Ok(next)
    }

    /// The committed acceptance record (artifact reference + receipt +
    /// assignment state), if any.
    pub fn acceptance(&self, package_id: &str) -> PResult<Option<Value>> {
        validate_id("package_id", package_id)?;
        read_doc(&self.state_path(&format!("acceptance/{package_id}.json")))
    }

    fn state_path(&self, rel: &str) -> PathBuf {
        self.root.join("state").join(rel)
    }
}

fn committed_offset(ranges: &[RangeRec]) -> u64 {
    ranges.last().map_or(0, |r| r.offset + r.length)
}

/// Sequential reader over the committed quarantine chunks of an upload, so
/// verification and publication stream instead of buffering the package
/// (declared packages may be up to 1 GiB, member_protocol §10.3).
struct ChunkStream<'a> {
    pool: &'a Pool,
    upload_id: &'a str,
    ranges: &'a [RangeRec],
    next: usize,
    current: Option<File>,
}

impl<'a> ChunkStream<'a> {
    fn new(pool: &'a Pool, upload_id: &'a str, ranges: &'a [RangeRec]) -> ChunkStream<'a> {
        ChunkStream {
            pool,
            upload_id,
            ranges,
            next: 0,
            current: None,
        }
    }
}

impl std::io::Read for ChunkStream<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(f) = self.current.as_mut() {
                let n = f.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.current = None;
            }
            if self.next >= self.ranges.len() {
                return Ok(0);
            }
            let rec = &self.ranges[self.next];
            self.next += 1;
            self.current = Some(File::open(self.pool.chunk_path(self.upload_id, rec))?);
        }
    }
}

/// Stream a reader to the end, returning (total bytes, SHA-256 hex).
fn stream_sha256(mut r: impl std::io::Read) -> std::io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut total: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
