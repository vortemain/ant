use build_action::{Action, CachePolicy};
use build_events::EventKind;
use build_graph::BuildGraph;
use local_executor::CancellationToken;
use scheduler::{EventStream, print_action_key, print_digests, run_plan};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

mod cache_runner;
mod front;
mod isolation;
mod remote_cli;
mod scheduler;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("ant: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8, String> {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        return Err(usage().to_owned());
    };
    if command == "--version" || command == "-V" {
        println!("Ant {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if command == "--help" || command == "-h" {
        println!("{}", usage());
        return Ok(0);
    }
    if command == "build" {
        return front::build_command(args);
    }
    if command == "init" {
        return front::init_command(args);
    }
    if command == "plan" {
        return plan_command(args);
    }
    if command == "adapt" {
        return adapt_command(args);
    }
    if command == "coordinator" {
        return remote_cli::coordinator_command(args);
    }
    if command == "worker" {
        return remote_cli::worker_command(args);
    }
    if command == "remote" {
        return remote_cli::remote_command(args);
    }
    if command == "sandbox" {
        return isolation::sandbox_command(args);
    }
    if command == "__gradle-cache-bridge" {
        let code = ant_remote::gradle_cache::bridge_main(args.collect())?;
        return Ok(u8::try_from(code).unwrap_or(1));
    }
    if command == "__sandbox-runner" {
        return isolation::runner_main(args);
    }
    if command != "run" {
        return Err(format!(
            "unknown command '{}'.\n{}",
            command.to_string_lossy(),
            usage()
        ));
    }

    let mut workspace = None;
    let mut plan_path = None;
    let mut cancel_file = None;
    let mut cwd = PathBuf::from(".");
    let mut cwd_was_set = false;
    let mut timeout_ms = None;
    let mut jobs: Option<usize> = None;
    let mut memory_mb: Option<u64> = None;
    let mut events_path: Option<PathBuf> = None;
    let mut cache_dir: Option<PathBuf> = None;
    let mut sandbox = false;
    let mut keep_going = false;
    let mut network_isolated = false;
    let mut enforce_memory = false;
    let mut passthrough = false;
    let mut process_args = Vec::new();
    while let Some(arg) = args.next() {
        if passthrough {
            process_args.push(arg.to_string_lossy().into_owned());
            continue;
        }
        if arg == "--" {
            passthrough = true;
        } else if arg == "--enforce-memory" {
            if enforce_memory {
                return Err("--enforce-memory may only be specified once".to_owned());
            }
            enforce_memory = true;
        } else if arg == "--network-isolated" {
            if network_isolated {
                return Err("--network-isolated may only be specified once".to_owned());
            }
            network_isolated = true;
        } else if arg == "--keep-going" {
            if keep_going {
                return Err("--keep-going may only be specified once".to_owned());
            }
            keep_going = true;
        } else if arg == "--sandbox" {
            if sandbox {
                return Err("--sandbox may only be specified once".to_owned());
            }
            sandbox = true;
        } else if arg == "--workspace" {
            workspace = Some(PathBuf::from(
                args.next().ok_or("--workspace requires a path")?,
            ));
        } else if arg == "--cwd" {
            cwd = PathBuf::from(
                args.next()
                    .ok_or("--cwd requires a workspace-relative path")?,
            );
            cwd_was_set = true;
        } else if arg == "--plan" {
            if plan_path.is_some() {
                return Err("--plan may only be specified once".to_owned());
            }
            plan_path = Some(PathBuf::from(
                args.next().ok_or("--plan requires a file path")?,
            ));
        } else if arg == "--cancel-file" {
            if cancel_file.is_some() {
                return Err("--cancel-file may only be specified once".to_owned());
            }
            cancel_file = Some(PathBuf::from(
                args.next().ok_or("--cancel-file requires a file path")?,
            ));
        } else if arg == "--timeout-ms" {
            if timeout_ms.is_some() {
                return Err("--timeout-ms may only be specified once".to_owned());
            }
            let value = args
                .next()
                .ok_or("--timeout-ms requires a positive integer")?;
            timeout_ms = Some(
                value
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "--timeout-ms requires a positive integer")?,
            );
        } else if arg == "--jobs" {
            if jobs.is_some() {
                return Err("--jobs may only be specified once".to_owned());
            }
            let value = args.next().ok_or("--jobs requires a positive integer")?;
            jobs = Some(
                value
                    .to_string_lossy()
                    .parse::<usize>()
                    .map_err(|_| "--jobs requires a positive integer")?,
            );
        } else if arg == "--memory-mb" {
            if memory_mb.is_some() {
                return Err("--memory-mb may only be specified once".to_owned());
            }
            let value = args
                .next()
                .ok_or("--memory-mb requires a positive integer")?;
            let parsed = value
                .to_string_lossy()
                .parse::<u64>()
                .map_err(|_| "--memory-mb requires a positive integer")?;
            if parsed == 0 {
                return Err("--memory-mb requires a positive integer".to_owned());
            }
            memory_mb = Some(parsed);
        } else if arg == "--events" {
            if events_path.is_some() {
                return Err("--events may only be specified once".to_owned());
            }
            events_path = Some(PathBuf::from(
                args.next().ok_or("--events requires a file path")?,
            ));
        } else if arg == "--cache-dir" {
            if cache_dir.is_some() {
                return Err("--cache-dir may only be specified once".to_owned());
            }
            cache_dir = Some(PathBuf::from(
                args.next().ok_or("--cache-dir requires a directory path")?,
            ));
        } else {
            return Err(format!(
                "unexpected argument '{}'.\n{}",
                arg.to_string_lossy(),
                usage()
            ));
        }
    }
    if passthrough && plan_path.is_some() {
        return Err("a plan file cannot be combined with '-- PROGRAM'".to_owned());
    }
    if plan_path.is_some() && cwd_was_set {
        return Err("--cwd applies only to the single-command form".to_owned());
    }
    if plan_path.is_some() && timeout_ms.is_some() {
        return Err(
            "--timeout-ms applies only to the single-command form; set timeout_ms in the plan"
                .to_owned(),
        );
    }
    if plan_path.is_none() && jobs.is_some() {
        return Err("--jobs applies only to plan execution".to_owned());
    }
    if plan_path.is_none() && enforce_memory {
        return Err("--enforce-memory applies only to plan execution".to_owned());
    }
    if !cfg!(windows) && enforce_memory {
        // Fail closed before anything runs: caps need Windows job objects.
        return Err("--enforce-memory is implemented with Windows job objects only".to_owned());
    }
    if plan_path.is_none() && network_isolated {
        return Err("--network-isolated applies only to plan execution".to_owned());
    }
    if plan_path.is_none() && keep_going {
        return Err("--keep-going applies only to plan execution".to_owned());
    }
    if plan_path.is_none() && memory_mb.is_some() {
        return Err("--memory-mb applies only to plan execution".to_owned());
    }
    if plan_path.is_none() && cache_dir.is_some() {
        // The single-command action is always `cache_policy: never`, so cache
        // participation there could never do anything; reject instead of
        // accepting a silent no-op.
        return Err("--cache-dir applies only to 'ant run --plan'".to_owned());
    }
    let jobs = jobs.unwrap_or(1);
    let workspace = workspace
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?)
        .canonicalize()
        .map_err(|error| format!("cannot access workspace: {error}"))?;
    let cancellation =
        cancel_file.map_or_else(CancellationToken::new, CancellationToken::from_file);
    install_ctrl_c_handler(&cancellation);
    let events = Mutex::new(EventStream::create(events_path, new_run_id())?);
    if let Some(plan_path) = plan_path {
        // Verified before any action starts; fails closed when not ready.
        let isolated = if network_isolated {
            Some(isolation::load_account()?)
        } else {
            None
        };
        let options = scheduler::RunOptions {
            capacity: scheduler::Capacity {
                cpu: jobs as u64,
                memory_mb,
            },
            keep_going,
            exec: cache_runner::ExecConfig {
                // Network isolation always includes write confinement.
                sandbox: sandbox || network_isolated,
                enforce_memory,
                isolated,
            },
        };
        return execute_plan(
            &workspace,
            &plan_path,
            &cancellation,
            options,
            cache_dir.as_deref(),
            &events,
        );
    }
    if !passthrough || process_args.is_empty() {
        return Err(usage().to_owned());
    }
    let action = Action {
        schema_version: 1,
        id: "cli-local-run".to_owned(),
        argv: process_args,
        working_directory: cwd,
        inputs: Vec::new(),
        outputs: Vec::new(),
        environment: Vec::new(),
        cache_policy: CachePolicy::Never,
        toolchain: None,
        timeout_ms,
        resources: None,
    };
    events
        .lock()
        .expect("events lock")
        .emit(EventKind::RunStarted {
            mode: build_events::RunMode::SingleCommand,
        });
    events
        .lock()
        .expect("events lock")
        .emit(EventKind::ActionStarted {
            action_id: action.id.clone(),
        });
    let result = match if sandbox {
        local_executor::execute_sandboxed_cancellable(&workspace, &action, &cancellation)
    } else {
        local_executor::execute_cancellable(&workspace, &action, &cancellation)
    } {
        Ok(result) => result,
        Err(local_executor::ExecutionError::Cancelled) => {
            println!("action={} status=cancelled", action.id);
            emit_single_finish(
                &events,
                &action.id,
                build_events::RunOutcome::Cancelled,
                scheduler::ActionFinishFacts::minimal(build_events::ActionStatus::Cancelled),
            );
            return Ok(130);
        }
        Err(error) => {
            emit_single_finish(
                &events,
                &action.id,
                build_events::RunOutcome::Failed,
                scheduler::ActionFinishFacts {
                    status: build_events::ActionStatus::Failed,
                    error: Some(error.to_string()),
                    ..scheduler::ActionFinishFacts::minimal(build_events::ActionStatus::Failed)
                },
            );
            return Err(error.to_string());
        }
    };
    if result.cancelled {
        println!("action={} status=cancelled", result.action_id);
        emit_single_finish(
            &events,
            &result.action_id,
            build_events::RunOutcome::Cancelled,
            scheduler::ActionFinishFacts::minimal(build_events::ActionStatus::Cancelled),
        );
        return Ok(130);
    }
    if result.timed_out {
        println!("action={} status=timed_out", result.action_id);
        emit_single_finish(
            &events,
            &result.action_id,
            build_events::RunOutcome::TimedOut,
            scheduler::ActionFinishFacts {
                status: build_events::ActionStatus::TimedOut,
                exit_code: result.status.code(),
                ..scheduler::ActionFinishFacts::minimal(build_events::ActionStatus::TimedOut)
            },
        );
        return Ok(124);
    }
    println!(
        "action={} exit_code={}",
        result.action_id,
        result
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |n| n.to_string())
    );
    print_digests("input", &result.input_digests);
    print_digests("output", &result.output_digests);
    let key = print_action_key(&action, &result.input_digests, &workspace, sandbox)?;
    emit_single_finish(
        &events,
        &result.action_id,
        if result.success() {
            build_events::RunOutcome::Succeeded
        } else {
            build_events::RunOutcome::Failed
        },
        scheduler::ActionFinishFacts {
            status: if result.success() {
                build_events::ActionStatus::Succeeded
            } else {
                build_events::ActionStatus::Failed
            },
            exit_code: result.status.code(),
            input_digests: &result.input_digests,
            output_digests: &result.output_digests,
            key_candidate: key.as_ref().map(|(digest, _)| digest.clone()),
            cache_eligible: key.as_ref().map(|(_, eligible)| *eligible),
            error: None,
        },
    );
    Ok(if result.success() { 0 } else { 1 })
}

fn emit_single_finish(
    events: &Mutex<EventStream>,
    action_id: &str,
    outcome: build_events::RunOutcome,
    facts: scheduler::ActionFinishFacts,
) {
    scheduler::emit_action_finished(events, action_id, facts);
    events
        .lock()
        .expect("events lock")
        .emit(build_events::EventKind::RunFinished {
            outcome,
            executed: 1,
            skipped: 0,
        });
}

/// Derive a per-invocation run identifier from the wall clock and process ID
/// without external dependencies. It correlates every event of one run.
pub(crate) fn new_run_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = DefaultHasher::new();
    (nanos, std::process::id()).hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn install_ctrl_c_handler(cancellation: &CancellationToken) {
    let token = cancellation.clone();
    let installed = ctrlc::set_handler(move || {
        token.cancel();
    });
    if let Err(error) = installed {
        eprintln!(
            "ant: warning: Ctrl+C handler unavailable ({error}); use --cancel-file for cooperative cancellation"
        );
    }
}

fn execute_plan(
    workspace: &Path,
    plan_path: &Path,
    cancellation: &CancellationToken,
    options: scheduler::RunOptions,
    cache_dir: Option<&Path>,
    events: &Mutex<EventStream>,
) -> Result<u8, String> {
    let graph = load_graph(plan_path)?;
    // Fully validate the graph before starting the first process.
    run_plan(&graph, workspace, cancellation, options, cache_dir, events)
}

fn plan_command(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8, String> {
    let Some(subcommand) = args.next() else {
        return Err("usage: ant plan <validate|describe|explain> --plan PLAN.json".to_owned());
    };
    if subcommand != "validate" && subcommand != "describe" && subcommand != "explain" {
        return Err(format!(
            "unknown plan command '{}'; expected 'validate', 'describe' or 'explain'",
            subcommand.to_string_lossy()
        ));
    }
    let subcommand = subcommand.to_string_lossy().into_owned();
    let mut plan_path = None;
    let mut cache_dir = None;
    let mut workspace = None;
    while let Some(option) = args.next() {
        if option == "--plan" {
            if plan_path.is_some() {
                return Err("--plan may only be specified once".to_owned());
            }
            plan_path = Some(PathBuf::from(
                args.next().ok_or("--plan requires a file path")?,
            ));
        } else if option == "--cache-dir" {
            if cache_dir.is_some() {
                return Err("--cache-dir may only be specified once".to_owned());
            }
            cache_dir = Some(PathBuf::from(
                args.next().ok_or("--cache-dir requires a directory path")?,
            ));
        } else if option == "--workspace" {
            if workspace.is_some() {
                return Err("--workspace may only be specified once".to_owned());
            }
            workspace = Some(PathBuf::from(
                args.next().ok_or("--workspace requires a path")?,
            ));
        } else {
            return Err(format!(
                "unexpected argument '{}' to plan {}",
                option.to_string_lossy(),
                subcommand
            ));
        }
    }
    let Some(path) = plan_path else {
        return Err(format!("usage: ant plan {} --plan PLAN.json", subcommand));
    };
    if subcommand != "explain" {
        if cache_dir.is_some() {
            return Err("--cache-dir applies only to 'ant plan explain'".to_owned());
        }
        if workspace.is_some() {
            return Err("--workspace applies only to 'ant plan explain'".to_owned());
        }
    }
    let graph = load_graph(&path)?;
    if subcommand == "explain" {
        return explain_plan(&graph, &path, cache_dir.as_deref(), workspace.as_deref());
    }
    let nodes = graph.ordered_nodes().map_err(|error| error.to_string())?;
    println!(
        "plan={} status=valid actions={}",
        path.display(),
        nodes.len()
    );
    for node in nodes {
        if subcommand == "validate" {
            println!("action={} status=planned", node.action.id);
            continue;
        }
        let argv = serde_json::to_string(&node.action.argv).map_err(|error| error.to_string())?;
        let dependencies = if node.dependencies.is_empty() {
            "-".to_owned()
        } else {
            node.dependencies.join(",")
        };
        let timeout = node
            .action
            .timeout_ms
            .map_or_else(|| "none".to_owned(), |ms| format!("{ms}ms"));
        let cache_policy =
            serde_json::to_string(&node.action.cache_policy).map_err(|error| error.to_string())?;
        println!(
            "action={} dependencies={} cwd={} argv={} cache_policy={} timeout={}",
            node.action.id,
            dependencies,
            node.action.working_directory.display(),
            argv,
            cache_policy,
            timeout
        );
        for input in &node.action.inputs {
            println!("  input={} kind={:?}", input.path.display(), input.kind);
        }
        for output in &node.action.outputs {
            println!("  output={} kind={:?}", output.path.display(), output.kind);
        }
    }
    Ok(0)
}

/// Report per-action cacheability diagnostics for a plan without executing
/// anything: the action-key candidate derived from pre-run input digests, the
/// blockers that make the key ineligible, and — when a cache directory is
/// given — whether an entry under that key already exists.
///
/// No cache lookup feeds execution here; reuse is not wired into `ant run`.
fn explain_plan(
    graph: &BuildGraph,
    plan_path: &Path,
    cache_dir: Option<&Path>,
    workspace_arg: Option<&Path>,
) -> Result<u8, String> {
    let nodes = graph.ordered_nodes().map_err(|error| error.to_string())?;
    let cache = cache_dir
        .map(build_cache::Cache::open)
        .transpose()
        .map_err(|error| format!("cannot open cache: {error}"))?;
    let workspace = workspace_arg
        .unwrap_or_else(|| std::path::Path::new("."))
        .canonicalize()
        .map_err(|error| format!("cannot access workspace: {error}"))?;
    println!(
        "plan={} mode=explain actions={} cache={}",
        plan_path.display(),
        nodes.len(),
        cache_dir.map_or_else(|| "none".to_owned(), |path| path.display().to_string())
    );
    let mut preparable = true;
    for node in nodes {
        let action = &node.action;
        let prepared = match local_executor::prepare_with_metadata(&workspace, action) {
            Ok(prepared) => prepared,
            Err(error) => {
                preparable = false;
                println!("action={} status=prepare_failed error={}", action.id, error);
                continue;
            }
        };
        let inputs: Vec<(PathBuf, build_digest::ContentDigest)> = prepared
            .input_digests
            .iter()
            .map(|digest| (digest.path.clone(), digest.digest))
            .collect();
        let candidate = build_digest::action_key_candidate(
            action,
            &inputs,
            // plan explain digests inputs with metadata (the same mode the run
            // path uses), so the fingerprint declares that capability.
            &build_digest::ExecutionFingerprint::local_metadata_fingerprinting(&prepared.workspace),
        )
        .map_err(|error| error.to_string())?;
        println!(
            "action={} status=prepared key_candidate={} cache_eligible={}",
            action.id, candidate.digest, candidate.cache_eligible
        );
        for digest in &prepared.input_digests {
            println!("  input={} digest={}", digest.path.display(), digest.digest);
        }
        for reason in &candidate.reasons {
            println!("  blocker={reason}");
        }
        if let Some(cache) = &cache {
            println!(
                "  cache_entry={}",
                if cache.entry_exists(&candidate.digest) {
                    "present"
                } else {
                    "absent"
                }
            );
        }
    }
    println!(
        "plan={} explain=complete result={}",
        plan_path.display(),
        if preparable {
            "prepared"
        } else {
            "prepare_failed"
        }
    );
    Ok(if preparable { 0 } else { 1 })
}

fn adapt_command(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8, String> {
    let mut adapter_name: Option<String> = None;
    let mut project: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut detect_only = false;
    let mut adapter_options = build_adapter::AdapterOptions::new();
    while let Some(option) = args.next() {
        if option == "--adapter" {
            if adapter_name.is_some() {
                return Err("--adapter may only be specified once".to_owned());
            }
            adapter_name = Some(
                args.next()
                    .ok_or("--adapter requires a name")?
                    .to_string_lossy()
                    .into_owned(),
            );
        } else if option == "--project" {
            if project.is_some() {
                return Err("--project may only be specified once".to_owned());
            }
            project = Some(PathBuf::from(
                args.next().ok_or("--project requires a path")?,
            ));
        } else if option == "--out" {
            if out.is_some() {
                return Err("--out may only be specified once".to_owned());
            }
            out = Some(PathBuf::from(
                args.next().ok_or("--out requires a file path")?,
            ));
        } else if option == "--option" {
            let assignment = args.next().ok_or("--option requires KEY=VALUE")?;
            adapter_options
                .insert_assignment(&assignment.to_string_lossy())
                .map_err(|error| error.to_string())?;
        } else if option == "--detect" {
            if detect_only {
                return Err("--detect may only be specified once".to_owned());
            }
            detect_only = true;
        } else {
            return Err(format!(
                "unexpected argument '{}' to adapt",
                option.to_string_lossy()
            ));
        }
    }
    const ADAPT_USAGE: &str = "usage: ant adapt [--adapter NAME] [--project PATH] [--option KEY=VALUE ...] --out PLAN.json | ant adapt --detect [--project PATH]";
    let project_path = project.unwrap_or_else(|| PathBuf::from("."));
    if !project_path.is_dir() {
        return Err(format!(
            "project directory {} does not exist",
            project_path.display()
        ));
    }
    if detect_only {
        if adapter_name.is_some() || out.is_some() || !adapter_options.is_empty() {
            return Err(
                "--detect cannot be combined with --adapter, --option, or --out".to_owned(),
            );
        }
        return Ok(detect_command(&project_path));
    }
    let Some(out_path) = out else {
        return Err(ADAPT_USAGE.to_owned());
    };
    let (adapter, selection) = match adapter_name {
        Some(name) => {
            let Some(adapter) = build_adapter::adapter(&name) else {
                let names = build_adapter::available()
                    .iter()
                    .map(|candidate| candidate.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "unknown adapter '{name}'; available adapters: {names}"
                ));
            };
            let detection = adapter
                .detect(&project_path)
                .map_err(|error| format!("adapter '{name}' failed: {error}"))?;
            if !detection.supported {
                return Err(format!(
                    "adapter '{name}' does not recognize project {}: {}",
                    project_path.display(),
                    detection.reason
                ));
            }
            (adapter, "explicit")
        }
        None => {
            let adapter = build_adapter::select_adapter(&project_path).map_err(|error| {
                format!(
                    "cannot select an adapter for project {}: {error}",
                    project_path.display()
                )
            })?;
            (adapter, "auto")
        }
    };
    let name = adapter.name();
    // Unknown option keys fail before generation (which would also reject
    // them) so the message names the adapter's accepted options.
    build_adapter::check_option_keys(adapter, &adapter_options)
        .map_err(|error| format!("adapter '{name}' failed: {error}"))?;
    let plan = adapter
        .generate(&project_path, &adapter_options)
        .map_err(|error| format!("adapter '{name}' failed: {error}"))?;
    if plan.actions.is_empty() {
        return Err(format!("adapter '{name}' generated a plan with no actions"));
    }
    // Re-validate through the same conversion `ant run --plan` performs, so
    // what is written here is guaranteed to load.
    plan.to_graph().map_err(|error| error.to_string())?;
    let json = serde_json::to_string_pretty(&plan).map_err(|error| error.to_string())?;
    fs::write(&out_path, json)
        .map_err(|error| format!("cannot write plan {}: {error}", out_path.display()))?;
    println!(
        "adapt={} adapter={name} selection={selection} project={} actions={}",
        out_path.display(),
        project_path.display(),
        plan.actions.len()
    );
    Ok(0)
}

/// Print one diagnostic line per registered adapter. Exit 0 when at least one
/// adapter recognizes the project, 1 otherwise. Never writes anything.
fn detect_command(project_path: &Path) -> u8 {
    let mut supported = 0usize;
    for report in build_adapter::detect_all(project_path) {
        match report.outcome {
            Ok(detection) if detection.supported => {
                supported += 1;
                println!("detect adapter={} supported=true", report.adapter);
            }
            Ok(detection) => println!(
                "detect adapter={} supported=false reason={}",
                report.adapter, detection.reason
            ),
            Err(error) => println!(
                "detect adapter={} supported=error reason={error}",
                report.adapter
            ),
        }
    }
    println!(
        "detect project={} supported_adapters={supported}",
        project_path.display()
    );
    u8::from(supported == 0)
}

pub(crate) fn load_graph(plan_path: &Path) -> Result<BuildGraph, String> {
    let content = fs::read_to_string(plan_path)
        .map_err(|error| format!("cannot read plan {}: {error}", plan_path.display()))?;
    // Windows PowerShell 5's `Set-Content -Encoding UTF8` (and other
    // Windows tools) prefix a UTF-8 BOM; accept it rather than failing with
    // a confusing "expected value at line 1 column 1".
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let plan: build_graph::PlanFile = serde_json::from_str(content)
        .map_err(|error| format!("invalid plan {}: {error}", plan_path.display()))?;
    if plan.schema_version != build_graph::PLAN_SCHEMA_VERSION {
        return Err(format!(
            "unsupported plan schema version {}",
            plan.schema_version
        ));
    }
    if plan.actions.is_empty() {
        return Err("plan must contain at least one action".to_owned());
    }
    plan.to_graph().map_err(|error| error.to_string())
}

fn usage() -> &'static str {
    "Ant - build anything, fast.\n\nEveryday:\n  ant build [--dev]   build this project (auto-detected) and collect deliverables in dist/\n  ant init            record this project's setup in ant.toml (optional)\n  ant --version\n\nSelf-hosting:\n  ant coordinator --data-dir DIR --client-token-file F --worker-token-file F [--viewer-token-file F]\n  ant worker --coordinator URL --token-file F --work-dir DIR [--sandbox]\n  ant remote run --coordinator URL --token-file F --plan PLAN.json\n  ant sandbox <setup [--dry-run]|status|teardown>\n\nEngine (advanced):\n  ant run [--workspace PATH] [--cwd RELATIVE_PATH] [--timeout-ms N] [--cancel-file PATH] [--events PATH] [--sandbox] -- PROGRAM [ARG ...]\n  ant run [--workspace PATH] [--cancel-file PATH] [--jobs N] [--memory-mb M] [--keep-going] [--enforce-memory] [--network-isolated] [--events PATH] [--sandbox] [--cache-dir DIR] --plan PLAN.json\n  ant plan validate --plan PLAN.json\n  ant plan describe --plan PLAN.json\n  ant plan explain --plan PLAN.json [--cache-dir DIR] [--workspace PATH]\n  ant adapt [--adapter NAME] [--project PATH] [--option KEY=VALUE ...] --out PLAN.json\n  ant adapt --detect [--project PATH]\n  ant sandbox <setup [--dry-run]|status|teardown>\n\nRuns local actions directly. By default the executor is not a sandbox and does not\nprovide cache reuse. On Windows, --sandbox confines writes to the workspace using a\nrestricted token (still not network-isolated, so cache reuse stays gated);\n--sandbox fails closed on other platforms and never falls back to unsandboxed\nexecution. --jobs N is a CPU-slot capacity and --memory-mb M an optional memory\nbudget: an action starts only when its declared resources (cpu, default 1;\nmemory_mb) fit, in FIFO order; these are admission weights, not runtime limits.\n--keep-going runs every action not downstream of a failure instead of stopping\nnew starts at the first failure; dependents of failed actions are skipped.\n--enforce-memory (Windows) turns each declared memory_mb into a job-wide\ncommitted-memory cap applied before the process starts; it fails closed.\n--network-isolated (Windows) runs each action write-confined as the dedicated\nsandbox account whose network is blocked by firewall rules; requires a one-time\nelevated 'ant sandbox setup' and is verified before every run (fails closed).\n--cache-dir opts a plan run into cache participation: a key is stored or looked\nup only when its candidate reported no blockers, which only --network-isolated\ncan achieve (it is the one executor that truthfully declares network isolation).\n'ant plan explain' reports per-action key candidates and cacheability blockers\nfor inspection only. 'ant adapt' generates a plan for a recognized project using\na build-system adapter (available: cargo, npm, cmake); without --adapter it uses the single\nadapter that recognizes the project and refuses to guess when several do.\n--option passes adapter-specific settings (cargo: profile=dev|release; npm:\noutputs=none|entry-points; cmake: config=Debug|Release); unknown keys or values\nfail before anything is written.\n'ant adapt --detect' reports every adapter's detection result without writing.\nGenerated actions declare what the adapter can honestly cover and default to\ncache_policy never."
}
