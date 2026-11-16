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
    /// Serve the shared Gradle task cache to sandboxed actions that ask for
    /// it (D-025; Linux `--sandbox` only).
    pub task_cache: bool,
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
    // A kept per-project workspace (D-048), only where results are never
    // shared: cache-producing workers always build from a fresh workspace.
    let sticky = spec
        .workspace_key
        .as_deref()
        .filter(|key| crate::protocol::valid_workspace_key(key) && !caches_results(config));
    if spec.workspace_key.is_some() && sticky.is_none() && !caches_results(config) {
        return failure(config, "invalid workspace key".to_owned());
    }
    let workspace = match sticky {
        Some(key) => config.work_dir.join("projects").join(key),
        None => config.work_dir.join(job_id),
    };
    if sticky.is_none() {
        let _ = std::fs::remove_dir_all(&workspace);
    }
    let result = (|| -> Result<JobResult, String> {
        if caches_results(config) {
            return run_cacheable(coordinator, config, &workspace, job_id, spec);
        }
        match sticky {
            Some(_) => {
                sync_workspace(coordinator, &workspace, &spec.action.id, &spec.inputs)?;
                prune_projects(&config.work_dir.join("projects"), KEPT_PROJECTS);
            }
            None => materialize(coordinator, &workspace, &spec.inputs)?,
        }
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let mut prepared = local_executor::prepare_with_metadata(&workspace, &spec.action)
            .map_err(|error| error.to_string())?;
        let cancellation = CancellationToken::new();
        let executed = with_streamed_output(
            coordinator,
            config,
            job_id,
            &mut prepared,
            &cancellation,
            |prepared| {
                if config.sandbox {
                    local_executor::execute_sandboxed_prepared(prepared, &cancellation)
                } else {
                    local_executor::execute_prepared(prepared, &cancellation)
                }
            },
        )
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
    if sticky.is_none() {
        let _ = std::fs::remove_dir_all(&workspace);
    }
    result.unwrap_or_else(|error| failure(config, error))
}

/// Run `run` with the action's output streamed to the coordinator while it
/// runs, so the developer sees it live (D-049); both the direct and the
/// sandboxed executors tee it. Everything is flushed before returning, so a
/// client that saw the job finish can read the complete output.
fn with_streamed_output<T>(
    coordinator: &Coordinator,
    config: &WorkerConfig,
    job_id: &str,
    prepared: &mut local_executor::PreparedAction,
    cancellation: &CancellationToken,
    run: impl FnOnce(&mut local_executor::PreparedAction) -> T,
) -> T {
    let log = std::sync::Arc::new(JobLogSink::default());
    prepared.output_capture = Some(local_executor::OutputCapture {
        sink: log.clone(),
        display_environment: Vec::new(),
    });
    let done = std::sync::atomic::AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(LOG_FLUSH_INTERVAL);
                // Every flush is also a heartbeat: the client may have
                // cancelled the job after another action failed (D-054).
                if log.flush(coordinator, config, job_id) {
                    cancellation.cancel();
                }
            }
        });
        let result = run(prepared);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        result
    });
    if log.flush(coordinator, config, job_id) {
        cancellation.cancel();
    }
    result
}

/// How often a running job's new output is sent to the coordinator.
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Collects a job's tool output between flushes (D-049).
#[derive(Default)]
struct JobLogSink {
    pending: std::sync::Mutex<Vec<u8>>,
    /// Bytes of an incomplete UTF-8 character held back for the next flush.
    carry: std::sync::Mutex<Vec<u8>>,
}

impl local_executor::OutputSink for JobLogSink {
    fn write(&self, bytes: &[u8]) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.extend_from_slice(bytes);
        }
    }
}

impl JobLogSink {
    /// Send what arrived since the last flush (possibly nothing: then it is
    /// a heartbeat). Returns whether the coordinator asked to stop the job.
    /// Best effort: output is a convenience and must never fail the build.
    fn flush(&self, coordinator: &Coordinator, config: &WorkerConfig, job_id: &str) -> bool {
        let Ok(mut carry) = self.carry.lock() else {
            return false;
        };
        let fresh = match self.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => return false,
        };
        carry.extend_from_slice(&fresh);
        // Hold back a trailing partial character (at most 3 bytes).
        let complete = match std::str::from_utf8(&carry) {
            Ok(_) => carry.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => carry.len(),
        };
        let text = String::from_utf8_lossy(&carry[..complete]).into_owned();
        carry.drain(..complete);
        let chunk = crate::protocol::LogChunk {
            worker_id: config.worker_id.clone(),
            text,
        };
        let path = format!("/v1/jobs/{job_id}/log");
        matches!(
            coordinator.post_json::<_, serde_json::Value>(&path, &chunk, "job output"),
            Ok(Some(answer)) if answer.get("cancel") == Some(&serde_json::Value::Bool(true))
        )
    }
}

/// Kept project workspaces per worker; the least recently used go first.
const KEPT_PROJECTS: usize = 8;

/// Bring a kept workspace up to date with `inputs` (D-048): write only
/// files whose content changed (with a fresh modification time, so build
/// tools see the change), leave unchanged files untouched (so their
/// incremental state stays valid), and delete files that were inputs last
/// time but are not any more. Files that were never inputs (tool outputs
/// such as `target/`) are left alone, like in a developer's own checkout.
fn sync_workspace(
    coordinator: &Coordinator,
    workspace: &Path,
    action_id: &str,
    inputs: &[Entry],
) -> Result<(), String> {
    use std::collections::BTreeMap;
    let ant_dir = workspace.join(".ant");
    std::fs::create_dir_all(ant_dir.join("tmp")).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(ant_dir.join("sync")).map_err(|error| error.to_string())?;
    let read = |path: &Path| -> BTreeMap<String, String> {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    };
    // What is on disk now (path -> content tag), shared by every action of
    // the project, so a file another action already wrote is not rewritten.
    let manifest_path = ant_dir.join("sync-manifest.json");
    let mut on_disk = read(&manifest_path);
    // This action's inputs last time: only these may be deleted when they
    // disappear, so one action never removes another action's sources.
    let action_hex = digest_bytes(action_id.as_bytes()).to_string();
    let action_path = ant_dir
        .join("sync")
        .join(format!("{}.json", action_hex.trim_start_matches("sha256:")));
    let previous = read(&action_path);
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for entry in inputs.iter().filter(|e| e.kind == EntryKind::Dir) {
        std::fs::create_dir_all(workspace.join(&entry.path)).map_err(|error| error.to_string())?;
        on_disk.insert(entry.path.clone(), "dir".to_owned());
        current.insert(entry.path.clone(), "dir".to_owned());
    }
    let mut changed: Vec<&Entry> = Vec::new();
    for entry in inputs.iter().filter(|e| e.kind == EntryKind::File) {
        let digest = entry.digest.clone().expect("validated file entry");
        let tag = format!("{digest}{}", if entry.executable { "+x" } else { "" });
        if on_disk.get(&entry.path) != Some(&tag) || !workspace.join(&entry.path).is_file() {
            changed.push(entry);
        }
        on_disk.insert(entry.path.clone(), tag);
        current.insert(entry.path.clone(), "file".to_owned());
    }
    for chunk in changed.chunks(256) {
        let digests: Vec<String> = chunk
            .iter()
            .map(|entry| entry.digest.clone().expect("validated file entry"))
            .collect();
        let blobs = coordinator.get_blobs(&digests)?;
        for (entry, bytes) in chunk.iter().zip(blobs) {
            let target = workspace.join(&entry.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            // Replace rather than rewrite in place (hard links elsewhere).
            let _ = std::fs::remove_file(&target);
            std::fs::write(&target, bytes).map_err(|error| error.to_string())?;
            if entry.executable {
                crate::protocol::make_executable(&target)?;
            }
        }
    }
    // This action's inputs that disappeared: files first, then now-empty
    // directories, deepest first. Never anything that was not an input.
    let mut gone: Vec<&String> = previous
        .keys()
        .filter(|path| !current.contains_key(*path))
        .collect();
    gone.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    for path in gone {
        let target = workspace.join(path);
        if previous[path] == "dir" {
            let _ = std::fs::remove_dir(&target);
        } else {
            let _ = std::fs::remove_file(&target);
        }
        on_disk.remove(path);
    }
    let text = serde_json::to_vec(&current).map_err(|error| error.to_string())?;
    std::fs::write(&action_path, text).map_err(|error| error.to_string())?;
    let text = serde_json::to_vec(&on_disk).map_err(|error| error.to_string())?;
    std::fs::write(&manifest_path, text).map_err(|error| error.to_string())
}

/// Keep the `keep` most recently used project workspaces.
fn prune_projects(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut projects: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let manifest = entry.path().join(".ant").join("sync-manifest.json");
            Some((
                std::fs::metadata(manifest).ok()?.modified().ok()?,
                entry.path(),
            ))
        })
        .collect();
    projects.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in projects.into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(path);
    }
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
    for entry in inputs.iter().filter(|e| e.kind == EntryKind::Dir) {
        std::fs::create_dir_all(workspace.join(&entry.path)).map_err(|error| error.to_string())?;
    }
    // Files arrive in batches (D-023); a chunk at a time bounds memory.
    let files: Vec<&Entry> = inputs
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .collect();
    for chunk in files.chunks(256) {
        let digests: Vec<String> = chunk
            .iter()
            .map(|entry| entry.digest.clone().expect("validated file entry"))
            .collect();
        let blobs = coordinator.get_blobs(&digests)?;
        for (entry, bytes) in chunk.iter().zip(blobs) {
            let target = workspace.join(&entry.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&target, bytes).map_err(|error| error.to_string())?;
            if entry.executable {
                crate::protocol::make_executable(&target)?;
            }
            set_mtime(&target)?;
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

/// Run the prepared action in the sandbox, behind the Gradle task-cache
/// bridge when the worker serves it and the action asks for it (D-025).
fn execute_with_task_cache(
    coordinator: &Coordinator,
    config: &WorkerConfig,
    workspace: &Path,
    prepared: &mut local_executor::PreparedAction,
    cancellation: &CancellationToken,
) -> Result<Result<local_executor::ExecutionResult, local_executor::ExecutionError>, String> {
    let run = |prepared: &local_executor::PreparedAction| {
        local_executor::execute_sandboxed_prepared(prepared, cancellation)
    };
    if !(config.task_cache && crate::gradle_cache::wants_task_cache(&prepared.action)) {
        return Ok(run(prepared));
    }
    #[cfg(unix)]
    {
        let ant = std::env::current_exe().map_err(|error| error.to_string())?;
        let bridge_dir = workspace.with_extension("bridge");
        prepared.sandbox_binds.push((
            bridge_dir.clone(),
            PathBuf::from(crate::gradle_cache::BRIDGE_DIR),
        ));
        prepared.action.argv = crate::gradle_cache::bridged_argv(&ant, &prepared.action.argv);
        let prepared = &*prepared;
        crate::gradle_cache::serve_while(coordinator, &bridge_dir, || run(prepared))
    }
    #[cfg(not(unix))]
    {
        let _ = (coordinator, workspace);
        Ok(run(prepared))
    }
}

/// The declared inputs' content digests computed from the job's entries
/// alone (no file is read): a file input is its entry's digest, a tree input
/// is `digest_tree_from_entries` over the entries below it. `None` when an
/// input is missing from the entries, which keeps the job out of the cache.
fn input_digests_from_entries(
    action: &build_action::Action,
    entries: &[Entry],
) -> Option<Vec<(PathBuf, build_digest::ContentDigest)>> {
    let mut digests = Vec::with_capacity(action.inputs.len());
    for input in &action.inputs {
        let base = input.path.to_string_lossy().replace('\\', "/");
        let digest = match input.kind {
            build_action::InputKind::File => entries
                .iter()
                .find(|entry| entry.path == base && entry.kind == EntryKind::File)?
                .digest
                .as_deref()?
                .parse()
                .ok()?,
            build_action::InputKind::Tree => {
                let prefix = format!("{base}/");
                let mut tree = Vec::new();
                for entry in entries {
                    let Some(rel) = entry.path.strip_prefix(&prefix) else {
                        continue;
                    };
                    tree.push(build_digest::TreeEntry {
                        path: rel.to_owned(),
                        is_dir: entry.kind == EntryKind::Dir,
                        digest: match &entry.digest {
                            Some(text) => Some(text.parse().ok()?),
                            None => None,
                        },
                        metadata: None,
                    });
                }
                if !entries
                    .iter()
                    .any(|entry| entry.path == base && entry.kind == EntryKind::Dir)
                {
                    return None;
                }
                build_digest::digest_tree_from_entries(&tree)
            }
        };
        digests.push((input.path.clone(), digest));
    }
    Some(digests)
}

/// Sandboxed Linux worker path (D-019, D-022): compute the path- and
/// time-independent action key from the job's own entries *before*
/// downloading inputs; on a hit return the stored outputs having fetched
/// only the project's root-level files; on a miss materialize everything,
/// confirm the files on disk hash to the same key, execute under the fixed
/// mount, and store the proven result.
fn run_cacheable(
    coordinator: &Coordinator,
    config: &WorkerConfig,
    workspace: &Path,
    job_id: &str,
    spec: &JobSpec,
) -> Result<JobResult, String> {
    // Root-level files only (manifests, lock files, toolchain pins): enough
    // for `--version` to see per-project toolchain selection.
    let root_files: Vec<Entry> = spec
        .inputs
        .iter()
        .filter(|entry| entry.kind == EntryKind::File && !entry.path.contains('/'))
        .cloned()
        .collect();
    materialize(coordinator, workspace, &root_files)?;
    let workspace = workspace
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let fingerprint =
        build_digest::ExecutionFingerprint::remote_worker_sandboxed(Path::new(WORKER_MOUNT));
    // Key with the toolchain this worker will actually run, never the one
    // the client observed when it generated the plan (D-020).
    let mut keyed_action = spec.action.clone();
    keyed_action.toolchain = observe_toolchain(&spec.action, &workspace);
    let key = match input_digests_from_entries(&spec.action, &spec.inputs) {
        Some(inputs) => {
            let candidate =
                build_digest::action_key_candidate(&keyed_action, &inputs, &fingerprint)
                    .map_err(|error| error.to_string())?;
            candidate
                .cache_eligible
                .then(|| candidate.digest.to_string())
        }
        None => None,
    };
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

    // Miss: materialize everything (normalized mtimes), then confirm the
    // files really hash to the key computed from the entries.
    materialize(coordinator, &workspace, &spec.inputs)?;
    let mut prepared =
        local_executor::prepare(&workspace, &spec.action).map_err(|error| error.to_string())?;
    prepared.sandbox_mount = Some(PathBuf::from(WORKER_MOUNT));
    let mut key = key;
    if let Some(expected) = &key {
        let on_disk: Vec<_> = prepared
            .input_digests
            .iter()
            .map(|entry| (entry.path.clone(), entry.digest))
            .collect();
        let check = build_digest::action_key_candidate(&keyed_action, &on_disk, &fingerprint)
            .map_err(|error| error.to_string())?;
        if &check.digest.to_string() != expected {
            eprintln!(
                "ant worker: inputs on disk do not match the job's declared digests; not caching this result"
            );
            key = None;
        }
    }
    let cancellation = CancellationToken::new();
    let executed = with_streamed_output(
        coordinator,
        config,
        job_id,
        &mut prepared,
        &cancellation,
        |prepared| {
            execute_with_task_cache(coordinator, config, &workspace, prepared, &cancellation)
        },
    )?
    .map_err(|error| error.to_string())?;
    let mut outputs = Vec::new();
    if executed.success() {
        outputs = upload_outputs(coordinator, &workspace, &spec.action)?;
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
                    executable: false,
                });
                for entry in build_digest::tree_entries(&path).map_err(|error| error.to_string())? {
                    let relative = format!("{base}/{}", entry.path);
                    if entry.is_dir {
                        entries.push(Entry {
                            path: relative,
                            kind: EntryKind::Dir,
                            digest: None,
                            executable: false,
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
        executable: crate::protocol::is_executable(path),
    })
}
