//! Issue #13 acceptance, deterministic half: the full S4 chain against
//! fake-tig with no Docker and no network beyond loopback —
//! precommit -> confirmed assignment -> package built via the S2 library ->
//! S3 upload + durable acceptance -> benchmark commitment (only after
//! acceptance; invariant 4 refusal tested) -> sampled nonces observed from
//! confirmed state -> proofs built SOLELY from the retained accepted package
//! (integrity recheck + tamper refusal) -> idempotent proof submission
//! (invariant 5 refusal + no double-apply) -> benchmark_id in
//! block.data.active_ids.benchmark -> retention-conditioned deletion
//! (refusal before ACTIVE, deletion after).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use spike::active::{apply_retention, build_proof_payload, commitment_body, load_accepted_package};
use spike::member::{
    OutputRecord, PackageIdentity, branch_root, build_manifest, build_tar, compress_zstd,
    derived_uuid, jsonify, leaf_hashes_bytes, merkle_root, outputs_ndjson, qualities_bytes,
    sha256_hex,
};
use spike::pool::Pool;
use spike::{Gateway, Ledger, TigClient, fold_intents, plan_precommit};

const PLAYER: &str = "0xp00l00000000000000000000000000000000000";

fn spawn_fake_tig() -> String {
    let fixture = format!("{}/../../fixtures/tig/v1", env!("CARGO_MANIFEST_DIR"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async move {
            let world = fake_tig::build_world(fake_tig::Config::new(fixture)).expect("world");
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tx.send(addr).expect("send addr");
            let _ = axum::serve(listener, fake_tig::router(world)).await;
        });
    });
    format!("http://{}", rx.recv().expect("addr received"))
}

fn advance(base: &str, count: u64) {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{base}/_fake/advance-block"))
        .json(&json!({ "count": count }))
        .send()
        .expect("advance");
    assert!(resp.status().is_success());
}

fn attempts_for(gw: &Gateway, intent_id: &str) -> usize {
    gw.ledger
        .read_all("attempts")
        .unwrap()
        .iter()
        .filter(|a| a.get("intent_id").and_then(Value::as_str) == Some(intent_id))
        .count()
}

fn benchmark_intents(gw: &Gateway, kind: &str) -> Vec<String> {
    fold_intents(&gw.ledger.read_all("intents").unwrap())
        .into_iter()
        .filter(|(_, rec)| rec.get("write_kind").and_then(Value::as_str) == Some(kind))
        .map(|(id, _)| id)
        .collect()
}

/// Build a complete proof-material package for the confirmed assignment via
/// the S2 member library (deterministic stand-in for container execution:
/// synthetic solutions, real leaf hashing / Merkle / archive code paths).
struct BuiltPackage {
    package_id: String,
    assignment_digest: String,
    declaration: Value,
    assignment_doc: Value,
    bytes: Vec<u8>,
    root_hex: String,
    qualities: Vec<i32>,
}

fn build_package(assignment: &Value) -> BuiltPackage {
    let benchmark_id = assignment["benchmark_id"].as_str().unwrap().to_owned();
    let num_nonces = assignment["details"]["num_nonces"].as_u64().unwrap();
    let records: Vec<OutputRecord> = (0..num_nonces)
        .map(|n| OutputRecord {
            nonce: n,
            // Includes values above 2^63 to exercise full-width u64 handling.
            runtime_signature: 17_446_744_073_709_551_615u64.wrapping_add(n * 7919),
            fuel_consumed: 1_000_000 + n * 13,
            solution: format!("s{n}"),
            cpu_arch: "arm64".to_owned(),
        })
        .collect();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let qualities: Vec<i32> = (0..num_nonces).map(|n| 40 + (n as i32 % 20)).collect();
    let leaves: Vec<[u8; 32]> = records.iter().map(|r| r.leaf_hash().unwrap()).collect();
    let root = merkle_root(&leaves).unwrap();

    let uuid = |role: &str| derived_uuid(&format!("spike-s4-test:{benchmark_id}:{role}"));
    let assignment_digest = sha256_hex(format!("spike-s4-test:digest:{benchmark_id}").as_bytes());
    let identity = PackageIdentity {
        benchmark_id: benchmark_id.clone(),
        assignment_digest: assignment_digest.clone(),
        member_id: uuid("member"),
        worker_id: uuid("worker"),
        slot_id: uuid("slot"),
        assignment_id: uuid("assignment"),
        package_id: uuid("package"),
        qualification_id: uuid("qualification"),
        qualification_spec_digest: sha256_hex(b"spike-s4-test:qualification-spec"),
        slot_generation: 1,
        compute: json!({
            "compute_kind": "CPU", "compute_type": "aws_t4g", "cpu_arch": "arm64",
            "cpu_vendor": "ARM", "cpu_cores": 2,
        }),
        member_agent_version: "spike-member/0.1.0-test".to_owned(),
        tig_upstream_commit: "ad08d1ea001a73ff5aab3b556d7f59246fece14e".to_owned(),
        benchmarker_version: "0.0.7".to_owned(),
        algorithm_binary_sha256: sha256_hex(b"spike-s4-test:algorithm-binary"),
        runtime_image_manifest_digest:
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        runtime_image_platform_digest:
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        runtime_platform: "linux/arm64".to_owned(),
        created_at: "2026-08-04T00:00:00Z".to_owned(),
    };
    let qualities_data = qualities_bytes(&qualities);
    let leaf_data = leaf_hashes_bytes(&leaves);
    let outputs_data = outputs_ndjson(&records).unwrap();
    let manifest = build_manifest(
        &identity,
        num_nonces,
        &root,
        &qualities_data,
        &leaf_data,
        &outputs_data,
    );
    let manifest_data = jsonify(&manifest).unwrap().into_bytes();
    let tar_data = build_tar(&manifest_data, &qualities_data, &leaf_data, &outputs_data).unwrap();
    let bytes = compress_zstd(&tar_data, 3).unwrap();
    let package_id = identity.package_id.clone();
    let declaration = json!({
        "package_id": package_id,
        "assignment_digest": assignment_digest,
        "media_type": "application/vnd.tig-pool.proof-material-v1.tar+zstd",
        "compressed_size_bytes": bytes.len() as u64,
        "uncompressed_size_bytes": tar_data.len() as u64,
        "manifest_sha256": sha256_hex(&manifest_data),
        "package_sha256": sha256_hex(&bytes),
    });
    let assignment_doc = json!({
        "assignment_digest": assignment_digest,
        "assignment_id": identity.assignment_id,
        "benchmark_id": benchmark_id,
        "slot_id": identity.slot_id,
        "slot_generation": 1,
        "network": "testnet",
    });
    BuiltPackage {
        package_id,
        assignment_digest,
        declaration,
        assignment_doc,
        bytes,
        root_hex: spike::member::hex(&root),
        qualities,
    }
}

fn upload(pool: &Pool, built: &BuiltPackage) {
    pool.register_assignment(&built.assignment_doc)
        .expect("register assignment");
    let session = pool.create_upload(&built.declaration).expect("upload");
    let mut offset = 0u64;
    for chunk in built.bytes.chunks(4096) {
        offset = pool
            .put_chunk(&session.upload_id, offset, &sha256_hex(chunk), chunk)
            .expect("chunk");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn full_chain_to_active_and_retention() {
    let base = spawn_fake_tig();
    let dir = std::env::temp_dir().join(format!("spike-s4-{}", std::process::id()));
    let pool_root = dir.join("pool");
    let _ = std::fs::remove_dir_all(&dir);
    let gw = Gateway {
        client: TigClient::new(&base, "fake-testnet-key").expect("client"),
        ledger: Ledger::open(dir.join("gateway")).expect("ledger"),
        player_id: PLAYER.to_owned(),
    };

    // ---- fresh precommit -> confirmed assignment (S1 path) --------------
    let block = gw.client.latest_block().expect("block");
    let block_id = block["id"].as_str().unwrap().to_owned();
    let challenges = gw.client.challenges(&block_id).expect("challenges");
    let plan =
        plan_precommit(&block, &challenges, PLAYER, "c001", "a011", "aws_t4g").expect("plan");
    gw.submit_precommit(&plan).expect("precommit");
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile");
    assert!(lines[0].contains("CONFIRMED"), "{lines:?}");
    let states = fold_intents(&gw.ledger.read_all("intents").unwrap());
    let benchmark_id = states
        .values()
        .find_map(|r| {
            (r["state"] == "CONFIRMED").then(|| r["benchmark_id"].as_str().unwrap().to_owned())
        })
        .expect("confirmed benchmark id");
    let assignment: Value = gw
        .ledger
        .read_doc(&format!("assignment-{benchmark_id}.json"))
        .unwrap()
        .expect("assignment doc");
    let num_nonces = assignment["details"]["num_nonces"].as_u64().unwrap();
    assert_eq!(num_nonces, 40, "fixture c001/t001 shape");

    // ---- package via S2 code paths; pool via S3 code paths --------------
    let built = build_package(&assignment);
    let pool = Pool::open(&pool_root).expect("pool");
    upload(&pool, &built);

    // ---- invariant 4: no commitment intent before durable acceptance ----
    // (a) the controller cannot even load a package that is not accepted;
    let refused = load_accepted_package(&pool, &built.package_id);
    assert!(refused.is_err(), "load must refuse before finalize");
    assert!(
        refused.unwrap_err().to_string().contains("invariant 4"),
        "refusal names the invariant"
    );
    // (b) the gateway refuses a commitment without a real acceptance record;
    let premature_body = json!({
        "benchmark_id": benchmark_id, "stopped": false,
        "merkle_root": built.root_hex, "solution_quality": built.qualities,
    });
    assert!(
        gw.submit_benchmark_commitment(&Value::Null, &premature_body)
            .is_err()
    );
    assert!(
        gw.submit_benchmark_commitment(&json!({"assignment_state": "RECEIVED"}), &premature_body)
            .is_err()
    );
    // (c) and no BENCHMARK intent was ever written by the refusals.
    assert!(benchmark_intents(&gw, "BENCHMARK").is_empty());

    // ---- durable acceptance, then commitment ----------------------------
    let outcome = pool.finalize(&built.package_id).expect("finalize");
    assert!(outcome.fresh);
    let pkg = load_accepted_package(&pool, &built.package_id).expect("accepted package loads");
    assert_eq!(pkg.benchmark_id, benchmark_id);
    assert_eq!(pkg.package_sha256, sha256_hex(&built.bytes));
    assert_eq!(pkg.root_hex, built.root_hex);
    assert_eq!(pkg.qualities, built.qualities);
    let body = commitment_body(&pkg).expect("commitment body");
    assert_eq!(
        body["solution_quality"].as_array().unwrap().len() as u64,
        40
    );
    let result = gw
        .submit_benchmark_commitment(&pkg.acceptance, &body)
        .expect("commitment");
    assert!(result.sent);
    assert_eq!(result.state, "SUBMITTED", "200 is not confirmation");
    // A blind identical retry while unresolved is refused (reconcile first).
    let retry = gw.submit_benchmark_commitment(&pkg.acceptance, &body);
    assert!(retry.unwrap_err().to_string().contains("reconcile"));

    // ---- sampled nonces from confirmed state, never the write response --
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile benchmark");
    assert!(
        lines.iter().any(|l| l.contains("BENCHMARK CONFIRMED")),
        "{lines:?}"
    );
    let confirmed = gw
        .ledger
        .read_doc(&format!("benchmark-confirmed-{benchmark_id}.json"))
        .unwrap()
        .expect("confirmed benchmark doc");
    let sampled: Vec<u64> = confirmed["entry"]["details"]["sampled_nonces"]
        .as_array()
        .expect("sampled nonces from confirmed read")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(sampled.len(), 3, "fixture c001 gte(2) + lt(1) samples");
    assert!(sampled.iter().all(|n| *n < num_nonces));

    // ---- invariant 5: no proof intent before the canonical payload ------
    let premature = gw.submit_proof(&benchmark_id);
    assert!(
        premature.unwrap_err().to_string().contains("invariant 5"),
        "proof submit must refuse before the durable payload exists"
    );
    assert!(benchmark_intents(&gw, "PROOF").is_empty());

    // ---- proofs come solely from the retained package: tamper refusal ---
    let object_path = pkg.object_path.clone();
    let original = std::fs::read(&object_path).unwrap();
    let mut tampered = original.clone();
    tampered.push(0x00);
    std::fs::write(&object_path, &tampered).unwrap();
    let bad = load_accepted_package(&pool, &built.package_id);
    assert!(
        bad.unwrap_err().to_string().contains("integrity failure"),
        "whole-package SHA-256 is recomputed before use"
    );
    std::fs::write(&object_path, &original).unwrap();

    // ---- canonical payload from the re-verified accepted package --------
    let pkg = load_accepted_package(&pool, &built.package_id).expect("reload");
    let payload = build_proof_payload(&pkg, &sampled).expect("payload");
    let proofs = payload["merkle_proofs"].as_array().unwrap();
    assert_eq!(proofs.len(), sampled.len());
    for (proof, nonce) in proofs.iter().zip(&sampled) {
        // Leaf rides with bare u64 integers (upstream OutputData encoding).
        assert_eq!(proof["leaf"]["nonce"].as_u64().unwrap(), *nonce);
        assert!(proof["leaf"]["runtime_signature"].is_u64());
        // Branch wire encoding: concatenated {depth:02x}{hash:064x} elements
        // (pinned upstream MerkleBranch serialization); decode and verify it
        // reproduces the submitted root from the leaf.
        let branch_str = proof["branch"].as_str().unwrap();
        assert_eq!(branch_str.len() % 66, 0);
        let decoded: Vec<(u8, [u8; 32])> = (0..branch_str.len() / 66)
            .map(|i| {
                let chunk = &branch_str[i * 66..(i + 1) * 66];
                let depth = u8::from_str_radix(&chunk[..2], 16).unwrap();
                let mut hash = [0u8; 32];
                for (j, b) in hash.iter_mut().enumerate() {
                    *b = u8::from_str_radix(&chunk[2 + j * 2..4 + j * 2], 16).unwrap();
                }
                (depth, hash)
            })
            .collect();
        let idx = usize::try_from(*nonce).unwrap();
        let leaf_hash = pkg.records[idx].leaf_hash().unwrap();
        let reproduced = branch_root(&leaf_hash, idx, &decoded).unwrap();
        assert_eq!(spike::member::hex(&reproduced), pkg.root_hex);
    }
    gw.ledger
        .write_doc(&format!("proof-payload-{benchmark_id}.json"), &payload)
        .unwrap();

    // ---- idempotent proof submission ------------------------------------
    let result = gw.submit_proof(&benchmark_id).expect("proof submit");
    assert!(result.sent);
    assert_eq!(result.state, "SUBMITTED");
    let proof_intent = result.intent_id.clone();
    assert_eq!(attempts_for(&gw, &proof_intent), 2, "attempt + response");
    // Repeat while unresolved: refused, and no new attempt was sent.
    let repeat = gw.submit_proof(&benchmark_id);
    assert!(repeat.unwrap_err().to_string().contains("reconcile"));
    assert_eq!(attempts_for(&gw, &proof_intent), 2, "no double-apply");

    // ---- proof confirmed from confirmed reads ---------------------------
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile proof");
    assert!(
        lines.iter().any(|l| l.contains("PROOF CONFIRMED")),
        "{lines:?}"
    );
    let proof_doc = gw
        .ledger
        .read_doc(&format!("proof-confirmed-{benchmark_id}.json"))
        .unwrap()
        .expect("proof confirmed doc");
    assert!(proof_doc["confirmed_at_block"].as_u64().is_some());
    // A repeated submit after confirmation is a no-send no-op.
    let after = gw.submit_proof(&benchmark_id).expect("idempotent resubmit");
    assert!(!after.sent);
    assert_eq!(after.state, "CONFIRMED");
    assert_eq!(attempts_for(&gw, &proof_intent), 2, "still no double-apply");

    // ---- retention refusal before ACTIVE --------------------------------
    let block = gw.client.latest_block().unwrap();
    let refused = apply_retention(&pool, &built.package_id, &block);
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("retention condition not satisfied"),
        "no deletion before the benchmark is ACTIVE"
    );
    assert!(
        object_path.exists(),
        "retained package survives the refusal"
    );

    // ---- ACTIVE: benchmark_id appears in block.data.active_ids.benchmark
    let mut active_block = None;
    for _ in 0..200 {
        advance(&base, 1);
        let block = gw.client.latest_block().unwrap();
        let active = block
            .pointer("/data/active_ids/benchmark")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .any(|id| id == benchmark_id)
            });
        if active {
            active_block = Some(block);
            break;
        }
    }
    let active_block = active_block.expect("benchmark reached ACTIVE");

    // ---- retention-conditioned deletion ---------------------------------
    let record = apply_retention(&pool, &built.package_id, &active_block).expect("retention");
    assert_eq!(record["deleted"], true);
    assert_eq!(record["object_existed"], true);
    assert_eq!(record["evidence"]["benchmark_id"], benchmark_id.as_str());
    assert!(record["evidence"]["block_height"].as_u64().unwrap() > 0);
    assert!(!object_path.exists(), "retained package deleted");
    // The compact acceptance/receipt record survives deletion (mining §9).
    assert!(
        pool.acceptance(&built.package_id)
            .expect("acceptance readable")
            .is_some()
    );

    let _ = std::fs::remove_dir_all(&dir);
    // Silence the unused-field warning honestly: the digest binds the upload.
    assert_eq!(built.assignment_digest.len(), 64);
}
