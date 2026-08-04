//! Spike gateway: the only component that holds the TIG API key and performs
//! protocol writes, driven by durably persisted write intents.
//!
//! Contract: `docs/tig_integration.md` §6–§11; plan: `docs/plans/protocol-spike.md`
//! phases S1 (issue #10) and S4 (issue #13). Disposable spike code —
//! shortcuts are allowed except for the guarantees under test: the intent and
//! attempt ledgers are durable (append + fsync) and written *before* any
//! network send; an HTTP 200 is never treated as confirmation; the precommit
//! lane is serialized (at most one unresolved precommit intent); BENCHMARK
//! and PROOF writes are idempotent per benchmark (reconcile before retry,
//! never two concurrent writes for one benchmark).

pub mod active;
pub mod member;
pub mod pool;
pub mod trust;

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Durable append-only ledger
// ---------------------------------------------------------------------------

pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Ledger> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating state dir {}", dir.display()))?;
        Ok(Ledger { dir })
    }

    /// Append one JSON record to `<name>.jsonl`, fsynced before returning.
    pub fn append(&self, name: &str, record: &Value) -> Result<()> {
        let path = self.dir.join(format!("{name}.jsonl"));
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    pub fn read_all(&self, name: &str) -> Result<Vec<Value>> {
        let path = self.dir.join(format!("{name}.jsonl"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(Into::into))
            .collect()
    }

    /// Write a whole JSON document (fsynced) — used for the confirmed
    /// assignment record and other durable state documents.
    pub fn write_doc(&self, name: &str, doc: &Value) -> Result<PathBuf> {
        let path = self.dir.join(name);
        let mut f = File::create(&path)?;
        f.write_all(serde_json::to_string_pretty(doc)?.as_bytes())?;
        f.sync_all()?;
        Ok(path)
    }

    /// Read back a whole JSON document written by `write_doc`, if present.
    pub fn read_doc(&self, name: &str) -> Result<Option<Value>> {
        let path = self.dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => {
                Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("parsing ledger doc {}", path.display())
                })?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading ledger doc {}", path.display())),
        }
    }
}

/// Latest state of each intent, folded from the append-only intent ledger.
pub fn fold_intents(records: &[Value]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for r in records {
        if let Some(id) = r.get("intent_id").and_then(Value::as_str) {
            out.insert(id.to_owned(), r.clone());
        }
    }
    out
}

fn is_unresolved(state: &str) -> bool {
    matches!(state, "PENDING" | "SUBMITTED" | "OUTCOME_UNKNOWN")
}

/// Outcome of an idempotent BENCHMARK/PROOF write request.
#[derive(Debug, Clone)]
pub struct WriteResult {
    pub intent_id: String,
    /// Whether a network send actually happened (false when the write was
    /// suppressed because confirmed state already covers it).
    pub sent: bool,
    pub state: String,
}

// ---------------------------------------------------------------------------
// TIG client (reads are public; writes carry the API key)
// ---------------------------------------------------------------------------

pub struct TigClient {
    pub base_url: String,
    api_key: String,
    http: reqwest::blocking::Client,
    /// API call count per endpoint (path without query), for the §7
    /// "TIG API call count" measurement.
    calls: std::sync::Mutex<BTreeMap<String, u64>>,
}

impl TigClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<TigClient> {
        Ok(TigClient {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(60))
                .build()?,
            calls: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn note_call(&self, endpoint: &str) {
        if let Ok(mut calls) = self.calls.lock() {
            *calls.entry(endpoint.to_owned()).or_insert(0) += 1;
        }
    }

    /// Calls made through this client instance, by endpoint.
    pub fn call_counts(&self) -> BTreeMap<String, u64> {
        self.calls
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    pub fn get(&self, path_and_query: &str) -> Result<Value> {
        let endpoint = path_and_query.split('?').next().unwrap_or(path_and_query);
        self.note_call(endpoint);
        let url = format!("{}/{}", self.base_url, path_and_query);
        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body: Value = resp.json().with_context(|| format!("decoding GET {url}"))?;
        if !status.is_success() {
            bail!("GET {url} -> {status}: {body}");
        }
        Ok(body)
    }

    /// POST a write. Returns (http_status, body). Does NOT interpret success —
    /// the caller records the attempt before calling and the response after.
    pub fn post_write(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        self.note_call(path);
        let url = format!("{}/{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .json(body)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, body))
    }

    pub fn latest_block(&self) -> Result<Value> {
        let v = self.get("get-block?include_data=true")?;
        v.get("block")
            .cloned()
            .ok_or_else(|| anyhow!("get-block: missing 'block' envelope"))
    }

    pub fn challenges(&self, block_id: &str) -> Result<Vec<Value>> {
        let v = self.get(&format!("get-challenges?block_id={block_id}"))?;
        v.get("challenges")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| anyhow!("get-challenges: missing 'challenges' array"))
    }

    pub fn benchmarks(&self, block_id: &str, player_id: &str) -> Result<Value> {
        self.get(&format!(
            "get-benchmarks?block_id={block_id}&player_id={player_id}"
        ))
    }
}

// ---------------------------------------------------------------------------
// Precommit planning
// ---------------------------------------------------------------------------

pub struct PrecommitPlan {
    pub body: Value,
    /// Fee per track under both candidate fee bases (the §8.1 open question):
    /// (track_id, fee_if_per_bundle, fee_if_per_nonce)
    pub fee_by_track: Vec<(String, u128, u128)>,
    pub max_collateral_per_bundle_basis: u128,
    pub max_collateral_per_nonce_basis: u128,
}

fn precise(v: &Value, key: &str) -> u128 {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Build a §6.1-shaped precommit body for `challenge_id`/`algorithm_id` with
/// exactly every live active track, plus fee/collateral calculations under
/// both candidate bases (recorded for the fee-basis question).
pub fn plan_precommit(
    block: &Value,
    challenges: &[Value],
    player_id: &str,
    challenge_id: &str,
    algorithm_id: &str,
    compute_type: &str,
) -> Result<PrecommitPlan> {
    let block_id = block
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("block missing id"))?;
    let active: Vec<&str> = block
        .pointer("/data/active_ids/challenge")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !active.contains(&challenge_id) {
        bail!("challenge {challenge_id} is not in the current active set {active:?}");
    }
    let ch = challenges
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(challenge_id))
        .ok_or_else(|| anyhow!("challenge {challenge_id} not in get-challenges"))?;
    let cfg = ch
        .get("config")
        .ok_or_else(|| anyhow!("challenge missing config"))?;
    let tracks = cfg
        .get("active_tracks")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("challenge missing active_tracks"))?;
    let min_bundles = cfg
        .get("min_num_bundles")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let max_fuel = cfg
        .get("max_fuel_budget")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let base_fee = precise(cfg, "base_fee");
    let per_nonce_fee = precise(cfg, "per_nonce_fee");

    let mut track_settings = serde_json::Map::new();
    let mut fee_by_track = Vec::new();
    for (track_id, tc) in tracks {
        let per_bundle = tc
            .get("num_nonces_per_bundle")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let num_bundles = min_bundles.max(1);
        let num_nonces = num_bundles * per_bundle;
        track_settings.insert(
            track_id.clone(),
            json!({
                "hyperparameters": null,
                "fuel_budget": max_fuel,
                "num_bundles": num_bundles,
            }),
        );
        // Both candidate fee bases (mining_system §6.8 vs tig_integration).
        let fee_bundles = base_fee + per_nonce_fee * u128::from(num_bundles);
        let fee_nonces = base_fee + per_nonce_fee * u128::from(num_nonces);
        fee_by_track.push((track_id.clone(), fee_bundles, fee_nonces));
    }
    let body = json!({
        "settings": {
            "player_id": player_id,
            "block_id": block_id,
            "challenge_id": challenge_id,
            "algorithm_id": algorithm_id,
            "track_id": "",
        },
        "track_settings": Value::Object(track_settings),
        "compute_type": compute_type,
    });
    let max_b = fee_by_track.iter().map(|f| f.1).max().unwrap_or(0);
    let max_n = fee_by_track.iter().map(|f| f.2).max().unwrap_or(0);
    Ok(PrecommitPlan {
        body,
        fee_by_track,
        max_collateral_per_bundle_basis: max_b,
        max_collateral_per_nonce_basis: max_n,
    })
}

pub fn payload_hash(body: &Value) -> String {
    let canonical = serde_json::to_string(body).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(canonical.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Gateway flow
// ---------------------------------------------------------------------------

pub struct Gateway {
    pub client: TigClient,
    pub ledger: Ledger,
    pub player_id: String,
}

impl Gateway {
    /// Serialized precommit lane: refuse a new intent while any prior
    /// precommit intent is unresolved (`tig_integration.md` §10).
    pub fn assert_lane_free(&self) -> Result<()> {
        let intents = fold_intents(&self.ledger.read_all("intents")?);
        for (id, rec) in intents {
            let state = rec.get("state").and_then(Value::as_str).unwrap_or("");
            if is_unresolved(state) {
                bail!("precommit lane busy: intent {id} is {state}; reconcile before submitting");
            }
        }
        Ok(())
    }

    /// Create the durable intent, record the attempt, send, record the
    /// response. Returns the intent id. A transport error or non-200 leaves
    /// the intent OUTCOME_UNKNOWN / FAILED for reconcile to resolve.
    pub fn submit_precommit(&self, plan: &PrecommitPlan) -> Result<String> {
        self.assert_lane_free()?;
        let hash = payload_hash(&plan.body);
        let intent_id = format!("intent_{}", &hash[..16]);
        self.ledger.append(
            "intents",
            &json!({
                "intent_id": intent_id,
                "write_kind": "PRECOMMIT",
                "state": "PENDING",
                "payload_sha256": hash,
                "payload": plan.body,
                "fee_by_track": plan.fee_by_track.iter().map(|(t, fb, fn_)| json!({
                    "track": t, "fee_per_bundle_basis": fb.to_string(), "fee_per_nonce_basis": fn_.to_string(),
                })).collect::<Vec<_>>(),
                "max_collateral_per_bundle_basis": plan.max_collateral_per_bundle_basis.to_string(),
                "max_collateral_per_nonce_basis": plan.max_collateral_per_nonce_basis.to_string(),
            }),
        )?;
        // Attempt recorded BEFORE the send (tig_integration §7.3 analogue).
        self.ledger.append(
            "attempts",
            &json!({ "intent_id": intent_id, "phase": "attempt", "endpoint": "submit-precommit", "payload_sha256": hash }),
        )?;
        let outcome = self.client.post_write("submit-precommit", &plan.body);
        match outcome {
            Ok((status, body)) => {
                self.ledger.append(
                    "attempts",
                    &json!({ "intent_id": intent_id, "phase": "response", "http_status": status, "body": body }),
                )?;
                if (200..300).contains(&status) {
                    let benchmark_id = body.get("benchmark_id").and_then(Value::as_str);
                    // HTTP success is NOT confirmation — state is SUBMITTED.
                    self.ledger.append(
                        "intents",
                        &json!({
                            "intent_id": intent_id, "write_kind": "PRECOMMIT", "state": "SUBMITTED",
                            "benchmark_id": benchmark_id, "payload_sha256": hash, "payload": plan.body,
                        }),
                    )?;
                } else {
                    self.ledger.append(
                        "intents",
                        &json!({ "intent_id": intent_id, "write_kind": "PRECOMMIT", "state": if status >= 500 { "OUTCOME_UNKNOWN" } else { "FAILED" }, "http_status": status, "payload_sha256": hash, "payload": plan.body }),
                    )?;
                }
            }
            Err(e) => {
                // Transport failure after send: the write may have landed.
                self.ledger.append(
                    "attempts",
                    &json!({ "intent_id": intent_id, "phase": "response", "transport_error": e.to_string() }),
                )?;
                self.ledger.append(
                    "intents",
                    &json!({ "intent_id": intent_id, "write_kind": "PRECOMMIT", "state": "OUTCOME_UNKNOWN", "payload_sha256": hash, "payload": plan.body }),
                )?;
            }
        }
        Ok(intent_id)
    }

    /// Any unresolved intent for `benchmark_id` other than `exclude`
    /// (tig_integration §11: never two concurrent writes for one benchmark).
    fn unresolved_for_benchmark(
        &self,
        benchmark_id: &str,
        exclude: &str,
    ) -> Result<Option<String>> {
        for (id, rec) in fold_intents(&self.ledger.read_all("intents")?) {
            if id == exclude {
                continue;
            }
            let state = rec.get("state").and_then(Value::as_str).unwrap_or("");
            if rec.get("benchmark_id").and_then(Value::as_str) == Some(benchmark_id)
                && is_unresolved(state)
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Shared BENCHMARK/PROOF write path under the S1 ledger discipline:
    /// durable intent before send, attempt/response ledger, 200 ≠
    /// confirmation. Idempotent per payload: an identical retry maps to the
    /// same intent; unresolved intents demand reconciliation before retry; a
    /// CONFIRMED intent (or a confirmed-read document from `reconcile`)
    /// short-circuits without sending.
    fn submit_lifecycle_write(
        &self,
        write_kind: &str,
        endpoint: &str,
        benchmark_id: &str,
        body: &Value,
        evidence: &Value,
    ) -> Result<WriteResult> {
        let hash = payload_hash(body);
        let intent_id = format!("intent_{}_{}", write_kind.to_ascii_lowercase(), &hash[..16]);
        let intents = fold_intents(&self.ledger.read_all("intents")?);
        if let Some(rec) = intents.get(&intent_id) {
            let state = rec.get("state").and_then(Value::as_str).unwrap_or("");
            if state == "CONFIRMED" {
                return Ok(WriteResult {
                    intent_id,
                    sent: false,
                    state: "CONFIRMED".to_owned(),
                });
            }
            if is_unresolved(state) {
                bail!(
                    "{write_kind} intent {intent_id} is {state}: reconcile against confirmed \
                     state before retrying (never resubmit an unresolved write)"
                );
            }
            // FAILED: an explicit retry of the identical payload is allowed
            // after reconciliation showed no confirmed entry.
        }
        // Reconcile-before-retry: if a previous reconcile already recorded
        // the confirmed entry for this benchmark, do not send again.
        let confirmed_doc = format!(
            "{}-confirmed-{benchmark_id}.json",
            write_kind.to_ascii_lowercase()
        );
        if self.ledger.read_doc(&confirmed_doc)?.is_some() {
            self.ledger.append(
                "intents",
                &json!({
                    "intent_id": intent_id, "write_kind": write_kind, "state": "CONFIRMED",
                    "benchmark_id": benchmark_id, "payload_sha256": hash,
                    "note": "confirmed entry already observed from confirmed reads; write suppressed",
                }),
            )?;
            return Ok(WriteResult {
                intent_id,
                sent: false,
                state: "CONFIRMED".to_owned(),
            });
        }
        if let Some(other) = self.unresolved_for_benchmark(benchmark_id, &intent_id)? {
            bail!(
                "refusing {write_kind} write: intent {other} for benchmark {benchmark_id} is \
                 unresolved (never two concurrent writes for the same benchmark)"
            );
        }

        self.append_presend_records(
            &intent_id,
            write_kind,
            endpoint,
            benchmark_id,
            &hash,
            evidence,
        )?;
        let outcome = self.client.post_write(endpoint, body);
        let state = match outcome {
            Ok((status, resp_body)) => {
                self.ledger.append(
                    "attempts",
                    &json!({ "intent_id": intent_id, "phase": "response", "http_status": status, "body": resp_body }),
                )?;
                if (200..300).contains(&status) {
                    // HTTP success is NOT confirmation — state is SUBMITTED.
                    "SUBMITTED"
                } else if status >= 500 {
                    "OUTCOME_UNKNOWN"
                } else {
                    "FAILED"
                }
            }
            Err(e) => {
                // Transport failure after send: the write may have landed.
                self.ledger.append(
                    "attempts",
                    &json!({ "intent_id": intent_id, "phase": "response", "transport_error": e.to_string() }),
                )?;
                "OUTCOME_UNKNOWN"
            }
        };
        self.ledger.append(
            "intents",
            &json!({
                "intent_id": intent_id, "write_kind": write_kind, "state": state,
                "benchmark_id": benchmark_id, "payload_sha256": hash, "evidence": evidence,
            }),
        )?;
        Ok(WriteResult {
            intent_id,
            sent: true,
            state: state.to_owned(),
        })
    }

    /// Exactly the durable records `submit_lifecycle_write` appends BEFORE
    /// the network send: the PENDING intent, then the attempt record.
    fn append_presend_records(
        &self,
        intent_id: &str,
        write_kind: &str,
        endpoint: &str,
        benchmark_id: &str,
        payload_sha256: &str,
        evidence: &Value,
    ) -> Result<()> {
        self.ledger.append(
            "intents",
            &json!({
                "intent_id": intent_id, "write_kind": write_kind, "state": "PENDING",
                "benchmark_id": benchmark_id, "payload_sha256": payload_sha256, "evidence": evidence,
            }),
        )?;
        self.ledger.append(
            "attempts",
            &json!({ "intent_id": intent_id, "phase": "attempt", "endpoint": endpoint, "payload_sha256": payload_sha256 }),
        )
    }

    /// Crash-point hook for the S5 restart tests: perform exactly the durable
    /// writes that precede the network send, then return WITHOUT sending —
    /// the ledger state a process kill between the intent/attempt append and
    /// the send leaves behind. Returns the intent id the identical payload
    /// maps to.
    pub fn simulate_crash_before_send(
        &self,
        write_kind: &str,
        endpoint: &str,
        benchmark_id: &str,
        body: &Value,
        evidence: &Value,
    ) -> Result<String> {
        let hash = payload_hash(body);
        let intent_id = format!("intent_{}_{}", write_kind.to_ascii_lowercase(), &hash[..16]);
        self.append_presend_records(
            &intent_id,
            write_kind,
            endpoint,
            benchmark_id,
            &hash,
            evidence,
        )?;
        Ok(intent_id)
    }

    /// Restart recovery for the crash window between intent persistence and a
    /// recorded send response (`tig_integration.md` §10 step 7: reconcile a
    /// write before retrying it). For each unresolved BENCHMARK/PROOF intent:
    ///
    /// - a recorded send response (success, error, or transport failure)
    ///   means the send happened — leave the intent for `reconcile`, which
    ///   adopts the write from confirmed state; never downgrade it here;
    /// - no recorded response AND the benchmark visible server-side means the
    ///   send landed with a lost response — leave it for `reconcile` too;
    /// - no recorded response AND nothing server-side means nothing was
    ///   applied: mark the intent FAILED so the serialized lane may resend
    ///   the identical payload exactly once.
    ///
    /// Residual risk, documented for the spike report: on live TIG a landed
    /// write could in principle lag visibility; the resend then fails as a
    /// duplicate (4xx -> FAILED) and `reconcile` still adopts the single
    /// confirmed entry, so the server applies the write at most once either
    /// way.
    pub fn recover_unsent_intents(&self) -> Result<Vec<String>> {
        let block = self.client.latest_block()?;
        let block_id = block
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("block missing id"))?;
        let benches = self.client.benchmarks(block_id, &self.player_id)?;
        let attempts = self.ledger.read_all("attempts")?;
        let mut out = Vec::new();
        for (intent_id, rec) in fold_intents(&self.ledger.read_all("intents")?) {
            let state = rec.get("state").and_then(Value::as_str).unwrap_or("");
            if !is_unresolved(state) {
                continue;
            }
            let write_kind = rec.get("write_kind").and_then(Value::as_str).unwrap_or("");
            let (section, id_key) = match write_kind {
                "BENCHMARK" => ("benchmarks", "id"),
                "PROOF" => ("proofs", "benchmark_id"),
                _ => {
                    out.push(format!(
                        "{intent_id}: {write_kind} not covered by unsent-intent recovery \
                         (precommits use the §10 lost-response lane search in reconcile)"
                    ));
                    continue;
                }
            };
            let has_response = attempts.iter().any(|a| {
                a.get("intent_id").and_then(Value::as_str) == Some(intent_id.as_str())
                    && a.get("phase").and_then(Value::as_str) == Some("response")
            });
            if has_response {
                out.push(format!(
                    "{intent_id}: {state} with a recorded send response — left for reconcile"
                ));
                continue;
            }
            let Some(benchmark_id) = rec.get("benchmark_id").and_then(Value::as_str) else {
                out.push(format!(
                    "{intent_id}: {state} without benchmark_id — skipped"
                ));
                continue;
            };
            let visible = benches
                .get(section)
                .and_then(Value::as_array)
                .is_some_and(|arr| {
                    arr.iter()
                        .any(|e| e.get(id_key).and_then(Value::as_str) == Some(benchmark_id))
                });
            if visible {
                out.push(format!(
                    "{intent_id}: no recorded response but {write_kind} for {benchmark_id} is \
                     visible server-side — left for reconcile"
                ));
                continue;
            }
            self.ledger.append(
                "intents",
                &json!({
                    "intent_id": intent_id, "write_kind": write_kind, "state": "FAILED",
                    "benchmark_id": benchmark_id,
                    "note": "restart recovery: intent persisted, no send response recorded, no \
                             server-side entry — nothing was applied; the lane may resend the \
                             identical payload once (tig_integration.md §10 step 7)",
                }),
            )?;
            out.push(format!(
                "{intent_id}: PENDING with no recorded send and no server-side entry -> FAILED \
                 (identical resend permitted)"
            ));
        }
        Ok(out)
    }

    /// Submit an explicit stopped benchmark (`tig_integration.md` §6.2):
    /// `stopped` true, `merkle_root` and `solution_quality` null. Durable
    /// proof-material acceptance is a prerequisite only for the NON-stopped
    /// write, so no acceptance record is demanded here; the caller supplies
    /// the stop decision evidence instead.
    pub fn submit_benchmark_stopped(
        &self,
        benchmark_id: &str,
        stop_evidence: &Value,
    ) -> Result<WriteResult> {
        let body = json!({
            "benchmark_id": benchmark_id,
            "stopped": true,
            "merkle_root": Value::Null,
            "solution_quality": Value::Null,
        });
        let evidence = json!({ "stopped": true, "stop_decision": stop_evidence });
        self.submit_lifecycle_write(
            "BENCHMARK",
            "submit-benchmark",
            benchmark_id,
            &body,
            &evidence,
        )
    }

    /// Submit the benchmark commitment (`tig_integration.md` §6.2).
    ///
    /// Architecture invariant 4: no commitment intent exists before durable
    /// package acceptance — the caller must present the pool's committed
    /// acceptance record, verified here BEFORE any intent is written.
    pub fn submit_benchmark_commitment(
        &self,
        acceptance: &Value,
        body: &Value,
    ) -> Result<WriteResult> {
        let benchmark_id = body
            .get("benchmark_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("commitment body missing benchmark_id"))?;
        let receipt = acceptance.get("receipt").ok_or_else(|| {
            anyhow!(
                "invariant 4 refusal: no durable acceptance receipt — refusing to create a \
                 benchmark commitment intent before durable package acceptance"
            )
        })?;
        if acceptance.get("assignment_state").and_then(Value::as_str)
            != Some("PACKAGE_DURABLY_ACCEPTED")
        {
            bail!(
                "invariant 4 refusal: acceptance record is not PACKAGE_DURABLY_ACCEPTED — \
                 refusing to create a benchmark commitment intent"
            );
        }
        if receipt.get("benchmark_id").and_then(Value::as_str) != Some(benchmark_id) {
            bail!("invariant 4 refusal: acceptance receipt is not for benchmark {benchmark_id}");
        }
        let package_sha256 = receipt
            .get("package_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("acceptance receipt missing package_sha256"))?;
        let evidence = json!({
            "durable_acceptance": {
                "receipt_id": receipt.get("receipt_id"),
                "package_id": receipt.get("package_id"),
                "package_sha256": package_sha256,
                "accepted_at": receipt.get("accepted_at"),
            }
        });
        self.submit_lifecycle_write(
            "BENCHMARK",
            "submit-benchmark",
            benchmark_id,
            body,
            &evidence,
        )
    }

    /// Submit the sampled proof (`tig_integration.md` §6.3) idempotently.
    ///
    /// Architecture invariant 5: no proof intent exists before a canonical
    /// proof payload for the confirmed sample is ready — the only path to a
    /// proof intent reads the durable `proof-payload-<benchmark_id>.json`
    /// document persisted by the S4 controller after constructing the payload
    /// from the retained accepted package.
    pub fn submit_proof(&self, benchmark_id: &str) -> Result<WriteResult> {
        let payload = self
            .ledger
            .read_doc(&format!("proof-payload-{benchmark_id}.json"))?
            .ok_or_else(|| {
                anyhow!(
                    "invariant 5 refusal: no durable canonical proof payload for {benchmark_id}; \
                 refusing to create a proof intent"
                )
            })?;
        if payload.get("benchmark_id").and_then(Value::as_str) != Some(benchmark_id) {
            bail!("proof payload document is not for benchmark {benchmark_id}");
        }
        let n = payload
            .get("merkle_proofs")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if n == 0 {
            bail!("proof payload for {benchmark_id} has no merkle_proofs");
        }
        let evidence = json!({ "proof_payload_doc": format!("proof-payload-{benchmark_id}.json"), "num_proofs": n });
        self.submit_lifecycle_write("PROOF", "submit-proof", benchmark_id, &payload, &evidence)
    }

    /// Advance every unresolved intent from confirmed reads only. Returns a
    /// human-readable status line per intent.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        let block = self.client.latest_block()?;
        let block_id = block
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("block missing id"))?;
        let block_height = block
            .pointer("/details/height")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let benches = self.client.benchmarks(block_id, &self.player_id)?;
        let precommits = benches
            .get("precommits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for (intent_id, rec) in fold_intents(&self.ledger.read_all("intents")?) {
            let state = rec.get("state").and_then(Value::as_str).unwrap_or("");
            if !is_unresolved(state) {
                out.push(format!("{intent_id}: {state} (terminal)"));
                continue;
            }
            let write_kind = rec
                .get("write_kind")
                .and_then(Value::as_str)
                .unwrap_or("PRECOMMIT");
            if write_kind != "PRECOMMIT" {
                out.push(self.reconcile_lifecycle_intent(
                    &intent_id,
                    write_kind,
                    &rec,
                    &benches,
                    block_id,
                    block_height,
                )?);
                continue;
            }
            // Match by benchmark_id when known, else by exact settings
            // (the lost-response search, §10).
            let known_id = rec.get("benchmark_id").and_then(Value::as_str);
            let payload_settings = rec
                .pointer("/payload/settings")
                .cloned()
                .unwrap_or(Value::Null);
            let matches: Vec<&Value> = precommits
                .iter()
                .filter(|p| match known_id {
                    Some(id) => p.get("benchmark_id").and_then(Value::as_str) == Some(id),
                    None => {
                        let s = p.get("settings");
                        s.and_then(|s| s.get("player_id")) == payload_settings.get("player_id")
                            && s.and_then(|s| s.get("challenge_id"))
                                == payload_settings.get("challenge_id")
                            && s.and_then(|s| s.get("algorithm_id"))
                                == payload_settings.get("algorithm_id")
                            && s.and_then(|s| s.get("block_id")) == payload_settings.get("block_id")
                    }
                })
                .collect();
            match (known_id, matches.as_slice()) {
                (_, [p]) => {
                    let confirmed = p.pointer("/state/block_confirmed").and_then(Value::as_u64);
                    if let Some(height) = confirmed {
                        let benchmark_id =
                            p.get("benchmark_id").and_then(Value::as_str).unwrap_or("?");
                        // Confirmed settings/details are authoritative and
                        // replace proposed values (§6.1).
                        let assignment = json!({
                            "benchmark_id": benchmark_id,
                            "intent_id": intent_id,
                            "confirmed_at_block": height,
                            "settings": p.get("settings"),
                            "details": p.get("details"),
                            "observed_via": { "block_id": block_id },
                        });
                        let path = self
                            .ledger
                            .write_doc(&format!("assignment-{benchmark_id}.json"), &assignment)?;
                        self.ledger.append(
                            "intents",
                            &json!({ "intent_id": intent_id, "write_kind": "PRECOMMIT", "state": "CONFIRMED", "benchmark_id": benchmark_id, "confirmed_at_block": height }),
                        )?;
                        out.push(format!(
                            "{intent_id}: CONFIRMED as {benchmark_id} at block {height} -> {}",
                            path.display()
                        ));
                    } else {
                        out.push(format!("{intent_id}: visible but not yet confirmed"));
                    }
                }
                (Some(_), []) => {
                    out.push(format!(
                        "{intent_id}: {state}, not yet visible in get-benchmarks"
                    ));
                }
                (Some(_), _) => {
                    out.push(format!("{intent_id}: multiple entries for one benchmark_id — operator resolution required"));
                }
                (None, []) => {
                    out.push(format!("{intent_id}: {state}, no candidate yet"));
                }
                (None, many) => {
                    // Ambiguous lost-response candidates: never blindly
                    // resubmit; leave for the operator (§10).
                    out.push(format!(
                        "{intent_id}: {state}, {} settings-matched candidates — operator resolution required",
                        many.len()
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Reconcile one unresolved BENCHMARK/PROOF intent from the confirmed
    /// `get-benchmarks` window. Confirmation evidence (`tig_integration.md`
    /// §7): a matching entry with non-null `state.block_confirmed`. On
    /// confirmation the whole confirmed entry is persisted as
    /// `<kind>-confirmed-<benchmark_id>.json` — the ONLY source later steps
    /// may take `sampled_nonces` or proof details from.
    fn reconcile_lifecycle_intent(
        &self,
        intent_id: &str,
        write_kind: &str,
        rec: &Value,
        benches: &Value,
        block_id: &str,
        block_height: u64,
    ) -> Result<String> {
        let state = rec.get("state").and_then(Value::as_str).unwrap_or("");
        let benchmark_id = rec
            .get("benchmark_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{write_kind} intent {intent_id} missing benchmark_id"))?;
        // `get-benchmarks.benchmarks` entries carry `id`; `proofs` entries
        // carry `benchmark_id` (pinned upstream `Benchmark`/`Proof` structs).
        let (section, id_key) = match write_kind {
            "BENCHMARK" => ("benchmarks", "id"),
            "PROOF" => ("proofs", "benchmark_id"),
            other => bail!("unknown write kind {other} for intent {intent_id}"),
        };
        let entry = benches
            .get(section)
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .find(|e| e.get(id_key).and_then(Value::as_str) == Some(benchmark_id))
            });
        let Some(entry) = entry else {
            return Ok(format!(
                "{intent_id}: {state}, {write_kind} not yet visible in get-benchmarks"
            ));
        };
        let Some(height) = entry
            .pointer("/state/block_confirmed")
            .and_then(Value::as_u64)
        else {
            return Ok(format!(
                "{intent_id}: {write_kind} visible but not yet confirmed"
            ));
        };
        let doc_name = format!(
            "{}-confirmed-{benchmark_id}.json",
            write_kind.to_ascii_lowercase()
        );
        let doc = json!({
            "benchmark_id": benchmark_id,
            "intent_id": intent_id,
            "confirmed_at_block": height,
            "entry": entry,
            "observed_via": { "block_id": block_id, "block_height": block_height },
        });
        let path = self.ledger.write_doc(&doc_name, &doc)?;
        self.ledger.append(
            "intents",
            &json!({
                "intent_id": intent_id, "write_kind": write_kind, "state": "CONFIRMED",
                "benchmark_id": benchmark_id, "confirmed_at_block": height,
            }),
        )?;
        Ok(format!(
            "{intent_id}: {write_kind} CONFIRMED at block {height} -> {}",
            path.display()
        ))
    }
}
