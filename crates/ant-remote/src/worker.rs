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
    }
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
            }
        }
    }
    Ok(())
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
