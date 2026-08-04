//! Deterministic S2 tests against `fixtures/benchmark-artifact/v1` golden
//! values — no Docker, no network. The Rust Merkle/leaf/package construction
//! must reproduce the fixture byte-for-byte (the same convention the pool's
//! verifier and `tools/build_fixture.py` pin from the upstream commit).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read as _;
use std::path::PathBuf;

use serde_json::Value;
use spike::member::{
    OutputRecord, branch_hex, branch_root, build_tar, compress_zstd, jsonify, leaf_hashes_bytes,
    merkle_branch, merkle_root, outputs_ndjson, qualities_bytes, sha256_hex,
};

const GOLDEN_ROOT: &str = "d734f0c0e487f4dde9d2d7b162fbf9abc1dce47cad4df6bb70fee8a91e221c8f";

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/benchmark-artifact/v1/cases/golden")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(golden_dir().join(name)).expect(name)
}

/// Decode the fixture's reviewable hex encoding (one record per line).
fn unhex_lines(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8(data.to_vec()).expect("utf8");
    let joined: String = text.split_whitespace().collect();
    (0..joined.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&joined[i..i + 2], 16).expect("hex"))
        .collect()
}

fn golden_records() -> Vec<OutputRecord> {
    String::from_utf8(read("outputs.ndjson"))
        .expect("utf8")
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("record json");
            OutputRecord::from_wire_json(&v).expect("wire record")
        })
        .collect()
}

#[test]
fn golden_leaf_hashes_reproduce() {
    let records = golden_records();
    assert_eq!(records.len(), 8);
    let expected = unhex_lines(&read("leaf-hashes.bin.hex"));
    let leaves: Vec<[u8; 32]> = records.iter().map(|r| r.leaf_hash().unwrap()).collect();
    assert_eq!(leaf_hashes_bytes(&leaves), expected);
}

#[test]
fn golden_merkle_root_reproduces() {
    let leaves: Vec<[u8; 32]> = golden_records()
        .iter()
        .map(|r| r.leaf_hash().unwrap())
        .collect();
    let root = merkle_root(&leaves).unwrap();
    assert_eq!(spike::member::hex(&root), GOLDEN_ROOT);
    let manifest: Value = serde_json::from_slice(&read("manifest.json")).unwrap();
    assert_eq!(
        manifest.pointer("/merkle/root").and_then(Value::as_str),
        Some(GOLDEN_ROOT)
    );
}

#[test]
fn golden_member_files_reproduce_byte_for_byte() {
    let records = golden_records();
    assert_eq!(outputs_ndjson(&records).unwrap(), read("outputs.ndjson"));

    let manifest: Value = serde_json::from_slice(&read("manifest.json")).unwrap();
    // Qualities are pinned by the fixture's hex file and the manifest hash.
    let qualities = unhex_lines(&read("qualities.i32le.hex"));
    assert_eq!(
        manifest
            .pointer("/files/qualities/sha256")
            .and_then(Value::as_str),
        Some(sha256_hex(&qualities).as_str())
    );
    let decoded: Vec<i32> = qualities
        .chunks(4)
        .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(qualities_bytes(&decoded), qualities);

    // The canonical manifest bytes hash to the value the fixture pins.
    let canonical = jsonify(&manifest).unwrap();
    assert_eq!(canonical.as_bytes(), &read("manifest.json")[..]);
}

#[test]
fn golden_branches_verify_against_root() {
    let leaves: Vec<[u8; 32]> = golden_records()
        .iter()
        .map(|r| r.leaf_hash().unwrap())
        .collect();
    let root = merkle_root(&leaves).unwrap();
    for idx in [1usize, 4, 6] {
        let branch = merkle_branch(&leaves, idx).unwrap();
        assert_eq!(branch.len(), 3, "8-leaf perfect tree branch length");
        assert_eq!(branch_root(&leaves[idx], idx, &branch).unwrap(), root);
        // Serialization shape: {depth:02x}{hash:064x} per element.
        assert_eq!(branch_hex(&branch).len(), 3 * (2 + 64));
    }
}

#[test]
fn unpaired_leaves_promote_unchanged() {
    // 10 leaves (the live S2 shape): the pinned convention promotes the
    // unpaired node instead of padding to tree capacity.
    let leaves: Vec<[u8; 32]> = (0u8..10).map(|i| [i; 32]).collect();
    let root = merkle_root(&leaves).unwrap();
    for idx in 0..leaves.len() {
        let branch = merkle_branch(&leaves, idx).unwrap();
        assert_eq!(branch_root(&leaves[idx], idx, &branch).unwrap(), root);
    }
    // Manual fold for the promoted tail: level sizes 10 -> 5 -> 3 -> 2 -> 1.
    let b3 = |l: &[u8; 32], r: &[u8; 32]| -> [u8; 32] {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(l);
        buf[32..].copy_from_slice(r);
        blake3::hash(&buf).into()
    };
    let l5: Vec<[u8; 32]> = leaves.chunks(2).map(|p| b3(&p[0], &p[1])).collect();
    let l3 = [b3(&l5[0], &l5[1]), b3(&l5[2], &l5[3]), l5[4]];
    let l2 = [b3(&l3[0], &l3[1]), l3[2]];
    assert_eq!(merkle_root(&leaves).unwrap(), b3(&l2[0], &l2[1]));
    assert_eq!(root, b3(&l2[0], &l2[1]));
}

#[test]
fn package_archive_round_trips_with_declared_size_and_checksum() {
    let records = golden_records();
    let leaves: Vec<[u8; 32]> = records.iter().map(|r| r.leaf_hash().unwrap()).collect();
    let manifest_data = read("manifest.json");
    let qualities = unhex_lines(&read("qualities.i32le.hex"));
    let leaf_data = leaf_hashes_bytes(&leaves);
    let outputs = outputs_ndjson(&records).unwrap();

    let tar_data = build_tar(&manifest_data, &qualities, &leaf_data, &outputs).unwrap();
    let compressed = compress_zstd(&tar_data, 19).unwrap();

    // One zstd frame that declares its content size (member_protocol §10.1).
    let content_size = zstd::zstd_safe::get_frame_content_size(&compressed)
        .expect("valid zstd frame")
        .expect("frame must declare content size");
    assert_eq!(content_size, tar_data.len() as u64);

    // Decompress and re-parse the tar: exactly four regular ustar entries in
    // the mandated order, byte-identical contents.
    let mut decompressed = Vec::new();
    zstd::stream::read::Decoder::new(&compressed[..])
        .unwrap()
        .read_to_end(&mut decompressed)
        .unwrap();
    assert_eq!(decompressed, tar_data);

    let mut archive = tar::Archive::new(&tar_data[..]);
    let expected: [(&str, &[u8]); 4] = [
        ("manifest.json", &manifest_data),
        ("qualities.i32le", &qualities),
        ("leaf-hashes.bin", &leaf_data),
        ("outputs.ndjson", &outputs),
    ];
    let mut n = 0;
    for (entry, (name, data)) in archive.entries().unwrap().zip(expected.iter()) {
        let mut entry = entry.unwrap();
        assert_eq!(entry.header().entry_type(), tar::EntryType::Regular);
        assert_eq!(entry.path().unwrap().to_str().unwrap(), *name);
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf[..], *data);
        n += 1;
    }
    assert_eq!(n, 4);
}

#[test]
fn wire_records_preserve_full_u64_range() {
    // Golden nonce 5 runtime_signature exceeds 2^63; nonce 1 exceeds 2^53.
    let records = golden_records();
    assert_eq!(records[5].runtime_signature, 17_446_744_073_709_551_615);
    assert_eq!(records[1].runtime_signature, 12_157_665_459_056_928_801);
    for r in &records {
        let wire = r.to_wire_value();
        let back = OutputRecord::from_wire_json(&wire).unwrap();
        assert_eq!(&back, r);
    }
}
