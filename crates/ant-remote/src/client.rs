//! `ant remote run`: execute a plan on remote workers. Actions run one at a
//! time in the plan's deterministic topological order; each action's
//! declared inputs are uploaded (only blobs the coordinator lacks), the job
//! is submitted and awaited, and on success its declared outputs are
//! downloaded, verified against their digests, and written into the local
//! workspace — so later actions (and the user) see them exactly as if the
//! action had run locally.

use crate::http::Coordinator;
use crate::protocol::{
    Entry, EntryKind, JobSpec, JobState, JobStatus, PROTOCOL_VERSION, SubmitResponse,
    validate_entries,
};
use build_action::{Action, InputKind, OutputKind};
use build_graph::BuildGraph;
use std::path::Path;
use std::time::{Duration, Instant};

/// Run the plan remotely; returns the process exit code (0 success, 1 action
/// failure, 124 timeout). Hard errors (transport, protocol) are `Err`.
pub fn run_plan(
    coordinator: &Coordinator,
    graph: &BuildGraph,
    workspace: &Path,
    poll: Duration,
) -> Result<u8, String> {
    run_plan_in(coordinator, graph, workspace, poll, None, None)
}

/// Receives a remote job's tool output as it arrives (D-049).
pub type OutputHandler<'a> = &'a (dyn Fn(&str) + Sync);

/// Like [`run_plan`], asking workers to keep this project's workspace
/// between jobs when `workspace_key` is given (D-048).
pub fn run_plan_in(
    coordinator: &Coordinator,
    graph: &BuildGraph,
    workspace: &Path,
    poll: Duration,
    workspace_key: Option<&str>,
    output: Option<OutputHandler<'_>>,
) -> Result<u8, String> {
    run_plan_across(
        &[coordinator],
        graph,
        workspace,
        poll,
        workspace_key,
        output,
    )
}

/// Like [`run_plan_in`], spreading ready actions over several servers
/// (D-060): each goes to the server with the fewest of this build's jobs in
/// flight; every server's own workers then build in parallel.
pub fn run_plan_across(
    coordinators: &[&Coordinator],
    graph: &BuildGraph,
    workspace: &Path,
    poll: Duration,
    workspace_key: Option<&str>,
    output: Option<OutputHandler<'_>>,
) -> Result<u8, String> {
    // Every action whose dependencies are done is submitted at once; the
    // coordinator queues them and each worker leases one at a time, so with
    // N workers up to N actions run in parallel (D-050). Fail-fast: after a
    // failure nothing new is submitted, actions already submitted finish.
    let nodes = graph.ordered_nodes().map_err(|error| error.to_string())?;
    let parallel = nodes.len() > 1;
    let mut pending: Vec<&build_graph::BuildNode> = nodes;
    let mut running: Vec<RunningJob<'_>> = Vec::new();
    let mut done: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut failed: Option<u8> = None;
    loop {
        if failed.is_none() {
            let mut index = 0;
            while index < pending.len() {
                let node = pending[index];
                if !node
                    .dependencies
                    .iter()
                    .all(|dependency| done.contains(dependency.as_str()))
                {
                    index += 1;
                    continue;
                }
                pending.remove(index);
                let action = &node.action;
                println!("action={} status=started remote=true", action.id);
                let inputs = collect_inputs(workspace, action)?;
                // The least busy server takes the next ready action (D-060).
                let server = (0..coordinators.len())
                    .min_by_key(|index| running.iter().filter(|job| job.server == *index).count())
                    .ok_or("no servers to build on")?;
                let coordinator = coordinators[server];
                upload_missing(coordinator, workspace, &inputs)?;
                let spec = JobSpec {
                    protocol_version: PROTOCOL_VERSION,
                    action: action.clone(),
                    inputs,
                    workspace_key: workspace_key.map(str::to_owned),
                };
                let submitted: SubmitResponse = coordinator
                    .post_json("/v1/jobs", &spec, "job submission")?
                    .ok_or("job submission returned no body")?;
                running.push(RunningJob {
                    server,
                    action,
                    job_id: submitted.job_id,
                    // Generous client-side bound: the action's own timeout
                    // plus queueing and transfer; none means wait.
                    deadline: action.timeout_ms.map(|ms| {
                        Instant::now() + Duration::from_millis(ms) + Duration::from_secs(600)
                    }),
                    log: output.map(|handler| LogFollower {
                        handler,
                        next: 0,
                        truncated: false,
                        prefix: parallel.then(|| format!("[{}] ", action.id)),
                        partial: String::new(),
                    }),
                });
            }
        }
        if running.is_empty() {
            break;
        }
        std::thread::sleep(poll);
        let mut index = 0;
        while index < running.len() {
            let job = &mut running[index];
            let coordinator = coordinators[job.server];
            let status: JobStatus =
                coordinator.get_json(&format!("/v1/jobs/{}", job.job_id), "job status")?;
            if let Some(follower) = &mut job.log
                && !follower.follow(coordinator, &job.job_id)
            {
                job.log = None;
            }
            if !matches!(status.state, JobState::Completed | JobState::Failed) {
                if job
                    .deadline
                    .is_some_and(|deadline| Instant::now() > deadline)
                {
                    return Err(format!(
                        "job {} did not finish in time (state {:?})",
                        job.job_id, status.state
                    ));
                }
                index += 1;
                continue;
            }
            let job = running.remove(index);
            if let Some(mut follower) = job.log {
                follower.finish();
            }
            let code = report(coordinators[job.server], workspace, job.action, &status)?;
            if code == 0 {
                done.insert(job.action.id.as_str());
            } else {
                failed.get_or_insert(code);
            }
        }
        // Fail fast (D-054): stop what is still queued or running instead
        // of letting it finish for nothing.
        if failed.is_some() && !running.is_empty() {
            for job in running.drain(..) {
                let path = format!("/v1/jobs/{}/cancel", job.job_id);
                let _ = coordinators[job.server].post_json::<_, serde_json::Value>(
                    &path,
                    &serde_json::json!({}),
                    "cancel",
                );
                println!(
                    "action={} status=cancelled remote_job={}",
                    job.action.id, job.job_id
                );
            }
        }
    }
    if let Some(code) = failed {
        for node in &pending {
            println!("action={} status=skipped", node.action.id);
        }
        return Ok(code);
    }
    if let Some(node) = pending.first() {
        return Err(format!(
            "action {} never became ready (unfinished dependencies)",
            node.action.id
        ));
    }
    Ok(0)
}

/// A submitted job the client is waiting for.
struct RunningJob<'a> {
    /// Index into the servers this build uses.
    server: usize,
    action: &'a Action,
    job_id: String,
    deadline: Option<Instant>,
    log: Option<LogFollower<'a>>,
}

fn rel(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Expand declared inputs into the transferred entry set.
fn collect_inputs(workspace: &Path, action: &Action) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    // Re-read only files whose stamp changed since the last run (D-027).
    let cache = workspace
        .canonicalize()
        .ok()
        .and_then(|root| build_digest::workspace_cache(&root));
    for input in &action.inputs {
        let base = rel(&input.path);
        let path = workspace.join(&input.path);
        match input.kind {
            InputKind::File => {
                let digest = match &cache {
                    Some(cache) => build_digest::digest_file_cached(&path, false, cache),
                    None => build_digest::digest_file(&path),
                }
                .map_err(|error| format!("input {base}: {error}"))?;
                entries.push(Entry {
                    path: base,
                    kind: EntryKind::File,
                    digest: Some(digest.to_string()),
                    executable: crate::protocol::is_executable(&path),
                });
            }
            InputKind::Tree => {
                entries.push(Entry {
                    path: base.clone(),
                    kind: EntryKind::Dir,
                    digest: None,
                    executable: false,
                });
                let tree = match &cache {
                    Some(cache) => build_digest::tree_entries_cached(&path, false, cache),
                    None => build_digest::tree_entries(&path),
                }
                .map_err(|error| format!("input {base}: {error}"))?;
                for entry in tree {
                    let path = format!("{base}/{}", entry.path);
                    entries.push(match entry.digest {
                        Some(digest) if !entry.is_dir => Entry {
                            executable: crate::protocol::is_executable(
                                &workspace.join(&input.path).join(&entry.path),
                            ),
                            path,
                            kind: EntryKind::File,
                            digest: Some(digest.to_string()),
                        },
                        _ => Entry {
                            path,
                            kind: EntryKind::Dir,
                            digest: None,
                            executable: false,
                        },
                    });
                }
            }
        }
    }
    validate_entries(&entries)?;
    Ok(entries)
}

fn upload_missing(
    coordinator: &Coordinator,
    workspace: &Path,
    inputs: &[Entry],
) -> Result<(), String> {
    let digests: Vec<String> = inputs
        .iter()
        .filter_map(|entry| entry.digest.clone())
        .collect();
    let missing: std::collections::HashSet<String> =
        coordinator.missing(digests)?.into_iter().collect();
    let mut uploaded = std::collections::HashSet::new();
    let mut blobs = Vec::new();
    for entry in inputs {
        let Some(digest) = &entry.digest else {
            continue;
        };
        if missing.contains(digest) && uploaded.insert(digest.clone()) {
            let bytes = std::fs::read(workspace.join(&entry.path))
                .map_err(|error| format!("input {}: {error}", entry.path))?;
            if &crate::blobs::digest_bytes(&bytes).to_string() != digest {
                return Err(format!("input {} changed while uploading", entry.path));
            }
            blobs.push(bytes);
        }
    }
    // Many small files per request (D-023).
    coordinator.put_blobs(&blobs)
}

/// Prints a job's output as it grows (D-049). With several jobs running at
/// once each line carries the action's id (D-050).
struct LogFollower<'a> {
    handler: OutputHandler<'a>,
    next: u64,
    truncated: bool,
    prefix: Option<String>,
    partial: String,
}

impl LogFollower<'_> {
    /// `false` when the coordinator cannot serve output (an older one):
    /// stop asking, the build goes on.
    fn follow(&mut self, coordinator: &Coordinator, job_id: &str) -> bool {
        let Ok(log) = coordinator.get_json::<crate::protocol::JobLog>(
            &format!("/v1/jobs/{job_id}/log?from={}", self.next),
            "job output",
        ) else {
            return false;
        };
        if !log.text.is_empty() {
            self.emit(&log.text);
        }
        if log.truncated && !self.truncated {
            self.truncated = true;
            self.emit("\n[ant: output truncated on the server; the build continues]\n");
        }
        self.next = log.next;
        true
    }

    fn emit(&mut self, text: &str) {
        let Some(prefix) = &self.prefix else {
            (self.handler)(text);
            return;
        };
        self.partial.push_str(text);
        let Some(end) = self.partial.rfind('\n') else {
            return;
        };
        let complete: String = self.partial.drain(..=end).collect();
        let mut out = String::with_capacity(complete.len() + prefix.len() * 4);
        for line in complete.split_inclusive('\n') {
            out.push_str(prefix);
            out.push_str(line);
        }
        (self.handler)(&out);
    }

    /// The last line, when the tool's output did not end with a newline.
    fn finish(&mut self) {
        if self.prefix.is_some() && !self.partial.is_empty() {
            self.emit("\n");
        }
    }
}

fn report(
    coordinator: &Coordinator,
    workspace: &Path,
    action: &Action,
    status: &JobStatus,
) -> Result<u8, String> {
    let id = &action.id;
    let Some(result) = &status.result else {
        println!(
            "action={id} status=failed remote_job={} error={}",
            status.job_id,
            status
                .error
                .as_deref()
                .unwrap_or("job failed without a result")
        );
        return Ok(1);
    };
    if let Some(error) = &result.error {
        println!(
            "action={id} status=failed remote_job={} worker={} error={error}",
            status.job_id, result.worker_id
        );
        return Ok(1);
    }
    if result.timed_out {
        println!(
            "action={id} status=timed_out remote_job={} worker={}",
            status.job_id, result.worker_id
        );
        return Ok(124);
    }
    let exit = result
        .exit_code
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    if !result.succeeded() {
        println!(
            "action={id} status=failed exit_code={exit} remote_job={} worker={}",
            status.job_id, result.worker_id
        );
        return Ok(1);
    }
    install_outputs(coordinator, workspace, action, &result.outputs)?;
    let status_word = if result.reused { "reused" } else { "succeeded" };
    println!(
        "action={id} status={status_word} exit_code={exit} remote_job={} worker={}",
        status.job_id, result.worker_id
    );
    for entry in &result.outputs {
        if let Some(digest) = &entry.digest {
            println!("output={} digest={digest}", entry.path);
        }
    }
    Ok(0)
}

/// Write remote outputs into the workspace: every path must be safe and lie
/// under one of the action's declared outputs, and every blob is verified.
fn install_outputs(
    coordinator: &Coordinator,
    workspace: &Path,
    action: &Action,
    outputs: &[Entry],
) -> Result<(), String> {
    validate_entries(outputs)?;
    let declared: Vec<(String, OutputKind)> = action
        .outputs
        .iter()
        .map(|o| (rel(&o.path), o.kind))
        .collect();
    for entry in outputs {
        let covered = declared.iter().any(|(path, kind)| match kind {
            OutputKind::File => entry.path == *path,
            OutputKind::Tree => entry.path == *path || entry.path.starts_with(&format!("{path}/")),
        });
        if !covered {
            return Err(format!(
                "worker returned undeclared output '{}'",
                entry.path
            ));
        }
    }
    // Replace declared outputs wholesale so stale files cannot linger.
    for (path, kind) in &declared {
        let target = workspace.join(path);
        match kind {
            OutputKind::File => {
                let _ = std::fs::remove_file(&target);
            }
            OutputKind::Tree => {
                let _ = std::fs::remove_dir_all(&target);
            }
        }
    }
    // Fetch every output file in batches, verified, before writing (D-023).
    let file_entries: Vec<&Entry> = outputs
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .collect();
    let digests: Vec<String> = file_entries
        .iter()
        .map(|entry| entry.digest.clone().expect("validated"))
        .collect();
    let blobs = coordinator.get_blobs(&digests)?;
    for entry in outputs.iter().filter(|e| e.kind == EntryKind::Dir) {
        std::fs::create_dir_all(workspace.join(&entry.path)).map_err(|error| error.to_string())?;
    }
    for (entry, bytes) in file_entries.into_iter().zip(blobs) {
        let target = workspace.join(&entry.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(&target, bytes).map_err(|error| error.to_string())?;
        if entry.executable {
            crate::protocol::make_executable(&target)?;
        }
    }
    Ok(())
}
