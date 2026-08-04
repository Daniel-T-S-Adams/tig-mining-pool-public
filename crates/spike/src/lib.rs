//! Spike gateway: the only component that holds the TIG API key and performs
//! protocol writes, driven by durably persisted write intents.
//!
//! Contract: `docs/tig_integration.md` §6–§11; plan: `docs/plans/protocol-spike.md`
//! phase S1 (issue #10). Disposable spike code — shortcuts are allowed except
//! for the guarantees under test: the intent and attempt ledgers are durable
//! (append + fsync) and written *before* any network send; an HTTP 200 is
//! never treated as confirmation; the precommit lane is serialized (at most
//! one unresolved precommit intent).

pub mod member;

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
    /// assignment record.
    pub fn write_doc(&self, name: &str, doc: &Value) -> Result<PathBuf> {
        let path = self.dir.join(name);
        let mut f = File::create(&path)?;
        f.write_all(serde_json::to_string_pretty(doc)?.as_bytes())?;
        f.sync_all()?;
        Ok(path)
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

// ---------------------------------------------------------------------------
// TIG client (reads are public; writes carry the API key)
// ---------------------------------------------------------------------------

pub struct TigClient {
    pub base_url: String,
    api_key: String,
    http: reqwest::blocking::Client,
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
        })
    }

    pub fn get(&self, path_and_query: &str) -> Result<Value> {
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

    /// Advance every unresolved intent from confirmed reads only. Returns a
    /// human-readable status line per intent.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        let block = self.client.latest_block()?;
        let block_id = block
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("block missing id"))?;
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
}
