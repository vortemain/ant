//! Plan execution over a validated DAG: sequential or bounded-parallel.
//!
//! Both modes share one contract: the full graph is validated before the
//! first process starts; a dependency runs to success before its dependents
//! start; the first failure, timeout, or cancellation request stops *new*
//! actions from starting while already-running actions are allowed to
//! terminate; actions that never started are reported as `skipped`.
//! Among simultaneously ready actions, scheduling order is not deterministic
//! when `--jobs` exceeds one; dependency correctness and final reporting are.
//!
//! Parallel runs are resource-weighted: `--jobs N` is a CPU-slot capacity and
//! `--memory-mb M` an optional memory budget. An action starts only when its
//! request (`resources.cpu`, default 1; `resources.memory_mb`, default none)
//! fits the remaining capacity. Admission is strict FIFO over the ready
//! queue: when the head action does not fit, nothing behind it may start, so
//! a heavy action can never be starved by a stream of light ones (at the cost
//! of some idle capacity). A request larger than the whole capacity is
//! clamped to it, so the action runs alone instead of deadlocking the run.
//! Requests are admission weights only; processes are not limited at runtime.
//!
//! With `keep_going` (`--keep-going`), a failed, timed-out, or unpreparable
//! action no longer stops new starts: every action that does not depend on it
//! (directly or transitively) still runs, and its dependents are reported as
//! `skipped`. Cancellation always stops new starts. The exit status reports
//! the worst outcome observed (hard error > cancelled > timed out > failed).
//!
//! Every state transition is mirrored into the structured event stream
//! (`build-events`) when one is configured.

use build_action::Action;
use build_events::{ActionStatus, EventKind, NamedDigest, RunMode, RunOutcome};
use build_graph::BuildGraph;
use local_executor::{ActionContentDigest, CancellationToken, ExecutionError};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Whether machine-readable `key=value` status lines are printed. `ant run`
/// keeps them (scripts and tests parse them); human front-ends such as
/// `ant build` turn them off and render events instead. Lines starting with
/// `ant: ` (warnings and notes) are always printed.
static MACHINE_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_machine_output(enabled: bool) {
    MACHINE_OUTPUT.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn emit_line(line: String) {
    if MACHINE_OUTPUT.load(std::sync::atomic::Ordering::Relaxed) || line.starts_with("ant: ") {
        println!("{line}");
    }
}

macro_rules! report {
    ($($arg:tt)*) => {
        emit_line(format!($($arg)*))
    };
}

use crate::cache_runner::{self, ActionRun, CacheParticipant};

/// How the plan stopped. `HardError` aborts the run the way preflight
/// failures always have: the message goes to stderr and the exit code is 1.
#[derive(Clone, Debug)]
enum Termination {
    Success,
    FailedProcess,
    TimedOut,
    Cancelled,
    HardError(String),
}

impl Termination {
    fn rank(&self) -> u8 {
        match self {
            Self::Success => 0,
            Self::FailedProcess => 1,
            Self::TimedOut => 2,
            Self::Cancelled => 3,
            Self::HardError(_) => 4,
        }
    }

    /// Keep the more severe outcome; the first of equal severity wins.
    fn worst(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

struct PlanResult {
    termination: Termination,
    executed: usize,
    skipped: Vec<String>,
}

struct Task {
    action: Action,
    dependencies: Vec<String>,
}

/// Buffered writer for the optional structured event stream.
pub struct EventStream {
    run_id: u64,
    sink: Option<Box<dyn build_events::EventSink>>,
}

impl EventStream {
    /// Create a stream writing to `path`, or a no-op stream when `path` is
    /// `None`. Fails if the file cannot be created.
    pub fn create(path: Option<PathBuf>, run_id: u64) -> Result<Self, String> {
        let sink = match path {
            None => None,
            Some(path) => Some(Box::new(build_events::JsonLinesFile::create(&path).map_err(
                |error| format!("cannot create events file {}: {error}", path.display()),
            )?) as Box<dyn build_events::EventSink>),
        };
        Ok(Self { run_id, sink })
    }

    /// A stream delivering events to an in-process sink (for example the
    /// human progress reporter of `ant build`).
    pub fn with_sink(sink: Box<dyn build_events::EventSink>, run_id: u64) -> Self {
        Self {
            run_id,
            sink: Some(sink),
        }
    }

    pub fn emit(&mut self, kind: EventKind) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        if sink
            .emit(build_events::Event::new(self.run_id, kind))
            .is_err()
        {
            eprintln!("ant: warning: could not write events; further event writes suppressed");
            self.sink = None;
        }
    }
}

/// A task's admission weight after clamping to the run's capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Weight {
    cpu: u64,
    memory_mb: u64,
}

/// Ready queue plus free capacity; guarded by one lock so admission is atomic.
struct Admission {
    ready: VecDeque<usize>,
    /// Admitted actions that have not yet released their capacity.
    running: usize,
    free_cpu: u64,
    /// `None` when no memory budget was configured.
    free_memory_mb: Option<u64>,
}

impl Admission {
    /// Pop the head task and reserve its weight if it fits (strict FIFO).
    fn admit(&mut self, weights: &[Weight]) -> Option<usize> {
        let &head = self.ready.front()?;
        let weight = weights[head];
        if weight.cpu > self.free_cpu {
            return None;
        }
        if let Some(free) = self.free_memory_mb
            && weight.memory_mb > free
        {
            return None;
        }
        self.ready.pop_front();
        self.running += 1;
        self.free_cpu -= weight.cpu;
        if let Some(free) = self.free_memory_mb.as_mut() {
            *free -= weight.memory_mb;
        }
        Some(head)
    }

    fn release(&mut self, weight: Weight) {
        self.running -= 1;
        self.free_cpu += weight.cpu;
        if let Some(free) = self.free_memory_mb.as_mut() {
            *free += weight.memory_mb;
        }
    }
}

/// Run capacity for resource-weighted admission.
#[derive(Clone, Copy, Debug)]
pub struct Capacity {
    pub cpu: u64,
    pub memory_mb: Option<u64>,
}

/// Plan-run settings chosen on the command line.
#[derive(Clone, Copy)]
pub struct RunOptions {
    pub capacity: Capacity,
    pub keep_going: bool,
    /// How each action executes (direct, sandboxed, network-isolated) and
    /// whether declared memory is enforced.
    pub exec: cache_runner::ExecConfig,
}

/// Clamp an action's request to the run capacity. Returns the weight and
/// whether clamping happened (the action will run alone).
fn clamp_weight(action: &Action, capacity: Capacity) -> (Weight, bool) {
    let request = action.resource_request();
    let requested_cpu = u64::from(request.cpu);
    let requested_memory = request.memory_mb.unwrap_or(0);
    let cpu = requested_cpu.min(capacity.cpu);
    let memory_mb = capacity
        .memory_mb
        .map_or(requested_memory, |budget| requested_memory.min(budget));
    (
        Weight { cpu, memory_mb },
        cpu != requested_cpu || memory_mb != requested_memory,
    )
}

/// State shared by parallel worker threads.
struct Shared {
    tasks: Vec<Task>,
    /// Number of unmet dependencies per task index.
    pending: Vec<AtomicUsize>,
    /// For each task index, the indexes unlocked when it succeeds.
    dependents: Vec<Vec<usize>>,
    queue: Mutex<Admission>,
    /// Clamped resource weight per task index.
    weights: Vec<Weight>,
    executed: Vec<AtomicBool>,
    executed_count: AtomicUsize,
    stop_starting: AtomicBool,
    /// `--keep-going`: failures do not stop independent actions.
    keep_going: bool,
    failed: AtomicBool,
    timed_out: AtomicBool,
    cancelled: AtomicBool,
    first_error: Mutex<Option<String>>,
    /// Serializes human-readable status lines so lines do not interleave.
    output: Mutex<()>,
    /// How actions execute; read by workers, set once at plan start.
    exec: cache_runner::ExecConfig,
    /// Cache participation (`--cache-dir`); mechanics behind the D-003 gate.
    participant: CacheParticipant,
}

impl Shared {
    /// A non-cancellation failure stops new starts unless keep-going is on;
    /// with keep-going the failed action's dependents are simply never
    /// queued, so they end up reported as skipped.
    fn on_failure(&self) {
        if !self.keep_going {
            self.stop_starting.store(true, Ordering::Relaxed);
        }
    }

    fn log(&self, line: String) {
        let _guard = self.output.lock().expect("output lock");
        report!("{line}");
    }
}

/// Execute a validated plan graph and return the process exit code.
///
/// A CPU capacity (`--jobs`) of 1 runs the strictly sequential path; larger
/// values run resource-weighted admission up to that capacity. Returns `Err`
/// only for hard preflight/execution failures that abort the plan.
pub fn run_plan(
    graph: &BuildGraph,
    workspace: &Path,
    cancellation: &CancellationToken,
    options: RunOptions,
    cache_dir: Option<&Path>,
    events: &Mutex<EventStream>,
) -> Result<u8, String> {
    let RunOptions { capacity, .. } = options;
    if capacity.cpu == 0 {
        return Err("--jobs requires a positive integer".to_owned());
    }
    if capacity.memory_mb == Some(0) {
        return Err("--memory-mb requires a positive integer".to_owned());
    }
    let jobs = usize::try_from(capacity.cpu).unwrap_or(usize::MAX);
    let nodes = graph.ordered_nodes().map_err(|error| error.to_string())?;
    let tasks: Vec<Task> = nodes
        .iter()
        .map(|node| Task {
            action: node.action.clone(),
            dependencies: node.dependencies.clone(),
        })
        .collect();
    // Sequential runs execute one action at a time, so weights only matter
    // (and are only reported) for parallel runs.
    if jobs > 1 {
        for task in &tasks {
            let (weight, clamped) = clamp_weight(&task.action, capacity);
            if clamped {
                let request = task.action.resource_request();
                report!(
                    "ant: note: action={} requests cpu={} memory_mb={} exceeding capacity cpu={} memory_mb={}; clamped to cpu={} memory_mb={} (the whole capacity)",
                    task.action.id,
                    request.cpu,
                    request.memory_mb.unwrap_or(0),
                    capacity.cpu,
                    capacity
                        .memory_mb
                        .map_or_else(|| "unbounded".to_owned(), |m| m.to_string()),
                    weight.cpu,
                    weight.memory_mb,
                );
            }
        }
    }
    events
        .lock()
        .expect("events lock")
        .emit(EventKind::RunStarted {
            mode: RunMode::Plan { jobs },
        });
    // Opening the requested cache directory fails hard: the user asked for
    // cache participation and a silently ignored flag would hide the mistake.
    let participant = CacheParticipant::new(cache_dir)?;
    let result = if jobs == 1 {
        run_sequential(
            workspace,
            cancellation,
            &tasks,
            options,
            &participant,
            events,
        )
    } else {
        run_parallel(workspace, cancellation, tasks, options, participant, events)
    };
    let outcome = match result.termination {
        Termination::Success => RunOutcome::Succeeded,
        Termination::FailedProcess | Termination::HardError(_) => RunOutcome::Failed,
        Termination::TimedOut => RunOutcome::TimedOut,
        Termination::Cancelled => RunOutcome::Cancelled,
    };
    events
        .lock()
        .expect("events lock")
        .emit(EventKind::RunFinished {
            outcome,
            executed: result.executed,
            skipped: result.skipped.len(),
        });
    match result.termination {
        Termination::Success => Ok(0),
        Termination::FailedProcess => Ok(1),
        Termination::TimedOut => Ok(124),
        Termination::Cancelled => Ok(130),
        Termination::HardError(message) => Err(message),
    }
}

fn run_sequential(
    workspace: &Path,
    cancellation: &CancellationToken,
    tasks: &[Task],
    options: RunOptions,
    participant: &CacheParticipant,
    events: &Mutex<EventStream>,
) -> PlanResult {
    let RunOptions {
        keep_going, exec, ..
    } = options;
    let mut executed_ids = BTreeSet::new();
    let mut termination = Termination::Success;
    // Actions that did not succeed (failed or never ran); with keep-going,
    // anything depending on one of these is skipped.
    let mut blocked: HashSet<String> = HashSet::new();
    for task in tasks {
        let id = task.action.id.clone();
        if task
            .dependencies
            .iter()
            .any(|dependency| blocked.contains(dependency))
        {
            blocked.insert(id);
            continue;
        }
        if keep_going && cancellation.is_cancelled() {
            termination = termination.worst(Termination::Cancelled);
            break;
        }
        report!("action={id} status=started");
        events
            .lock()
            .expect("events lock")
            .emit(EventKind::ActionStarted {
                action_id: id.clone(),
            });
        executed_ids.insert(id.clone());
        match cache_runner::run_action(participant, workspace, &task.action, cancellation, exec) {
            Ok(ActionRun::Reused {
                candidate,
                input_digests,
                output_digests,
            }) => {
                report!("action={id} status=reused");
                print_digests("input", &input_digests);
                print_digests("output", &output_digests);
                // A reused key is blocker-free by definition (D-003 gate).
                report!(
                    "action={id} key_candidate={} cache_eligible=true blockers=[]",
                    candidate.digest
                );
                emit_action_finished(
                    events,
                    &id,
                    ActionFinishFacts {
                        status: ActionStatus::Reused,
                        exit_code: None,
                        input_digests: &input_digests,
                        output_digests: &output_digests,
                        key_candidate: Some(candidate.digest.to_string()),
                        cache_eligible: Some(true),
                        error: None,
                    },
                );
            }
            Ok(ActionRun::Executed {
                result,
                candidate,
                store_warning,
            }) => {
                if let Some(warning) = store_warning {
                    report!("ant: warning: action={id} {warning}");
                }
                if result.cancelled {
                    report!("action={} status=cancelled", result.action_id);
                    emit_action_finished(
                        events,
                        &result.action_id,
                        ActionFinishFacts::minimal(ActionStatus::Cancelled),
                    );
                    termination = termination.worst(Termination::Cancelled);
                    break;
                }
                if result.timed_out {
                    report!("action={} status=timed_out", result.action_id);
                    emit_action_finished(
                        events,
                        &result.action_id,
                        ActionFinishFacts {
                            status: ActionStatus::TimedOut,
                            exit_code: result.status.code(),
                            ..ActionFinishFacts::minimal(ActionStatus::TimedOut)
                        },
                    );
                    termination = termination.worst(Termination::TimedOut);
                    if keep_going {
                        blocked.insert(id);
                        continue;
                    }
                    break;
                }
                let exit_code = result.status.code();
                report!(
                    "action={} status={} exit_code={}",
                    result.action_id,
                    if result.success() {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    exit_code.map_or_else(|| "signal".to_owned(), |n| n.to_string())
                );
                print_digests("input", &result.input_digests);
                print_digests("output", &result.output_digests);
                print_memory(&result);
                report!(
                    "action={} key_candidate={} cache_eligible={} blockers={:?}",
                    result.action_id,
                    candidate.digest,
                    candidate.cache_eligible,
                    candidate.reasons
                );
                emit_action_finished(
                    events,
                    &result.action_id,
                    ActionFinishFacts {
                        status: if result.success() {
                            ActionStatus::Succeeded
                        } else {
                            ActionStatus::Failed
                        },
                        exit_code,
                        input_digests: &result.input_digests,
                        output_digests: &result.output_digests,
                        key_candidate: Some(candidate.digest.to_string()),
                        cache_eligible: Some(candidate.cache_eligible),
                        error: None,
                    },
                );
                if !result.success() {
                    termination = termination.worst(Termination::FailedProcess);
                    if keep_going {
                        blocked.insert(id);
                        continue;
                    }
                    break;
                }
            }
            Err(ExecutionError::Cancelled) => {
                report!("action={id} status=cancelled");
                emit_action_finished(
                    events,
                    &id,
                    ActionFinishFacts::minimal(ActionStatus::Cancelled),
                );
                termination = termination.worst(Termination::Cancelled);
                break;
            }
            Err(error) => {
                let message = error.to_string();
                report!("action={id} status=failed error={message}");
                emit_action_finished(
                    events,
                    &id,
                    ActionFinishFacts {
                        status: ActionStatus::Failed,
                        error: Some(message.clone()),
                        ..ActionFinishFacts::minimal(ActionStatus::Failed)
                    },
                );
                termination = termination.worst(Termination::HardError(message));
                if keep_going {
                    blocked.insert(id);
                    continue;
                }
                break;
            }
        }
    }
    let skipped = report_skipped(tasks, &executed_ids, events);
    PlanResult {
        termination,
        executed: executed_ids.len(),
        skipped,
    }
}

fn run_parallel(
    workspace: &Path,
    cancellation: &CancellationToken,
    tasks: Vec<Task>,
    options: RunOptions,
    participant: CacheParticipant,
    events: &Mutex<EventStream>,
) -> PlanResult {
    let RunOptions { capacity, .. } = options;
    let total = tasks.len();
    let mut position: HashMap<String, usize> = HashMap::with_capacity(total);
    for (index, task) in tasks.iter().enumerate() {
        position.insert(task.action.id.clone(), index);
    }
    let mut pending = vec![0usize; total];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); total];
    for (index, task) in tasks.iter().enumerate() {
        let mut unique = HashSet::new();
        for dependency in &task.dependencies {
            if unique.insert(dependency.as_str()) {
                pending[index] += 1;
                let dep_index = position[dependency];
                dependents[dep_index].push(index);
            }
        }
    }
    // Initially ready tasks in deterministic action-ID order.
    let mut initial: Vec<usize> = (0..total).filter(|&index| pending[index] == 0).collect();
    initial.sort_by(|&left, &right| tasks[left].action.id.cmp(&tasks[right].action.id));
    let queue: VecDeque<usize> = initial.into_iter().collect();
    let weights: Vec<Weight> = tasks
        .iter()
        .map(|task| clamp_weight(&task.action, capacity).0)
        .collect();
    // One worker per CPU slot is the most that can ever be admitted at once.
    let jobs = usize::try_from(capacity.cpu)
        .unwrap_or(usize::MAX)
        .min(total.max(1));

    let shared = Arc::new(Shared {
        tasks,
        pending: pending.into_iter().map(AtomicUsize::new).collect(),
        dependents,
        queue: Mutex::new(Admission {
            ready: queue,
            running: 0,
            free_cpu: capacity.cpu,
            free_memory_mb: capacity.memory_mb,
        }),
        weights,
        executed: (0..total).map(|_| AtomicBool::new(false)).collect(),
        executed_count: AtomicUsize::new(0),
        stop_starting: AtomicBool::new(false),
        keep_going: options.keep_going,
        failed: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        first_error: Mutex::new(None),
        output: Mutex::new(()),
        exec: options.exec,
        participant,
    });
    let workspace = workspace.to_path_buf();
    let cancellation = cancellation.clone();

    thread::scope(|scope| {
        for _ in 0..jobs {
            let shared = Arc::clone(&shared);
            let workspace = workspace.clone();
            let cancellation = cancellation.clone();
            scope.spawn(move || worker_loop(&shared, &workspace, &cancellation, events));
        }
    });

    let shared = match Arc::try_unwrap(shared) {
        Ok(shared) => shared,
        Err(_) => {
            // Worker threads are joined by `thread::scope`; a lingering clone
            // would be an internal bug. Report it as a hard plan error.
            return PlanResult {
                termination: Termination::HardError(
                    "scheduler state still shared after workers exited".to_owned(),
                ),
                executed: 0,
                skipped: Vec::new(),
            };
        }
    };
    let first_error = shared.first_error.lock().expect("error lock").take();
    if let Some(message) = first_error {
        let skipped = skipped_ids(&shared);
        print_skipped_lines(&skipped);
        emit_skipped(events, &skipped);
        return PlanResult {
            termination: Termination::HardError(message),
            executed: shared.executed_count.load(Ordering::Relaxed),
            skipped,
        };
    }
    let skipped = skipped_ids(&shared);
    print_skipped_lines(&skipped);
    emit_skipped(events, &skipped);
    let termination = if shared.cancelled.load(Ordering::Relaxed) {
        Termination::Cancelled
    } else if shared.timed_out.load(Ordering::Relaxed) {
        Termination::TimedOut
    } else if shared.failed.load(Ordering::Relaxed) {
        Termination::FailedProcess
    } else {
        Termination::Success
    };
    PlanResult {
        termination,
        executed: shared.executed_count.load(Ordering::Relaxed),
        skipped,
    }
}

fn skipped_ids(shared: &Shared) -> Vec<String> {
    let mut skipped: Vec<String> = shared
        .tasks
        .iter()
        .enumerate()
        .filter(|(index, _)| !shared.executed[*index].load(Ordering::Relaxed))
        .map(|(_, task)| task.action.id.clone())
        .collect();
    skipped.sort();
    skipped
}

fn print_skipped_lines(skipped: &[String]) {
    for id in skipped {
        report!("action={id} status=skipped");
    }
}

fn emit_skipped(events: &Mutex<EventStream>, skipped: &[String]) {
    let mut stream = events.lock().expect("events lock");
    for id in skipped {
        stream.emit(EventKind::ActionSkipped {
            action_id: id.clone(),
        });
    }
}

fn worker_loop(
    shared: &Shared,
    workspace: &Path,
    cancellation: &CancellationToken,
    events: &Mutex<EventStream>,
) {
    loop {
        if shared.stop_starting.load(Ordering::Relaxed) {
            return;
        }
        if cancellation.is_cancelled() {
            shared.cancelled.store(true, Ordering::Relaxed);
            shared.stop_starting.store(true, Ordering::Relaxed);
            return;
        }
        let (next, idle) = {
            let mut queue = shared.queue.lock().expect("queue lock");
            let next = queue.admit(&shared.weights);
            // Nothing ready and nothing running: no further task can ever
            // become ready (remaining tasks are blocked by failures).
            (next, queue.ready.is_empty() && queue.running == 0)
        };
        let Some(index) = next else {
            if idle {
                return;
            }
            // Waiting for a dependency to finish or for capacity to free up.
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        run_task(shared, workspace, cancellation, events, index);
        // Release capacity on every outcome; dependents were already queued.
        shared
            .queue
            .lock()
            .expect("queue lock")
            .release(shared.weights[index]);
    }
}

fn run_task(
    shared: &Shared,
    workspace: &Path,
    cancellation: &CancellationToken,
    events: &Mutex<EventStream>,
    index: usize,
) {
    let id = shared.tasks[index].action.id.clone();
    shared.log(format!("action={id} status=started"));
    events
        .lock()
        .expect("events lock")
        .emit(EventKind::ActionStarted {
            action_id: id.clone(),
        });
    let run = match cache_runner::run_action(
        &shared.participant,
        workspace,
        &shared.tasks[index].action,
        cancellation,
        shared.exec,
    ) {
        Ok(run) => run,
        Err(ExecutionError::Cancelled) => {
            shared.log(format!("action={id} status=cancelled"));
            events
                .lock()
                .expect("events lock")
                .emit(EventKind::ActionFinished {
                    action_id: id.clone(),
                    status: ActionStatus::Cancelled,
                    exit_code: None,
                    input_digests: Vec::new(),
                    output_digests: Vec::new(),
                    key_candidate: None,
                    cache_eligible: None,
                    error: None,
                });
            shared.cancelled.store(true, Ordering::Relaxed);
            shared.stop_starting.store(true, Ordering::Relaxed);
            return;
        }
        Err(error) => {
            let message = error.to_string();
            shared.log(format!("action={id} status=failed error={message}"));
            events
                .lock()
                .expect("events lock")
                .emit(EventKind::ActionFinished {
                    action_id: id.clone(),
                    status: ActionStatus::Failed,
                    exit_code: None,
                    input_digests: Vec::new(),
                    output_digests: Vec::new(),
                    key_candidate: None,
                    cache_eligible: None,
                    error: Some(message.clone()),
                });
            let mut guard = shared.first_error.lock().expect("error lock");
            if guard.is_none() {
                *guard = Some(message);
            }
            shared.failed.store(true, Ordering::Relaxed);
            shared.executed[index].store(true, Ordering::Relaxed);
            shared.executed_count.fetch_add(1, Ordering::Relaxed);
            shared.on_failure();
            return;
        }
    };
    shared.executed[index].store(true, Ordering::Relaxed);
    shared.executed_count.fetch_add(1, Ordering::Relaxed);
    // A reused action satisfied the task without spawning a process; it
    // reaches the same completion tail so dependents unlock either way.
    match run {
        ActionRun::Reused {
            candidate,
            input_digests,
            output_digests,
        } => {
            shared.log(format!("action={id} status=reused"));
            // Digest and key lines belong to this action's status line;
            // serialize them so concurrent workers do not interleave.
            let _guard = shared.output.lock().expect("output lock");
            print_digests("input", &input_digests);
            print_digests("output", &output_digests);
            // A reused key is blocker-free by definition (D-003 gate).
            report!(
                "action={id} key_candidate={} cache_eligible=true blockers=[]",
                candidate.digest
            );
            drop(_guard);
            events
                .lock()
                .expect("events lock")
                .emit(EventKind::ActionFinished {
                    action_id: id.clone(),
                    status: ActionStatus::Reused,
                    exit_code: None,
                    input_digests: named_digests(&input_digests),
                    output_digests: named_digests(&output_digests),
                    key_candidate: Some(candidate.digest.to_string()),
                    cache_eligible: Some(true),
                    error: None,
                });
        }
        ActionRun::Executed {
            result,
            candidate,
            store_warning,
        } => {
            let exit_code = result.status.code();
            if result.cancelled {
                shared.log(format!("action={id} status=cancelled"));
                events
                    .lock()
                    .expect("events lock")
                    .emit(EventKind::ActionFinished {
                        action_id: id.clone(),
                        status: ActionStatus::Cancelled,
                        exit_code,
                        input_digests: Vec::new(),
                        output_digests: Vec::new(),
                        key_candidate: None,
                        cache_eligible: None,
                        error: None,
                    });
                shared.cancelled.store(true, Ordering::Relaxed);
                shared.stop_starting.store(true, Ordering::Relaxed);
                return;
            }
            if result.timed_out {
                shared.log(format!("action={id} status=timed_out"));
                events
                    .lock()
                    .expect("events lock")
                    .emit(EventKind::ActionFinished {
                        action_id: id.clone(),
                        status: ActionStatus::TimedOut,
                        exit_code,
                        input_digests: Vec::new(),
                        output_digests: Vec::new(),
                        key_candidate: None,
                        cache_eligible: None,
                        error: None,
                    });
                shared.timed_out.store(true, Ordering::Relaxed);
                shared.on_failure();
                return;
            }
            if !result.success() {
                shared.log(format!(
                    "action={id} status=failed exit_code={}",
                    exit_code.map_or_else(|| "signal".to_owned(), |n| n.to_string())
                ));
                {
                    let _guard = shared.output.lock().expect("output lock");
                    print_memory(&result);
                }
                events
                    .lock()
                    .expect("events lock")
                    .emit(EventKind::ActionFinished {
                        action_id: id.clone(),
                        status: ActionStatus::Failed,
                        exit_code,
                        input_digests: Vec::new(),
                        output_digests: Vec::new(),
                        key_candidate: None,
                        cache_eligible: None,
                        error: None,
                    });
                shared.failed.store(true, Ordering::Relaxed);
                shared.on_failure();
                return;
            }
            if let Some(warning) = store_warning {
                let _guard = shared.output.lock().expect("output lock");
                report!("ant: warning: action={id} {warning}");
            }
            shared.log(format!(
                "action={id} status=succeeded exit_code={}",
                exit_code.map_or_else(|| "signal".to_owned(), |n| n.to_string())
            ));
            // Digest and key lines belong to this action's status line;
            // serialize them so concurrent workers do not interleave.
            {
                let _guard = shared.output.lock().expect("output lock");
                print_digests("input", &result.input_digests);
                print_digests("output", &result.output_digests);
                print_memory(&result);
                report!(
                    "action={id} key_candidate={} cache_eligible={} blockers={:?}",
                    candidate.digest,
                    candidate.cache_eligible,
                    candidate.reasons
                );
            }
            events
                .lock()
                .expect("events lock")
                .emit(EventKind::ActionFinished {
                    action_id: id.clone(),
                    status: ActionStatus::Succeeded,
                    exit_code,
                    input_digests: named_digests(&result.input_digests),
                    output_digests: named_digests(&result.output_digests),
                    key_candidate: Some(candidate.digest.to_string()),
                    cache_eligible: Some(candidate.cache_eligible),
                    error: None,
                });
        }
    }
    // Unlock dependents; they become available to any worker.
    let mut queue = shared.queue.lock().expect("queue lock");
    for &dependent in &shared.dependents[index] {
        if shared.pending[dependent].fetch_sub(1, Ordering::Relaxed) == 1 {
            queue.ready.push_back(dependent);
        }
    }
}

fn report_skipped(
    tasks: &[Task],
    executed_ids: &BTreeSet<String>,
    events: &Mutex<EventStream>,
) -> Vec<String> {
    let skipped: Vec<String> = tasks
        .iter()
        .filter(|task| !executed_ids.contains(&task.action.id))
        .map(|task| task.action.id.clone())
        .collect();
    for id in &skipped {
        report!("action={id} status=skipped");
    }
    emit_skipped(events, &skipped);
    skipped
}

/// The observed facts of one finished action, shared by the console line
/// printer and the event stream.
pub struct ActionFinishFacts<'a> {
    pub status: ActionStatus,
    pub exit_code: Option<i32>,
    pub input_digests: &'a [ActionContentDigest],
    pub output_digests: &'a [ActionContentDigest],
    pub key_candidate: Option<String>,
    pub cache_eligible: Option<bool>,
    pub error: Option<String>,
}

impl<'a> ActionFinishFacts<'a> {
    pub fn minimal(status: ActionStatus) -> Self {
        Self {
            status,
            exit_code: None,
            input_digests: &[],
            output_digests: &[],
            key_candidate: None,
            cache_eligible: None,
            error: None,
        }
    }
}

/// Emit one `ActionFinished` event carrying the observed facts.
pub fn emit_action_finished(
    events: &Mutex<EventStream>,
    action_id: &str,
    facts: ActionFinishFacts,
) {
    events
        .lock()
        .expect("events lock")
        .emit(EventKind::ActionFinished {
            action_id: action_id.to_owned(),
            status: facts.status,
            exit_code: facts.exit_code,
            input_digests: named_digests(facts.input_digests),
            output_digests: named_digests(facts.output_digests),
            key_candidate: facts.key_candidate,
            cache_eligible: facts.cache_eligible,
            error: facts.error,
        });
}

fn named_digests(digests: &[ActionContentDigest]) -> Vec<NamedDigest> {
    digests
        .iter()
        .map(|entry| NamedDigest {
            path: entry.path.display().to_string(),
            digest: entry.digest.to_string(),
        })
        .collect()
}

/// Print the measured peak memory (and the enforced cap, if any) when the
/// executor measured it. Windows counts a commit charge the cap rejected in
/// the job's peak (verified empirically), so a peak above the cap means the
/// cap refused at least one allocation.
fn print_memory(result: &local_executor::ExecutionResult) {
    if let Some(peak) = result.peak_memory_bytes {
        let (limit, hit) = match result.memory_limit_bytes {
            Some(limit) => (limit.to_string(), peak > limit),
            None => ("none".to_owned(), false),
        };
        report!(
            "action={} peak_memory_bytes={peak} memory_limit_bytes={limit} memory_limit_hit={hit}",
            result.action_id
        );
    }
}

pub fn print_digests(kind: &str, digests: &[ActionContentDigest]) {
    for entry in digests {
        report!("{kind}={} digest={}", entry.path.display(), entry.digest);
    }
}

/// Print the explainable action-key line; returns the key and its
/// cacheability so the event stream can carry the same facts.
pub fn print_action_key(
    action: &Action,
    input_digests: &[ActionContentDigest],
    workspace: &Path,
    sandbox: bool,
) -> Result<Option<(String, bool)>, String> {
    let inputs: Vec<_> = input_digests
        .iter()
        .map(|entry| (entry.path.clone(), entry.digest))
        .collect();
    // The fingerprint must match the digest mode the run actually used. The
    // direct run path digests inputs with metadata; the sandboxed run path
    // additionally declares the `sandboxed` capability. `ant plan explain`
    // intentionally does NOT pass `sandbox`: its key is executor-agnostic and
    // only the reported blockers differ.
    let fingerprint = if sandbox {
        local_executor::sandboxed_fingerprint(workspace)
    } else {
        build_digest::ExecutionFingerprint::local_metadata_fingerprinting(workspace)
    };
    let candidate = build_digest::action_key_candidate(action, &inputs, &fingerprint)
        .map_err(|error| error.to_string())?;
    report!(
        "action={} key_candidate={} cache_eligible={} blockers={:?}",
        action.id,
        candidate.digest,
        candidate.cache_eligible,
        candidate.reasons
    );
    Ok(Some((
        candidate.digest.to_string(),
        candidate.cache_eligible,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(ready: &[usize], cpu: u64, memory_mb: Option<u64>) -> Admission {
        Admission {
            ready: ready.iter().copied().collect(),
            running: 0,
            free_cpu: cpu,
            free_memory_mb: memory_mb,
        }
    }

    fn weight(cpu: u64, memory_mb: u64) -> Weight {
        Weight { cpu, memory_mb }
    }

    #[test]
    fn termination_keeps_the_worst_outcome() {
        let worst = Termination::Success
            .worst(Termination::FailedProcess)
            .worst(Termination::TimedOut)
            .worst(Termination::FailedProcess);
        assert!(matches!(worst, Termination::TimedOut));
        let worst = worst
            .worst(Termination::HardError("x".to_owned()))
            .worst(Termination::Cancelled);
        assert!(matches!(worst, Termination::HardError(ref message) if message == "x"));
    }

    #[test]
    fn admission_reserves_and_releases_cpu() {
        let weights = [weight(2, 0), weight(1, 0), weight(1, 0)];
        let mut queue = admission(&[0, 1, 2], 3, None);
        assert_eq!(queue.admit(&weights), Some(0));
        assert_eq!(queue.admit(&weights), Some(1));
        assert_eq!(queue.admit(&weights), None, "capacity exhausted");
        queue.release(weights[0]);
        assert_eq!(queue.admit(&weights), Some(2));
        assert_eq!(queue.free_cpu, 1);
    }

    #[test]
    fn admission_is_strict_fifo_so_heavy_actions_are_not_starved() {
        // Head needs 3 slots, only 2 are free: the light action behind it must
        // not jump the queue.
        let weights = [weight(1, 0), weight(3, 0), weight(1, 0)];
        let mut queue = admission(&[0, 1, 2], 3, None);
        assert_eq!(queue.admit(&weights), Some(0));
        assert_eq!(queue.admit(&weights), None, "heavy head blocks backfill");
        queue.release(weights[0]);
        assert_eq!(queue.admit(&weights), Some(1));
        assert_eq!(queue.admit(&weights), None);
        queue.release(weights[1]);
        assert_eq!(queue.admit(&weights), Some(2));
    }

    #[test]
    fn admission_honors_memory_budget_only_when_configured() {
        let weights = [weight(1, 600), weight(1, 600)];
        let mut budgeted = admission(&[0, 1], 4, Some(1000));
        assert_eq!(budgeted.admit(&weights), Some(0));
        assert_eq!(budgeted.admit(&weights), None, "600 + 600 exceeds 1000");
        budgeted.release(weights[0]);
        assert_eq!(budgeted.admit(&weights), Some(1));

        let mut unbounded = admission(&[0, 1], 4, None);
        assert_eq!(unbounded.admit(&weights), Some(0));
        assert_eq!(unbounded.admit(&weights), Some(1));
    }

    #[test]
    fn oversized_requests_are_clamped_to_capacity() {
        let mut action = build_action::Action {
            schema_version: 1,
            id: "big".to_owned(),
            argv: vec!["tool".to_owned()],
            working_directory: PathBuf::from("."),
            inputs: Vec::new(),
            outputs: Vec::new(),
            environment: Vec::new(),
            cache_policy: build_action::CachePolicy::Never,
            toolchain: None,
            timeout_ms: None,
            resources: Some(build_action::ResourceRequest {
                cpu: 16,
                memory_mb: Some(9000),
            }),
        };
        let capacity = Capacity {
            cpu: 4,
            memory_mb: Some(2048),
        };
        assert_eq!(clamp_weight(&action, capacity), (weight(4, 2048), true));
        let unbounded = Capacity {
            cpu: 4,
            memory_mb: None,
        };
        assert_eq!(clamp_weight(&action, unbounded), (weight(4, 9000), true));
        action.resources = None;
        assert_eq!(clamp_weight(&action, capacity), (weight(1, 0), false));
    }
}
