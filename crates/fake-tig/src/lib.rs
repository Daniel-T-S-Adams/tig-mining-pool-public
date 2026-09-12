//! A deterministic local stand-in for the TIG API surface the pool uses.
//!
//! Contract: `docs/tig_integration.md`. Serves fixture-backed, block-consistent
//! state; accepts the three protocol writes and reflects them into confirmed
//! state only after block advancement (an HTTP 200 is never confirmation).
//! Never shares configuration or credentials with live testnet: the API key is
//! an obvious fake and all state is in-memory per run.
//!
//! Determinism: no clocks, no randomness. Timestamps derive from the fixture
//! anchor; sampled nonces derive from the benchmark ID.
//!
//! Admin surface (not part of TIG): `POST /_fake/advance-block`,
//! `POST /_fake/inject`, `POST /_fake/verify`, `POST /_fake/fraud`,
//! `GET /_fake/state`.
//!
//! `verify` and `fraud` exist because `tig_integration.md` §7 maps two
//! lifecycle facts to rulings TIG makes and a client cannot cause. Like every
//! other confirmation here, a ruling is asked for now and published by the
//! next block: a block already served never changes what it said.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

pub const DEFAULT_API_KEY: &str = "fake-testnet-key";

#[derive(Clone, Debug)]
pub struct Config {
    pub fixture_dir: PathBuf,
    pub api_key: String,
    /// Blocks between a write being accepted and appearing as confirmed.
    pub confirm_delay: u32,
    /// Serve the fixture as though its pool player had this id.
    ///
    /// The fixture's pool player is a deliberately unreal placeholder, and a
    /// production binary's configuration refuses anything but a real
    /// address. Rewriting the id when the fixture loads lets one binary run
    /// against this server with the configuration it would run against
    /// testnet with, while every test keeps the placeholder it was written
    /// against. Every occurrence is rewritten — block, OPoW, player data —
    /// so the server cannot describe one player two ways.
    pub pool_player_id: Option<String>,
}

impl Config {
    pub fn new(fixture_dir: impl Into<PathBuf>) -> Self {
        Config {
            fixture_dir: fixture_dir.into(),
            api_key: DEFAULT_API_KEY.to_owned(),
            confirm_delay: 1,
            pool_player_id: None,
        }
    }
}

struct Fixture {
    block_template: Value,
    challenges: Value,
    algorithms: Value,
    opow: Value,
    player: Value,
    tracks: Value,
}

#[derive(Clone, Debug)]
struct BenchmarkSubmission {
    stopped: bool,
    merkle_root: Option<String>,
    solution_quality: Option<Vec<i64>>,
    submitted_height: u32,
    confirmed: Option<u32>,
    sampled_nonces: Option<Vec<u64>>,
}

#[derive(Clone, Debug)]
struct ProofSubmission {
    submitted_height: u32,
    confirmed: Option<u32>,
    submission_delay: u32,
    block_active: Option<u32>,
}

#[derive(Clone, Debug)]
struct Bench {
    id: String,
    settings: Value,
    challenge_id: String,
    num_nonces: u64,
    num_bundles: u64,
    fuel_budget: u64,
    hyperparameters: Value,
    compute_type: String,
    fee_paid: String,
    rand_hash: String,
    block_started: u32,
    precommit_confirmed: Option<u32>,
    submission: Option<BenchmarkSubmission>,
    proof: Option<ProofSubmission>,
    active_from: Option<u32>,
    active_until: Option<u32>,
    /// Height at which TIG published the verification event
    /// (`tig_integration.md` §7: id in `block.data.confirmed_ids.verified`).
    ///
    /// A ruling the pool cannot cause, so nothing a client submits sets it —
    /// only `/_fake/verify` does, the same way `/_fake/advance-block` moves a
    /// chain no client controls.
    verified: Option<u32>,
    /// Height at which TIG ruled the benchmark fraudulent (§7: entry in
    /// `get-benchmarks.frauds` with non-null `state.block_confirmed`).
    fraud: Option<u32>,
    /// A ruling asked for and not yet published.
    ///
    /// Both controls record a request and `advance_block` publishes it, the
    /// way every other confirmation in this fake already works. Stamping the
    /// *current* height instead rewrote a block that had already been served
    /// under the same id: two reads of block B would disagree, which is a
    /// thing no chain does and which would let a test satisfy §9's
    /// block-consistency check while the reads it compared were inconsistent.
    /// It also made the event invisible to a driver that read the block before
    /// asking for the ruling.
    verify_pending: bool,
    fraud_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjectMode {
    /// Respond 400 without applying the request.
    Reject,
    /// Respond 429 with Retry-After without applying.
    RateLimit,
    /// Respond 500 without applying.
    Error,
    /// Apply the request, then respond 500 (the lost-response case).
    Ambiguous,
}

pub struct World {
    cfg: Config,
    fixture: Fixture,
    height: u32,
    round: u32,
    timestamp: u64,
    seconds_between_blocks: u64,
    blocks_per_round: u32,
    next_bench_seq: u64,
    benchmarks: BTreeMap<String, Bench>,
    injections: HashMap<String, VecDeque<InjectMode>>,
    /// Writes that actually reached this server, per endpoint.
    ///
    /// Counted on arrival, before any injection is applied, because the
    /// question slice-1 criterion E3 asks is how many requests the client
    /// SENT — not how many the server chose to answer, and not how many
    /// benchmarks resulted. A client that resends an ambiguous write shows
    /// up here even if the second request would have been deduplicated.
    writes_received: HashMap<String, u32>,
}

pub type SharedWorld = Arc<Mutex<World>>;

/// Replace every string value equal to `from` — and every object key equal
/// to it, since the fixtures key coinbase maps by player id — with `to`.
fn rewrite_strings(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(s) if s == from => *s = to.to_owned(),
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite_strings(v, from, to)),
        Value::Object(map) => {
            let renamed: Vec<String> = map.keys().filter(|k| *k == from).cloned().collect();
            for key in renamed {
                if let Some(v) = map.remove(&key) {
                    map.insert(to.to_owned(), v);
                }
            }
            map.values_mut().for_each(|v| rewrite_strings(v, from, to));
        }
        _ => {}
    }
}

fn read_fixture_file(dir: &Path, name: &str) -> Result<Value, String> {
    let path = dir.join(name);
    let bytes =
        std::fs::read(&path).map_err(|e| format!("cannot read fixture {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("fixture {} is not valid JSON: {e}", path.display()))
}

fn get_u64(v: &Value, path: &[&str]) -> Result<u64, String> {
    let mut cur = v;
    for k in path {
        cur = cur
            .get(k)
            .ok_or_else(|| format!("fixture missing field {}", path.join(".")))?;
    }
    cur.as_u64()
        .ok_or_else(|| format!("fixture field {} is not a u64", path.join(".")))
}

impl World {
    pub fn from_fixture_dir(cfg: Config) -> Result<World, String> {
        let dir = cfg.fixture_dir.clone();
        let block_template = read_fixture_file(&dir, "get-block.json")?;
        let mut fixture = Fixture {
            challenges: read_fixture_file(&dir, "get-challenges.json")?,
            algorithms: read_fixture_file(&dir, "get-algorithms.json")?,
            opow: read_fixture_file(&dir, "get-opow.json")?,
            player: read_fixture_file(&dir, "get-player-data.json")?,
            tracks: read_fixture_file(&dir, "get-tracks-data.json")?,
            block_template,
        };
        if let Some(wanted) = cfg.pool_player_id.as_deref() {
            let placeholder = fixture
                .player
                .get("player")
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str)
                .ok_or("fixture get-player-data.json names no player.id")?
                .to_owned();
            for doc in [
                &mut fixture.block_template,
                &mut fixture.opow,
                &mut fixture.player,
                &mut fixture.challenges,
                &mut fixture.algorithms,
                &mut fixture.tracks,
            ] {
                rewrite_strings(doc, &placeholder, wanted);
            }
        }
        let height = u32::try_from(get_u64(
            &fixture.block_template,
            &["block", "details", "height"],
        )?)
        .map_err(|_| "fixture height out of range".to_owned())?;
        let round = u32::try_from(get_u64(
            &fixture.block_template,
            &["block", "details", "round"],
        )?)
        .map_err(|_| "fixture round out of range".to_owned())?;
        let timestamp = get_u64(&fixture.block_template, &["block", "details", "timestamp"])?;
        let seconds_between_blocks = get_u64(
            &fixture.block_template,
            &["block", "config", "rounds", "seconds_between_blocks"],
        )?;
        let blocks_per_round = u32::try_from(get_u64(
            &fixture.block_template,
            &["block", "config", "rounds", "blocks_per_round"],
        )?)
        .map_err(|_| "fixture blocks_per_round out of range".to_owned())?;
        Ok(World {
            cfg,
            fixture,
            height,
            round,
            timestamp,
            seconds_between_blocks,
            blocks_per_round,
            next_bench_seq: 1,
            benchmarks: BTreeMap::new(),
            injections: HashMap::new(),
            writes_received: HashMap::new(),
        })
    }

    fn block_id(&self) -> String {
        format!("block_{}", self.height)
    }

    fn challenge_config(&self, challenge_id: &str) -> Option<&Value> {
        self.fixture
            .block_template
            .get("block")?
            .get("config")?
            .get("challenges")?
            .get(challenge_id)
    }

    fn pool_player_id(&self) -> String {
        self.fixture
            .player
            .get("player")
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    /// Deterministic sampled-nonce selection: evenly spaced with an offset
    /// derived from the benchmark ID. No randomness.
    fn sample_nonces(id: &str, num_nonces: u64, count: u64) -> Vec<u64> {
        let count = count.min(num_nonces);
        let offset = id.bytes().map(u64::from).sum::<u64>() % num_nonces.max(1);
        let mut picked = Vec::new();
        let mut i = 0u64;
        while (picked.len() as u64) < count {
            let mut candidate = (offset + i * num_nonces / count.max(1)) % num_nonces;
            while picked.contains(&candidate) {
                candidate = (candidate + 1) % num_nonces;
            }
            picked.push(candidate);
            i += 1;
        }
        picked.sort_unstable();
        picked
    }

    fn sample_count(&self, challenge_id: &str) -> u64 {
        let Some(cfg) = self.challenge_config(challenge_id) else {
            return 3;
        };
        let gte = cfg
            .get("num_samples_gte_average")
            .and_then(Value::as_u64)
            .unwrap_or(2);
        let lt = cfg
            .get("num_samples_lt_average")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        gte + lt
    }

    fn delay_multiplier(&self, challenge_id: &str) -> f64 {
        self.challenge_config(challenge_id)
            .and_then(|c| c.get("submission_delay_multiplier"))
            .and_then(Value::as_f64)
            .unwrap_or(3.0)
    }

    fn lifespan(&self, challenge_id: &str) -> u32 {
        self.challenge_config(challenge_id)
            .and_then(|c| c.get("lifespan_period"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(120)
    }

    pub fn advance_block(&mut self) {
        self.height += 1;
        self.timestamp += self.seconds_between_blocks;
        if self.blocks_per_round > 0 && self.height.is_multiple_of(self.blocks_per_round) {
            self.round += 1;
        }
        let height = self.height;
        let delay = self.cfg.confirm_delay;
        let mut sampling: Vec<(String, String, u64)> = Vec::new();
        let mut activation: Vec<(String, String, u32)> = Vec::new();
        for bench in self.benchmarks.values_mut() {
            // A ruling asked for since the last block is published by this
            // one. Same discipline as every confirmation below: a block's
            // contents are settled when it is minted, so an id already served
            // never changes what it said.
            if std::mem::take(&mut bench.verify_pending) {
                bench.verified = Some(height);
            }
            if std::mem::take(&mut bench.fraud_pending) {
                bench.fraud = Some(height);
            }
            if bench.precommit_confirmed.is_none() && height >= bench.block_started + delay {
                bench.precommit_confirmed = Some(height);
            }
            if let Some(sub) = bench.submission.as_mut()
                && sub.confirmed.is_none()
                && bench.precommit_confirmed.is_some()
                && height >= sub.submitted_height + delay
            {
                sub.confirmed = Some(height);
                if !sub.stopped {
                    sampling.push((
                        bench.id.clone(),
                        bench.challenge_id.clone(),
                        bench.num_nonces,
                    ));
                }
            }
            if let Some(proof) = bench.proof.as_mut()
                && proof.confirmed.is_none()
                && height >= proof.submitted_height + delay
            {
                proof.confirmed = Some(height);
                activation.push((
                    bench.id.clone(),
                    bench.challenge_id.clone(),
                    proof.submission_delay,
                ));
            }
        }
        for (id, challenge_id, num_nonces) in sampling {
            let count = self.sample_count(&challenge_id);
            let nonces = World::sample_nonces(&id, num_nonces, count);
            if let Some(bench) = self.benchmarks.get_mut(&id)
                && let Some(sub) = bench.submission.as_mut()
            {
                sub.sampled_nonces = Some(nonces);
            }
        }
        for (id, challenge_id, submission_delay) in activation {
            let multiplier = self.delay_multiplier(&challenge_id);
            let lifespan = self.lifespan(&challenge_id);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let activation_delay = (f64::from(submission_delay) * multiplier).ceil() as u32;
            if let Some(bench) = self.benchmarks.get_mut(&id) {
                if let Some(proof) = bench.proof.as_mut() {
                    proof.block_active = Some(height + activation_delay);
                }
                bench.active_from = Some(height + activation_delay);
                bench.active_until = Some(bench.block_started + lifespan);
            }
        }
    }

    fn confirmed_at(&self, height: u32) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut precommits = Vec::new();
        let mut benchmarks = Vec::new();
        let mut proofs = Vec::new();
        for bench in self.benchmarks.values() {
            if bench.precommit_confirmed == Some(height) {
                precommits.push(bench.id.clone());
            }
            if bench.submission.as_ref().and_then(|s| s.confirmed) == Some(height) {
                benchmarks.push(bench.id.clone());
            }
            if bench.proof.as_ref().and_then(|p| p.confirmed) == Some(height) {
                proofs.push(bench.id.clone());
            }
        }
        (precommits, benchmarks, proofs)
    }

    /// Ids TIG ruled on at this height. §7 reads each from its own place —
    /// verification from `block.data.confirmed_ids.verified`, fraud from a
    /// `get-benchmarks.frauds` entry — so they are kept apart here too rather
    /// than folded into one "ruled" set.
    fn ruled_at(&self, height: u32) -> (Vec<String>, Vec<String>) {
        let mut verified = Vec::new();
        let mut frauds = Vec::new();
        for bench in self.benchmarks.values() {
            if bench.verified == Some(height) {
                verified.push(bench.id.clone());
            }
            if bench.fraud == Some(height) {
                frauds.push(bench.id.clone());
            }
        }
        (verified, frauds)
    }

    fn active_benchmark_ids(&self) -> Vec<String> {
        self.benchmarks
            .values()
            .filter(|b| {
                matches!((b.active_from, b.active_until), (Some(from), Some(until))
                    if self.height >= from && self.height < until)
            })
            .map(|b| b.id.clone())
            .collect()
    }

    fn current_block_json(&self) -> Value {
        let mut block = self
            .fixture
            .block_template
            .get("block")
            .cloned()
            .unwrap_or_else(|| json!({}));
        block["id"] = json!(self.block_id());
        block["details"]["prev_block_id"] = json!(format!("block_{}", self.height - 1));
        block["details"]["height"] = json!(self.height);
        block["details"]["round"] = json!(self.round);
        block["details"]["timestamp"] = json!(self.timestamp);
        let (precommits, benchmarks, proofs) = self.confirmed_at(self.height);
        block["details"]["num_confirmed"]["precommit"] = json!(precommits.len());
        block["details"]["num_confirmed"]["benchmark"] = json!(benchmarks.len());
        block["details"]["num_confirmed"]["proof"] = json!(proofs.len());
        block["data"]["confirmed_ids"]["precommit"] = json!(precommits);
        block["data"]["confirmed_ids"]["benchmark"] = json!(benchmarks);
        block["data"]["confirmed_ids"]["proof"] = json!(proofs);
        let (verified, frauds) = self.ruled_at(self.height);
        block["details"]["num_confirmed"]["verified"] = json!(verified.len());
        block["details"]["num_confirmed"]["fraud"] = json!(frauds.len());
        block["data"]["confirmed_ids"]["verified"] = json!(verified);
        block["data"]["confirmed_ids"]["fraud"] = json!(frauds);
        let active = self.active_benchmark_ids();
        block["details"]["num_active"]["benchmark"] = json!(active.len());
        block["data"]["active_ids"]["benchmark"] = json!(active);
        json!({ "block": block })
    }

    fn precommit_json(&self, bench: &Bench) -> Value {
        json!({
            "benchmark_id": bench.id,
            "settings": bench.settings,
            "details": {
                "block_started": bench.block_started,
                "num_nonces": bench.num_nonces,
                "num_bundles": bench.num_bundles,
                "rand_hash": bench.rand_hash,
                "fee_paid": bench.fee_paid,
                "fuel_budget": bench.fuel_budget,
                "hyperparameters": bench.hyperparameters,
                "compute_type": bench.compute_type,
            },
            "state": { "block_confirmed": bench.precommit_confirmed },
        })
    }

    fn benchmarks_json(&self) -> Value {
        let mut precommits = Vec::new();
        let mut benchmarks = Vec::new();
        let mut proofs = Vec::new();
        let mut frauds = Vec::new();
        for bench in self.benchmarks.values() {
            precommits.push(self.precommit_json(bench));
            if let Some(sub) = &bench.submission {
                benchmarks.push(json!({
                    "id": bench.id,
                    "details": {
                        "stopped": sub.stopped,
                        "num_active_bundles": bench.num_bundles,
                        "average_quality_by_bundle": null,
                        "merkle_root": sub.merkle_root,
                        "sampled_nonces": sub.confirmed.is_some()
                            .then(|| sub.sampled_nonces.clone())
                            .flatten(),
                    },
                    "state": { "block_confirmed": sub.confirmed },
                    "solution_quality": sub.solution_quality,
                }));
            }
            if let Some(proof) = &bench.proof {
                proofs.push(json!({
                    "benchmark_id": bench.id,
                    "details": {
                        "submission_delay": proof.submission_delay,
                        "block_active": proof.block_active,
                    },
                    "state": { "block_confirmed": proof.confirmed },
                }));
            }
            // §7: "Matching entry in `get-benchmarks.frauds` with non-null
            // `state.block_confirmed`". Those two fields are what the mapping
            // names and all this emits.
            //
            // The live record almost certainly carries more — an allegation,
            // a reason — but `fixtures/tig/v1/get-benchmarks.json` pins only
            // an empty `frauds: []`, so there is nothing to copy and the rest
            // would be invention. A fake that guessed extra fields would
            // teach the pool to read fields TIG may not send. Issue #31's
            // fixtures v2 is where a real shape gets pinned.
            if let Some(confirmed) = bench.fraud {
                frauds.push(json!({
                    "benchmark_id": bench.id,
                    "state": { "block_confirmed": confirmed },
                }));
            }
        }
        json!({
            "precommits": precommits,
            "benchmarks": benchmarks,
            "proofs": proofs,
            "frauds": frauds,
        })
    }

    fn count_write(&mut self, target: &str) {
        *self.writes_received.entry(target.to_string()).or_insert(0) += 1;
    }

    fn take_injection(&mut self, target: &str) -> Option<InjectMode> {
        if let Some(queue) = self.injections.get_mut(target)
            && let Some(mode) = queue.pop_front()
        {
            return Some(mode);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

struct ApiError(StatusCode, Value, Vec<(&'static str, String)>);

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> ApiError {
        ApiError(
            StatusCode::BAD_REQUEST,
            json!({ "error": msg.into() }),
            Vec::new(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.0, Json(self.1)).into_response();
        for (name, value) in self.2 {
            if let Ok(v) = value.parse() {
                response.headers_mut().insert(name, v);
            }
        }
        response
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn injected_response(mode: InjectMode) -> ApiError {
    match mode {
        InjectMode::Reject => ApiError::bad_request("injected rejection"),
        InjectMode::RateLimit => ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "injected rate limit" }),
            vec![("retry-after", "5".to_owned())],
        ),
        InjectMode::Error | InjectMode::Ambiguous => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "injected internal error" }),
            Vec::new(),
        ),
    }
}

fn lock(world: &SharedWorld) -> std::sync::MutexGuard<'_, World> {
    world
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reads that document a `block_id` parameter require the latest block
/// (`docs/tig_integration.md` §8); anything else is a refresh conflict.
fn require_latest_block(w: &World, params: &HashMap<String, String>) -> Result<(), ApiError> {
    match params.get("block_id") {
        Some(id) if *id == w.block_id() => Ok(()),
        Some(_) => Err(ApiError::bad_request("block must be latest")),
        None => Err(ApiError::bad_request("missing block_id")),
    }
}

fn require_api_key(w: &World, headers: &HeaderMap) -> Result<(), ApiError> {
    let provided = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    if provided == Some(w.cfg.api_key.as_str()) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "invalid api key" }),
            Vec::new(),
        ))
    }
}

fn check_read_injection(world: &SharedWorld, target: &str) -> Result<(), ApiError> {
    let mut w = lock(world);
    if let Some(mode) = w.take_injection(target) {
        return Err(injected_response(mode));
    }
    Ok(())
}

async fn get_block(State(world): State<SharedWorld>) -> ApiResult {
    check_read_injection(&world, "get-block")?;
    let w = lock(&world);
    Ok(Json(w.current_block_json()))
}

async fn fixture_read(
    world: &SharedWorld,
    target: &str,
    params: &HashMap<String, String>,
    pick: impl FnOnce(&World) -> Value,
) -> ApiResult {
    check_read_injection(world, target)?;
    let w = lock(world);
    require_latest_block(&w, params)?;
    Ok(Json(pick(&w)))
}

async fn get_challenges(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    fixture_read(&world, "get-challenges", &params, |w| {
        w.fixture.challenges.clone()
    })
    .await
}

async fn get_algorithms(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    fixture_read(&world, "get-algorithms", &params, |w| {
        w.fixture.algorithms.clone()
    })
    .await
}

async fn get_opow(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    fixture_read(&world, "get-opow", &params, |w| w.fixture.opow.clone()).await
}

async fn get_player_data(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    fixture_read(&world, "get-player-data", &params, |w| {
        w.fixture.player.clone()
    })
    .await
}

async fn get_benchmarks(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    check_read_injection(&world, "get-benchmarks")?;
    let w = lock(&world);
    require_latest_block(&w, &params)?;
    // Required and matched, so the stand-in stops masking an unscoped call.
    // tig_integration.md §5 pins this read as player-scoped, and it is the
    // sole authority for every §7 confirmation; a handler that ignored
    // player_id let a caller omit it and still pass every test.
    let player_id = params
        .get("player_id")
        .ok_or_else(|| ApiError::bad_request("missing player_id"))?;
    let fixture_player = w
        .fixture
        .player
        .get("player")
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !player_id.eq_ignore_ascii_case(fixture_player) {
        return Err(ApiError::bad_request(
            "player_id does not match this fixture's player",
        ));
    }
    Ok(Json(w.benchmarks_json()))
}

async fn get_tracks_data(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    check_read_injection(&world, "get-tracks-data")?;
    let w = lock(&world);
    require_latest_block(&w, &params)?;
    let challenge_id = params
        .get("challenge_id")
        .ok_or_else(|| ApiError::bad_request("missing challenge_id"))?;
    let tracks = w
        .fixture
        .tracks
        .get(challenge_id)
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(Json(json!({ "tracks": tracks })))
}

async fn get_benchmark_data(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    check_read_injection(&world, "get-benchmark-data")?;
    let w = lock(&world);
    let id = params
        .get("benchmark_id")
        .ok_or_else(|| ApiError::bad_request("missing benchmark_id"))?;
    let bench = w.benchmarks.get(id).ok_or(ApiError(
        StatusCode::NOT_FOUND,
        json!({ "error": "unknown benchmark" }),
        Vec::new(),
    ))?;
    let all = w.benchmarks_json();
    let find = |key: &str, id_key: &str| -> Value {
        all.get(key)
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|e| e.get(id_key).and_then(Value::as_str) == Some(id))
                    .cloned()
            })
            .unwrap_or(Value::Null)
    };
    Ok(Json(json!({
        "precommit": w.precommit_json(bench),
        "benchmark": find("benchmarks", "id"),
        "proof": find("proofs", "benchmark_id"),
        // Through the same `frauds` collection `get-benchmarks` serves, so
        // the two endpoints cannot describe one ruling differently.
        "fraud": find("frauds", "benchmark_id"),
    })))
}

async fn get_binary_blob(Query(params): Query<HashMap<String, String>>) -> Response {
    match params.get("algorithm_id") {
        Some(id) => format!("FAKE-BINARY-{id}").into_response(),
        None => ApiError::bad_request("missing algorithm_id").into_response(),
    }
}

async fn get_round_emissions(
    State(world): State<SharedWorld>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    let w = lock(&world);
    let round = params
        .get("round")
        .ok_or_else(|| ApiError::bad_request("missing round"))?;
    Ok(Json(json!({
        "round": round,
        "emissions": { w.pool_player_id(): "0" },
    })))
}

fn as_object(v: &Value) -> Result<&Map<String, Value>, ApiError> {
    v.as_object()
        .ok_or_else(|| ApiError::bad_request("body must be a JSON object"))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request(format!("missing string field {key}")))
}

async fn submit_precommit(
    State(world): State<SharedWorld>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let mut w = lock(&world);
    require_api_key(&w, &headers)?;
    w.count_write("submit-precommit");
    let injection = w.take_injection("submit-precommit");
    if let Some(mode) = injection
        && mode != InjectMode::Ambiguous
    {
        return Err(injected_response(mode));
    }

    let settings = body
        .get("settings")
        .ok_or_else(|| ApiError::bad_request("missing settings"))?;
    let player_id = str_field(settings, "player_id")?;
    if player_id != w.pool_player_id() {
        return Err(ApiError::bad_request("unknown player_id"));
    }
    let block_id = str_field(settings, "block_id")?;
    let latest = w.block_id();
    let second_latest = format!("block_{}", w.height - 1);
    if block_id != latest && block_id != second_latest {
        return Err(ApiError::bad_request(
            "settings.block_id must be the latest or second-latest block",
        ));
    }
    let challenge_id = str_field(settings, "challenge_id")?.to_owned();
    let algorithm_id = str_field(settings, "algorithm_id")?.to_owned();
    if !str_field(settings, "track_id")?.is_empty() {
        return Err(ApiError::bad_request("submitted track_id must be empty"));
    }
    let challenge_cfg = w
        .challenge_config(&challenge_id)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("unknown challenge_id"))?;
    let active_tracks = challenge_cfg
        .get("active_tracks")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("challenge has no active tracks"))?;

    let track_settings = as_object(
        body.get("track_settings")
            .ok_or_else(|| ApiError::bad_request("missing track_settings"))?,
    )?
    .clone();
    let mut submitted: Vec<&String> = track_settings.keys().collect();
    let mut active: Vec<&String> = active_tracks.keys().collect();
    submitted.sort();
    active.sort();
    if submitted != active {
        return Err(ApiError::bad_request(
            "track_settings must contain exactly every active track",
        ));
    }
    let compute_type = str_field(&body, "compute_type")?.to_owned();

    // TIG selects the track; the fake picks deterministically.
    let selected_track = active
        .first()
        .map(|s| (*s).clone())
        .ok_or_else(|| ApiError::bad_request("challenge has no active tracks"))?;
    let selected = track_settings
        .get(&selected_track)
        .cloned()
        .unwrap_or(Value::Null);
    let num_bundles = selected
        .get("num_bundles")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::bad_request("track_settings missing num_bundles"))?;
    let fuel_budget = selected
        .get("fuel_budget")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::bad_request("track_settings missing fuel_budget"))?;
    let min_bundles = challenge_cfg
        .get("min_num_bundles")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let max_fuel = challenge_cfg
        .get("max_fuel_budget")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    if num_bundles < min_bundles {
        return Err(ApiError::bad_request("num_bundles below min_num_bundles"));
    }
    if fuel_budget > max_fuel {
        return Err(ApiError::bad_request("fuel_budget above max_fuel_budget"));
    }
    let per_bundle = active_tracks
        .get(&selected_track)
        .and_then(|t| t.get("num_nonces_per_bundle"))
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::bad_request("track missing num_nonces_per_bundle"))?;
    let num_nonces = num_bundles * per_bundle;

    // PreciseNumber arithmetic on decimal strings; integer math only.
    let base_fee: u128 = challenge_cfg
        .get("base_fee")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let per_nonce_fee: u128 = challenge_cfg
        .get("per_nonce_fee")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let fee_paid = (base_fee + per_nonce_fee * u128::from(num_nonces)).to_string();

    let seq = w.next_bench_seq;
    w.next_bench_seq += 1;
    // Dash-form id so pool-side identifier validation (lowercase alnum +
    // dash) accepts fake benchmark ids on the S4 end-to-end path; real TIG
    // ids are 32-hex and pass the same rule.
    let id = format!("bench-{seq:04}");
    let bench = Bench {
        id: id.clone(),
        settings: json!({
            "player_id": player_id,
            "block_id": block_id,
            "challenge_id": challenge_id,
            "algorithm_id": algorithm_id,
            "track_id": selected_track,
        }),
        challenge_id,
        num_nonces,
        num_bundles,
        fuel_budget,
        hyperparameters: selected
            .get("hyperparameters")
            .cloned()
            .unwrap_or(Value::Null),
        compute_type,
        fee_paid,
        rand_hash: format!("rh_{id}"),
        block_started: w.height,
        precommit_confirmed: None,
        submission: None,
        proof: None,
        active_from: None,
        active_until: None,
        verified: None,
        fraud: None,
        verify_pending: false,
        fraud_pending: false,
    };
    w.benchmarks.insert(id.clone(), bench);

    if injection == Some(InjectMode::Ambiguous) {
        return Err(injected_response(InjectMode::Ambiguous));
    }
    Ok(Json(json!({ "benchmark_id": id })))
}

async fn submit_benchmark(
    State(world): State<SharedWorld>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let mut w = lock(&world);
    require_api_key(&w, &headers)?;
    w.count_write("submit-benchmark");
    let injection = w.take_injection("submit-benchmark");
    if let Some(mode) = injection
        && mode != InjectMode::Ambiguous
    {
        return Err(injected_response(mode));
    }

    let id = str_field(&body, "benchmark_id")?.to_owned();
    let stopped = body
        .get("stopped")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("missing bool field stopped"))?;
    let merkle_root = body.get("merkle_root").cloned().unwrap_or(Value::Null);
    let solution_quality = body.get("solution_quality").cloned().unwrap_or(Value::Null);
    let height = w.height;

    let bench = w
        .benchmarks
        .get(&id)
        .ok_or_else(|| ApiError::bad_request("unknown benchmark_id"))?;
    if bench.precommit_confirmed.is_none() {
        return Err(ApiError::bad_request("precommit not confirmed"));
    }
    if bench.submission.is_some() {
        return Err(ApiError::bad_request("benchmark already submitted"));
    }

    let submission = if stopped {
        if !merkle_root.is_null() || !solution_quality.is_null() {
            return Err(ApiError::bad_request(
                "stopped submission must have null merkle_root and solution_quality",
            ));
        }
        BenchmarkSubmission {
            stopped: true,
            merkle_root: None,
            solution_quality: None,
            submitted_height: height,
            confirmed: None,
            sampled_nonces: None,
        }
    } else {
        let root = merkle_root
            .as_str()
            .ok_or_else(|| ApiError::bad_request("missing merkle_root"))?;
        if root.len() != 64
            || !root
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(ApiError::bad_request(
                "merkle_root must be 64 lowercase hex characters",
            ));
        }
        let quality = solution_quality
            .as_array()
            .ok_or_else(|| ApiError::bad_request("missing solution_quality"))?;
        if quality.len() as u64 != bench.num_nonces {
            return Err(ApiError::bad_request(format!(
                "solution_quality must have exactly num_nonces = {} entries",
                bench.num_nonces
            )));
        }
        let values: Option<Vec<i64>> = quality.iter().map(Value::as_i64).collect();
        let values = values
            .ok_or_else(|| ApiError::bad_request("solution_quality entries must be integers"))?;
        BenchmarkSubmission {
            stopped: false,
            merkle_root: Some(root.to_owned()),
            solution_quality: Some(values),
            submitted_height: height,
            confirmed: None,
            sampled_nonces: None,
        }
    };
    if let Some(bench) = w.benchmarks.get_mut(&id) {
        bench.submission = Some(submission);
    }

    if injection == Some(InjectMode::Ambiguous) {
        return Err(injected_response(InjectMode::Ambiguous));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn submit_proof(
    State(world): State<SharedWorld>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let mut w = lock(&world);
    require_api_key(&w, &headers)?;
    w.count_write("submit-proof");
    let injection = w.take_injection("submit-proof");
    if let Some(mode) = injection
        && mode != InjectMode::Ambiguous
    {
        return Err(injected_response(mode));
    }

    let id = str_field(&body, "benchmark_id")?.to_owned();
    let height = w.height;
    let bench = w
        .benchmarks
        .get(&id)
        .ok_or_else(|| ApiError::bad_request("unknown benchmark_id"))?;
    let submission = bench
        .submission
        .as_ref()
        .ok_or_else(|| ApiError::bad_request("benchmark not submitted"))?;
    if submission.stopped {
        return Err(ApiError::bad_request("stopped benchmark takes no proof"));
    }
    if submission.confirmed.is_none() {
        return Err(ApiError::bad_request("benchmark not confirmed"));
    }
    let sampled = submission
        .sampled_nonces
        .clone()
        .ok_or_else(|| ApiError::bad_request("sampled nonces not yet published"))?;
    if bench.proof.is_some() {
        return Err(ApiError::bad_request("proof already submitted"));
    }

    let proofs = body
        .get("merkle_proofs")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("missing merkle_proofs"))?;
    let mut provided = Vec::new();
    for entry in proofs {
        let leaf = entry
            .get("leaf")
            .ok_or_else(|| ApiError::bad_request("merkle proof missing leaf"))?;
        for field in ["nonce", "runtime_signature", "fuel_consumed"] {
            if leaf.get(field).and_then(Value::as_u64).is_none() {
                return Err(ApiError::bad_request(format!(
                    "leaf missing u64 field {field}"
                )));
            }
        }
        for field in ["solution", "cpu_arch"] {
            str_field(leaf, field)?;
        }
        if entry.get("branch").and_then(Value::as_str).is_none() {
            return Err(ApiError::bad_request("merkle proof missing branch"));
        }
        provided.push(leaf.get("nonce").and_then(Value::as_u64).unwrap_or(0));
    }
    let mut expected = sampled;
    provided.sort_unstable();
    expected.sort_unstable();
    if provided != expected {
        return Err(ApiError::bad_request(
            "merkle_proofs must contain every sampled nonce exactly once and no other nonce",
        ));
    }

    let block_started = bench.block_started;
    if let Some(bench) = w.benchmarks.get_mut(&id) {
        bench.proof = Some(ProofSubmission {
            submitted_height: height,
            confirmed: None,
            submission_delay: height.saturating_sub(block_started),
            block_active: None,
        });
    }

    if injection == Some(InjectMode::Ambiguous) {
        return Err(injected_response(InjectMode::Ambiguous));
    }
    Ok(Json(json!({ "ok": true, "verified": true })))
}

async fn fake_advance(State(world): State<SharedWorld>, body: Option<Json<Value>>) -> ApiResult {
    let count = body
        .as_ref()
        .and_then(|Json(v)| v.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let mut w = lock(&world);
    for _ in 0..count {
        w.advance_block();
    }
    Ok(Json(
        json!({ "height": w.height, "block_id": w.block_id() }),
    ))
}

async fn fake_inject(State(world): State<SharedWorld>, Json(body): Json<Value>) -> ApiResult {
    let target = str_field(&body, "target")?.to_owned();
    let mode = match str_field(&body, "mode")? {
        "reject" => InjectMode::Reject,
        "rate_limit" => InjectMode::RateLimit,
        "error" => InjectMode::Error,
        "ambiguous" => InjectMode::Ambiguous,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown inject mode {other}"
            )));
        }
    };
    let count = body.get("count").and_then(Value::as_u64).unwrap_or(1);
    let mut w = lock(&world);
    let queue = w.injections.entry(target).or_default();
    for _ in 0..count {
        queue.push_back(mode);
    }
    Ok(Json(json!({ "ok": true })))
}

/// Publish the verification event for a benchmark
/// (`tig_integration.md` §7: id in `block.data.confirmed_ids.verified`).
///
/// A `/_fake/` control rather than something a client submission causes,
/// because verification is TIG's ruling and the pool has no way to ask for it
/// — the same reason `/_fake/advance-block` exists for a chain no client
/// moves.
///
/// Refuses a benchmark whose proof is not confirmed. §4.5's ladder runs
/// `PROOF_CONFIRMED -> VERIFYING -> ACTIVE`, so a verification before a
/// confirmed proof is a state the real chain does not produce, and a fake that
/// produced it would let a test assert the pool handles something that cannot
/// happen while missing what does.
async fn fake_verify(State(world): State<SharedWorld>, Json(body): Json<Value>) -> ApiResult {
    let id = str_field(&body, "benchmark_id")?.to_owned();
    let mut w = lock(&world);
    let height = w.height;
    let bench = w
        .benchmarks
        .get_mut(&id)
        .ok_or_else(|| ApiError::bad_request(format!("no benchmark {id}")))?;
    if bench.proof.as_ref().and_then(|p| p.confirmed).is_none() {
        return Err(ApiError::bad_request(format!(
            "benchmark {id} has no confirmed proof; §4.5 verifies after PROOF_CONFIRMED"
        )));
    }
    // The one ordering that is refused, and unlike the reverse it has an
    // authority: §4.5 lists FRAUDULENT as a terminal branch, and a chain does
    // not leave one. Fraud *after* verification is permitted — see
    // `fake_fraud`.
    if bench.fraud.is_some() || bench.fraud_pending {
        return Err(ApiError::bad_request(format!(
            "benchmark {id} was ruled fraudulent, which §4.5 makes terminal"
        )));
    }
    bench.verify_pending = true;
    Ok(Json(
        json!({ "ok": true, "benchmark_id": id, "published_at": height + 1 }),
    ))
}

/// Rule a benchmark fraudulent (§7: an entry in `get-benchmarks.frauds` with
/// non-null `state.block_confirmed`).
///
/// Also a TIG ruling, and also one the pool cannot cause — which is exactly
/// why `mining_system.md` treats it as the outcome method verification exists
/// to catch.
///
/// A benchmark must exist, and **nothing else is required**. In particular a
/// verified benchmark can still be ruled fraudulent: §7 lists "Fraud
/// confirmed" and "Verification event" as independent evidence with no
/// ordering or exclusivity between them, `workflow::confirm_fraud` is
/// documented as reachable from any non-terminal state that owns a benchmark
/// (and `Verified` is deliberately non-terminal), and `restart.rs` applies
/// fraud *before* the forward steps precisely because one §10 window can carry
/// the same benchmark id in `frauds` and in `verified`. The fixture case
/// `fraud_confirmed_after_proof` starts at VERIFYING.
///
/// An earlier version of this refused it, on the reasoning that a fake should
/// not produce states the chain does not. That reasoning is right and was
/// applied without checking: nothing says the chain does not produce this, two
/// documents and the pool's own code say it does, and the refusal made the
/// VERIFIED -> FRAUDULENT path undriveable from a server while a test pinned
/// the false rule in place.
async fn fake_fraud(State(world): State<SharedWorld>, Json(body): Json<Value>) -> ApiResult {
    let id = str_field(&body, "benchmark_id")?.to_owned();
    let mut w = lock(&world);
    let height = w.height;
    let bench = w
        .benchmarks
        .get_mut(&id)
        .ok_or_else(|| ApiError::bad_request(format!("no benchmark {id}")))?;
    bench.fraud_pending = true;
    Ok(Json(
        json!({ "ok": true, "benchmark_id": id, "published_at": height + 1 }),
    ))
}

async fn fake_state(State(world): State<SharedWorld>) -> ApiResult {
    let w = lock(&world);
    Ok(Json(json!({
        "height": w.height,
        "round": w.round,
        "block_id": w.block_id(),
        "benchmarks": w.benchmarks_json(),
        "active": w.active_benchmark_ids(),
        "writes_received": w.writes_received,
    })))
}

pub fn build_world(cfg: Config) -> Result<SharedWorld, String> {
    Ok(Arc::new(Mutex::new(World::from_fixture_dir(cfg)?)))
}

pub fn router(world: SharedWorld) -> Router {
    Router::new()
        .route("/get-block", get(get_block))
        .route("/get-challenges", get(get_challenges))
        .route("/get-algorithms", get(get_algorithms))
        .route("/get-opow", get(get_opow))
        .route("/get-player-data", get(get_player_data))
        .route("/get-benchmarks", get(get_benchmarks))
        .route("/get-tracks-data", get(get_tracks_data))
        .route("/get-benchmark-data", get(get_benchmark_data))
        .route("/get-binary-blob", get(get_binary_blob))
        .route("/get-round-emissions", get(get_round_emissions))
        .route("/submit-precommit", post(submit_precommit))
        .route("/submit-benchmark", post(submit_benchmark))
        .route("/submit-proof", post(submit_proof))
        .route("/_fake/advance-block", post(fake_advance))
        .route("/_fake/inject", post(fake_inject))
        .route("/_fake/verify", post(fake_verify))
        .route("/_fake/fraud", post(fake_fraud))
        .route("/_fake/state", get(fake_state))
        .with_state(world)
}
