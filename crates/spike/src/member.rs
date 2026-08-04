//! Spike member agent library: pure proof-material construction.
//!
//! Contract: `docs/member_protocol.md` §10 (package), §7 (assignment
//! identity/digest); Merkle and leaf-hash conventions pinned from upstream
//! commit `ad08d1ea001a73ff5aab3b556d7f59246fece14e` as transcribed in
//! `fixtures/benchmark-artifact/v1/README.md` and verified against that
//! fixture's golden values by `tests/member_package.rs`.
//!
//! Disposable spike code (`docs/plans/protocol-spike.md` §3). Everything in
//! this module is deterministic and needs neither Docker nor network; the
//! `spike-member` binary owns container execution.

use std::io::Write as _;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Canonical JSON (upstream `jsonify`)
// ---------------------------------------------------------------------------

/// Upstream `tig-utils` `jsonify`: compact serde_json output with object keys
/// sorted ascending, recursively. This workspace's serde_json uses the
/// default BTreeMap-backed map (no `preserve_order` feature anywhere in the
/// lockfile), so serializing a `Value` already yields sorted keys; numbers
/// are emitted exactly (u64 is lossless above 2^53). For the ASCII-only
/// values used on this path it equals RFC 8785.
pub fn jsonify(value: &Value) -> Result<String> {
    serde_json::to_string(value).context("canonical JSON serialization")
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Output records and TIG leaf hashes
// ---------------------------------------------------------------------------

/// One per-nonce output, exactly the fields of upstream `OutputData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputRecord {
    pub nonce: u64,
    pub runtime_signature: u64,
    pub fuel_consumed: u64,
    pub solution: String,
    pub cpu_arch: String,
}

impl OutputRecord {
    /// Parse the JSON written by `tig-runtime --output` (bare u64 integers,
    /// upstream `jsonify(OutputData)`), ignoring any extra keys (the
    /// reference slave appends `quality` to the same file).
    pub fn from_runtime_json(v: &Value) -> Result<OutputRecord> {
        let field = |k: &str| v.get(k).ok_or_else(|| anyhow!("output missing '{k}'"));
        Ok(OutputRecord {
            nonce: field("nonce")?
                .as_u64()
                .ok_or_else(|| anyhow!("nonce not u64"))?,
            runtime_signature: field("runtime_signature")?
                .as_u64()
                .ok_or_else(|| anyhow!("runtime_signature not u64"))?,
            fuel_consumed: field("fuel_consumed")?
                .as_u64()
                .ok_or_else(|| anyhow!("fuel_consumed not u64"))?,
            solution: field("solution")?
                .as_str()
                .ok_or_else(|| anyhow!("solution not a string"))?
                .to_owned(),
            cpu_arch: field("cpu_arch")?
                .as_str()
                .ok_or_else(|| anyhow!("cpu_arch not a string"))?
                .to_owned(),
        })
    }

    /// Parse one member-wire record (`output-record.schema.json`):
    /// `runtime_signature`/`fuel_consumed` are decimal strings on the wire.
    pub fn from_wire_json(v: &Value) -> Result<OutputRecord> {
        let dec = |k: &str| -> Result<u64> {
            v.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("record missing decimal-string '{k}'"))?
                .parse::<u64>()
                .with_context(|| format!("parsing '{k}' as u64"))
        };
        Ok(OutputRecord {
            nonce: v
                .get("nonce")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("record missing u64 'nonce'"))?,
            runtime_signature: dec("runtime_signature")?,
            fuel_consumed: dec("fuel_consumed")?,
            solution: v
                .get("solution")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("record missing 'solution'"))?
                .to_owned(),
            cpu_arch: v
                .get("cpu_arch")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("record missing 'cpu_arch'"))?
                .to_owned(),
        })
    }

    /// The member-wire form (`member_protocol.md` §10.2): u64s as canonical
    /// decimal strings so no JSON implementation can round them.
    pub fn to_wire_value(&self) -> Value {
        json!({
            "nonce": self.nonce,
            "runtime_signature": self.runtime_signature.to_string(),
            "fuel_consumed": self.fuel_consumed.to_string(),
            "solution": self.solution,
            "cpu_arch": self.cpu_arch,
        })
    }

    /// Upstream `OutputData::calc_solution_signature`:
    /// `u64_le(blake3(jsonify(solution))[0..8])` where the solution is
    /// jsonified as a JSON string (quotes and escapes included).
    pub fn solution_signature(&self) -> Result<u64> {
        let canonical = jsonify(&Value::String(self.solution.clone()))?;
        let digest = blake3::hash(canonical.as_bytes());
        let first8: [u8; 8] = digest.as_bytes()[0..8]
            .try_into()
            .context("blake3 digest shorter than 8 bytes")?;
        Ok(u64::from_le_bytes(first8))
    }

    /// Upstream `OutputData -> OutputMetaData -> MerkleHash`: blake3 over the
    /// canonical JSON of the four metadata fields as bare JSON integers.
    pub fn leaf_hash(&self) -> Result<[u8; 32]> {
        let meta = json!({
            "nonce": self.nonce,
            "runtime_signature": self.runtime_signature,
            "fuel_consumed": self.fuel_consumed,
            "solution_signature": self.solution_signature()?,
        });
        Ok(blake3::hash(jsonify(&meta)?.as_bytes()).into())
    }
}

// ---------------------------------------------------------------------------
// Merkle tree (upstream `tig-utils/src/merkle_tree.rs`)
// ---------------------------------------------------------------------------

fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    blake3::hash(&buf).into()
}

/// `MerkleTree::calc_merkle_root`: fold the ordered leaf list pairwise with
/// `blake3(left || right)`; an unpaired trailing node is promoted unchanged.
/// Leaves are NOT padded to the power-of-two capacity.
pub fn merkle_root(leaves: &[[u8; 32]]) -> Result<[u8; 32]> {
    if leaves.is_empty() {
        bail!("merkle root of zero leaves is undefined");
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => hash_pair(l, r),
                [odd] => *odd,
                _ => unreachable!("chunks(2) yields 1..=2 items"),
            })
            .collect();
    }
    level.first().copied().ok_or_else(|| anyhow!("empty level"))
}

/// `MerkleTree::calc_merkle_branch`: ordered `(depth, sibling)` pairs for the
/// leaf at `idx`. Levels where the node is unpaired contribute no element.
pub fn merkle_branch(leaves: &[[u8; 32]], idx: usize) -> Result<Vec<(u8, [u8; 32])>> {
    if idx >= leaves.len() {
        bail!("branch index {idx} out of range ({} leaves)", leaves.len());
    }
    let mut level = leaves.to_vec();
    let mut idx = idx;
    let mut depth: u8 = 0;
    let mut branch = Vec::new();
    while level.len() > 1 {
        let sibling = if idx.is_multiple_of(2) {
            idx + 1
        } else {
            idx - 1
        };
        if sibling < level.len() {
            branch.push((depth, level[sibling]));
        }
        level = level
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => hash_pair(l, r),
                [odd] => *odd,
                _ => unreachable!("chunks(2) yields 1..=2 items"),
            })
            .collect();
        idx /= 2;
        depth = depth.saturating_add(1);
    }
    Ok(branch)
}

/// `MerkleBranch::calc_merkle_root`: recompute the root from a leaf hash, its
/// nonce/index, and the branch.
pub fn branch_root(leaf: &[u8; 32], idx: usize, branch: &[(u8, [u8; 32])]) -> Result<[u8; 32]> {
    let mut root = *leaf;
    let mut idx = idx;
    let mut curr: u8 = 0;
    for (depth, hash) in branch {
        if curr > *depth {
            bail!("invalid branch: depth {depth} after {curr}");
        }
        while curr != *depth {
            idx /= 2;
            curr = curr.saturating_add(1);
        }
        root = if idx.is_multiple_of(2) {
            hash_pair(&root, hash)
        } else {
            hash_pair(hash, &root)
        };
        idx /= 2;
        curr = curr.saturating_add(1);
    }
    Ok(root)
}

/// Branch serialization: `{depth:02x}{hash:064x}` concatenated per element.
pub fn branch_hex(branch: &[(u8, [u8; 32])]) -> String {
    branch
        .iter()
        .map(|(d, h)| format!("{d:02x}{}", hex(h)))
        .collect()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Smallest power of two >= n (the TIG tree capacity for n leaves).
pub fn tree_capacity(n: u64) -> u64 {
    n.max(1).next_power_of_two()
}

// ---------------------------------------------------------------------------
// Package files (member_protocol.md §10.2)
// ---------------------------------------------------------------------------

/// `qualities.i32le`: `num_nonces` signed 32-bit little-endian qualities in
/// ascending nonce order.
pub fn qualities_bytes(qualities: &[i32]) -> Vec<u8> {
    qualities.iter().flat_map(|q| q.to_le_bytes()).collect()
}

/// `leaf-hashes.bin`: `num_nonces` consecutive 32-byte leaf hashes.
pub fn leaf_hashes_bytes(leaves: &[[u8; 32]]) -> Vec<u8> {
    leaves.iter().flatten().copied().collect()
}

/// `outputs.ndjson`: one canonical JSON object and one LF per nonce.
pub fn outputs_ndjson(records: &[OutputRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for r in records {
        out.extend_from_slice(jsonify(&r.to_wire_value())?.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

fn file_entry(name: &str, encoding: &str, data: &[u8], record_count: u64) -> Value {
    json!({
        "name": name,
        "size_bytes": data.len(),
        "sha256": sha256_hex(data),
        "record_count": record_count,
        "encoding": encoding,
    })
}

/// Everything identity-shaped the manifest needs beyond the computed files.
/// The spike has no real enrollment flow, so the UUID identities are derived
/// deterministically from the benchmark id (documented in the run report) —
/// honest stand-ins, not fixtures copied from `fixtures/benchmark-artifact`.
pub struct PackageIdentity {
    pub benchmark_id: String,
    pub assignment_digest: String,
    pub member_id: String,
    pub worker_id: String,
    pub slot_id: String,
    pub assignment_id: String,
    pub package_id: String,
    pub qualification_id: String,
    pub qualification_spec_digest: String,
    pub slot_generation: u64,
    pub compute: Value,
    pub member_agent_version: String,
    pub tig_upstream_commit: String,
    pub benchmarker_version: String,
    pub algorithm_binary_sha256: String,
    pub runtime_image_manifest_digest: String,
    pub runtime_image_platform_digest: String,
    pub runtime_platform: String,
    pub created_at: String,
}

/// Deterministic RFC 4122-shaped UUID (version 4 / variant 1 bit pattern)
/// derived from a label: SHA-256 truncated to 16 bytes. Spike stand-in for
/// pool-issued identities; deterministic so a re-run is comparable.
pub fn derived_uuid(label: &str) -> String {
    let mut h = Sha256::new();
    h.update(label.as_bytes());
    let d = h.finalize();
    let mut b: [u8; 16] = [0; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = 0x40 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let s = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

/// Build the package manifest (`package-manifest.schema.json`).
#[allow(clippy::too_many_arguments)]
pub fn build_manifest(
    id: &PackageIdentity,
    num_nonces: u64,
    root: &[u8; 32],
    qualities: &[u8],
    leaf_hashes: &[u8],
    outputs: &[u8],
) -> Value {
    json!({
        "protocol_version": "0.1.0",
        "package_format": "proof-material-v1",
        "package_id": id.package_id,
        "assignment_id": id.assignment_id,
        "assignment_digest": id.assignment_digest,
        "benchmark_id": id.benchmark_id,
        "member_id": id.member_id,
        "worker_id": id.worker_id,
        "slot_id": id.slot_id,
        "slot_generation": id.slot_generation,
        "compute": id.compute,
        "qualification_id": id.qualification_id,
        "qualification_spec_digest": id.qualification_spec_digest,
        "network": "testnet",
        "tig_api_base_url": "https://testnet-api.tig.foundation",
        "nonce_range": { "start": 0, "end_exclusive": num_nonces, "num_nonces": num_nonces },
        "merkle": {
            "algorithm": "tig-merkle-blake3-v1",
            "leaf_encoding": "tig-output-metadata-v1",
            "tree_capacity": tree_capacity(num_nonces),
            "root": hex(root),
        },
        "versions": {
            "member_agent_version": id.member_agent_version,
            "tig_upstream_commit": id.tig_upstream_commit,
            "benchmarker_version": id.benchmarker_version,
            "algorithm_binary_sha256": id.algorithm_binary_sha256,
            "runtime_image_manifest_digest": id.runtime_image_manifest_digest,
            "runtime_image_platform_digest": id.runtime_image_platform_digest,
            "runtime_platform": id.runtime_platform,
        },
        "files": {
            "qualities": file_entry("qualities.i32le", "signed-i32-little-endian", qualities, num_nonces),
            "leaf_hashes": file_entry("leaf-hashes.bin", "raw-32-byte-blake3-hashes", leaf_hashes, num_nonces),
            "outputs": file_entry("outputs.ndjson", "rfc8785-json-lines-lf", outputs, num_nonces),
        },
        "created_at": id.created_at,
    })
}

// ---------------------------------------------------------------------------
// Archive (member_protocol.md §10.1): ustar tar in one zstd frame
// ---------------------------------------------------------------------------

/// Serialize the four package members as one POSIX ustar archive, in the
/// mandated order, with meaningless metadata zeroed (modes/owners/timestamps
/// "have no meaning and are ignored").
pub fn build_tar(
    manifest: &[u8],
    qualities: &[u8],
    leaf_hashes: &[u8],
    outputs: &[u8],
) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in [
        ("manifest.json", manifest),
        ("qualities.i32le", qualities),
        ("leaf-hashes.bin", leaf_hashes),
        ("outputs.ndjson", outputs),
    ] {
        let mut header = tar::Header::new_ustar();
        header.set_path(name).context("tar entry name")?;
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, data).context("tar append")?;
    }
    builder.into_inner().context("tar finish")
}

/// Compress the tar stream as one Zstandard frame with declared content size
/// and frame checksum (window far below the 64 MiB bound at this level).
pub fn compress_zstd(tar_bytes: &[u8], level: i32) -> Result<Vec<u8>> {
    let mut encoder =
        zstd::stream::write::Encoder::new(Vec::new(), level).context("zstd encoder")?;
    encoder.include_checksum(true).context("zstd checksum")?;
    encoder
        .set_pledged_src_size(Some(tar_bytes.len() as u64))
        .context("zstd content size")?;
    encoder.write_all(tar_bytes).context("zstd write")?;
    encoder.finish().context("zstd finish")
}

// ---------------------------------------------------------------------------
// RFC 3339 timestamp (UTC, seconds) without a date-time dependency
// ---------------------------------------------------------------------------

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian,
/// days-from-civil inverse; valid for any timestamp this spike can produce).
pub fn rfc3339_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs = unix_secs % 86_400;
    // civil_from_days (Howard Hinnant), shifted era arithmetic.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_767_225_599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn derived_uuid_is_rfc4122_shaped_and_stable() {
        let u = derived_uuid("spike-member:v0:test:member");
        assert_eq!(u, derived_uuid("spike-member:v0:test:member"));
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn tree_capacity_next_power_of_two() {
        assert_eq!(tree_capacity(1), 1);
        assert_eq!(tree_capacity(8), 8);
        assert_eq!(tree_capacity(10), 16);
    }

    #[test]
    fn jsonify_sorts_keys_and_keeps_u64_exact() {
        let v = json!({"b": 18_446_744_073_709_551_615u64, "a": 1});
        assert_eq!(jsonify(&v).unwrap(), r#"{"a":1,"b":18446744073709551615}"#);
    }
}
