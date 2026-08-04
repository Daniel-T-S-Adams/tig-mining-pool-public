//! Issue #14 acceptance, deterministic half (fake-tig ONLY — deliberately
//! bad work never goes to shared testnet):
//!
//! 1. Member circuit breaker: local screening of the fixture solution-invalid
//!    and method-non-reproducible packages (classifications cross-checked
//!    against `fixtures/benchmark-artifact/v1/expected.json`), chargeable
//!    failures counted per member, and — once `f > k` (fixture stand-in
//!    tier) — a further offer produces NO precommit intent and NO fake-tig
//!    write (asserted via `/_fake/state`).
//! 2. Restart reconciliation without duplicate TIG writes, two crash points:
//!    (a) kill after send, before the response is recorded (fake-tig
//!    `ambiguous` injection: applied server-side, 500 returned) — restart,
//!    reconcile adopts the write from confirmed state, resubmit is
//!    suppressed; covered for BOTH the benchmark and the proof write;
//!    (b) kill between intent-append and send (nothing sent) — restart,
//!    reconcile finds nothing, only then may the lane resend; exactly one
//!    server-side application either way.
//! 3. Stopped-path classification against fake-tig (deterministic twin of
//!    the live-testnet stopped run): confirmed `details.stopped = true`,
//!    no sampled nonces, no proof intent — chargeable outcome, NOT fraud.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use serde_json::{Value, json};
use spike::active::{build_proof_payload, commitment_body, load_accepted_package};
use spike::member::{
    OutputRecord, PackageIdentity, build_manifest, build_tar, compress_zstd, derived_uuid, jsonify,
    leaf_hashes_bytes, merkle_root, outputs_ndjson, qualities_bytes, sha256_hex,
};
use spike::pool::Pool;
use spike::trust::{
    GuardDecision, MemberTrust, PolicyStandIn, screen_reproduction, screen_solution_quality,
};
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

fn inject(base: &str, target: &str, mode: &str) {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{base}/_fake/inject"))
        .json(&json!({ "target": target, "mode": mode }))
        .send()
        .expect("inject");
    assert!(resp.status().is_success());
}

/// Server-side truth from the fake's admin surface.
fn fake_state(base: &str) -> Value {
    reqwest::blocking::get(format!("{base}/_fake/state"))
        .expect("state")
        .json()
        .expect("state json")
}

fn server_entries(state: &Value, section: &str, id_key: &str, id: &str) -> usize {
    state
        .pointer(&format!("/benchmarks/{section}"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|e| e.get(id_key).and_then(Value::as_str) == Some(id))
                .count()
        })
        .unwrap_or(0)
}

fn server_precommit_count(state: &Value) -> usize {
    state
        .pointer("/benchmarks/precommits")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn gateway(base: &str, dir: &std::path::Path) -> Gateway {
    Gateway {
        client: TigClient::new(base, "fake-testnet-key").expect("client"),
        ledger: Ledger::open(dir).expect("ledger"),
        player_id: PLAYER.to_owned(),
    }
}

fn attempts_with_phase(gw: &Gateway, intent_id: &str, phase: &str) -> usize {
    gw.ledger
        .read_all("attempts")
        .unwrap()
        .iter()
        .filter(|a| {
            a.get("intent_id").and_then(Value::as_str) == Some(intent_id)
                && a.get("phase").and_then(Value::as_str) == Some(phase)
        })
        .count()
}

/// Drive one precommit to a confirmed assignment; returns the benchmark id.
fn confirmed_assignment(base: &str, gw: &Gateway) -> String {
    let block = gw.client.latest_block().expect("block");
    let block_id = block["id"].as_str().unwrap().to_owned();
    let challenges = gw.client.challenges(&block_id).expect("challenges");
    let plan =
        plan_precommit(&block, &challenges, PLAYER, "c001", "a011", "aws_t4g").expect("plan");
    gw.submit_precommit(&plan).expect("precommit");
    advance(base, 1);
    let lines = gw.reconcile().expect("reconcile");
    assert!(lines.iter().any(|l| l.contains("CONFIRMED")), "{lines:?}");
    fold_intents(&gw.ledger.read_all("intents").unwrap())
        .values()
        .find_map(|r| {
            (r["state"] == "CONFIRMED" && r["write_kind"] == "PRECOMMIT")
                .then(|| r["benchmark_id"].as_str().unwrap().to_owned())
        })
        .expect("confirmed benchmark id")
}

/// Build a complete proof-material package for the confirmed assignment via
/// the S2 member library (deterministic stand-in for container execution).
struct BuiltPackage {
    package_id: String,
    declaration: Value,
    assignment_doc: Value,
    bytes: Vec<u8>,
}

fn build_package(gw: &Gateway, benchmark_id: &str) -> BuiltPackage {
    let assignment: Value = gw
        .ledger
        .read_doc(&format!("assignment-{benchmark_id}.json"))
        .unwrap()
        .expect("assignment doc");
    let num_nonces = assignment["details"]["num_nonces"].as_u64().unwrap();
    let records: Vec<OutputRecord> = (0..num_nonces)
        .map(|n| OutputRecord {
            nonce: n,
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
    let uuid = |role: &str| derived_uuid(&format!("spike-s5-test:{benchmark_id}:{role}"));
    let assignment_digest = sha256_hex(format!("spike-s5-test:digest:{benchmark_id}").as_bytes());
    let identity = PackageIdentity {
        benchmark_id: benchmark_id.to_owned(),
        assignment_digest: assignment_digest.clone(),
        member_id: uuid("member"),
        worker_id: uuid("worker"),
        slot_id: uuid("slot"),
        assignment_id: uuid("assignment"),
        package_id: uuid("package"),
        qualification_id: uuid("qualification"),
        qualification_spec_digest: sha256_hex(b"spike-s5-test:qualification-spec"),
        slot_generation: 1,
        compute: json!({
            "compute_kind": "CPU", "compute_type": "aws_t4g", "cpu_arch": "arm64",
            "cpu_vendor": "ARM", "cpu_cores": 2,
        }),
        member_agent_version: "spike-member/0.1.0-test".to_owned(),
        tig_upstream_commit: "ad08d1ea001a73ff5aab3b556d7f59246fece14e".to_owned(),
        benchmarker_version: "0.0.7".to_owned(),
        algorithm_binary_sha256: sha256_hex(b"spike-s5-test:algorithm-binary"),
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
        declaration,
        assignment_doc,
        bytes,
    }
}

fn upload_and_accept(pool: &Pool, built: &BuiltPackage) {
    pool.register_assignment(&built.assignment_doc)
        .expect("register assignment");
    let session = pool.create_upload(&built.declaration).expect("upload");
    let mut offset = 0u64;
    for chunk in built.bytes.chunks(4096) {
        offset = pool
            .put_chunk(&session.upload_id, offset, &sha256_hex(chunk), chunk)
            .expect("chunk");
    }
    assert_eq!(offset, built.bytes.len() as u64);
    assert!(pool.finalize(&built.package_id).expect("finalize").fresh);
}

// ---------------------------------------------------------------------------
// Fixture readers (fixtures/benchmark-artifact/v1)
// ---------------------------------------------------------------------------

fn artifact_fixture_path(rel: &str) -> String {
    format!(
        "{}/../../fixtures/benchmark-artifact/v1/{rel}",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn read_fixture_json(rel: &str) -> Value {
    let path = artifact_fixture_path(rel);
    serde_json::from_slice(&std::fs::read(&path).expect("fixture file")).expect("fixture json")
}

/// Decode `qualities.i32le.hex`: one line per i32, 8 hex chars little-endian.
fn read_fixture_qualities(case: &str) -> Vec<i32> {
    let text = std::fs::read_to_string(artifact_fixture_path(&format!(
        "cases/{case}/qualities.i32le.hex"
    )))
    .expect("qualities hex");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let bytes: Vec<u8> = (0..4)
                .map(|i| u8::from_str_radix(&line[i * 2..i * 2 + 2], 16).unwrap())
                .collect();
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
        .collect()
}

/// `(nonce, runtime_signature)` samples ordered by nonce.
type SignatureSamples = Vec<(u64, u64)>;

/// Read the packaged vs re-executed runtime signatures from the fixture's
/// `reproduction.json`, ordered by nonce.
fn read_fixture_reproduction() -> (SignatureSamples, SignatureSamples) {
    let doc = read_fixture_json("cases/method-non-reproducible/reproduction.json");
    let pick = |key: &str| -> Vec<(u64, u64)> {
        let map = doc[key].as_object().expect("signature map");
        let mut v: Vec<(u64, u64)> = map
            .iter()
            .map(|(nonce, sig)| {
                (
                    nonce.parse::<u64>().unwrap(),
                    sig.as_str().unwrap().parse::<u64>().unwrap(),
                )
            })
            .collect();
        v.sort_unstable();
        v
    };
    (
        pick("packaged_runtime_signatures"),
        pick("reproduced_runtime_signatures"),
    )
}

// ---------------------------------------------------------------------------
// 1. Member circuit breaker (+ deterministic stopped-path classification)
// ---------------------------------------------------------------------------

#[test]
fn circuit_breaker_stops_new_commitments() {
    let base = spawn_fake_tig();
    let dir = std::env::temp_dir().join(format!("spike-s5-breaker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let gw = gateway(&base, &dir.join("gateway"));
    let trust = MemberTrust::open(dir.join("trust")).expect("trust");
    // Fixture policy stand-ins (mining_system §11 leaves real values open):
    // tier k = 2, min quality 40, X = 2 TIG.
    let policy = PolicyStandIn::fixture_v1();
    let member = "member_alpha";
    let expected = read_fixture_json("expected.json");

    // ---- screening: solution-invalid (timed for the S5 measurement) -------
    let qualities = read_fixture_qualities("solution-invalid");
    let t0 = std::time::Instant::now();
    let s_invalid = screen_solution_quality(&qualities, 2, &policy).expect("screen");
    let solution_screen_us = t0.elapsed().as_micros();
    assert_eq!(s_invalid.outcome, "SOLUTION_INVALID");
    // Cross-check against the fixture's independently recorded expectation.
    let exp = &expected["cases"]["solution-invalid"]["expected_classification"];
    assert_eq!(Some(s_invalid.fraud), exp["fraud"].as_bool());
    assert_eq!(
        Some(s_invalid.chargeable_tier_failure),
        exp["chargeable_tier_failure"].as_bool()
    );
    assert_eq!(
        Some(s_invalid.attribution.as_str()),
        exp["attribution"].as_str()
    );

    // ---- screening: method-non-reproducible -------------------------------
    let (packaged, reproduced) = read_fixture_reproduction();
    let t0 = std::time::Instant::now();
    let s_method = screen_reproduction(&packaged, &reproduced).expect("screen");
    let method_screen_us = t0.elapsed().as_micros();
    assert_eq!(s_method.outcome, "METHOD_NON_REPRODUCIBLE");
    assert_eq!(
        s_method.detail["mismatched_nonces"],
        json!([2, 5]),
        "fixture reproduction disagrees at nonces 2 and 5"
    );
    let exp = &expected["cases"]["method-non-reproducible"]["expected_classification"];
    assert_eq!(
        Some(s_method.chargeable_tier_failure),
        exp["chargeable_tier_failure"].as_bool()
    );
    assert_eq!(Some(true), exp["method_loss_rule"].as_bool());
    assert!(s_method.method_loss);
    assert_eq!(
        Some(s_method.attribution.as_str()),
        exp["attribution"].as_str()
    );
    println!(
        "screening wall time: solution-quality {solution_screen_us} us, \
         method-reproduction {method_screen_us} us"
    );

    // ---- method loss does NOT move f; breaker stays closed ----------------
    trust
        .record_screening(member, "bench-method-1", &s_method, &policy)
        .unwrap();
    assert_eq!(trust.chargeable_failures(member).unwrap(), 0);
    assert_eq!(trust.method_loss_events(member).unwrap(), 1);
    assert!(!trust.breaker_open(member, &policy).unwrap());

    // ---- f = 1, 2 (<= k): breaker closed, offers still create intents ----
    for i in 0..2 {
        trust
            .record_screening(member, &format!("bench-invalid-{i}"), &s_invalid, &policy)
            .unwrap();
    }
    assert_eq!(trust.chargeable_failures(member).unwrap(), 2);
    assert!(!trust.breaker_open(member, &policy).unwrap());
    let block = gw.client.latest_block().unwrap();
    let block_id = block["id"].as_str().unwrap().to_owned();
    let challenges = gw.client.challenges(&block_id).unwrap();
    let plan =
        plan_precommit(&block, &challenges, PLAYER, "c001", "a011", "aws_t4g").expect("plan");
    let decision = trust
        .guard_precommit(&gw, member, &policy, &plan)
        .expect("guarded precommit");
    let benchmark_id = match decision {
        GuardDecision::Submitted { .. } => {
            // Resolve the serialized lane before anything else happens.
            advance(&base, 1);
            gw.reconcile().expect("reconcile");
            fold_intents(&gw.ledger.read_all("intents").unwrap())
                .values()
                .find_map(|r| {
                    (r["state"] == "CONFIRMED")
                        .then(|| r["benchmark_id"].as_str().unwrap().to_owned())
                })
                .expect("confirmed")
        }
        GuardDecision::Refused { .. } => panic!("breaker must be closed at f = 2 <= k = 2"),
    };
    assert_eq!(server_precommit_count(&fake_state(&base)), 1);

    // ---- deterministic stopped-path classification (twin of the live run) -
    let stop = gw
        .submit_benchmark_stopped(&benchmark_id, &json!({ "reason": "test stop" }))
        .expect("stopped submission");
    assert!(stop.sent);
    advance(&base, 1);
    gw.reconcile().expect("reconcile stopped");
    let confirmed = gw
        .ledger
        .read_doc(&format!("benchmark-confirmed-{benchmark_id}.json"))
        .unwrap()
        .expect("stopped confirmed doc");
    assert_eq!(
        confirmed.pointer("/entry/details/stopped"),
        Some(&json!(true))
    );
    assert_eq!(
        confirmed.pointer("/entry/details/sampled_nonces"),
        Some(&Value::Null),
        "stopped benchmark gets no sampled nonces"
    );
    let frauds = fake_state(&base)
        .pointer("/benchmarks/frauds")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(frauds, 0, "stopped is not fraud (invariant 18)");
    assert!(
        fold_intents(&gw.ledger.read_all("intents").unwrap())
            .values()
            .all(|r| r["write_kind"] != "PROOF"),
        "no proof intent may exist for a stopped benchmark"
    );
    // The stopped outcome is itself a chargeable capacity failure: f = 3 > k.
    trust
        .record_screening(
            member,
            &benchmark_id,
            &spike::trust::Screening {
                outcome: "STOPPED".to_owned(),
                attribution: "NONE".to_owned(),
                fraud: false,
                chargeable_tier_failure: true,
                method_loss: false,
                detail: json!({
                    "rule": "mining_system.md §8: no bundle met the minimum verification \
                             quality — chargeable, not fraud",
                    "confirmed_at_block": confirmed.get("confirmed_at_block"),
                }),
            },
            &policy,
        )
        .unwrap();
    assert_eq!(trust.chargeable_failures(member).unwrap(), 3);
    assert!(
        trust.breaker_open(member, &policy).unwrap(),
        "f = 3 > k = 2"
    );

    // ---- post-breaker offer: NO intent, NO fake-tig write -----------------
    let intents_before = gw.ledger.read_all("intents").unwrap().len();
    let state_before = fake_state(&base);
    let block = gw.client.latest_block().unwrap();
    let block_id = block["id"].as_str().unwrap().to_owned();
    let challenges = gw.client.challenges(&block_id).unwrap();
    let plan =
        plan_precommit(&block, &challenges, PLAYER, "c001", "a011", "aws_t4g").expect("plan");
    match trust
        .guard_precommit(&gw, member, &policy, &plan)
        .expect("guard decision")
    {
        GuardDecision::Refused {
            chargeable_failures,
            tier_k,
            ..
        } => {
            assert_eq!(chargeable_failures, 3);
            assert_eq!(tier_k, 2);
        }
        GuardDecision::Submitted { intent_id } => {
            panic!("breaker open but intent {intent_id} was created")
        }
    }
    assert_eq!(
        gw.ledger.read_all("intents").unwrap().len(),
        intents_before,
        "no intent record was appended by the refused offer"
    );
    let state_after = fake_state(&base);
    assert_eq!(
        server_precommit_count(&state_after),
        server_precommit_count(&state_before),
        "no new benchmark was created server-side by the refused offer"
    );
    assert_eq!(
        state_after.pointer("/benchmarks/benchmarks"),
        state_before.pointer("/benchmarks/benchmarks"),
        "no benchmark submission happened either"
    );

    // ---- breaker state survives restart (durable ledger) ------------------
    drop(trust);
    let trust = MemberTrust::open(dir.join("trust")).expect("reopen trust");
    assert!(trust.breaker_open(member, &policy).unwrap());
    // A different member is unaffected.
    assert!(!trust.breaker_open("member_beta", &policy).unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2a. Crash after send, before response recorded (ambiguous) — BENCHMARK and
//     PROOF writes: reconcile adopts, resubmit suppressed, one write each.
// ---------------------------------------------------------------------------

#[test]
fn restart_after_ambiguous_write_adopts_without_resubmit() {
    let base = spawn_fake_tig();
    let dir = std::env::temp_dir().join(format!("spike-s5-ambig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let gw_dir = dir.join("gateway");
    let pool = Pool::open(dir.join("pool")).expect("pool");

    let gw = gateway(&base, &gw_dir);
    let benchmark_id = confirmed_assignment(&base, &gw);
    let built = build_package(&gw, &benchmark_id);
    upload_and_accept(&pool, &built);
    let pkg = load_accepted_package(&pool, &built.package_id).expect("accepted");
    let body = commitment_body(&pkg).expect("body");

    // ---- crash point (a) on submit-benchmark ------------------------------
    inject(&base, "submit-benchmark", "ambiguous");
    let result = gw
        .submit_benchmark_commitment(&pkg.acceptance, &body)
        .expect("ambiguous submission");
    assert!(result.sent);
    assert_eq!(
        result.state, "OUTCOME_UNKNOWN",
        "500 after apply leaves the outcome unknown"
    );
    let benchmark_intent = result.intent_id.clone();
    // Server-side: the write WAS applied despite the 500.
    assert_eq!(
        server_entries(&fake_state(&base), "benchmarks", "id", &benchmark_id),
        1
    );

    // ---- restart: fresh process state over the same durable ledgers -------
    drop(gw);
    let gw = gateway(&base, &gw_dir);
    // Unsent-intent recovery must NOT touch an intent with a recorded
    // response — the write may have landed (and here it did).
    let lines = gw.recover_unsent_intents().expect("recovery");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&benchmark_intent) && l.contains("left for reconcile")),
        "{lines:?}"
    );
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile");
    assert!(
        lines.iter().any(|l| l.contains("BENCHMARK CONFIRMED")),
        "{lines:?}"
    );
    // The write is adopted from confirmed state; an identical retry does not
    // send.
    let retry = gw
        .submit_benchmark_commitment(&pkg.acceptance, &body)
        .expect("post-restart retry");
    assert!(!retry.sent, "adopted write is never resubmitted");
    assert_eq!(retry.state, "CONFIRMED");
    assert_eq!(
        server_entries(&fake_state(&base), "benchmarks", "id", &benchmark_id),
        1,
        "exactly one benchmark submission server-side"
    );
    assert_eq!(
        attempts_with_phase(&gw, &benchmark_intent, "attempt"),
        1,
        "exactly one send ever happened"
    );

    // ---- crash point (a) on submit-proof ----------------------------------
    let confirmed = gw
        .ledger
        .read_doc(&format!("benchmark-confirmed-{benchmark_id}.json"))
        .unwrap()
        .expect("confirmed benchmark doc");
    let sampled: Vec<u64> = confirmed["entry"]["details"]["sampled_nonces"]
        .as_array()
        .expect("sampled nonces")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let pkg = load_accepted_package(&pool, &built.package_id).expect("reload");
    let payload = build_proof_payload(&pkg, &sampled).expect("payload");
    gw.ledger
        .write_doc(&format!("proof-payload-{benchmark_id}.json"), &payload)
        .unwrap();
    inject(&base, "submit-proof", "ambiguous");
    let result = gw.submit_proof(&benchmark_id).expect("ambiguous proof");
    assert!(result.sent);
    assert_eq!(result.state, "OUTCOME_UNKNOWN");
    let proof_intent = result.intent_id.clone();
    assert_eq!(
        server_entries(&fake_state(&base), "proofs", "benchmark_id", &benchmark_id),
        1
    );

    // ---- restart again, reconcile, no proof resubmission ------------------
    drop(gw);
    let gw = gateway(&base, &gw_dir);
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile proof");
    assert!(
        lines.iter().any(|l| l.contains("PROOF CONFIRMED")),
        "{lines:?}"
    );
    let retry = gw
        .submit_proof(&benchmark_id)
        .expect("post-restart proof retry");
    assert!(!retry.sent, "adopted proof write is never resubmitted");
    assert_eq!(retry.state, "CONFIRMED");
    assert_eq!(
        server_entries(&fake_state(&base), "proofs", "benchmark_id", &benchmark_id),
        1,
        "exactly one proof submission server-side"
    );
    assert_eq!(attempts_with_phase(&gw, &proof_intent, "attempt"), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2b. Crash between intent-append and send: nothing was sent; reconcile finds
//     nothing; only then may the lane resend — exactly one application.
// ---------------------------------------------------------------------------

#[test]
fn restart_with_unsent_intent_resends_exactly_once() {
    let base = spawn_fake_tig();
    let dir = std::env::temp_dir().join(format!("spike-s5-unsent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let gw_dir = dir.join("gateway");
    let pool = Pool::open(dir.join("pool")).expect("pool");

    let gw = gateway(&base, &gw_dir);
    let benchmark_id = confirmed_assignment(&base, &gw);
    let built = build_package(&gw, &benchmark_id);
    upload_and_accept(&pool, &built);
    let pkg = load_accepted_package(&pool, &built.package_id).expect("accepted");
    let body = commitment_body(&pkg).expect("body");

    // ---- crash point (b): the durable pre-send records exist, nothing sent
    let intent_id = gw
        .simulate_crash_before_send(
            "BENCHMARK",
            "submit-benchmark",
            &benchmark_id,
            &body,
            &json!({ "test": "crash between intent-append and send" }),
        )
        .expect("crash hook");
    assert_eq!(
        server_entries(&fake_state(&base), "benchmarks", "id", &benchmark_id),
        0,
        "nothing reached the server before the crash"
    );

    // ---- restart ----------------------------------------------------------
    drop(gw);
    let gw = gateway(&base, &gw_dir);
    // The unresolved PENDING intent blocks a blind identical retry...
    let blocked = gw.submit_benchmark_commitment(&pkg.acceptance, &body);
    assert!(
        blocked.unwrap_err().to_string().contains("reconcile"),
        "no resend before reconciliation"
    );
    // ...reconcile finds nothing for it...
    let lines = gw.reconcile().expect("reconcile");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&intent_id) && l.contains("not yet visible")),
        "{lines:?}"
    );
    // ...and only the recovery policy (no recorded response AND nothing
    // server-side) releases the lane for one identical resend.
    let lines = gw.recover_unsent_intents().expect("recovery");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&intent_id) && l.contains("identical resend permitted")),
        "{lines:?}"
    );
    let resend = gw
        .submit_benchmark_commitment(&pkg.acceptance, &body)
        .expect("resend after recovery");
    assert_eq!(
        resend.intent_id, intent_id,
        "identical payload maps to the same intent"
    );
    assert!(resend.sent);
    assert_eq!(
        resend.state, "SUBMITTED",
        "server accepted (200) — proving no prior application existed"
    );

    // ---- confirm and verify exactly one server-side application -----------
    advance(&base, 1);
    let lines = gw.reconcile().expect("reconcile after resend");
    assert!(
        lines.iter().any(|l| l.contains("BENCHMARK CONFIRMED")),
        "{lines:?}"
    );
    assert_eq!(
        server_entries(&fake_state(&base), "benchmarks", "id", &benchmark_id),
        1,
        "exactly one benchmark submission server-side after recovery"
    );
    // Two attempt records (the crashed one and the resend), one response.
    assert_eq!(attempts_with_phase(&gw, &intent_id, "attempt"), 2);
    assert_eq!(attempts_with_phase(&gw, &intent_id, "response"), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
