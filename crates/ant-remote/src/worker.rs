//! The worker: lease a job, materialize exactly its declared inputs into a
//! fresh workspace (every blob re-verified), run the action with the local
//! executor (optionally under the Windows sandbox), upload declared outputs,
//! and report. Workers execute untrusted build code; run them under
//! `--sandbox` (and on a dedicated machine or account) where possible.

use crate::blobs::digest_bytes;
use crate::http::Coordinator;
use crate::protocol::{
    Entry, EntryKind, JobResult, JobSpec, LeaseRequest, LeaseResponse, PROTOCOL_VERSION,
    validate_entries,
};
use build_action::OutputKind;
use local_executor::CancellationToken;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct WorkerConfig {
    pub worker_id: String,
    pub work_dir: PathBuf,
    pub sandbox: bool,
    /// Exit after one job (or after the first empty poll when no job is
    /// queued yet and `once` is set). Used by tests and one-shot workers.
    pub once: bool,
    pub poll_interval: Duration,
}

/// Run the worker loop. Returns after one job when `once` is set.
pub fn run(coordinator: &Coordinator, config: &WorkerConfig) -> Result<(), String> {
    std::fs::create_dir_all(&config.work_dir)
        .map_err(|error| format!("cannot create work dir: {error}"))?;
    let request = LeaseRequest {
        protocol_version: PROTOCOL_VERSION,
        worker_id: config.worker_id.clone(),
        os: std::env::consts::OS.to_owned(),
    };
    loop {
        let lease: Option<LeaseResponse> =
            match coordinator.post_json("/v1/lease", &request, "lease") {
                Ok(lease) => lease,
                Err(error) => {
                    // Coordinator restarts or network blips: back off and retry.
                    eprintln!("ant worker: {error}; retrying");
                    std::thread::sleep(config.poll_interval * 5);
                    continue;
                }
            };
        let Some(lease) = lease else {
            std::thread::sleep(config.poll_interval);
            continue;
        };
        println!(
            "worker={} job={} action={} status=started",
            config.worker_id, lease.job_id, lease.spec.action.id
        );
        let result = execute_job(coordinator, config, &lease.job_id, &lease.spec);
        let succeeded = result.succeeded();
        let path = format!("/v1/jobs/{}/result", lease.job_id);
        coordinator.post_json::<_, serde_json::Value>(&path, &result, "result report")?;
        println!(
            "worker={} job={} status={}",
            config.worker_id,
            lease.job_id,
            if succeeded { "succeeded" } else { "failed" }
        );
        if config.once {
            return Ok(());
        }
    }
}

fn failure(config: &WorkerConfig, error: String) -> JobResult {
    JobResult {
        worker_id: config.worker_id.clone(),
        exit_code: None,
        timed_out: false,
        error: Some(error),
        outputs: Vec::new(),
        reused: false,
        cache_key: None,
    }
}

/// Where a sandboxed Linux worker mounts every job's workspace (D-019):
/// identical across jobs and workers, on the sandbox's private /tmp.
pub const WORKER_MOUNT: &str = "/tmp/ant/ws";

/// One modification time for every materialized input (2000-01-01 UTC), so
/// filesystem metadata is a deterministic function of content (D-019).
fn normalized_mtime() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800)
}

/// Whether this worker produces cache-eligible, path- and time-independent
/// results: the Linux bubblewrap sandbox (no network, fixed mount).
fn caches_results(config: &WorkerConfig) -> bool {
    config.sandbox && cfg!(target_os = "linux")
}

fn execute_job(
    coordinator: &Coordinator,
    config: &WorkerConfig,
    job_id: &str,
    spec: &JobSpec,
) -> JobResult {
    // Never trust the coordinator's documents more than a client's.
    if let Err(error) = spec.action.validate() {
        return failure(config, format!("invalid action: {error:?}"));
    }
    if let Err(error) = validate_entries(&spec.inputs) {
        return failure(config, error);
    }
    let workspace = config.work_dir.join(job_id);
    let _ = std::fs::remove_dir_all(&workspace);
    let result = (|| -> Result<JobResult, String> {
        materialize(coordinator, &workspace, &spec.inputs)?;
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if caches_results(config) {
            return run_cacheable(coordinator, config, &workspace, spec);
        }
        let prepared = local_executor::prepare_with_metadata(&workspace, &spec.action)
            .map_err(|error| error.to_string())?;
        let cancellation = CancellationToken::new();
        let executed = if config.sandbox {
            local_executor::execute_sandboxed_prepared(&prepared, &cancellation)
        } else {
            local_executor::execute_prepared(&prepared, &cancellation)
        }
        .map_err(|error| error.to_string())?;
        let mut outputs = Vec::new();
        if executed.success() {
            outputs = upload_outputs(coordinator, &workspace, &spec.action)?;
        }
        Ok(JobResult {
            worker_id: config.worker_id.clone(),
            exit_code: executed.status.code(),
            timed_out: executed.timed_out,
            error: None,
            outputs,
            reused: false,
            cache_key: None,
        })
    })();
    let _ = std::fs::remove_dir_all(&workspace);
    result.unwrap_or_else(|error| failure(config, error))
}

/// Write every declared input into `workspace`, verifying each blob.
fn materialize(
    coordinator: &Coordinator,
    workspace: &Path,
    inputs: &[Entry],
) -> Result<(), String> {
    std::fs::create_dir_all(workspace).map_err(|error| error.to_string())?;
    // Staging for actions that point TEMP/TMP inside the workspace.
    std::fs::create_dir_all(workspace.join(".ant").join("tmp"))
        .map_err(|error| error.to_string())?;
    for entry in inputs {
        let target = workspace.join(&entry.path);
        match entry.kind {
            EntryKind::Dir => {
                std::fs::create_dir_all(&target).map_err(|error| error.to_string())?
            }
            EntryKind::File => {
                let digest = entry.digest.as_deref().expect("validated file entry");
                let bytes = coordinator.get_blob(digest)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&target, bytes).map_err(|error| error.to_string())?;
                set_mtime(&target)?;
            }
        }
    }
    // Directories last (writing files changes their parents' mtimes),
    // deepest first so a parent is stamped after its children.
    let mut dirs: Vec<&Entry> = inputs.iter().filter(|e| e.kind == EntryKind::Dir).collect();
    dirs.sort_by_key(|entry| std::cmp::Reverse(entry.path.matches('/').count()));
    for entry in dirs {
        set_mtime(&workspace.join(&entry.path))?;
    }
    Ok(())
}

fn set_mtime(path: &Path) -> Result<(), String> {
    let file = if path.is_dir() {
        // Directory handles carry their own mtime on Unix; Windows workers
        // never produce cacheable results, so directories are skipped there.
        if cfg!(windows) {
            return Ok(());
        }
        std::fs::File::open(path)
    } else {
        std::fs::File::options().write(true).open(path)
    }
    .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    file.set_modified(normalized_mtime())
        .map_err(|error| format!("cannot set mtime on {}: {error}", path.display()))
}

/// Identify the program the action will run *on this worker*: argv[0]
/// resolved against the worker's PATH, its digest (cached per binary), and
/// its `--version` line (run in the workspace, so per-project toolchain
/// files such as `rust-toolchain.toml` take effect). Tools without
/// `--version` (for example dash) are identified by digest alone. `None`
/// when the plan declares no toolchain or the program cannot be resolved,
/// which keeps the action out of the cache.
fn observe_toolchain(
    action: &build_action::Action,
    workspace: &Path,
) -> Option<build_action::ToolchainIdentity> {
    action.toolchain.as_ref()?;
    let program = Path::new(action.argv.first()?);
    let resolved = if program.is_absolute() {
        program.to_path_buf()
    } else {
        let path = build_digest::local_runtime_environment()
            .into_iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value)?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())?
    };
    let canonical = resolved.canonicalize().ok()?;
    let digest = build_adapter::toolchain_digest(&canonical).ok()?;
    let version = std::process::Command::new(&resolved)
        .arg("--version")
        .current_dir(workspace)
        .env_clear()
        .envs(build_digest::local_runtime_environment())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(|line| line.trim().to_owned())
        })
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| "(no --version)".to_owned());
    Some(build_action::ToolchainIdentity {
        name: canonical
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        version,
        executable_digest: digest.to_string(),
    })
}

/// Sandboxed Linux worker path (D-019): compute the path- and
/// time-independent action key, serve a cache hit without executing, or
/// execute under the fixed mount and store the proven result.
fn run_cacheable(
    coordinator: &Coordinator,
    config: &WorkerConfig,
    workspace: &Path,
    spec: &JobSpec,
) -> Result<JobResult, String> {
    // Content-only digests: with normalized mtimes, metadata is determined
    // by content, so these keys are complete (see the fingerprint docs).
    let mut prepared =
        local_executor::prepare(workspace, &spec.action).map_err(|error| error.to_string())?;
    prepared.sandbox_mount = Some(PathBuf::from(WORKER_MOUNT));
    let fingerprint =
        build_digest::ExecutionFingerprint::remote_worker_sandboxed(Path::new(WORKER_MOUNT));
    let inputs: Vec<_> = prepared
        .input_digests
        .iter()
        .map(|entry| (entry.path.clone(), entry.digest))
        .collect();
    // Key with the toolchain this worker will actually run, never the one
    // the client observed when it generated the plan (D-020).
    let mut keyed_action = spec.action.clone();
    keyed_action.toolchain = observe_toolchain(&spec.action, workspace);
    let candidate = build_digest::action_key_candidate(&keyed_action, &inputs, &fingerprint)
        .map_err(|error| error.to_string())?;
    let key = candidate
        .cache_eligible
        .then(|| candidate.digest.to_string());
    if let Some(key) = &key
        && let Some(entry) = coordinator.cache_lookup(key)?
    {
        return Ok(JobResult {
            worker_id: config.worker_id.clone(),
            exit_code: Some(0),
            timed_out: false,
            error: None,
            outputs: entry.outputs,
            reused: true,
            cache_key: Some(key.clone()),
        });
    }
    let executed = local_executor::execute_sandboxed_prepared(&prepared, &CancellationToken::new())
        .map_err(|error| error.to_string())?;
    let mut outputs = Vec::new();
    if executed.success() {
        outputs = upload_outputs(coordinator, workspace, &spec.action)?;
        if let Some(key) = &key {
            coordinator.cache_store(key, &config.worker_id, &outputs)?;
        }
    }
    Ok(JobResult {
        worker_id: config.worker_id.clone(),
        exit_code: executed.status.code(),
        timed_out: executed.timed_out,
        error: None,
        outputs,
        reused: false,
        cache_key: key,
    })
}

fn upload_outputs(
    coordinator: &Coordinator,
    workspace: &Path,
    action: &build_action::Action,
) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for output in &action.outputs {
        let base = output.path.to_string_lossy().replace('\\', "/");
        let path = workspace.join(&output.path);
        match output.kind {
            OutputKind::File => entries.push(upload_file(coordinator, &path, base)?),
            OutputKind::Tree => {
                entries.push(Entry {
                    path: base.clone(),
                    kind: EntryKind::Dir,
                    digest: None,
                });
                for entry in build_digest::tree_entries(&path).map_err(|error| error.to_string())? {
                    let relative = format!("{base}/{}", entry.path);
                    if entry.is_dir {
                        entries.push(Entry {
                            path: relative,
                            kind: EntryKind::Dir,
                            digest: None,
                        });
                    } else {
                        entries.push(upload_file(coordinator, &path.join(&entry.path), relative)?);
                    }
                }
            }
        }
    }
    Ok(entries)
}

fn upload_file(coordinator: &Coordinator, path: &Path, relative: String) -> Result<Entry, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read output {}: {error}", path.display()))?;
    let digest = digest_bytes(&bytes).to_string();
    if !coordinator.missing(vec![digest.clone()])?.is_empty() {
        coordinator.put_blob(&bytes)?;
    }
    Ok(Entry {
        path: relative,
        kind: EntryKind::File,
        digest: Some(digest),
    })
}
