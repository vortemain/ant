//! Direct process execution for a local build action.
//!
//! This executor validates paths and runs the command in the requested
//! workspace directory. It is NOT a sandbox: the child process can access the
//! user's normal OS permissions and filesystem. Never use this component as a
//! security boundary for untrusted multi-tenant workloads.

use build_action::{Action, InputKind, OutputKind, ValidationError};
use build_digest::{
    ContentDigest, DigestError, digest_file, digest_file_with_metadata, digest_tree,
    digest_tree_with_metadata,
};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum ExecutionError {
    InvalidAction(ValidationError),
    WorkspaceUnavailable {
        path: PathBuf,
        source: std::io::Error,
    },
    WorkingDirectoryEscapesWorkspace(PathBuf),
    InputMissing(PathBuf),
    InputTypeMismatch(PathBuf),
    InputDigest(DigestError),
    InputChangedDuringExecution(PathBuf),
    OutputPathEscapesWorkspace(PathBuf),
    OutputMissing(PathBuf),
    OutputTypeMismatch(PathBuf),
    OutputDigest(DigestError),
    Cancelled,
    Spawn(std::io::Error),
    /// The restricted-token sandbox could not be set up (or is unavailable on
    /// this platform). The sandbox fails closed: this error means the action
    /// was NOT executed unsandboxed.
    Sandbox(String),
    /// A requested resource limit could not be applied; the process was
    /// not run (fail closed).
    ResourceLimit(String),
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAction(e) => write!(f, "invalid action: {e:?}"),
            Self::WorkspaceUnavailable { path, source } => {
                write!(f, "cannot access workspace {}: {source}", path.display())
            }
            Self::WorkingDirectoryEscapesWorkspace(path) => write!(
                f,
                "working directory resolves outside the workspace: {}",
                path.display()
            ),
            Self::InputMissing(path) => {
                write!(f, "declared input does not exist: {}", path.display())
            }
            Self::InputTypeMismatch(path) => {
                write!(f, "declared input has the wrong type: {}", path.display())
            }
            Self::InputDigest(error) => write!(f, "cannot digest declared input: {error}"),
            Self::InputChangedDuringExecution(path) => write!(
                f,
                "declared input changed while action ran: {}",
                path.display()
            ),
            Self::OutputPathEscapesWorkspace(path) => write!(
                f,
                "declared output resolves outside the workspace: {}",
                path.display()
            ),
            Self::OutputMissing(path) => {
                write!(f, "declared output was not created: {}", path.display())
            }
            Self::OutputTypeMismatch(path) => {
                write!(f, "declared output has the wrong type: {}", path.display())
            }
            Self::OutputDigest(error) => write!(f, "cannot digest declared output: {error}"),
            Self::Cancelled => write!(f, "action cancelled before process start"),
            Self::Spawn(e) => write!(f, "could not start action process: {e}"),
            Self::Sandbox(message) => write!(f, "sandbox unavailable: {message}"),
            Self::ResourceLimit(message) => write!(f, "resource limit unavailable: {message}"),
        }
    }
}

impl std::error::Error for ExecutionError {}

#[derive(Debug)]
pub struct ExecutionResult {
    pub action_id: String,
    pub status: ExitStatus,
    pub cancelled: bool,
    pub timed_out: bool,
    pub input_digests: Vec<ActionContentDigest>,
    pub output_digests: Vec<ActionContentDigest>,
    /// The job-wide committed-memory cap applied to this run, if any.
    pub memory_limit_bytes: Option<u64>,
    /// Measured peak committed memory of the process tree, when it was
    /// tracked by a job object (Windows); `None` when not measured.
    pub peak_memory_bytes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ActionContentDigest {
    pub path: PathBuf,
    pub digest: ContentDigest,
}

impl ExecutionResult {
    pub fn success(&self) -> bool {
        !self.cancelled && !self.timed_out && self.status.success()
    }
}

/// Thread-safe cooperative cancellation signal for a running local process.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    cancel_file: Option<PathBuf>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel when either `cancel()` is called or this marker file exists.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_file: Some(path.into()),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
            || self.cancel_file.as_ref().is_some_and(|path| path.exists())
    }
}

/// Execute a declared action directly on the current machine.
///
/// Standard output and error are inherited, so users see live process output.
/// The child environment is cleared and then populated with a small platform
/// allowlist plus action-declared values. This is not isolation: the child runs
/// with the caller's OS identity and can still inspect files and processes
/// permitted to that identity.
pub fn execute(workspace: &Path, action: &Action) -> Result<ExecutionResult, ExecutionError> {
    execute_cancellable(workspace, action, &CancellationToken::new())
}

/// The verified pre-execution state of one action: the canonicalized
/// workspace, plus digests of every declared input. Produced by `prepare` or
/// `prepare_with_metadata`; `execute_prepared` consumes it so input digesting
/// happens exactly once before any process starts.
#[derive(Clone, Debug)]
pub struct PreparedAction {
    pub action: Action,
    /// Canonicalized workspace the action was prepared against.
    pub workspace: PathBuf,
    pub input_digests: Vec<ActionContentDigest>,
    /// Whether the digests above include filesystem metadata. `execute_prepared`
    /// re-verifies inputs in the same mode so the comparison is meaningful.
    pub metadata_fingerprinting: bool,
    /// Job-wide committed-memory cap to enforce at execution, in bytes.
    /// `None` (the default) runs uncapped; see [`PreparedAction::enforce_memory_limit`].
    pub memory_limit_bytes: Option<u64>,
}

impl PreparedAction {
    /// Turn the action's declared `resources.memory_mb` into an enforced
    /// job-wide cap for execution (Windows job objects). Actions that declare
    /// no memory stay uncapped. Execution fails closed — the process never
    /// runs uncapped — when a cap is requested but cannot be applied,
    /// including on platforms without job objects.
    pub fn enforce_memory_limit(&mut self) {
        self.memory_limit_bytes = self
            .action
            .resource_request()
            .memory_mb
            .map(|mib| mib.saturating_mul(1024 * 1024));
    }
}

/// Validate an action and verify all declared inputs, computing their
/// content digests. No process is started; cancellation is not consulted
/// until `execute_prepared`.
pub fn prepare(workspace: &Path, action: &Action) -> Result<PreparedAction, ExecutionError> {
    prepare_with_mode(workspace, action, false)
}

/// Like `prepare`, but declared input digests include filesystem metadata
/// (size and last-modification time). Use this mode only together with the
/// matching `ExecutionFingerprint::local_metadata_fingerprinting` when
/// reporting action keys: digests and key must be produced in the same mode.
pub fn prepare_with_metadata(
    workspace: &Path,
    action: &Action,
) -> Result<PreparedAction, ExecutionError> {
    prepare_with_mode(workspace, action, true)
}

fn prepare_with_mode(
    workspace: &Path,
    action: &Action,
    metadata_fingerprinting: bool,
) -> Result<PreparedAction, ExecutionError> {
    action.validate().map_err(ExecutionError::InvalidAction)?;
    let workspace =
        workspace
            .canonicalize()
            .map_err(|source| ExecutionError::WorkspaceUnavailable {
                path: workspace.to_path_buf(),
                source,
            })?;
    let requested_cwd = workspace.join(&action.working_directory);
    let cwd =
        requested_cwd
            .canonicalize()
            .map_err(|source| ExecutionError::WorkspaceUnavailable {
                path: requested_cwd.clone(),
                source,
            })?;
    if !cwd.starts_with(&workspace) {
        return Err(ExecutionError::WorkingDirectoryEscapesWorkspace(cwd));
    }
    let mut input_digests = Vec::with_capacity(action.inputs.len());
    for input in &action.inputs {
        let path = workspace.join(&input.path);
        let resolved = path
            .canonicalize()
            .map_err(|_| ExecutionError::InputMissing(path.clone()))?;
        reject_symlink_components(&workspace, &input.path).map_err(ExecutionError::InputDigest)?;
        if !resolved.starts_with(&workspace) {
            return Err(ExecutionError::WorkingDirectoryEscapesWorkspace(resolved));
        }
        let correct_type = match input.kind {
            InputKind::File => resolved.is_file(),
            InputKind::Tree => resolved.is_dir(),
        };
        if !correct_type {
            return Err(ExecutionError::InputTypeMismatch(path));
        }
        let digest = digest_declared_path(&resolved, input.kind, metadata_fingerprinting)
            .map_err(ExecutionError::InputDigest)?;
        input_digests.push(ActionContentDigest {
            path: input.path.clone(),
            digest,
        });
    }
    for output in &action.outputs {
        workspace_path(&workspace, &output.path)
            .map_err(|_| ExecutionError::OutputPathEscapesWorkspace(output.path.clone()))?;
        reject_existing_symlink_components(&workspace, &output.path)
            .map_err(ExecutionError::OutputDigest)?;
    }
    Ok(PreparedAction {
        action: action.clone(),
        workspace,
        input_digests,
        metadata_fingerprinting,
        memory_limit_bytes: None,
    })
}

/// Execute an action prepared by `prepare` or `prepare_with_metadata` and
/// terminate its direct child process when cancellation is requested. Inputs
/// are re-verified in the digest mode chosen at preparation time. Declared
/// outputs are digested by content only, so a cache can verify a restored
/// output without reproducing timestamps. On Windows, descendants are also
/// terminated through a kill-on-close job object; on other platforms
/// descendant cleanup remains platform-dependent and is not guaranteed.
pub fn execute_prepared(
    prepared: &PreparedAction,
    cancellation: &CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    let action = &prepared.action;
    let workspace = &prepared.workspace;
    if cancellation.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let requested_cwd = workspace.join(&action.working_directory);
    let cwd =
        requested_cwd
            .canonicalize()
            .map_err(|source| ExecutionError::WorkspaceUnavailable {
                path: requested_cwd.clone(),
                source,
            })?;
    let mut command = Command::new(&action.argv[0]);
    command.args(&action.argv[1..]).current_dir(cwd).env_clear();
    command.envs(build_digest::local_runtime_environment());
    command.envs(action.environment.iter().map(|(key, value)| (key, value)));
    #[cfg(not(windows))]
    if prepared.memory_limit_bytes.is_some() {
        return Err(ExecutionError::ResourceLimit(
            "memory limits are enforced with Windows job objects only".to_owned(),
        ));
    }
    #[cfg(windows)]
    if prepared.memory_limit_bytes.is_some() {
        use std::os::windows::process::CommandExt as _;
        // CREATE_SUSPENDED: the cap is applied before the first instruction.
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_SUSPENDED);
    }
    let mut child = command.spawn().map_err(ExecutionError::Spawn)?;
    // Windows only: descendants are placed in a kill-on-close job object so a
    // cancelled or timed-out action cannot leave orphaned grandchildren. The
    // guard must stay alive until the child has exited.
    #[cfg(windows)]
    let job_guard = match prepared.memory_limit_bytes {
        None => attach_child_to_job(&child),
        Some(limit) => match attach_capped_and_resume(&child, limit) {
            Ok(job) => Some(job),
            Err(message) => {
                // The process is still suspended and never ran user code.
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExecutionError::ResourceLimit(message));
            }
        },
    };
    let timeout = action.timeout_ms.map(Duration::from_millis);
    let (status, cancelled, timed_out) = wait_for_child(&mut child, cancellation, timeout)?;
    #[cfg(windows)]
    let peak_memory_bytes = job_guard
        .as_ref()
        .and_then(|job| ant_sandbox_win::job::peak_job_memory(job).ok());
    #[cfg(not(windows))]
    let peak_memory_bytes = None;
    let mut output_digests = Vec::new();
    if status.success() && !cancelled && !timed_out {
        output_digests = verify_inputs_and_digest_outputs(prepared)?;
    }
    Ok(ExecutionResult {
        action_id: action.id.clone(),
        status,
        cancelled,
        timed_out,
        input_digests: prepared.input_digests.clone(),
        output_digests,
        memory_limit_bytes: prepared.memory_limit_bytes,
        peak_memory_bytes,
    })
}

/// Re-verify declared inputs in the prepared digest mode and digest declared
/// outputs by content only. Shared by the direct and sandboxed run paths so
/// both enforce the same input-unchanged and output-coverage contracts.
fn verify_inputs_and_digest_outputs(
    prepared: &PreparedAction,
) -> Result<Vec<ActionContentDigest>, ExecutionError> {
    let action = &prepared.action;
    let workspace = &prepared.workspace;
    let mut output_digests = Vec::with_capacity(action.outputs.len());
    for (input, before) in action.inputs.iter().zip(&prepared.input_digests) {
        let path = workspace.join(&before.path);
        reject_symlink_components(workspace, &input.path).map_err(ExecutionError::InputDigest)?;
        let digest = digest_declared_path(&path, input.kind, prepared.metadata_fingerprinting)
            .map_err(ExecutionError::InputDigest)?;
        if digest != before.digest {
            return Err(ExecutionError::InputChangedDuringExecution(
                before.path.clone(),
            ));
        }
    }
    for output in &action.outputs {
        let path = workspace.join(&output.path);
        let resolved = path
            .canonicalize()
            .map_err(|_| ExecutionError::OutputMissing(path.clone()))?;
        reject_symlink_components(workspace, &output.path).map_err(ExecutionError::OutputDigest)?;
        if !resolved.starts_with(workspace) {
            return Err(ExecutionError::OutputPathEscapesWorkspace(resolved));
        }
        let correct_type = match output.kind {
            OutputKind::File => resolved.is_file(),
            OutputKind::Tree => resolved.is_dir(),
        };
        if !correct_type {
            return Err(ExecutionError::OutputTypeMismatch(path));
        }
        let digest = match output.kind {
            OutputKind::File => digest_file(&resolved),
            OutputKind::Tree => digest_tree(&resolved),
        }
        .map_err(ExecutionError::OutputDigest)?;
        output_digests.push(ActionContentDigest {
            path: output.path.clone(),
            digest,
        });
    }
    Ok(output_digests)
}

/// Prepare and then execute an action in one step.
///
/// This is the local executor's default run mode: declared inputs are digested
/// with filesystem metadata (`prepare_with_metadata`), which matches the
/// `ExecutionFingerprint::local_metadata_fingerprinting` used when reporting
/// action keys. For explicit digest-mode control use `prepare`/`execute_prepared`.
pub fn execute_cancellable(
    workspace: &Path,
    action: &Action,
    cancellation: &CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    let prepared = prepare_with_metadata(workspace, action)?;
    execute_prepared(&prepared, cancellation)
}

/// Execute an action inside the Windows restricted-token filesystem sandbox.
///
/// Convenience wrapper: prepares with metadata fingerprints (matching
/// `ExecutionFingerprint::local_sandboxed`) and delegates to
/// `execute_sandboxed_prepared`. Callers that already hold a `PreparedAction`
/// (for example the cache-aware run path, which computes the action key from
/// the pre-run digests) should call `execute_sandboxed_prepared` directly so
/// inputs are digested exactly once.
///
/// Non-Windows: fails closed with `ExecutionError::Sandbox`. The sandbox is a
/// Windows-only capability (decision D-007); there is no fallback execution.
#[cfg(windows)]
pub fn execute_sandboxed_cancellable(
    workspace: &Path,
    action: &Action,
    cancellation: &CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    let prepared = prepare_with_metadata(workspace, action)?;
    execute_sandboxed_prepared(&prepared, cancellation)
}

/// Execute a prepared action inside the Windows restricted-token filesystem
/// sandbox.
///
/// The run path mirrors `execute_prepared`: the sandbox is prepared (fail
/// closed — any error here means the action is NOT executed unsandboxed), the
/// confined process is spawned with the platform allowlist plus the action's
/// declared environment, and a 40ms poll loop watches for cancellation or
/// timeout. On a clean exit the same input-unchanged and output-coverage
/// checks as the direct path run.
#[cfg(windows)]
pub fn execute_sandboxed_prepared(
    prepared: &PreparedAction,
    cancellation: &CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    use std::os::windows::process::ExitStatusExt as _;

    let action = &prepared.action;
    if cancellation.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let sandbox = ant_sandbox_win::Sandbox::prepare(&prepared.workspace)
        .map_err(|error| ExecutionError::Sandbox(error.to_string()))?;
    let requested_cwd = prepared.workspace.join(&action.working_directory);
    let cwd =
        requested_cwd
            .canonicalize()
            .map_err(|source| ExecutionError::WorkspaceUnavailable {
                path: requested_cwd.clone(),
                source,
            })?;
    // The canonicalized path carries a `\\?\` verbatim prefix on Windows.
    // That form is correct for the SID derivation and ACL grant (which use
    // the canonical bytes), but `cmd.exe` refuses verbatim paths as a cwd
    // ("UNC paths are not supported") and falls back to the Windows
    // directory, which the restricted token cannot write. Strip the prefix
    // for the spawn cwd only; the sandbox's own `workspace` field stays
    // canonical.
    let cwd = strip_verbatim_prefix(&cwd);

    // Build the confined environment: Ant's platform allowlist plus the
    // action's declared values. The values are not secret (the cache key
    // digests declared env), so passing them as plain strings through the
    // restricted token is consistent with the direct run path.
    let mut environment: Vec<(String, String)> = build_digest::local_runtime_environment()
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    environment.extend(action.environment.iter().cloned());

    let mut child = sandbox
        .spawn_with_memory_limit(
            &action.argv,
            &cwd,
            &environment,
            prepared.memory_limit_bytes,
        )
        .map_err(|error| match prepared.memory_limit_bytes {
            Some(_) => ExecutionError::ResourceLimit(error.to_string()),
            None => ExecutionError::Sandbox(error.to_string()),
        })?;
    let timeout = action.timeout_ms.map(Duration::from_millis);
    let start = Instant::now();
    let (code, cancelled, timed_out) = loop {
        if cancellation.is_cancelled() {
            child.terminate();
            // Drain the exit code so the result reports what the process
            // actually returned (terminating with 1 is the convention here).
            let code = loop {
                match child
                    .try_wait()
                    .map_err(|error| ExecutionError::Sandbox(error.to_string()))?
                {
                    Some(c) => break c,
                    None => std::thread::sleep(Duration::from_millis(40)),
                }
            };
            break (code, true, false);
        }
        if let Some(code) = child
            .try_wait()
            .map_err(|error| ExecutionError::Sandbox(error.to_string()))?
        {
            break (code, false, false);
        }
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            child.terminate();
            let code = loop {
                match child
                    .try_wait()
                    .map_err(|error| ExecutionError::Sandbox(error.to_string()))?
                {
                    Some(c) => break c,
                    None => std::thread::sleep(Duration::from_millis(40)),
                }
            };
            break (code, false, true);
        }
        std::thread::sleep(Duration::from_millis(40));
    };

    let status = ExitStatus::from_raw(code);
    let mut output_digests = Vec::new();
    if status.success() && !cancelled && !timed_out {
        output_digests = verify_inputs_and_digest_outputs(prepared)?;
    }
    Ok(ExecutionResult {
        action_id: action.id.clone(),
        status,
        cancelled,
        timed_out,
        input_digests: prepared.input_digests.clone(),
        output_digests,
        memory_limit_bytes: prepared.memory_limit_bytes,
        peak_memory_bytes: child.peak_memory_bytes(),
    })
}

/// The dedicated, firewall-scoped sandbox account an action runs as for
/// network isolation (decision D-013), plus the path of Ant's own runner
/// executable that is launched as that account.
#[cfg(windows)]
pub struct IsolatedAccount {
    pub user: String,
    pub user_sid: String,
    pub password: ant_sandbox_win::Secret,
    pub runner: PathBuf,
}

/// Placeholder so callers compile on every platform; network isolation is
/// Windows-only and fails closed elsewhere.
#[cfg(not(windows))]
pub struct IsolatedAccount {
    _private: (),
}

/// Execute a prepared action write-confined *and* network-isolated: the
/// workspace is granted to the sandbox account (and the per-workspace
/// capability SID), then Ant's runner is started as that account via
/// Secondary Logon — suspended, joined to a kill-on-close job (plus the
/// optional memory cap), then resumed. The runner restricts its own token and
/// runs the action. The firewall rules scoped to the account apply to the
/// runner and every descendant. Cancellation/timeout terminate the job;
/// inputs and outputs are verified exactly as on the other paths.
#[cfg(windows)]
pub fn execute_network_isolated_prepared(
    prepared: &PreparedAction,
    cancellation: &CancellationToken,
    account: &IsolatedAccount,
) -> Result<ExecutionResult, ExecutionError> {
    use std::os::windows::process::ExitStatusExt as _;

    let action = &prepared.action;
    if cancellation.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    // Workspace owner applies both grants; the runner cannot change the DACL.
    ant_sandbox_win::Sandbox::prepare(&prepared.workspace)
        .map_err(|error| ExecutionError::Sandbox(error.to_string()))?;
    ant_sandbox_win::Sandbox::grant_workspace_to_sid(&prepared.workspace, &account.user_sid)
        .map_err(|error| ExecutionError::Sandbox(error.to_string()))?;
    let requested_cwd = prepared.workspace.join(&action.working_directory);
    let cwd =
        requested_cwd
            .canonicalize()
            .map_err(|source| ExecutionError::WorkspaceUnavailable {
                path: requested_cwd.clone(),
                source,
            })?;
    let cwd = strip_verbatim_prefix(&cwd);
    let workspace = strip_verbatim_prefix(&prepared.workspace);

    let mut environment: Vec<(String, String)> = build_digest::local_runtime_environment()
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    environment.extend(action.environment.iter().cloned());

    let mut runner_argv = vec![
        account.runner.to_string_lossy().into_owned(),
        "__sandbox-runner".to_owned(),
        "--workspace".to_owned(),
        workspace.to_string_lossy().into_owned(),
        "--".to_owned(),
    ];
    runner_argv.extend(action.argv.iter().cloned());
    let mut child = ant_sandbox_win::spawn_as_user(
        &account.user,
        &account.password,
        &runner_argv,
        &cwd,
        &environment,
        prepared.memory_limit_bytes,
    )
    .map_err(|error| match prepared.memory_limit_bytes {
        Some(_) => ExecutionError::ResourceLimit(error.to_string()),
        None => ExecutionError::Sandbox(error.to_string()),
    })?;
    let timeout = action.timeout_ms.map(Duration::from_millis);
    let (code, cancelled, timed_out) = wait_sandboxed_child(&mut child, cancellation, timeout)?;
    let status = ExitStatus::from_raw(code);
    let mut output_digests = Vec::new();
    if status.success() && !cancelled && !timed_out {
        output_digests = verify_inputs_and_digest_outputs(prepared)?;
    }
    Ok(ExecutionResult {
        action_id: action.id.clone(),
        status,
        cancelled,
        timed_out,
        input_digests: prepared.input_digests.clone(),
        output_digests,
        memory_limit_bytes: prepared.memory_limit_bytes,
        peak_memory_bytes: child.peak_memory_bytes(),
    })
}

/// Non-Windows stub: network isolation is Windows-only; fails closed.
#[cfg(not(windows))]
pub fn execute_network_isolated_prepared(
    _prepared: &PreparedAction,
    _cancellation: &CancellationToken,
    _account: &IsolatedAccount,
) -> Result<ExecutionResult, ExecutionError> {
    Err(ExecutionError::Sandbox(
        "network isolation is implemented for Windows only".to_owned(),
    ))
}

/// Poll a sandbox child every 40ms for exit, cancellation, or timeout;
/// returns `(exit_code, cancelled, timed_out)`.
#[cfg(windows)]
fn wait_sandboxed_child(
    child: &mut ant_sandbox_win::Child,
    cancellation: &CancellationToken,
    timeout: Option<Duration>,
) -> Result<(u32, bool, bool), ExecutionError> {
    let start = Instant::now();
    let drain = |child: &mut ant_sandbox_win::Child| -> Result<u32, ExecutionError> {
        loop {
            match child
                .try_wait()
                .map_err(|error| ExecutionError::Sandbox(error.to_string()))?
            {
                Some(code) => return Ok(code),
                None => std::thread::sleep(Duration::from_millis(40)),
            }
        }
    };
    loop {
        if cancellation.is_cancelled() {
            child.terminate();
            return Ok((drain(child)?, true, false));
        }
        if let Some(code) = child
            .try_wait()
            .map_err(|error| ExecutionError::Sandbox(error.to_string()))?
        {
            return Ok((code, false, false));
        }
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            child.terminate();
            return Ok((drain(child)?, false, true));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// Non-Windows stub: the restricted-token sandbox is Windows-only (decision
/// D-007). Fails closed rather than executing unsandboxed.
#[cfg(not(windows))]
pub fn execute_sandboxed_cancellable(
    _workspace: &Path,
    _action: &Action,
    _cancellation: &CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    Err(ExecutionError::Sandbox(
        "the restricted-token sandbox is implemented for Windows only".to_owned(),
    ))
}

/// Non-Windows stub for the prepared variant; same fail-closed contract.
#[cfg(not(windows))]
pub fn execute_sandboxed_prepared(
    _prepared: &PreparedAction,
    _cancellation: &CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    Err(ExecutionError::Sandbox(
        "the restricted-token sandbox is implemented for Windows only".to_owned(),
    ))
}

fn wait_for_child(
    child: &mut Child,
    cancellation: &CancellationToken,
    timeout: Option<Duration>,
) -> Result<(ExitStatus, bool, bool), ExecutionError> {
    let start = Instant::now();
    loop {
        if cancellation.is_cancelled() {
            terminate_child(child)?;
            return Ok((child.wait().map_err(ExecutionError::Spawn)?, true, false));
        }
        if let Some(status) = child.try_wait().map_err(ExecutionError::Spawn)? {
            return Ok((status, false, false));
        }
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            terminate_child(child)?;
            return Ok((child.wait().map_err(ExecutionError::Spawn)?, false, true));
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn terminate_child(child: &mut Child) -> Result<(), ExecutionError> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(source) => Err(ExecutionError::Spawn(source)),
    }
}

/// Strip the Windows `\\?\` verbatim prefix that `Path::canonicalize` adds.
/// `cmd.exe` rejects verbatim paths as a current directory ("UNC paths are not
/// supported"); the sandboxed spawn passes this stripped path as the child cwd.
#[cfg(windows)]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else {
        path.to_path_buf()
    }
}

/// Attach a `CREATE_SUSPENDED` child to a kill-on-close job, cap the job's
/// committed memory, and only then resume it. Any failure is returned so the
/// caller can kill the never-resumed process (fail closed).
#[cfg(windows)]
fn attach_capped_and_resume(child: &Child, limit_bytes: u64) -> Result<win32job::Job, String> {
    use std::os::windows::io::AsRawHandle;
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&info)
        .map_err(|error| format!("cannot create job object: {error}"))?;
    job.assign_process(child.as_raw_handle() as isize)
        .map_err(|error| format!("cannot assign process to job object: {error}"))?;
    ant_sandbox_win::job::set_job_memory_limit(&job, limit_bytes)
        .map_err(|error| error.to_string())?;
    match ant_sandbox_win::job::resume_process_threads(child.id()) {
        Ok(0) => Err("no thread found to resume after applying the memory limit".to_owned()),
        Ok(_) => Ok(job),
        Err(error) => Err(error.to_string()),
    }
}

/// Attach a freshly spawned child to a kill-on-close Windows job object.
///
/// Every process the child later starts joins the same job. Dropping the
/// returned guard terminates any process still in the job, which closes the
/// gap where cancellation kills the direct child but grandchildren survive.
/// If the child has already exited or the job cannot be created, no guard is
/// returned; execution continues and cleanup is best-effort.
#[cfg(windows)]
fn attach_child_to_job(child: &Child) -> Option<win32job::Job> {
    use std::os::windows::io::AsRawHandle;
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&info).ok()?;
    job.assign_process(child.as_raw_handle() as isize).ok()?;
    Some(job)
}

fn digest_declared_path(
    path: &Path,
    kind: InputKind,
    metadata_fingerprinting: bool,
) -> Result<ContentDigest, DigestError> {
    match (kind, metadata_fingerprinting) {
        (InputKind::File, false) => digest_file(path),
        (InputKind::File, true) => digest_file_with_metadata(path),
        (InputKind::Tree, false) => digest_tree(path),
        (InputKind::Tree, true) => digest_tree_with_metadata(path),
    }
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<(), DigestError> {
    reject_existing_symlink_components(root, relative)?;
    let target = root.join(relative);
    let metadata = std::fs::symlink_metadata(&target).map_err(|source| DigestError::Io {
        path: target.clone(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(target));
    }
    Ok(())
}

fn reject_existing_symlink_components(root: &Path, relative: &Path) -> Result<(), DigestError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if let std::path::Component::Normal(name) = component {
            current.push(name);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(DigestError::Symlink(current));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(source) => {
                    return Err(DigestError::Io {
                        path: current,
                        source,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Resolve a relative workspace path and ensure its nearest existing parent is
/// rooted inside the workspace. Useful for future pre-execution output checks.
pub fn workspace_path(workspace: &Path, relative: &Path) -> Result<PathBuf, ExecutionError> {
    let root = workspace
        .canonicalize()
        .map_err(|source| ExecutionError::WorkspaceUnavailable {
            path: workspace.to_path_buf(),
            source,
        })?;
    let candidate = root.join(relative);
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| ExecutionError::WorkingDirectoryEscapesWorkspace(candidate.clone()))?;
    }
    let resolved =
        ancestor
            .canonicalize()
            .map_err(|source| ExecutionError::WorkspaceUnavailable {
                path: ancestor.to_path_buf(),
                source,
            })?;
    if !resolved.starts_with(&root) {
        return Err(ExecutionError::WorkingDirectoryEscapesWorkspace(resolved));
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_action::{CachePolicy, Input, Output, ToolchainIdentity};
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("ant-exec-{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(relative);
            fs::write(&path, contents).expect("write file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A command that runs for several seconds without producing output.
    fn long_running_command() -> Vec<String> {
        #[cfg(windows)]
        return vec![
            "cmd".to_owned(),
            "/C".to_owned(),
            "ping -n 6 127.0.0.1 >nul".to_owned(),
        ];
        #[cfg(not(windows))]
        return vec!["sh".to_owned(), "-c".to_owned(), "sleep 5".to_owned()];
    }

    /// A command that copies `in.txt` to `out.txt` in its working directory.
    fn copy_command() -> Vec<String> {
        #[cfg(windows)]
        return vec![
            "cmd".to_owned(),
            "/C".to_owned(),
            "copy /Y in.txt out.txt >nul".to_owned(),
        ];
        #[cfg(not(windows))]
        return vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "cp in.txt out.txt".to_owned(),
        ];
    }

    fn action(id: &str, argv: Vec<String>) -> Action {
        Action {
            schema_version: 1,
            id: id.to_owned(),
            argv,
            working_directory: PathBuf::from("."),
            inputs: Vec::new(),
            outputs: Vec::new(),
            environment: Vec::new(),
            cache_policy: CachePolicy::Never,
            toolchain: None,
            timeout_ms: None,
            resources: None,
        }
    }

    #[test]
    fn successful_action_reports_input_and_output_digests() {
        let dir = TempDir::new("success");
        dir.write("in.txt", b"input content");
        let mut action = action("copy-input", copy_command());
        action.inputs.push(Input {
            path: PathBuf::from("in.txt"),
            kind: InputKind::File,
        });
        action.outputs.push(Output {
            path: PathBuf::from("out.txt"),
            kind: OutputKind::File,
        });

        let result = execute(dir.path(), &action).expect("execution succeeds");
        assert!(result.success(), "status {:?}", result.status);
        assert!(!result.cancelled && !result.timed_out);
        assert_eq!(result.input_digests.len(), 1);
        assert_eq!(result.output_digests.len(), 1);
        assert_eq!(
            result.input_digests[0].digest,
            digest_file_with_metadata(&dir.path().join("in.txt")).expect("digest")
        );
        assert_eq!(
            result.output_digests[0].digest,
            digest_file(&dir.path().join("out.txt")).expect("digest")
        );
    }

    #[test]
    fn memory_limit_is_opt_in_and_derived_from_declared_memory() {
        let dir = TempDir::new("mem-optin");
        let mut undeclared =
            prepare(dir.path(), &action("plain", copy_command())).expect("prepare");
        assert_eq!(
            undeclared.memory_limit_bytes, None,
            "no cap unless requested"
        );
        undeclared.enforce_memory_limit();
        assert_eq!(
            undeclared.memory_limit_bytes, None,
            "undeclared memory stays uncapped"
        );

        let mut declared = action("declared", copy_command());
        declared.resources = Some(build_action::ResourceRequest {
            cpu: 1,
            memory_mb: Some(64),
        });
        let mut prepared = prepare(dir.path(), &declared).expect("prepare");
        prepared.enforce_memory_limit();
        assert_eq!(prepared.memory_limit_bytes, Some(64 * 1024 * 1024));
    }

    /// The capped path spawns suspended, applies the cap, and resumes; a
    /// trivial command must run normally and report its measured peak.
    #[cfg(windows)]
    #[test]
    fn capped_execution_resumes_the_child_and_measures_peak() {
        let dir = TempDir::new("mem-capped");
        let mut capped = action(
            "capped",
            vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                "echo ok> out.txt".to_owned(),
            ],
        );
        capped.resources = Some(build_action::ResourceRequest {
            cpu: 1,
            memory_mb: Some(256),
        });
        let mut prepared = prepare(dir.path(), &capped).expect("prepare");
        prepared.enforce_memory_limit();
        let result = execute_prepared(&prepared, &CancellationToken::new()).expect("execute");
        assert!(result.success(), "status {:?}", result.status);
        assert!(
            dir.path().join("out.txt").exists(),
            "the resumed child must actually run"
        );
        assert_eq!(result.memory_limit_bytes, Some(256 * 1024 * 1024));
        let peak = result
            .peak_memory_bytes
            .expect("peak measured through the job");
        assert!(peak > 0 && peak < 256 * 1024 * 1024, "peak {peak}");
    }

    #[test]
    fn declared_environment_reaches_the_child() {
        let dir = TempDir::new("env");
        let mut action = action("env-echo", Vec::new());
        action
            .environment
            .push(("ANT_TEST_VAR".to_owned(), "ant-value".to_owned()));
        #[cfg(windows)]
        {
            action.argv = vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                "echo %ANT_TEST_VAR%> out.txt".to_owned(),
            ];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "echo $ANT_TEST_VAR > out.txt".to_owned(),
            ];
        }
        action.outputs.push(Output {
            path: PathBuf::from("out.txt"),
            kind: OutputKind::File,
        });

        let result = execute(dir.path(), &action).expect("execution succeeds");
        assert!(result.success(), "status {:?}", result.status);
        let content = fs::read_to_string(dir.path().join("out.txt")).expect("output file");
        assert!(
            content.trim().contains("ant-value"),
            "output was {content:?}"
        );
    }

    #[test]
    fn timeout_stops_the_child_and_reports_timed_out() {
        let dir = TempDir::new("timeout");
        let mut action = action("slow", long_running_command());
        action.timeout_ms = Some(400);

        let result = execute(dir.path(), &action).expect("timeout handled as a result");
        assert!(result.timed_out, "expected timeout");
        assert!(!result.cancelled);
        assert!(!result.success());
    }

    #[test]
    fn cancellation_before_start_returns_error() {
        let dir = TempDir::new("pre-cancel");
        let token = CancellationToken::new();
        token.cancel();
        let error = execute_cancellable(
            dir.path(),
            &action(
                "never-starts",
                vec!["cmd".to_owned(), "/C".to_owned(), "rem".to_owned()],
            ),
            &token,
        )
        .unwrap_err();
        assert!(matches!(error, ExecutionError::Cancelled), "got {error:?}");
    }

    #[test]
    fn cancellation_during_execution_kills_the_child() {
        let dir = TempDir::new("mid-cancel");
        let token = CancellationToken::new();
        let cancel_from_thread = token.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            cancel_from_thread.cancel();
        });

        let result = execute_cancellable(
            dir.path(),
            &action("killed-mid-run", long_running_command()),
            &token,
        )
        .expect("cancellation handled as a result");
        assert!(result.cancelled, "expected cancellation");
        assert!(!result.timed_out);
        assert!(!result.success());
    }

    #[test]
    fn cancel_file_created_during_execution_kills_the_child() {
        let dir = TempDir::new("cancel-file");
        let cancel_file = dir.path().join("cancel.marker");
        let token = CancellationToken::from_file(&cancel_file);
        let spawn_marker = cancel_file.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            fs::write(&spawn_marker, b"stop").expect("create cancel marker");
        });

        let result = execute_cancellable(
            dir.path(),
            &action("killed-by-file", long_running_command()),
            &token,
        )
        .expect("cancellation handled as a result");
        assert!(result.cancelled, "expected cancellation via marker file");
    }

    #[test]
    fn missing_input_is_rejected_before_execution() {
        let dir = TempDir::new("missing-input");
        let mut action = action("needs-input", Vec::new());
        action.argv = vec!["cmd".to_owned(), "/C".to_owned(), "rem".to_owned()];
        action.inputs.push(Input {
            path: PathBuf::from("ghost.txt"),
            kind: InputKind::File,
        });

        let error = execute(dir.path(), &action).unwrap_err();
        assert!(
            matches!(error, ExecutionError::InputMissing(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn prepare_rejects_missing_input_without_spawning() {
        let dir = TempDir::new("prepare-missing");
        let mut action = action("prepare-missing", Vec::new());
        // The command would create a marker file if it were ever spawned.
        #[cfg(windows)]
        {
            action.argv = vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                "echo x> spawned.txt".to_owned(),
            ];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "echo x > spawned.txt".to_owned(),
            ];
        }
        action.inputs.push(Input {
            path: PathBuf::from("ghost.txt"),
            kind: InputKind::File,
        });

        let error = prepare(dir.path(), &action).unwrap_err();
        assert!(
            matches!(error, ExecutionError::InputMissing(_)),
            "got {error:?}"
        );
        assert!(
            !dir.path().join("spawned.txt").exists(),
            "prepare must not spawn the command"
        );
    }

    #[test]
    fn metadata_mode_detects_mtime_only_input_change() {
        let dir = TempDir::new("meta-mode");
        dir.write("in.txt", b"stable content");
        let mut action = action("meta-input", Vec::new());
        #[cfg(windows)]
        {
            action.argv = vec!["cmd".to_owned(), "/C".to_owned(), "rem".to_owned()];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()];
        }
        action.inputs.push(Input {
            path: PathBuf::from("in.txt"),
            kind: InputKind::File,
        });

        // Metadata mode: a timestamp-only change is a changed input.
        let prepared = prepare_with_metadata(dir.path(), &action).expect("prepare");
        assert!(prepared.metadata_fingerprinting);
        set_file_mtime(
            &dir.path().join("in.txt"),
            std::time::SystemTime::now() + Duration::from_secs(90),
        );
        let error = execute_prepared(&prepared, &CancellationToken::new()).unwrap_err();
        assert!(
            matches!(error, ExecutionError::InputChangedDuringExecution(ref path) if path == &PathBuf::from("in.txt")),
            "got {error:?}"
        );

        // Content mode: the same timestamp-only change is not a changed input,
        // and the prepared digests equal the plain content digests.
        let prepared = prepare(dir.path(), &action).expect("prepare");
        assert!(!prepared.metadata_fingerprinting);
        assert_eq!(
            prepared.input_digests[0].digest,
            digest_file(&dir.path().join("in.txt")).expect("plain digest")
        );
        let result = execute_prepared(&prepared, &CancellationToken::new()).expect("plain execute");
        assert!(result.success(), "status {:?}", result.status);
    }

    /// Set a file's modification time through the standard library.
    fn set_file_mtime(path: &Path, time: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for times");
        file.set_times(std::fs::FileTimes::new().set_modified(time))
            .expect("set times");
    }

    #[test]
    fn undeclared_output_is_rejected_after_execution() {
        let dir = TempDir::new("missing-output");
        let mut action = action("no-output", Vec::new());
        #[cfg(windows)]
        {
            action.argv = vec!["cmd".to_owned(), "/C".to_owned(), "echo hi".to_owned()];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec!["sh".to_owned(), "-c".to_owned(), "echo hi".to_owned()];
        }
        action.outputs.push(Output {
            path: PathBuf::from("never-created.txt"),
            kind: OutputKind::File,
        });

        let error = execute(dir.path(), &action).unwrap_err();
        assert!(
            matches!(error, ExecutionError::OutputMissing(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn wrong_output_type_is_rejected() {
        let dir = TempDir::new("output-type");
        dir.write("created.txt", b"file");
        let mut action = action("wrong-type", Vec::new());
        #[cfg(windows)]
        {
            action.argv = vec!["cmd".to_owned(), "/C".to_owned(), "rem".to_owned()];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()];
        }
        action.outputs.push(Output {
            path: PathBuf::from("created.txt"),
            kind: OutputKind::Tree,
        });

        let error = execute(dir.path(), &action).unwrap_err();
        assert!(
            matches!(error, ExecutionError::OutputTypeMismatch(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn working_directory_validation_is_enforced() {
        let dir = TempDir::new("cwd-validation");
        let mut action = action("escapes", Vec::new());
        #[cfg(windows)]
        {
            action.argv = vec!["cmd".to_owned(), "/C".to_owned(), "rem".to_owned()];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()];
        }
        action.working_directory = PathBuf::from("..");

        let error = execute(dir.path(), &action).unwrap_err();
        assert!(
            matches!(error, ExecutionError::InvalidAction(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn unknown_program_reports_spawn_error() {
        let dir = TempDir::new("spawn-error");
        // Clearing the child environment guarantees PATH lookup cannot find it.
        let action = action(
            "no-such-tool",
            vec!["ant-never-existing-program-4f2a".to_owned()],
        );
        let error = execute(dir.path(), &action).unwrap_err();
        assert!(matches!(error, ExecutionError::Spawn(_)), "got {error:?}");
    }

    #[test]
    fn input_modified_during_execution_is_detected() {
        let dir = TempDir::new("input-race");
        dir.write("in.txt", b"before");
        let mut action = action("touch-input", Vec::new());
        // The action modifies its own declared input.
        #[cfg(windows)]
        {
            action.argv = vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                "echo after> in.txt".to_owned(),
            ];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "echo after > in.txt".to_owned(),
            ];
        }
        action.inputs.push(Input {
            path: PathBuf::from("in.txt"),
            kind: InputKind::File,
        });

        let error = execute(dir.path(), &action).unwrap_err();
        assert!(
            matches!(error, ExecutionError::InputChangedDuringExecution(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn toolchain_identity_is_carried_without_validation_of_existence() {
        let dir = TempDir::new("toolchain");
        let mut action = action("with-toolchain", Vec::new());
        #[cfg(windows)]
        {
            action.argv = vec!["cmd".to_owned(), "/C".to_owned(), "exit 0".to_owned()];
        }
        #[cfg(not(windows))]
        {
            action.argv = vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()];
        }
        action.toolchain = Some(ToolchainIdentity {
            name: "test".to_owned(),
            version: "1".to_owned(),
            executable_digest: format!("sha256:{}", "0".repeat(64)),
        });

        let result = execute(dir.path(), &action).expect("execution succeeds");
        assert!(result.success());
    }
}
