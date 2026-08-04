//! Issue #12 acceptance (phase S3): resumable chunked upload into
//! quarantine with a durable committed-range ledger, the ordered
//! durable-acceptance saga with its §8.2 crash matrix, byte-identical
//! immutable receipts, slot re-offer after durable acceptance, and rejection
//! of corrupt/truncated inputs with quarantine intact.
//!
//! Deterministic and offline: the package under test is built from the
//! golden fixture (`fixtures/benchmark-artifact/v1/cases/golden`) exactly as
//! the S2 member library builds it; the compressed digest is computed from
//! the produced bytes (run-scoped digest — spike plan §8 Q4).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use serde_json::{Value, json};
use spike::member::{build_tar, compress_zstd, sha256_hex};
use spike::pool::{CrashPoint, ErrorCode, Pool};

const BENCHMARK_ID: &str = "511ca6ea841e0c82fd8ee83959b31e99";
const PACKAGE_ID: &str = "d6efe693-e2db-477a-ac74-c865f728df45";
const SLOT_ID: &str = "1bfebb66-124a-487e-9e32-e13667d38735";
const ASSIGNMENT_ID: &str = "98259cdf-ac20-42d9-a284-2169d6c6a9f1";
// The golden package compresses to ~2 KiB, so tests chunk at 512 bytes to
// exercise multiple committed ranges.
const CHUNK: usize = 512;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/benchmark-artifact/v1/cases/golden")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(golden_dir().join(name)).expect(name)
}

fn unhex_lines(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8(data.to_vec()).expect("utf8");
    let joined: String = text.split_whitespace().collect();
    (0..joined.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&joined[i..i + 2], 16).expect("hex"))
        .collect()
}

/// A real proof-material package built from the golden fixture members
/// (tar in mandated order, one zstd frame), plus its true declaration.
struct Package {
    bytes: Vec<u8>,
    manifest_sha256: String,
    uncompressed_size: u64,
}

fn build_package() -> Package {
    let manifest = read("manifest.json");
    let qualities = unhex_lines(&read("qualities.i32le.hex"));
    let leaf_hashes = unhex_lines(&read("leaf-hashes.bin.hex"));
    let outputs = read("outputs.ndjson");
    let tar = build_tar(&manifest, &qualities, &leaf_hashes, &outputs).expect("tar");
    let bytes = compress_zstd(&tar, 3).expect("zstd");
    Package {
        manifest_sha256: sha256_hex(&manifest),
        uncompressed_size: tar.len() as u64,
        bytes,
    }
}

fn assignment_digest() -> String {
    sha256_hex(b"spike-s3-assignment")
}

fn declaration(p: &Package) -> Value {
    json!({
        "package_id": PACKAGE_ID,
        "assignment_digest": assignment_digest(),
        "media_type": "application/vnd.tig-pool.proof-material-v1.tar+zstd",
        "compressed_size_bytes": p.bytes.len() as u64,
        "uncompressed_size_bytes": p.uncompressed_size,
        "manifest_sha256": p.manifest_sha256,
        "package_sha256": sha256_hex(&p.bytes),
    })
}

fn assignment() -> Value {
    json!({
        "assignment_digest": assignment_digest(),
        "assignment_id": ASSIGNMENT_ID,
        "benchmark_id": BENCHMARK_ID,
        "slot_id": SLOT_ID,
        "slot_generation": 1u64,
        "network": "testnet",
    })
}

fn fresh_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spike-pool-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn pool_with_assignment(root: &PathBuf) -> Pool {
    let pool = Pool::open(root).expect("open pool");
    pool.register_assignment(&assignment()).expect("assignment");
    pool
}

fn upload_all(pool: &Pool, upload_id: &str, bytes: &[u8], chunk: usize) -> u64 {
    let mut offset = 0u64;
    while (offset as usize) < bytes.len() {
        let end = (offset as usize + chunk).min(bytes.len());
        let c = &bytes[offset as usize..end];
        offset = pool
            .put_chunk(upload_id, offset, &sha256_hex(c), c)
            .expect("chunk");
    }
    offset
}

fn accepted_key(root: &std::path::Path, sha: &str) -> PathBuf {
    root.join(format!(
        "accepted/testnet/{BENCHMARK_ID}/{PACKAGE_ID}/{sha}.tar.zst"
    ))
}

// ---------------------------------------------------------------------------
// Criterion 1: resumable upload, durable committed ranges, idempotent retry
// ---------------------------------------------------------------------------

#[test]
fn interrupted_upload_resumes_from_last_durable_offset() {
    let root = fresh_root("resume");
    let pkg = build_package();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&declaration(&pkg)).expect("create");
    assert!(!session.resumed);
    assert_eq!(session.committed_offset, 0);

    let c0 = &pkg.bytes[0..CHUNK];
    let committed = pool
        .put_chunk(&session.upload_id, 0, &sha256_hex(c0), c0)
        .expect("chunk 0");
    assert_eq!(committed, CHUNK as u64);

    // Kill the writer between the chunk object write and the range commit.
    let c1 = &pkg.bytes[CHUNK..2 * CHUNK];
    let e = pool
        .put_chunk_crash(
            &session.upload_id,
            CHUNK as u64,
            &sha256_hex(c1),
            c1,
            Some(CrashPoint::AfterChunkObjectWrite),
        )
        .expect_err("simulated crash");
    assert_eq!(e.code, ErrorCode::SimulatedCrash);

    // Restart: a new pool instance over the same root sees only durable state.
    drop(pool);
    let pool = Pool::open(&root).expect("reopen");
    let status = pool.upload_status(&session.upload_id).expect("status");
    // The acknowledged offset never ran ahead of the durable ledger.
    assert_eq!(status["committed_offset"].as_u64(), Some(CHUNK as u64));

    // Resuming the same declaration returns the same live upload and offset.
    let resumed = pool.create_upload(&declaration(&pkg)).expect("resume");
    assert!(resumed.resumed);
    assert_eq!(resumed.upload_id, session.upload_id);
    assert_eq!(resumed.committed_offset, CHUNK as u64);

    // An identical retry adopts the orphaned chunk object idempotently.
    let committed = pool
        .put_chunk(&session.upload_id, CHUNK as u64, &sha256_hex(c1), c1)
        .expect("identical retry adopts");
    assert_eq!(committed, 2 * CHUNK as u64);

    // A conflicting retry of an already-committed range fails.
    let mut wrong = c0.to_vec();
    wrong[0] ^= 0xff;
    let e = pool
        .put_chunk(&session.upload_id, 0, &sha256_hex(&wrong), &wrong)
        .expect_err("conflicting retry");
    assert_eq!(e.code, ErrorCode::ChunkConflict);
}

#[test]
fn upload_guards_offset_checksum_and_declared_size() {
    let root = fresh_root("guards");
    let pkg = build_package();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&declaration(&pkg)).expect("create");

    // A higher offset returns OFFSET_MISMATCH with the committed offset.
    let c0 = &pkg.bytes[0..CHUNK];
    let e = pool
        .put_chunk(&session.upload_id, CHUNK as u64, &sha256_hex(c0), c0)
        .expect_err("ahead of committed");
    assert_eq!(e.code, ErrorCode::OffsetMismatch);
    assert_eq!(e.committed_offset, Some(0));

    // Chunk bytes must match the declared chunk checksum.
    let e = pool
        .put_chunk(&session.upload_id, 0, &sha256_hex(b"other"), c0)
        .expect_err("bad checksum");
    assert_eq!(e.code, ErrorCode::ChunkChecksumMismatch);
    assert_eq!(
        pool.upload_status(&session.upload_id).expect("status")["committed_offset"].as_u64(),
        Some(0)
    );

    // Bytes beyond the declared package size are never accepted.
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);
    let extra = [0u8; 16];
    let e = pool
        .put_chunk(
            &session.upload_id,
            pkg.bytes.len() as u64,
            &sha256_hex(&extra),
            &extra,
        )
        .expect_err("beyond declared size");
    assert_eq!(e.code, ErrorCode::BeyondDeclaredSize);

    // Same package_id with a changed declaration conflicts.
    let mut changed = declaration(&pkg);
    changed["compressed_size_bytes"] = json!(pkg.bytes.len() as u64 + 1);
    let e = pool
        .create_upload(&changed)
        .expect_err("changed declaration");
    assert_eq!(e.code, ErrorCode::DeclarationConflict);
}

// ---------------------------------------------------------------------------
// Criterion 2: ordered saga — verify, publish, then commit artifact + receipt
// ---------------------------------------------------------------------------

#[test]
fn finalization_publishes_then_commits_artifact_reference_and_receipt() {
    let root = fresh_root("saga");
    let pkg = build_package();
    let decl = declaration(&pkg);
    let sha = decl["package_sha256"].as_str().unwrap().to_owned();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&decl).expect("create");
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);

    let outcome = pool.finalize(PACKAGE_ID).expect("finalize");
    assert!(outcome.fresh);

    // The accepted object exists at its deterministic key, byte-identical,
    // outside the state ledger (package persisted outside the database).
    let object = std::fs::read(accepted_key(&root, &sha)).expect("accepted object");
    assert_eq!(object, pkg.bytes);

    // The committed artifact reference carries the §8.3 fields.
    let acceptance = pool
        .acceptance(PACKAGE_ID)
        .expect("read")
        .expect("committed");
    let a = &acceptance["artifact"];
    assert_eq!(a["kind"], "PACKAGE");
    assert_eq!(a["backend"], "FILESYSTEM");
    assert_eq!(a["lifecycle_state"], "ACCEPTED");
    assert_eq!(a["sha256"].as_str(), Some(sha.as_str()));
    assert_eq!(a["compressed_size"].as_u64(), Some(pkg.bytes.len() as u64));
    assert_eq!(a["uncompressed_size"].as_u64(), Some(pkg.uncompressed_size));
    assert_eq!(
        a["manifest_sha256"].as_str(),
        Some(pkg.manifest_sha256.as_str())
    );
    assert_eq!(
        a["object_key"].as_str(),
        Some(format!("accepted/testnet/{BENCHMARK_ID}/{PACKAGE_ID}/{sha}.tar.zst").as_str())
    );
    assert_eq!(acceptance["assignment_state"], "PACKAGE_DURABLY_ACCEPTED");

    // The receipt binds the package, slot release, and deletion permission.
    let receipt: Value = serde_json::from_str(&outcome.receipt_json).unwrap();
    assert_eq!(receipt["package_id"].as_str(), Some(PACKAGE_ID));
    assert_eq!(receipt["package_sha256"].as_str(), Some(sha.as_str()));
    assert_eq!(receipt["member_may_delete_package"], true);
    assert_eq!(receipt["slot_released"], true);
}

// ---------------------------------------------------------------------------
// Criterion 3: §8.2 crash matrix
// ---------------------------------------------------------------------------

#[test]
fn crash_after_publication_recovers_and_completes_same_transaction() {
    let root = fresh_root("crash-pub");
    let pkg = build_package();
    let decl = declaration(&pkg);
    let sha = decl["package_sha256"].as_str().unwrap().to_owned();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&decl).expect("create");
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);

    // Crash after the accepted object is published, before the state commit.
    let e = pool
        .finalize_crash(PACKAGE_ID, Some(CrashPoint::AfterPublication))
        .expect_err("simulated crash");
    assert_eq!(e.code, ErrorCode::SimulatedCrash);
    assert!(accepted_key(&root, &sha).exists());
    assert!(pool.acceptance(PACKAGE_ID).expect("read").is_none());

    // Restart: recovery discovers the deterministic object, verifies it, and
    // completes the same transaction.
    drop(pool);
    let pool = Pool::open(&root).expect("reopen");
    let outcome = pool.finalize(PACKAGE_ID).expect("recovery finalize");
    assert!(outcome.fresh);
    let object = std::fs::read(accepted_key(&root, &sha)).expect("object");
    assert_eq!(object, pkg.bytes);
    assert!(pool.acceptance(PACKAGE_ID).expect("read").is_some());
}

#[test]
fn retried_finalization_returns_byte_identical_receipt() {
    let root = fresh_root("receipt");
    let pkg = build_package();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&declaration(&pkg)).expect("create");
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);

    let first = pool.finalize(PACKAGE_ID).expect("finalize");
    assert!(first.fresh);

    // Crash after commit, before the response: the retry — from a fresh
    // process — returns the SAME immutable receipt, byte-identical.
    drop(pool);
    let pool = Pool::open(&root).expect("reopen");
    let second = pool.finalize(PACKAGE_ID).expect("retry");
    assert!(!second.fresh);
    assert_eq!(first.receipt_json, second.receipt_json);
    let third = pool.finalize(PACKAGE_ID).expect("retry 2");
    assert_eq!(first.receipt_json, third.receipt_json);
}

// ---------------------------------------------------------------------------
// Criterion 4: slot re-offer after durable acceptance
// ---------------------------------------------------------------------------

#[test]
fn durable_acceptance_releases_slot_exactly_once_and_reoffer_is_admissible() {
    let root = fresh_root("slot");
    let pkg = build_package();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&declaration(&pkg)).expect("create");
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);

    // Before durable acceptance the slot is occupied and cannot be re-offered
    // (member_protocol §9: RECEIVED never releases the slot).
    let view = pool.slot_view(SLOT_ID).expect("view");
    assert!(view.occupied);
    assert_eq!(view.generation, 1);
    let e = pool
        .offer_slot(SLOT_ID, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .expect_err("occupied");
    assert_eq!(e.code, ErrorCode::SlotOccupied);

    // Durable acceptance releases the slot exactly once.
    pool.finalize(PACKAGE_ID).expect("finalize");
    assert_eq!(pool.release_count(SLOT_ID, 1).expect("count"), 1);
    let view = pool.slot_view(SLOT_ID).expect("view");
    assert!(!view.occupied);

    // A retried finalization does not release it again.
    pool.finalize(PACKAGE_ID).expect("retry");
    assert_eq!(pool.release_count(SLOT_ID, 1).expect("count"), 1);

    // The benchmark is still non-terminal in pool state: durably accepted,
    // commitment/proof/TIG-terminal stages have not happened (S4 scope).
    let acceptance = pool.acceptance(PACKAGE_ID).expect("read").expect("doc");
    assert_eq!(acceptance["assignment_state"], "PACKAGE_DURABLY_ACCEPTED");

    // A second offer on the same slot is admissible immediately.
    let next = pool
        .offer_slot(SLOT_ID, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .expect("re-offer");
    assert_eq!(next, 2);
    let view = pool.slot_view(SLOT_ID).expect("view");
    assert!(view.occupied);
    assert_eq!(view.generation, 2);
}

// ---------------------------------------------------------------------------
// Criterion 5: corrupt/truncated inputs abort with quarantine intact
// ---------------------------------------------------------------------------

fn quarantine_chunk_count(root: &std::path::Path, upload_id: &str) -> usize {
    std::fs::read_dir(root.join("quarantine").join(upload_id))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn wrong_declared_package_sha256_aborts_before_publication() {
    let root = fresh_root("bad-sha");
    let pkg = build_package();
    let mut decl = declaration(&pkg);
    let wrong_sha = sha256_hex(b"not the package");
    decl["package_sha256"] = json!(wrong_sha);
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&decl).expect("create");
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);

    let e = pool.finalize(PACKAGE_ID).expect_err("sha mismatch");
    assert_eq!(e.code, ErrorCode::PackageSha256Mismatch);

    // Publication never happened and no receipt was committed; the
    // quarantine ledger and chunk objects remain intact and resumable.
    assert!(!accepted_key(&root, &wrong_sha).exists());
    assert!(!root.join("accepted/testnet").exists());
    assert!(pool.acceptance(PACKAGE_ID).expect("read").is_none());
    let expected_chunks = pkg.bytes.len().div_ceil(CHUNK);
    assert_eq!(
        quarantine_chunk_count(&root, &session.upload_id),
        expected_chunks
    );
    assert_eq!(
        pool.upload_status(&session.upload_id).expect("status")["committed_offset"].as_u64(),
        Some(pkg.bytes.len() as u64)
    );
}

#[test]
fn truncated_upload_fails_ledger_completeness_with_quarantine_intact() {
    let root = fresh_root("truncated");
    let pkg = build_package();
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&declaration(&pkg)).expect("create");
    // Upload all but the final chunk (the fixtures' truncated-upload case).
    let truncated = &pkg.bytes[..pkg.bytes.len() - (pkg.bytes.len() % CHUNK).max(1)];
    let committed = upload_all(&pool, &session.upload_id, truncated, CHUNK);
    assert!(committed < pkg.bytes.len() as u64);

    let e = pool.finalize(PACKAGE_ID).expect_err("incomplete");
    assert_eq!(e.code, ErrorCode::LedgerIncomplete);
    assert!(!root.join("accepted/testnet").exists());
    assert!(pool.acceptance(PACKAGE_ID).expect("read").is_none());
    // Quarantine intact: the member resumes from the durable offset.
    let resumed = pool.create_upload(&declaration(&pkg)).expect("resume");
    assert_eq!(resumed.committed_offset, committed);
}

#[test]
fn wrong_declared_manifest_sha256_fails_structural_touch() {
    let root = fresh_root("bad-manifest");
    let pkg = build_package();
    let mut decl = declaration(&pkg);
    decl["manifest_sha256"] = json!(sha256_hex(b"not the manifest"));
    let pool = pool_with_assignment(&root);
    let session = pool.create_upload(&decl).expect("create");
    upload_all(&pool, &session.upload_id, &pkg.bytes, CHUNK);

    let e = pool.finalize(PACKAGE_ID).expect_err("manifest mismatch");
    assert_eq!(e.code, ErrorCode::StructuralMismatch);
    assert!(!root.join("accepted/testnet").exists());
    assert!(pool.acceptance(PACKAGE_ID).expect("read").is_none());
}
