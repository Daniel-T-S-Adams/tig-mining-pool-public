//! Spike S4 controller library: benchmark commitment, sampled-proof
//! construction, and retention-conditioned deletion — everything between
//! durable package acceptance (S3) and a confirmed ACTIVE benchmark.
//!
//! Contracts: `docs/tig_integration.md` §6.2–§6.3 (write bodies), §7
//! (confirmation mapping), `docs/mining_system.md` §9 (retention),
//! `docs/architecture.md` §13 invariants 4 and 5. Plan:
//! `docs/plans/protocol-spike.md` phase S4 (issue #13).
//!
//! The rules under test, enforced here in code:
//!
//! - Proofs are constructed SOLELY from the pool's retained accepted package:
//!   [`load_accepted_package`] refuses to load anything that is not covered by
//!   a committed durable-acceptance record, recomputes the whole-package
//!   SHA-256 before use, and re-verifies every internal consistency rule
//!   (leaf hashes, Merkle root, nonce coverage) against the manifest.
//! - The commitment body and the proof payload are derived from that loaded
//!   package only — no member involvement after acceptance.
//! - Deletion of the retained package is gated on the documented retention
//!   condition (`mining_system.md` §9): the benchmark observed in
//!   `block.data.active_ids.benchmark`.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::member::{
    OutputRecord, branch_hex, branch_root, leaf_hashes_bytes, merkle_branch, merkle_root,
    rfc3339_utc, sha256_hex,
};
use crate::pool::Pool;

/// The retained accepted package, fully re-verified from the accepted object.
#[derive(Debug)]
pub struct AcceptedPackage {
    pub package_id: String,
    pub benchmark_id: String,
    /// Whole-package SHA-256, recomputed from the accepted object bytes and
    /// verified against the durable-acceptance record.
    pub package_sha256: String,
    pub object_path: PathBuf,
    pub acceptance: Value,
    pub manifest: Value,
    pub num_nonces: u64,
    pub qualities: Vec<i32>,
    pub records: Vec<OutputRecord>,
    pub leaves: Vec<[u8; 32]>,
    /// Recomputed Merkle root (verified equal to the manifest root).
    pub root_hex: String,
}

fn manifest_str<'v>(manifest: &'v Value, ptr: &str) -> Result<&'v str> {
    manifest
        .pointer(ptr)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("package manifest missing {ptr}"))
}

/// Load and re-verify the retained accepted package for `package_id`.
///
/// Refuses (with no side effects) unless the pool holds a committed
/// durable-acceptance record — this is the code path that makes architecture
/// invariant 4 hold for commitment and keeps proof construction bound to the
/// accepted bytes.
pub fn load_accepted_package(pool: &Pool, package_id: &str) -> Result<AcceptedPackage> {
    let acceptance = pool
        .acceptance(package_id)
        .map_err(|e| anyhow!("reading acceptance record: {e}"))?
        .ok_or_else(|| {
            anyhow!(
                "invariant 4 refusal: no durable acceptance record for package {package_id}; \
                 the retained accepted package does not exist"
            )
        })?;
    let artifact = acceptance
        .get("artifact")
        .ok_or_else(|| anyhow!("acceptance record missing artifact reference"))?;
    let object_key = artifact
        .get("object_key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("artifact reference missing object_key"))?;
    let declared_sha = artifact
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("artifact reference missing sha256"))?;
    let benchmark_id = artifact
        .get("benchmark_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("artifact reference missing benchmark_id"))?
        .to_owned();
    let uncompressed_size = artifact
        .get("uncompressed_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("artifact reference missing uncompressed_size"))?;

    let object_path = pool.root().join(object_key);
    let bytes = std::fs::read(&object_path)
        .with_context(|| format!("reading accepted object {}", object_path.display()))?;

    // Recompute the whole-package SHA-256 before any use of the contents.
    let package_sha256 = sha256_hex(&bytes);
    if package_sha256 != declared_sha {
        bail!(
            "retained package integrity failure: accepted object hashes to {package_sha256}, \
             durable acceptance recorded {declared_sha} — refusing to use it"
        );
    }

    // Decompress (bounded by the recorded uncompressed size) and read the
    // four archive members.
    let mut decoder = zstd::stream::read::Decoder::new(&bytes[..]).context("zstd decoder")?;
    let mut tar_bytes = Vec::new();
    std::io::Read::take(&mut decoder, uncompressed_size + 1)
        .read_to_end(&mut tar_bytes)
        .context("decompressing accepted package")?;
    if tar_bytes.len() as u64 != uncompressed_size {
        bail!(
            "accepted package decompresses to {} bytes, acceptance recorded {uncompressed_size}",
            tar_bytes.len()
        );
    }
    let mut manifest_data: Option<Vec<u8>> = None;
    let mut qualities_data: Option<Vec<u8>> = None;
    let mut leaf_hash_data: Option<Vec<u8>> = None;
    let mut outputs_data: Option<Vec<u8>> = None;
    let mut archive = tar::Archive::new(tar_bytes.as_slice());
    for entry in archive.entries().context("tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let name = entry
            .path()
            .context("tar entry path")?
            .to_string_lossy()
            .into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).context("tar entry read")?;
        match name.as_str() {
            "manifest.json" => manifest_data = Some(data),
            "qualities.i32le" => qualities_data = Some(data),
            "leaf-hashes.bin" => leaf_hash_data = Some(data),
            "outputs.ndjson" => outputs_data = Some(data),
            other => bail!("unexpected archive member {other}"),
        }
    }
    let manifest_data = manifest_data.ok_or_else(|| anyhow!("package missing manifest.json"))?;
    let qualities_data =
        qualities_data.ok_or_else(|| anyhow!("package missing qualities.i32le"))?;
    let leaf_hash_data =
        leaf_hash_data.ok_or_else(|| anyhow!("package missing leaf-hashes.bin"))?;
    let outputs_data = outputs_data.ok_or_else(|| anyhow!("package missing outputs.ndjson"))?;

    let manifest: Value =
        serde_json::from_slice(&manifest_data).context("parsing package manifest")?;
    if manifest_str(&manifest, "/benchmark_id")? != benchmark_id {
        bail!("package manifest benchmark_id does not match the acceptance record");
    }
    let num_nonces = manifest
        .pointer("/nonce_range/num_nonces")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("manifest missing nonce_range.num_nonces"))?;

    // Qualities: exactly num_nonces little-endian i32s.
    if qualities_data.len() as u64 != num_nonces * 4 {
        bail!(
            "qualities.i32le is {} bytes, expected {} for {num_nonces} nonces",
            qualities_data.len(),
            num_nonces * 4
        );
    }
    let qualities: Vec<i32> = qualities_data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|arr| i32::from_le_bytes(*arr))
        .collect();

    // Output records: one per nonce, ascending coverage of [0, num_nonces).
    let outputs_text = std::str::from_utf8(&outputs_data).context("outputs.ndjson utf-8")?;
    let mut records = Vec::new();
    for (i, line) in outputs_text.lines().enumerate() {
        let v: Value = serde_json::from_str(line)
            .with_context(|| format!("parsing outputs.ndjson line {i}"))?;
        let record = OutputRecord::from_wire_json(&v)?;
        if record.nonce != i as u64 {
            bail!("outputs.ndjson line {i} carries nonce {}", record.nonce);
        }
        records.push(record);
    }
    if records.len() as u64 != num_nonces {
        bail!(
            "outputs.ndjson has {} records, expected {num_nonces}",
            records.len()
        );
    }

    // Leaf hashes and root must reproduce from the records.
    let leaves = records
        .iter()
        .map(OutputRecord::leaf_hash)
        .collect::<Result<Vec<_>>>()?;
    if leaf_hashes_bytes(&leaves) != leaf_hash_data {
        bail!("leaf-hashes.bin does not reproduce from the output records");
    }
    let root = merkle_root(&leaves)?;
    let root_hex = crate::member::hex(&root);
    if manifest_str(&manifest, "/merkle/root")? != root_hex {
        bail!("recomputed Merkle root does not match the package manifest root");
    }

    Ok(AcceptedPackage {
        package_id: package_id.to_owned(),
        benchmark_id,
        package_sha256,
        object_path,
        acceptance,
        manifest,
        num_nonces,
        qualities,
        records,
        leaves,
        root_hex,
    })
}

/// The `submit-benchmark` body (`tig_integration.md` §6.2) for a non-stopped
/// benchmark, built solely from the retained accepted package: `merkle_root`
/// is the recomputed (manifest-verified) root, `solution_quality` has exactly
/// `num_nonces` signed entries in ascending nonce order.
pub fn commitment_body(pkg: &AcceptedPackage) -> Result<Value> {
    if pkg.root_hex.len() != 64
        || !pkg
            .root_hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        bail!("merkle root is not 64 lowercase hex characters");
    }
    Ok(json!({
        "benchmark_id": pkg.benchmark_id,
        "stopped": false,
        "merkle_root": pkg.root_hex,
        "solution_quality": pkg.qualities,
    }))
}

/// Build the canonical `submit-proof` payload (`tig_integration.md` §6.3) for
/// the confirmed sampled nonces, solely from the retained accepted package.
///
/// Wire encoding pinned from upstream commit `ad08d1ea…`
/// (`tig-utils/src/merkle_tree.rs`, verified against the Python benchmarker's
/// `MerkleBranch.to_str`): each proof is `{leaf, branch}` where `leaf` is the
/// `OutputData` with bare u64 integers and `branch` is the concatenation of
/// `{depth:02x}{sibling_hash:064x}` per element. Every branch is re-verified
/// against the submitted root before the payload is returned.
pub fn build_proof_payload(pkg: &AcceptedPackage, sampled_nonces: &[u64]) -> Result<Value> {
    if sampled_nonces.is_empty() {
        bail!("no sampled nonces to prove");
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut proofs = Vec::new();
    for &nonce in sampled_nonces {
        if nonce >= pkg.num_nonces {
            bail!(
                "sampled nonce {nonce} outside the package nonce range [0, {})",
                pkg.num_nonces
            );
        }
        if !seen.insert(nonce) {
            bail!("sampled nonce {nonce} listed more than once");
        }
        let idx = usize::try_from(nonce).context("nonce exceeds usize")?;
        let record = &pkg.records[idx];
        let branch = merkle_branch(&pkg.leaves, idx)?;
        // Each branch must resolve the leaf back to the submitted root.
        let recomputed = branch_root(&pkg.leaves[idx], idx, &branch)?;
        if crate::member::hex(&recomputed) != pkg.root_hex {
            bail!("branch for nonce {nonce} does not resolve to the submitted merkle root");
        }
        proofs.push(json!({
            "leaf": {
                "nonce": record.nonce,
                "runtime_signature": record.runtime_signature,
                "fuel_consumed": record.fuel_consumed,
                "solution": record.solution,
                "cpu_arch": record.cpu_arch,
            },
            "branch": branch_hex(&branch),
        }));
    }
    Ok(json!({
        "benchmark_id": pkg.benchmark_id,
        "merkle_proofs": proofs,
    }))
}

/// Apply the v0 retention rule (`mining_system.md` §9): the retained package
/// may be deleted once the benchmark is observed ACTIVE — its ID present in
/// `block.data.active_ids.benchmark` of a current block. Refuses otherwise.
/// Returns the durable retention record (condition, evidence, deletion
/// result); the caller persists it.
pub fn apply_retention(pool: &Pool, package_id: &str, block: &Value) -> Result<Value> {
    let acceptance = pool
        .acceptance(package_id)
        .map_err(|e| anyhow!("reading acceptance record: {e}"))?
        .ok_or_else(|| anyhow!("no durable acceptance record for package {package_id}"))?;
    let artifact = acceptance
        .get("artifact")
        .ok_or_else(|| anyhow!("acceptance record missing artifact reference"))?;
    let benchmark_id = artifact
        .get("benchmark_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("artifact reference missing benchmark_id"))?;
    let object_key = artifact
        .get("object_key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("artifact reference missing object_key"))?;

    let active: Vec<&str> = block
        .pointer("/data/active_ids/benchmark")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !active.contains(&benchmark_id) {
        bail!(
            "retention condition not satisfied: benchmark {benchmark_id} is not in \
             block.data.active_ids.benchmark of the presented block — refusing to delete \
             the retained package"
        );
    }
    let block_id = block.get("id").and_then(Value::as_str).unwrap_or("?");
    let height = block
        .pointer("/details/height")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let object_path = pool.root().join(object_key);
    let existed = object_path.exists();
    if existed {
        std::fs::remove_file(&object_path)
            .with_context(|| format!("deleting {}", object_path.display()))?;
    }
    let deleted = !object_path.exists();
    Ok(json!({
        "retention_rule": "mining_system.md §9 v0: delete large proof material once TIG confirms the benchmark ACTIVE",
        "condition": "benchmark_id in block.data.active_ids.benchmark",
        "evidence": { "block_id": block_id, "block_height": height, "benchmark_id": benchmark_id },
        "package_id": package_id,
        "object_key": object_key,
        "object_existed": existed,
        "deleted": deleted,
        "deleted_at": rfc3339_utc(unix_now()),
    }))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Peak resident set size of this process in KiB (`VmHWM` from
/// `/proc/self/status`), for the proof-construction memory measurement.
pub fn vm_hwm_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|l| {
        l.strip_prefix("VmHWM:")?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn vm_hwm_reads_on_linux() {
        assert!(vm_hwm_kib().unwrap_or(0) > 0);
    }

    #[test]
    fn jsonify_of_leaf_is_key_sorted() {
        // The proof leaf rides as bare u64s; canonical serialization sorts keys.
        let v = json!({"nonce": 1u64, "cpu_arch": "arm64"});
        assert_eq!(
            crate::member::jsonify(&v).unwrap(),
            r#"{"cpu_arch":"arm64","nonce":1}"#
        );
    }
}
