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
    let actions = graph.ordered_actions().map_err(|error| error.to_string())?;
    let mut remaining: Vec<&str> = actions.iter().map(|action| action.id.as_str()).collect();
    for action in actions {
        remaining.remove(0);
        println!("action={} status=started remote=true", action.id);
        let inputs = collect_inputs(workspace, action)?;
        upload_missing(coordinator, workspace, &inputs)?;
        let spec = JobSpec {
            protocol_version: PROTOCOL_VERSION,
            action: action.clone(),
            inputs,
        };
        let submitted: SubmitResponse = coordinator
            .post_json("/v1/jobs", &spec, "job submission")?
            .ok_or("job submission returned no body")?;
        let status = wait(coordinator, &submitted.job_id, action, poll)?;
        let code = report(coordinator, workspace, action, &status)?;
        if code != 0 {
            for id in &remaining {
                println!("action={id} status=skipped");
            }
            return Ok(code);
        }
    }
    Ok(0)
}

fn rel(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Expand declared inputs into the transferred entry set.
fn collect_inputs(workspace: &Path, action: &Action) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for input in &action.inputs {
        let base = rel(&input.path);
        let path = workspace.join(&input.path);
        match input.kind {
            InputKind::File => {
                let digest = build_digest::digest_file(&path)
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
                for entry in build_digest::tree_entries(&path)
                    .map_err(|error| format!("input {base}: {error}"))?
                {
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
    for entry in inputs {
        let Some(digest) = &entry.digest else {
            continue;
        };
        if missing.contains(digest) && uploaded.insert(digest.clone()) {
            let bytes = std::fs::read(workspace.join(&entry.path))
                .map_err(|error| format!("input {}: {error}", entry.path))?;
            let sent = coordinator.put_blob(&bytes)?;
            if &sent.to_string() != digest {
                return Err(format!("input {} changed while uploading", entry.path));
            }
        }
    }
    Ok(())
}

fn wait(
    coordinator: &Coordinator,
    job_id: &str,
    action: &Action,
    poll: Duration,
) -> Result<JobStatus, String> {
    // Generous client-side bound: the action's own timeout plus queueing and
    // transfer; without a timeout the client waits until the job finishes.
    let deadline = action
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms) + Duration::from_secs(600));
    loop {
        let status: JobStatus =
            coordinator.get_json(&format!("/v1/jobs/{job_id}"), "job status")?;
        if matches!(status.state, JobState::Completed | JobState::Failed) {
            return Ok(status);
        }
        if deadline.is_some_and(|deadline| Instant::now() > deadline) {
            return Err(format!(
                "job {job_id} did not finish in time (state {:?})",
                status.state
            ));
        }
        std::thread::sleep(poll);
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
    for entry in outputs {
        let target = workspace.join(&entry.path);
        match entry.kind {
            EntryKind::Dir => {
                std::fs::create_dir_all(&target).map_err(|error| error.to_string())?
            }
            EntryKind::File => {
                let bytes = coordinator.get_blob(entry.digest.as_deref().expect("validated"))?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&target, bytes).map_err(|error| error.to_string())?;
                if entry.executable {
                    crate::protocol::make_executable(&target)?;
                }
            }
        }
    }
    Ok(())
}
