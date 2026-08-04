//! Spike member agent CLI. See `docs/plans/protocol-spike.md` §5 (phase S2).
//!
//! Executes every nonce of a confirmed testnet assignment inside the pinned
//! challenge runtime container, collects the ordered quality vector and
//! per-nonce `OutputData`, builds the TIG Merkle material, and assembles a
//! complete proof-material package (`docs/member_protocol.md` §10). It never
//! performs a TIG protocol write and never touches the API key: benchmark
//! commitment is phase S4.
//!
//! Usage:
//!   spike-member run --assignment <gateway assignment doc> \
//!     [--config config/tig_integration.json] [--data-dir data/spike-member] \
//!     [--container <name>] [--platform-digest sha256:...] [--zstd-level 19]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use spike::member::{
    OutputRecord, PackageIdentity, build_manifest, build_tar, compress_zstd, derived_uuid, hex,
    jsonify, leaf_hashes_bytes, merkle_root, outputs_ndjson, qualities_bytes, rfc3339_utc,
    sha256_hex, tree_capacity,
};

const MEMBER_AGENT_VERSION: &str = "spike-member/0.1.0";

/// Challenge id -> monorepo challenge name (runtime image key), per the
/// pinned upstream `tig-runtime` dispatch table and `slave.yml`.
fn challenge_name(challenge_id: &str) -> Result<&'static str> {
    Ok(match challenge_id {
        "c001" => "satisfiability",
        "c002" => "vehicle_routing",
        "c003" => "knapsack",
        "c004" => "vector_search",
        "c005" => "hypergraph",
        "c006" => "neuralnet_optimizer",
        "c007" => "job_scheduling",
        "c008" => "energy_arbitrage",
        other => bail!("unknown challenge id {other}"),
    })
}

struct Args {
    assignment: PathBuf,
    config: PathBuf,
    data_dir: PathBuf,
    container: Option<String>,
    platform_digest: Option<String>,
    zstd_level: i32,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    match it.next().as_deref() {
        Some("run") => {}
        other => bail!("usage: spike-member run [flags]; got {other:?}"),
    }
    let mut a = Args {
        assignment: PathBuf::new(),
        config: "config/tig_integration.json".into(),
        data_dir: "data/spike-member".into(),
        container: None,
        platform_digest: None,
        zstd_level: 19,
    };
    while let Some(flag) = it.next() {
        let v = it
            .next()
            .ok_or_else(|| anyhow!("flag {flag} needs a value"))?;
        match flag.as_str() {
            "--assignment" => a.assignment = v.into(),
            "--config" => a.config = v.into(),
            "--data-dir" => a.data_dir = v.into(),
            "--container" => a.container = Some(v),
            "--platform-digest" => a.platform_digest = Some(v),
            "--zstd-level" => a.zstd_level = v.parse().context("--zstd-level")?,
            _ => bail!("unknown flag {flag}"),
        }
    }
    if a.assignment.as_os_str().is_empty() {
        bail!("--assignment <path> is required");
    }
    Ok(a)
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn get_str<'a>(v: &'a Value, ptr: &str) -> Result<&'a str> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string at {ptr}"))
}

fn get_u64(v: &Value, ptr: &str) -> Result<u64> {
    v.pointer(ptr)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing u64 at {ptr}"))
}

/// Run a command, returning (exit_code, stdout, stderr) without interpreting
/// failure — callers decide, mirroring the reference slave's rules.
fn run_cmd(program: &str, args: &[&str]) -> Result<(i32, String, String)> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("spawning {program}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn run_ok(program: &str, args: &[&str]) -> Result<String> {
    let (code, stdout, stderr) = run_cmd(program, args)?;
    if code != 0 {
        bail!("{program} {args:?} failed with exit {code}: {stderr}");
    }
    Ok(stdout)
}

/// Process CPU time of this agent (self + reaped children, e.g. the docker
/// CLI) in milliseconds, from /proc/self/stat. The nonce computation itself
/// runs under containerd, not as our child, so this measures member CPU
/// spent OUTSIDE nonce execution. Assumes the standard 100 Hz tick.
fn member_cpu_ms() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").context("/proc/self/stat")?;
    let after = stat
        .rsplit_once(')')
        .ok_or_else(|| anyhow!("malformed /proc/self/stat"))?
        .1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // fields[0] is state (field 3); utime/stime/cutime/cstime are fields 14-17.
    let tick = |i: usize| -> Result<u64> {
        fields
            .get(i)
            .ok_or_else(|| anyhow!("short /proc/self/stat"))?
            .parse::<u64>()
            .context("parsing /proc/self/stat")
    };
    let ticks = tick(11)? + tick(12)? + tick(13)? + tick(14)?;
    Ok(ticks * 1000 / 100)
}

fn download_algorithm(
    base_url: &str,
    algorithm_id: &str,
    dest_tar_gz: &Path,
    extract_dir: &Path,
) -> Result<(u64, String, u64)> {
    let start = Instant::now();
    let url = format!("{base_url}/get-binary-blob?algorithm_id={algorithm_id}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} -> {}", resp.status());
    }
    let bytes = resp.bytes().context("downloading algorithm blob")?;
    std::fs::create_dir_all(
        dest_tar_gz
            .parent()
            .ok_or_else(|| anyhow!("tarball path has no parent"))?,
    )?;
    std::fs::write(dest_tar_gz, &bytes)?;
    let sha = sha256_hex(&bytes);
    std::fs::create_dir_all(extract_dir)?;
    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(&bytes[..]));
    tar::Archive::new(gz)
        .unpack(extract_dir)
        .context("extracting algorithm tar.gz")?;
    Ok((
        bytes.len() as u64,
        sha,
        u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    ))
}

/// Find the single `.so` under `<extract_dir>/<cpu_arch>/`.
fn find_so(extract_dir: &Path, cpu_arch: &str) -> Result<PathBuf> {
    let dir = extract_dir.join(cpu_arch);
    let mut sos: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "so"))
        .collect();
    match (sos.pop(), sos.is_empty()) {
        (Some(p), true) => Ok(p),
        (Some(_), false) => bail!("multiple .so files under {}", dir.display()),
        (None, _) => bail!("no .so under {}", dir.display()),
    }
}

/// Ensure the pinned runtime container is running with bounded resources and
/// the algorithms/results mounts. Returns the container name.
fn ensure_container(
    name: &str,
    image: &str,
    algorithms_dir: &Path,
    results_dir: &Path,
) -> Result<()> {
    let ps = run_ok("docker", &["ps", "--format", "{{.Names}}"])?;
    if ps.lines().any(|l| l.trim() == name) {
        return Ok(());
    }
    // Remove a stopped container with the same name, if any.
    let _ = run_cmd("docker", &["rm", "-f", name]);
    let alg = algorithms_dir
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF8 algorithms dir"))?;
    let res = results_dir
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF8 results dir"))?;
    run_ok(
        "docker",
        &[
            "run",
            "-d",
            "--name",
            name,
            "--cpus",
            "2",
            "--memory",
            "2g",
            "--pids-limit",
            "512",
            "--network",
            "none",
            "-v",
            &format!("{alg}:/app/algorithms"),
            "-v",
            &format!("{res}:/app/results"),
            image,
            "sleep",
            "infinity",
        ],
    )?;
    Ok(())
}

/// Resolve the platform-specific manifest digest for this host architecture
/// from the pinned multi-platform manifest list.
fn resolve_platform_digest(image: &str, cpu_arch: &str) -> Result<String> {
    let out = run_ok("docker", &["manifest", "inspect", image])?;
    let v: Value = serde_json::from_str(&out).context("parsing docker manifest inspect")?;
    let manifests = v
        .get("manifests")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("manifest list missing 'manifests'"))?;
    for m in manifests {
        let os = m.pointer("/platform/os").and_then(Value::as_str);
        let arch = m.pointer("/platform/architecture").and_then(Value::as_str);
        if os == Some("linux") && arch == Some(cpu_arch) {
            return m
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("platform manifest missing digest"));
        }
    }
    bail!("no linux/{cpu_arch} entry in manifest list for {image}");
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let total_start = Instant::now();
    let a = parse_args()?;
    let config = read_json(&a.config)?;
    let assignment = read_json(&a.assignment)?;

    // ---- assignment + config fields ------------------------------------
    let benchmark_id = get_str(&assignment, "/benchmark_id")?.to_owned();
    let settings = assignment
        .get("settings")
        .cloned()
        .ok_or_else(|| anyhow!("assignment missing settings"))?;
    let details = assignment
        .get("details")
        .cloned()
        .ok_or_else(|| anyhow!("assignment missing details"))?;
    let challenge_id = get_str(&settings, "/challenge_id")?.to_owned();
    let algorithm_id = get_str(&settings, "/algorithm_id")?.to_owned();
    let rand_hash = get_str(&details, "/rand_hash")?.to_owned();
    let num_nonces = get_u64(&details, "/num_nonces")?;
    let fuel_budget = get_u64(&details, "/fuel_budget")?;
    let block_started = get_u64(&details, "/block_started")?;
    let ch_name = challenge_name(&challenge_id)?;

    let base_url = get_str(&config, "/network/api_base_url")?.to_owned();
    let cpu_arch = get_str(&config, "/spike/cpu_architecture")?.to_owned();
    let compute_type = get_str(&config, "/spike/compute_type")?.to_owned();
    let upstream_commit = get_str(&config, "/upstream/commit")?.to_owned();
    let benchmarker_version =
        get_str(&config, "/upstream/benchmarker_container_version")?.to_owned();
    let max_age = get_u64(
        &config,
        "/spike/workflow_guardrails/max_assignment_age_blocks",
    )?;
    let package_due_age = get_u64(&config, "/spike/workflow_guardrails/package_due_age_blocks")?;
    let image_key = format!("/images/{ch_name}_runtime");
    let image_ref = get_str(&config, &format!("{image_key}/reference"))?.to_owned();
    let image_manifest_digest =
        get_str(&config, &format!("{image_key}/manifest_digest"))?.to_owned();
    let repo = image_ref
        .rsplit_once(':')
        .map_or(image_ref.as_str(), |(r, _)| r);
    let image = format!("{repo}@{image_manifest_digest}");

    // ---- guardrails: never start work that cannot meet its deadline -----
    let client = spike::TigClient::new(&base_url, String::new())?;
    let block = client.get("get-block")?;
    let height = get_u64(&block, "/block/details/height")?;
    if height >= block_started + max_age {
        bail!(
            "assignment too old to start: height {height} >= block_started {block_started} + \
             max_assignment_age_blocks {max_age}; submit a fresh precommit via spike-gateway"
        );
    }
    println!(
        "assignment {benchmark_id}: {challenge_id}/{algorithm_id} track {} \
         num_nonces {num_nonces} age {} blocks (package due before {})",
        get_str(&settings, "/track_id")?,
        height - block_started,
        block_started + package_due_age,
    );

    // ---- workspace -------------------------------------------------------
    std::fs::create_dir_all(&a.data_dir)?;
    let data_dir = std::fs::canonicalize(&a.data_dir)?;
    let algorithms_dir = data_dir.join("algorithms");
    let results_dir = data_dir.join("results");
    let package_dir = data_dir.join(format!("package-{benchmark_id}"));
    std::fs::create_dir_all(algorithms_dir.join(ch_name))?;
    std::fs::create_dir_all(&results_dir)?;
    std::fs::create_dir_all(&package_dir)?;

    // ---- algorithm binary ------------------------------------------------
    let tarball = data_dir.join(format!("{algorithm_id}.tar.gz"));
    let extract_dir = algorithms_dir.join(ch_name);
    let (blob_bytes, blob_sha256, download_ms) =
        download_algorithm(&base_url, &algorithm_id, &tarball, &extract_dir)?;
    let so_host = find_so(&extract_dir, &cpu_arch)?;
    let so_bytes = std::fs::read(&so_host)?;
    let so_sha256 = sha256_hex(&so_bytes);
    let so_name = so_host
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("bad .so name"))?
        .to_owned();
    let so_container = format!("/app/algorithms/{ch_name}/{cpu_arch}/{so_name}");
    println!("algorithm blob {blob_bytes} bytes sha256 {blob_sha256} ({download_ms} ms)");

    // ---- runtime container ------------------------------------------------
    let container = a
        .container
        .clone()
        .unwrap_or_else(|| format!("spike-{ch_name}"));
    ensure_container(&container, &image, &algorithms_dir, &results_dir)?;
    let platform_digest = match &a.platform_digest {
        Some(d) => d.clone(),
        None => resolve_platform_digest(&image, &cpu_arch)
            .context("resolving platform digest (pass --platform-digest to override)")?,
    };

    // ---- execute every nonce ----------------------------------------------
    let settings_json = jsonify(&settings)?;
    let out_dir_container = format!("/app/results/{benchmark_id}");
    let out_dir_host = results_dir.join(&benchmark_id);
    let fuel_arg = fuel_budget.to_string();
    let mut records: Vec<OutputRecord> = Vec::new();
    let mut qualities: Vec<i32> = Vec::new();
    let mut per_nonce = Vec::new();
    let exec_start = Instant::now();
    for nonce in 0..num_nonces {
        let nonce_arg = nonce.to_string();
        let t0 = Instant::now();
        let (code, _, stderr) = run_cmd(
            "docker",
            &[
                "exec",
                &container,
                "tig-runtime",
                &settings_json,
                &rand_hash,
                &nonce_arg,
                &so_container,
                "--fuel",
                &fuel_arg,
                "--output",
                &out_dir_container,
            ],
        )?;
        let runtime_ms = t0.elapsed().as_millis();
        let out_file = out_dir_host.join(format!("{nonce}.json"));
        if !out_file.exists() {
            // Reference slave rule: missing output is fatal regardless of code.
            bail!("nonce {nonce}: tig-runtime produced no output (exit {code}): {stderr}");
        }
        let t1 = Instant::now();
        let out_file_container = format!("{out_dir_container}/{nonce}.json");
        let (vcode, vstdout, vstderr) = run_cmd(
            "docker",
            &[
                "exec",
                &container,
                "tig-verifier",
                &settings_json,
                &rand_hash,
                &nonce_arg,
                &out_file_container,
            ],
        )?;
        let verifier_ms = t1.elapsed().as_millis();
        if vcode != 0 {
            bail!("nonce {nonce}: tig-verifier rejected the solution (exit {vcode}): {vstderr}");
        }
        let quality: i32 = vstdout
            .lines()
            .last()
            .and_then(|l| l.strip_prefix("quality: "))
            .ok_or_else(|| anyhow!("nonce {nonce}: no 'quality: ' line in tig-verifier output"))?
            .trim()
            .parse()
            .with_context(|| format!("nonce {nonce}: parsing quality"))?;
        let record = OutputRecord::from_runtime_json(&read_json(&out_file)?)?;
        if record.nonce != nonce {
            bail!("nonce {nonce}: output file claims nonce {}", record.nonce);
        }
        if record.cpu_arch != cpu_arch {
            bail!(
                "nonce {nonce}: output cpu_arch {} != qualified {cpu_arch}",
                record.cpu_arch
            );
        }
        println!(
            "nonce {nonce}: runtime {runtime_ms} ms, verifier {verifier_ms} ms, \
             quality {quality}, fuel {}",
            record.fuel_consumed
        );
        per_nonce.push(json!({
            "nonce": nonce,
            "runtime_ms": u64::try_from(runtime_ms).unwrap_or(u64::MAX),
            "verifier_ms": u64::try_from(verifier_ms).unwrap_or(u64::MAX),
            "quality": quality,
            "fuel_consumed": record.fuel_consumed.to_string(),
            "runtime_exit_code": code,
        }));
        qualities.push(quality);
        records.push(record);
    }
    let execution_ms = u64::try_from(exec_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    // ---- Merkle material ----------------------------------------------------
    let package_start = Instant::now();
    let leaves = records
        .iter()
        .map(OutputRecord::leaf_hash)
        .collect::<Result<Vec<_>>>()?;
    let root = merkle_root(&leaves)?;

    // ---- assignment identity (member_protocol.md §7) ------------------------
    // The spike has no pool-side member protocol yet, so the pool-issued
    // identities are deterministic stand-ins derived from the benchmark id;
    // every protocol-meaningful field is real.
    let uuid = |role: &str| derived_uuid(&format!("spike-member:v0:{benchmark_id}:{role}"));
    let cpu_cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let compute = json!({
        "compute_kind": "CPU",
        "compute_type": compute_type,
        "cpu_arch": cpu_arch,
        "cpu_vendor": "ARM",
        "cpu_cores": cpu_cores,
    });
    let mut identity_details = details.clone();
    if let Some(obj) = identity_details.as_object_mut() {
        // Member-boundary rule (§10.2): fuel_budget rides as a decimal string.
        obj.insert("fuel_budget".into(), Value::String(fuel_budget.to_string()));
    }
    let qualification_spec_digest = sha256_hex(b"spike-member:qualification-spec:v0");
    let assignment_identity = json!({
        "protocol_version": "0.1.0",
        "package_format": "proof-material-v1",
        "network": "testnet",
        "tig_api_base_url": base_url,
        "pool_player_id": get_str(&settings, "/player_id")?,
        "assignment_id": uuid("assignment"),
        "offer_id": uuid("offer"),
        "member_id": uuid("member"),
        "worker_id": uuid("worker"),
        "slot_id": uuid("slot"),
        "slot_generation": 1,
        "compute": compute,
        "qualification_id": uuid("qualification"),
        "qualification_spec_digest": qualification_spec_digest,
        "decision_block_id": get_str(&assignment, "/observed_via/block_id")?,
        "confirmed_precommit": {
            "benchmark_id": benchmark_id,
            "settings": settings,
            "details": identity_details,
            "block_confirmed": get_u64(&assignment, "/confirmed_at_block")?,
        },
        "nonce_range": { "start": 0, "end_exclusive": num_nonces, "num_nonces": num_nonces },
        "algorithm_binary_sha256": blob_sha256,
        "runtime": {
            "tig_upstream_commit": upstream_commit,
            "benchmarker_version": benchmarker_version,
            "image_manifest_digest": image_manifest_digest,
            "image_platform_digest": platform_digest,
            "platform": format!("linux/{cpu_arch}"),
        },
        "merkle": {
            "algorithm": "tig-merkle-blake3-v1",
            "leaf_encoding": "tig-output-metadata-v1",
            "tree_capacity": tree_capacity(num_nonces),
        },
        "package_limits": {
            "max_compressed_bytes": 1_073_741_824u64,
            "max_uncompressed_bytes": 2_147_483_648u64,
            "max_manifest_bytes": 262_144,
            "max_output_record_bytes": 1_048_576,
            "min_upload_chunk_bytes": 1_048_576,
            "max_upload_chunk_bytes": 8_388_608,
        },
    });
    let assignment_digest = sha256_hex(jsonify(&assignment_identity)?.as_bytes());

    // ---- package (member_protocol.md §10) ------------------------------------
    let created_at = rfc3339_utc(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock before epoch")?
            .as_secs(),
    );
    let identity = PackageIdentity {
        benchmark_id: benchmark_id.clone(),
        assignment_digest: assignment_digest.clone(),
        member_id: uuid("member"),
        worker_id: uuid("worker"),
        slot_id: uuid("slot"),
        assignment_id: uuid("assignment"),
        package_id: uuid("package"),
        qualification_id: uuid("qualification"),
        qualification_spec_digest,
        slot_generation: 1,
        compute: assignment_identity
            .get("compute")
            .cloned()
            .ok_or_else(|| anyhow!("identity missing compute"))?,
        member_agent_version: MEMBER_AGENT_VERSION.to_owned(),
        tig_upstream_commit: upstream_commit,
        benchmarker_version,
        algorithm_binary_sha256: blob_sha256.clone(),
        runtime_image_manifest_digest: image_manifest_digest.clone(),
        runtime_image_platform_digest: platform_digest.clone(),
        runtime_platform: format!("linux/{cpu_arch}"),
        created_at,
    };
    let qualities_data = qualities_bytes(&qualities);
    let leaf_hashes_data = leaf_hashes_bytes(&leaves);
    let outputs_data = outputs_ndjson(&records)?;
    let manifest = build_manifest(
        &identity,
        num_nonces,
        &root,
        &qualities_data,
        &leaf_hashes_data,
        &outputs_data,
    );
    let manifest_data = jsonify(&manifest)?.into_bytes();
    let tar_data = build_tar(
        &manifest_data,
        &qualities_data,
        &leaf_hashes_data,
        &outputs_data,
    )?;
    let package_data = compress_zstd(&tar_data, a.zstd_level)?;
    let package_ms = u64::try_from(package_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    // ---- persist everything ---------------------------------------------------
    let write = |name: &str, data: &[u8]| -> Result<()> {
        std::fs::write(package_dir.join(name), data).with_context(|| format!("writing {name}"))
    };
    write("manifest.json", &manifest_data)?;
    write("qualities.i32le", &qualities_data)?;
    write("leaf-hashes.bin", &leaf_hashes_data)?;
    write("outputs.ndjson", &outputs_data)?;
    write("package.tar.zst", &package_data)?;
    std::fs::write(
        package_dir.join("assignment-identity.json"),
        serde_json::to_string_pretty(&assignment_identity)?,
    )?;
    let package_sha256 = sha256_hex(&package_data);
    let upload_declaration = json!({
        "package_id": identity.package_id,
        "assignment_digest": assignment_digest,
        "media_type": "application/vnd.tig-pool.proof-material-v1.tar+zstd",
        "compressed_size_bytes": package_data.len(),
        "uncompressed_size_bytes": tar_data.len(),
        "manifest_sha256": sha256_hex(&manifest_data),
        "package_sha256": package_sha256,
    });
    std::fs::write(
        package_dir.join("upload-declaration.json"),
        serde_json::to_string_pretty(&upload_declaration)?,
    )?;

    // ---- measurements -----------------------------------------------------------
    let fuel: Vec<u64> = records.iter().map(|r| r.fuel_consumed).collect();
    let fuel_sum: u128 = fuel.iter().map(|&f| u128::from(f)).sum();
    let total_ms = u64::try_from(total_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let report = json!({
        "phase": "S2",
        "benchmark_id": benchmark_id,
        "assignment_doc": a.assignment.display().to_string(),
        "assignment_digest": assignment_digest,
        "merkle_root": hex(&root),
        "quality_vector": qualities,
        "algorithm": {
            "algorithm_id": algorithm_id,
            "blob_bytes": blob_bytes,
            "blob_sha256": blob_sha256,
            "so_file": so_name,
            "so_sha256": so_sha256,
            "download_ms": download_ms,
        },
        "runtime_image": {
            "image": image,
            "manifest_digest": image_manifest_digest,
            "platform_digest": platform_digest,
            "platform": format!("linux/{cpu_arch}"),
            "container": container,
        },
        "per_nonce": per_nonce,
        "measurements": {
            "total_wall_ms": total_ms,
            "execution_wall_ms": execution_ms,
            "package_build_ms": package_ms,
            "member_cpu_outside_nonce_execution_ms": member_cpu_ms()?,
            "wall_outside_nonce_execution_ms": total_ms.saturating_sub(execution_ms),
            "package_compressed_bytes": package_data.len(),
            "package_uncompressed_tar_bytes": tar_data.len(),
            "package_compressed_bytes_per_nonce": package_data.len() as u64 / num_nonces.max(1),
            "member_file_bytes": {
                "manifest.json": manifest_data.len(),
                "qualities.i32le": qualities_data.len(),
                "leaf-hashes.bin": leaf_hashes_data.len(),
                "outputs.ndjson": outputs_data.len(),
            },
            "fuel_consumed": {
                "min": fuel.iter().min().copied().unwrap_or(0).to_string(),
                "max": fuel.iter().max().copied().unwrap_or(0).to_string(),
                "sum": fuel_sum.to_string(),
                "mean": (fuel_sum / u128::from(num_nonces.max(1))).to_string(),
            },
            "zstd_level": a.zstd_level,
        },
        "upload_declaration": upload_declaration,
        "notes": [
            "pool-issued identities (member/worker/slot/assignment/package/qualification UUIDs, qualification_spec_digest, slot_generation) are deterministic spike stand-ins derived from the benchmark id — there is no pool-side member protocol yet",
            "member_cpu_outside_nonce_execution_ms is utime+stime+cutime+cstime of this process from /proc/self/stat at 100 Hz; docker CLI children are included, container-side nonce computation is not",
        ],
    });
    std::fs::write(
        package_dir.join("run-report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;

    println!("merkle root {}", hex(&root));
    println!("assignment digest {assignment_digest}");
    println!(
        "package {} bytes compressed ({} tar), sha256 {package_sha256}",
        package_data.len(),
        tar_data.len(),
    );
    println!("package dir {}", package_dir.display());
    Ok(())
}
