#!/usr/bin/env python3
"""Deterministic builder/verifier for fixtures/benchmark-artifact/v1.

Regenerates every derived file in this fixture set from the constants below
and the pinned upstream TIG conventions, then verifies the committed files
byte-for-byte. Run from anywhere:

    python3 fixtures/benchmark-artifact/v1/tools/build_fixture.py          # verify
    python3 fixtures/benchmark-artifact/v1/tools/build_fixture.py --write  # rewrite

Dependency: `pip install blake3` (the official Rust-backed binding).

Conventions implemented here are copied from the pinned upstream commit
ad08d1ea001a73ff5aab3b556d7f59246fece14e of github.com/tig-foundation/tig-monorepo:

- jsonify (tig-utils/src/json.rs): serde_json compact serialization with
  object keys sorted ascending, recursively. For the ASCII-only, integer-only
  values used in this fixture it equals RFC 8785 (JCS) canonical JSON, which
  is what json.dumps(sort_keys=True, separators=(",", ":")) produces.
- u8s_from_str / u64s_from_str (tig-utils/src/hash.rs): blake3 of the UTF-8
  bytes; u64s are little-endian 8-byte words of that 32-byte hash.
- OutputData -> OutputMetaData -> MerkleHash (tig-structs/src/core.rs):
  solution_signature = u64s_from_str(jsonify(solution))[0] where the solution
  is jsonified as a JSON string; leaf hash = blake3(jsonify(OutputMetaData))
  over keys {fuel_consumed, nonce, runtime_signature, solution_signature}
  serialized as bare JSON integers (serde_json prints u64 exactly).
- MerkleTree::calc_merkle_root (tig-utils/src/merkle_tree.rs): fold the leaf
  list pairwise with blake3(left32 || right32); an unpaired trailing node is
  promoted unchanged. Leaves are NOT padded to the power-of-two capacity.
- MerkleBranch (tig-utils/src/merkle_tree.rs): list of (depth, hash) pairs,
  serialized as hex {depth:02x}{hash:064x} concatenated per element.

No value in this file is a live protocol constant. All ids and digests are
constructed fixtures; opaque digests are SHA-256 of documented preimage
strings so nothing is unexplained randomness.
"""

import argparse
import hashlib
import json
import struct
import sys
from pathlib import Path

try:
    import blake3
except ImportError:  # pragma: no cover
    sys.exit("this tool requires the 'blake3' python package (pip install blake3)")

V1 = Path(__file__).resolve().parent.parent
CASES = V1 / "cases"

# --------------------------------------------------------------------------
# Canonical JSON (RFC 8785 subset: ASCII strings, integers, no floats)
# --------------------------------------------------------------------------


def jcs(value) -> str:
    def check(v):
        if isinstance(v, float):
            raise ValueError("floats are forbidden in this fixture")
        if isinstance(v, str) and not v.isascii():
            raise ValueError("non-ASCII strings are forbidden in this fixture")
        if isinstance(v, dict):
            for k, x in v.items():
                check(k)
                check(x)
        if isinstance(v, list):
            for x in v:
                check(x)

    check(value)
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def b3(data: bytes) -> bytes:
    return blake3.blake3(data).digest()


# --------------------------------------------------------------------------
# Upstream leaf / tree / branch conventions
# --------------------------------------------------------------------------


def solution_signature(solution: str) -> int:
    # OutputData::calc_solution_signature: u64s_from_str(jsonify(&self.solution))[0]
    digest = b3(jcs(solution).encode())
    return struct.unpack("<Q", digest[:8])[0]


def leaf_hash(record: dict) -> bytes:
    # OutputData -> OutputMetaData -> MerkleHash
    meta = {
        "nonce": record["nonce"],
        "runtime_signature": int(record["runtime_signature"]),
        "fuel_consumed": int(record["fuel_consumed"]),
        "solution_signature": solution_signature(record["solution"]),
    }
    return b3(jcs(meta).encode())


def merkle_root(leaves: list[bytes]) -> bytes:
    hashes = list(leaves)
    while len(hashes) > 1:
        nxt = []
        for i in range(0, len(hashes), 2):
            if i + 1 < len(hashes):
                nxt.append(b3(hashes[i] + hashes[i + 1]))
            else:
                nxt.append(hashes[i])  # unpaired node promoted unchanged
        hashes = nxt
    return hashes[0]


def merkle_branch(leaves: list[bytes], idx: int) -> list[tuple[int, bytes]]:
    hashes = list(leaves)
    branch = []
    depth = 0
    while len(hashes) > 1:
        nxt = []
        for i in range(0, len(hashes), 2):
            if i + 1 < len(hashes):
                if idx >> 1 == i // 2:
                    sibling = hashes[i + 1] if idx % 2 == 0 else hashes[i]
                    branch.append((depth, sibling))
                nxt.append(b3(hashes[i] + hashes[i + 1]))
            else:
                nxt.append(hashes[i])
        hashes = nxt
        idx //= 2
        depth += 1
    return branch


def branch_hex(branch: list[tuple[int, bytes]]) -> str:
    return "".join(f"{d:02x}{h.hex()}" for d, h in branch)


def branch_verify(leaf: bytes, idx: int, branch: list[tuple[int, bytes]]) -> bytes:
    # MerkleBranch::calc_merkle_root
    root = leaf
    curr_depth = 0
    for depth, h in branch:
        if curr_depth > depth:
            raise ValueError("invalid branch")
        while curr_depth != depth:
            idx //= 2
            curr_depth += 1
        root = b3(root + h if idx % 2 == 0 else h + root)
        idx //= 2
        curr_depth += 1
    return root


# --------------------------------------------------------------------------
# Fixture constants (constructed; obviously fake; no credentials)
# --------------------------------------------------------------------------

UPSTREAM_COMMIT = "ad08d1ea001a73ff5aab3b556d7f59246fece14e"

IDS = {
    "member_id": "11111111-1111-4111-8111-111111111111",
    "worker_id": "22222222-2222-4222-8222-222222222222",
    "slot_id": "33333333-3333-4333-8333-333333333333",
    "offer_id": "44444444-4444-4444-8444-444444444444",
    "qualification_id": "55555555-5555-4555-8555-555555555555",
    "assignment_id": "66666666-6666-4666-8666-666666666666",
    "package_id": "77777777-7777-4777-8777-777777777777",
}

# Opaque digests: SHA-256 of documented preimage strings (see README).
PREIMAGES = {
    "qualification_spec_digest": "fixture:qualification-spec:v1",
    "algorithm_binary_sha256": "fixture:algorithm-binary:a011:v1",
    "algorithm_binary_sha256_wrong": "fixture:algorithm-binary:WRONG",
    "image_manifest_digest": "fixture:runtime-image-manifest:v1",
    "image_platform_digest": "fixture:runtime-image-platform:linux-amd64:v1",
    "package_bytes_declared": "fixture:package-bytes:golden",
    "package_bytes_corrupted": "fixture:package-bytes:corrupted-in-transit",
}


def preimage_sha(name: str) -> str:
    return sha256_hex(PREIMAGES[name].encode())


COMPUTE = {
    "compute_kind": "CPU",
    "compute_type": "aws_c7i",
    "cpu_arch": "amd64",
    "cpu_vendor": "INTEL",
    "cpu_cores": 4,
}

NUM_NONCES = 8
TREE_CAPACITY = 8  # next power of two >= num_nonces

ASSIGNMENT_IDENTITY = {
    "protocol_version": "0.1.0",
    "package_format": "proof-material-v1",
    "network": "testnet",
    "tig_api_base_url": "https://testnet-api.tig.foundation",
    "pool_player_id": "0xp00l00000000000000000000000000000000000",
    "assignment_id": IDS["assignment_id"],
    "offer_id": IDS["offer_id"],
    "member_id": IDS["member_id"],
    "worker_id": IDS["worker_id"],
    "slot_id": IDS["slot_id"],
    "slot_generation": 3,
    "compute": COMPUTE,
    "qualification_id": IDS["qualification_id"],
    "qualification_spec_digest": preimage_sha("qualification_spec_digest"),
    "decision_block_id": "block_100080",
    "confirmed_precommit": {
        "benchmark_id": "bench_c001_t900_000001",
        "settings": {
            "player_id": "0xp00l00000000000000000000000000000000000",
            "block_id": "block_100081",
            "challenge_id": "c001",
            "algorithm_id": "a011",
            "track_id": "t900",
        },
        "details": {
            "block_started": 100081,
            "num_nonces": NUM_NONCES,
            "num_bundles": 2,
            "rand_hash": "randhash_block_100081_fixture",
            "fee_paid": "18000000000000000",
            "hyperparameters": None,
            "fuel_budget": "20000000000",
            "compute_type": "aws_c7i",
        },
        "block_confirmed": 100082,
    },
    "nonce_range": {"start": 0, "end_exclusive": NUM_NONCES, "num_nonces": NUM_NONCES},
    "algorithm_binary_sha256": preimage_sha("algorithm_binary_sha256"),
    "runtime": {
        "tig_upstream_commit": UPSTREAM_COMMIT,
        "benchmarker_version": "0.1.0-fixture",
        "image_manifest_digest": "sha256:" + preimage_sha("image_manifest_digest"),
        "image_platform_digest": "sha256:" + preimage_sha("image_platform_digest"),
        "platform": "linux/amd64",
    },
    "merkle": {
        "algorithm": "tig-merkle-blake3-v1",
        "leaf_encoding": "tig-output-metadata-v1",
        "tree_capacity": TREE_CAPACITY,
    },
    "package_limits": {
        "max_compressed_bytes": 1073741824,
        "max_uncompressed_bytes": 2147483648,
        "max_manifest_bytes": 262144,
        "max_output_record_bytes": 1048576,
        "min_upload_chunk_bytes": 1048576,
        "max_upload_chunk_bytes": 8388608,
    },
}

ASSIGNMENT_DIGEST = sha256_hex(jcs(ASSIGNMENT_IDENTITY).encode())

CREATED_AT = "2026-02-01T00:00:00Z"
SAMPLED_NONCES = [1, 4, 6]

# Golden output records: (runtime_signature, fuel_consumed, solution, quality).
# One runtime_signature above 2**63 and one above 2**53 pin lossless u64
# handling on both the decimal-string wire form and the bare-integer leaf
# preimage form.
GOLDEN_ROWS = [
    (4602930012176195438, 1310720123, "sol-c001-t900-n0", 55),
    (12157665459056928801, 1489274201, "sol-c001-t900-n1", 62),
    (305419896, 1210000000, "sol-c001-t900-n2", 48),
    (8624975790449270271, 1750001111, "sol-c001-t900-n3", 71),
    (1311768467294899695, 1400000000, "sol-c001-t900-n4", 55),
    (17446744073709551615, 1620304050, "sol-c001-t900-n5", 60),
    (9223372036854775809, 1234567890, "sol-c001-t900-n6", 49),
    (42, 1999999999, "sol-c001-t900-n7", 66),
]

# Structurally valid, solution-invalid: qualities all below the invented
# minimum verification quality (40); solutions are marked bad on purpose.
SOLUTION_INVALID_ROWS = [
    (7000000000000000001, 1310000001, "badsol-c001-t900-n0", -1),
    (7000000000000000002, 1310000002, "badsol-c001-t900-n1", 0),
    (7000000000000000003, 1310000003, "badsol-c001-t900-n2", 3),
    (7000000000000000004, 1310000004, "badsol-c001-t900-n3", -5),
    (7000000000000000005, 1310000005, "badsol-c001-t900-n4", 2),
    (7000000000000000006, 1310000006, "badsol-c001-t900-n5", 0),
    (7000000000000000007, 1310000007, "badsol-c001-t900-n6", -2),
    (7000000000000000008, 1310000008, "badsol-c001-t900-n7", 1),
]

# Method-non-reproducible: identical to golden except fabricated
# runtime_signatures at nonces 2 and 5 (golden value + 1). The package is
# internally consistent; re-execution (reproduction.json) disagrees.
NON_REPRODUCIBLE_ROWS = [
    (rs + 1 if n in (2, 5) else rs, fuel, sol, q)
    for n, (rs, fuel, sol, q) in enumerate(GOLDEN_ROWS)
]


def records(rows, nonces=None):
    nonces = list(range(len(rows))) if nonces is None else nonces
    return [
        {
            "nonce": nonce,
            "runtime_signature": str(rs),
            "fuel_consumed": str(fuel),
            "solution": sol,
            "cpu_arch": "amd64",
        }
        for nonce, (rs, fuel, sol, _q) in zip(nonces, rows)
    ]


def qualities(rows) -> list[int]:
    return [q for (_rs, _fuel, _sol, q) in rows]


# --------------------------------------------------------------------------
# Package assembly
# --------------------------------------------------------------------------


def outputs_bytes(recs) -> bytes:
    return b"".join(jcs(r).encode() + b"\n" for r in recs)


def qualities_bytes(qs) -> bytes:
    return b"".join(struct.pack("<i", q) for q in qs)


def file_entry(name, encoding, data: bytes, record_count: int) -> dict:
    return {
        "name": name,
        "size_bytes": len(data),
        "sha256": sha256_hex(data),
        "record_count": record_count,
        "encoding": encoding,
    }


def build_manifest(recs, qs, leaves, root_hex, **overrides) -> dict:
    manifest = {
        "protocol_version": "0.1.0",
        "package_format": "proof-material-v1",
        "package_id": IDS["package_id"],
        "assignment_id": IDS["assignment_id"],
        "assignment_digest": ASSIGNMENT_DIGEST,
        "benchmark_id": "bench_c001_t900_000001",
        "member_id": IDS["member_id"],
        "worker_id": IDS["worker_id"],
        "slot_id": IDS["slot_id"],
        "slot_generation": 3,
        "compute": COMPUTE,
        "qualification_id": IDS["qualification_id"],
        "qualification_spec_digest": preimage_sha("qualification_spec_digest"),
        "network": "testnet",
        "tig_api_base_url": "https://testnet-api.tig.foundation",
        "nonce_range": {
            "start": 0,
            "end_exclusive": NUM_NONCES,
            "num_nonces": NUM_NONCES,
        },
        "merkle": {
            "algorithm": "tig-merkle-blake3-v1",
            "leaf_encoding": "tig-output-metadata-v1",
            "tree_capacity": TREE_CAPACITY,
            "root": root_hex,
        },
        "versions": {
            "member_agent_version": "0.1.0-fixture",
            "tig_upstream_commit": UPSTREAM_COMMIT,
            "benchmarker_version": "0.1.0-fixture",
            "algorithm_binary_sha256": preimage_sha("algorithm_binary_sha256"),
            "runtime_image_manifest_digest": "sha256:"
            + preimage_sha("image_manifest_digest"),
            "runtime_image_platform_digest": "sha256:"
            + preimage_sha("image_platform_digest"),
            "runtime_platform": "linux/amd64",
        },
        "files": {
            "qualities": file_entry(
                "qualities.i32le",
                "signed-i32-little-endian",
                qualities_bytes(qs),
                len(qs),
            ),
            "leaf_hashes": file_entry(
                "leaf-hashes.bin",
                "raw-32-byte-blake3-hashes",
                b"".join(leaves),
                len(leaves),
            ),
            "outputs": file_entry(
                "outputs.ndjson",
                "rfc8785-json-lines-lf",
                outputs_bytes(recs),
                len(recs),
            ),
        },
        "created_at": CREATED_AT,
    }
    manifest.update(overrides)
    return manifest


def package_files(recs, qs, leaves, manifest) -> dict[str, bytes]:
    """The four logical archive members, binary ones hex-encoded for review."""
    return {
        # manifest.json is RFC 8785 canonical bytes: single line, no BOM,
        # no trailing whitespace (member_protocol.md §10.2).
        "manifest.json": jcs(manifest).encode(),
        "outputs.ndjson": outputs_bytes(recs),
        "qualities.i32le.hex": b"".join(
            struct.pack("<i", q).hex().encode() + b"\n" for q in qs
        ),
        "leaf-hashes.bin.hex": b"".join(h.hex().encode() + b"\n" for h in leaves),
    }


def flip_first_byte(h: bytes) -> bytes:
    return bytes([h[0] ^ 0x01]) + h[1:]


def build_cases() -> dict[str, dict[str, bytes]]:
    cases: dict[str, dict[str, bytes]] = {}

    # ---- golden ----------------------------------------------------------
    g_recs = records(GOLDEN_ROWS)
    g_qs = qualities(GOLDEN_ROWS)
    g_leaves = [leaf_hash(r) for r in g_recs]
    g_root = merkle_root(g_leaves).hex()
    g_manifest = build_manifest(g_recs, g_qs, g_leaves, g_root)
    cases["golden"] = package_files(g_recs, g_qs, g_leaves, g_manifest)

    # ---- bad-package-checksum (upload declaration level) -----------------
    cases["bad-package-checksum"] = {
        "upload.json": pretty(
            {
                "note": "Whole-package compressed bytes are not pinned by this fixture (see README); digests here are documented placeholder values.",
                "declared": {
                    "package_id": IDS["package_id"],
                    "assignment_digest": ASSIGNMENT_DIGEST,
                    "media_type": "application/vnd.tig-pool.proof-material-v1.tar+zstd",
                    "compressed_size_bytes": 40960,
                    "uncompressed_size_bytes": 65536,
                    "manifest_sha256": sha256_hex(jcs(g_manifest).encode()),
                    "package_sha256": preimage_sha("package_bytes_declared"),
                },
                "observed": {
                    "committed_bytes": 40960,
                    "received_sha256": preimage_sha("package_bytes_corrupted"),
                },
            }
        )
    }

    # ---- truncated-upload (upload never completes) -----------------------
    cases["truncated-upload"] = {
        "upload.json": pretty(
            {
                "note": "Upload stalls before all declared bytes arrive; the package deadline passes. See expected.json for both attribution sub-outcomes.",
                "declared": {
                    "package_id": IDS["package_id"],
                    "assignment_digest": ASSIGNMENT_DIGEST,
                    "media_type": "application/vnd.tig-pool.proof-material-v1.tar+zstd",
                    "compressed_size_bytes": 40960,
                    "uncompressed_size_bytes": 65536,
                    "manifest_sha256": sha256_hex(jcs(g_manifest).encode()),
                    "package_sha256": preimage_sha("package_bytes_declared"),
                },
                "observed": {
                    "committed_offset": 24576,
                    "finalize_called": False,
                    "package_due_before_block": 100191,
                    "latest_confirmed_height_at_evaluation": 100191,
                    "pool_and_tig_available_through_deadline": True,
                },
            }
        )
    }

    # ---- missing-manifest-entries ---------------------------------------
    mm = build_manifest(g_recs, g_qs, g_leaves, g_root)
    del mm["files"]["leaf_hashes"]
    del mm["merkle"]["root"]
    c = package_files(g_recs, g_qs, g_leaves, mm)
    cases["missing-manifest-entries"] = c

    # ---- wrong-declared-size --------------------------------------------
    ws = build_manifest(g_recs, g_qs, g_leaves, g_root)
    ws["files"]["outputs"]["size_bytes"] = (
        len(outputs_bytes(g_recs)) + 7
    )  # declared, not actual
    cases["wrong-declared-size"] = package_files(g_recs, g_qs, g_leaves, ws)

    # ---- missing-nonce (nonce 5 absent everywhere; manifest consistent
    #      with the 7-record files, so only cross-field coverage fails) ----
    keep = [i for i in range(NUM_NONCES) if i != 5]
    mn_rows = [GOLDEN_ROWS[i] for i in keep]
    mn_recs = records(mn_rows, nonces=keep)
    mn_qs = qualities(mn_rows)
    mn_leaves = [leaf_hash(r) for r in mn_recs]
    mn_root = merkle_root(mn_leaves).hex()
    mn_manifest = build_manifest(mn_recs, mn_qs, mn_leaves, mn_root)
    cases["missing-nonce"] = package_files(mn_recs, mn_qs, mn_leaves, mn_manifest)

    # ---- duplicate-nonce (record 4 replaced by a copy of record 3) -------
    dup_rows = list(GOLDEN_ROWS)
    dup_rows[4] = GOLDEN_ROWS[3]
    dup_nonces = [0, 1, 2, 3, 3, 5, 6, 7]
    dup_recs = records(dup_rows, nonces=dup_nonces)
    dup_qs = qualities(dup_rows)
    dup_leaves = [leaf_hash(r) for r in dup_recs]
    dup_root = merkle_root(dup_leaves).hex()
    dup_manifest = build_manifest(dup_recs, dup_qs, dup_leaves, dup_root)
    cases["duplicate-nonce"] = package_files(dup_recs, dup_qs, dup_leaves, dup_manifest)

    # ---- leaf-hash-mismatch (leaf 3 corrupted; file and root internally
    #      consistent, but leaf 3 does not reproduce from its record) ------
    lh_leaves = list(g_leaves)
    lh_leaves[3] = flip_first_byte(g_leaves[3])
    lh_root = merkle_root(lh_leaves).hex()
    lh_manifest = build_manifest(g_recs, g_qs, lh_leaves, lh_root)
    cases["leaf-hash-mismatch"] = package_files(g_recs, g_qs, lh_leaves, lh_manifest)

    # ---- merkle-root-mismatch (leaves reproduce; declared root wrong) ----
    bad_root = g_root[:-1] + ("0" if g_root[-1] != "0" else "1")
    mr_manifest = build_manifest(g_recs, g_qs, g_leaves, bad_root)
    cases["merkle-root-mismatch"] = package_files(g_recs, g_qs, g_leaves, mr_manifest)

    # ---- identity-mismatch (digest of a mutated identity + wrong binary) -
    mutated = json.loads(jcs(ASSIGNMENT_IDENTITY))
    mutated["slot_generation"] = 2
    im_manifest = build_manifest(
        g_recs,
        g_qs,
        g_leaves,
        g_root,
        assignment_digest=sha256_hex(jcs(mutated).encode()),
        slot_generation=2,
    )
    im_manifest["versions"]["algorithm_binary_sha256"] = preimage_sha(
        "algorithm_binary_sha256_wrong"
    )
    cases["identity-mismatch"] = package_files(g_recs, g_qs, g_leaves, im_manifest)

    # ---- solution-invalid (mechanically perfect; semantically worthless) -
    si_recs = records(SOLUTION_INVALID_ROWS)
    si_qs = qualities(SOLUTION_INVALID_ROWS)
    si_leaves = [leaf_hash(r) for r in si_recs]
    si_root = merkle_root(si_leaves).hex()
    si_manifest = build_manifest(si_recs, si_qs, si_leaves, si_root)
    cases["solution-invalid"] = package_files(si_recs, si_qs, si_leaves, si_manifest)

    # ---- method-non-reproducible (mechanically perfect; re-execution
    #      disagrees at nonces 2 and 5) ------------------------------------
    nr_recs = records(NON_REPRODUCIBLE_ROWS)
    nr_qs = qualities(NON_REPRODUCIBLE_ROWS)
    nr_leaves = [leaf_hash(r) for r in nr_recs]
    nr_root = merkle_root(nr_leaves).hex()
    nr_manifest = build_manifest(nr_recs, nr_qs, nr_leaves, nr_root)
    nr = package_files(nr_recs, nr_qs, nr_leaves, nr_manifest)
    nr["reproduction.json"] = pretty(
        {
            "note": "Re-execution of the pinned binary/runtime with the assignment seed material. Disagrees with the packaged records at nonces 2 and 5.",
            "reproduced_runtime_signatures": {
                str(n): str(GOLDEN_ROWS[n][0]) for n in range(NUM_NONCES)
            },
            "packaged_runtime_signatures": {
                str(n): str(NON_REPRODUCIBLE_ROWS[n][0]) for n in range(NUM_NONCES)
            },
            "mismatched_nonces": [2, 5],
        }
    )
    cases["method-non-reproducible"] = nr

    return cases


def pretty(value) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def golden_proofs() -> dict:
    g_recs = records(GOLDEN_ROWS)
    g_leaves = [leaf_hash(r) for r in g_recs]
    proofs = {}
    for n in SAMPLED_NONCES:
        br = merkle_branch(g_leaves, n)
        assert branch_verify(g_leaves[n], n, br) == merkle_root(g_leaves)
        proofs[str(n)] = {
            "leaf": g_recs[n],
            "leaf_hash": g_leaves[n].hex(),
            "branch": [[d, h.hex()] for d, h in br],
            "branch_serialized": branch_hex(br),
        }
    return proofs


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true", help="rewrite fixture files")
    parser.add_argument(
        "--summary", action="store_true", help="print computed values as JSON"
    )
    args = parser.parse_args()

    cases = build_cases()
    files = {"assignment.json": pretty(ASSIGNMENT_IDENTITY)}
    for case, members in cases.items():
        for name, data in members.items():
            files[f"cases/{case}/{name}"] = data

    if args.summary:
        g_recs = records(GOLDEN_ROWS)
        g_leaves = [leaf_hash(r) for r in g_recs]
        print(
            json.dumps(
                {
                    "assignment_digest": ASSIGNMENT_DIGEST,
                    "golden_leaf_hashes": [h.hex() for h in g_leaves],
                    "golden_merkle_root": merkle_root(g_leaves).hex(),
                    "golden_solution_signatures": {
                        str(r["nonce"]): str(solution_signature(r["solution"]))
                        for r in g_recs
                    },
                    "golden_proofs": golden_proofs(),
                    "manifest_sha256": {
                        case: sha256_hex(members["manifest.json"])
                        for case, members in cases.items()
                        if "manifest.json" in members
                    },
                    "manifest_size_bytes": {
                        case: len(members["manifest.json"])
                        for case, members in cases.items()
                        if "manifest.json" in members
                    },
                    "case_roots": {
                        case: json.loads(members["manifest.json"])
                        .get("merkle", {})
                        .get("root")
                        for case, members in cases.items()
                        if "manifest.json" in members
                    },
                },
                indent=2,
            )
        )
        return 0

    status = 0
    for rel, data in sorted(files.items()):
        path = V1 / rel
        if args.write:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            print(f"wrote {rel} ({len(data)} bytes)")
        else:
            if not path.exists():
                print(f"MISSING {rel}")
                status = 1
            elif path.read_bytes() != data:
                print(f"MISMATCH {rel}")
                status = 1
    if not args.write:
        print("verify: OK" if status == 0 else "verify: FAILED")
    return status


if __name__ == "__main__":
    sys.exit(main())
