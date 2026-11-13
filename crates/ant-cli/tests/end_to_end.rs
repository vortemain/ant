//! End-to-end validation of the `ant` binary against real plan files.
//!
//! These tests exercise the full CLI path: plan loading, graph validation,
//! sequential execution, digests, cancellation, and timeout reporting.

use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ant-cli-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
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

fn ant() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ant"));
    command.env("RUST_BACKTRACE", "0");
    command
}

fn run_ant(args: &[&str]) -> (i32, String, String) {
    let output = ant().args(args).output().expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn plan_action(id: &str, command: &str, dependencies: &[&str]) -> serde_json::Value {
    json!({
        "action": {
            "schema_version": 1,
            "id": id,
            "argv": if cfg!(windows) {
                vec!["cmd", "/C", command]
            } else {
                vec!["sh", "-c", command]
            },
            "working_directory": ".",
            "inputs": [],
            "outputs": [],
            "environment": [],
            "cache_policy": "never",
            "timeout_ms": 30000
        },
        "dependencies": dependencies,
    })
}

/// A shell command that runs for roughly `seconds - 1` seconds without
/// output: `ping -n` on Windows (one ping per second), `sleep` elsewhere.
fn slow_command(seconds: u32) -> String {
    if cfg!(windows) {
        format!("ping -n {seconds} 127.0.0.1 >nul")
    } else {
        format!("sleep {}", seconds.saturating_sub(1))
    }
}

fn write_plan(dir: &TempDir, actions: &[serde_json::Value]) -> String {
    let plan = json!({ "schema_version": 1, "actions": actions });
    let path = dir.write(
        "plan.json",
        &serde_json::to_string_pretty(&plan).expect("plan json"),
    );
    path.to_string_lossy().into_owned()
}

/// A plan action declaring everything it can: an input, an output, cache
/// policy "eligible", and a well-formed toolchain identity. The only
/// remaining blockers are the executor capability ones (network isolation;
/// unsandboxed executor when --sandbox is not used).
fn cacheable_plan_action(id: &str, command: &str, input: &str, output: &str) -> serde_json::Value {
    json!({
        "action": {
            "schema_version": 1,
            "id": id,
            "argv": if cfg!(windows) {
                vec!["cmd", "/C", command]
            } else {
                vec!["sh", "-c", command]
            },
            "working_directory": ".",
            "inputs": [{ "path": input, "kind": "file" }],
            "outputs": [{ "path": output, "kind": "file" }],
            "environment": [],
            "cache_policy": "eligible",
            "toolchain": {
                "name": "test-toolchain",
                "version": "1.0.0",
                "executable_digest": format!("sha256:{}", "0".repeat(64))
            },
            "timeout_ms": 30000
        },
        "dependencies": [],
    })
}

#[test]
fn version_and_help_commands_succeed() {
    let (code, stdout, _) = run_ant(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Ant"), "stdout was {stdout:?}");

    let (code, stdout, _) = run_ant(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("ant run"), "stdout was {stdout:?}");
}

#[test]
fn plan_executes_actions_in_dependency_order() {
    let dir = TempDir::new("plan-order");
    let plan = write_plan(
        &dir,
        &[
            plan_action("first", "echo first> a.txt", &[]),
            plan_action("second", "echo second> b.txt", &["first"]),
            plan_action("third", "echo third> c.txt", &["second"]),
        ],
    );
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &workspace, "--plan", &plan]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(stdout.contains("action=first status=started"));
    // Dependencies come before dependents in execution order.
    let first_index = stdout.find("action=first status=started").expect("first");
    let second_index = stdout.find("action=second status=started").expect("second");
    let third_index = stdout.find("action=third status=started").expect("third");
    assert!(first_index < second_index && second_index < third_index);
    assert!(
        stdout.contains("key_candidate="),
        "action keys must be reported: {stdout:?}"
    );
}

#[test]
fn plan_rejects_unknown_dependency() {
    let dir = TempDir::new("plan-unknown");
    let plan = write_plan(&dir, &[plan_action("a", "echo hi", &["ghost"])]);
    let (code, _, stderr) = run_ant(&["plan", "validate", "--plan", &plan]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("unknown action 'ghost'"),
        "stderr was {stderr:?}"
    );
}

#[test]
fn plan_rejects_cycle() {
    let dir = TempDir::new("plan-cycle");
    let plan = write_plan(
        &dir,
        &[
            plan_action("a", "echo a", &["b"]),
            plan_action("b", "echo b", &["a"]),
        ],
    );
    let (code, _, stderr) = run_ant(&["plan", "validate", "--plan", &plan]);
    assert_eq!(code, 1);
    assert!(stderr.contains("cycle"), "stderr was {stderr:?}");
}

#[test]
fn plan_describe_shows_contract_without_executing() {
    let dir = TempDir::new("plan-describe");
    let plan = write_plan(&dir, &[plan_action("only", "echo hi", &[])]);
    let (code, stdout, _) = run_ant(&["plan", "describe", "--plan", &plan]);
    assert_eq!(code, 0);
    assert!(stdout.contains("status=valid actions=1"));
    assert!(stdout.contains("action=only dependencies=-"));
    assert!(stdout.contains("cache_policy="));
    // `describe` must not run the command: the echoed file must not exist.
    assert!(
        !dir.path().join("a.txt").exists(),
        "describe must not execute actions"
    );
}

#[test]
fn failed_action_stops_the_plan() {
    let dir = TempDir::new("plan-fail");
    let plan = write_plan(
        &dir,
        &[
            plan_action("fails", "exit 3", &[]),
            plan_action("never-runs", "echo never> never.txt", &["fails"]),
        ],
    );
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, _) = run_ant(&["run", "--workspace", &workspace, "--plan", &plan]);
    assert_eq!(code, 1);
    assert!(stdout.contains("action=fails status=failed exit_code=3"));
    assert!(
        !stdout.contains("action=never-runs status=started"),
        "fail-fast violated: {stdout:?}"
    );
}

#[test]
fn timed_out_plan_action_reports_exit_124() {
    let dir = TempDir::new("plan-timeout");
    let mut action = plan_action("slow", &slow_command(6), &[]);
    action["action"]["timeout_ms"] = json!(500);
    let plan = write_plan(&dir, &[action]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, _) = run_ant(&["run", "--workspace", &workspace, "--plan", &plan]);
    assert_eq!(code, 124, "stdout was {stdout:?}");
    assert!(stdout.contains("status=timed_out"));
}

#[test]
fn cancel_file_stops_plan_with_exit_130() {
    let dir = TempDir::new("plan-cancel");
    let mut action = plan_action("slow", &slow_command(6), &[]);
    action["action"]["timeout_ms"] = json!(30000);
    let plan = write_plan(&dir, &[action]);
    let cancel_file = dir.path().join("stop.marker");
    let workspace = dir.path().to_string_lossy().into_owned();

    let child = ant()
        .args([
            "run",
            "--workspace",
            &workspace,
            "--plan",
            &plan,
            "--cancel-file",
        ])
        .arg(&cancel_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ant run");
    // Give the action time to start, then request cancellation.
    std::thread::sleep(Duration::from_millis(700));
    fs::write(&cancel_file, b"stop").expect("create cancel marker");
    let output = child.wait_with_output().expect("wait for ant");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout was {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status=cancelled"), "stdout was {stdout:?}");
}

#[test]
fn single_command_form_runs_and_reports_digests() {
    let dir = TempDir::new("single-run");
    fs::write(dir.path().join("in.txt"), b"content").expect("input");
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = if cfg!(windows) {
        run_ant(&[
            "run",
            "--workspace",
            &workspace,
            "--cwd",
            ".",
            "--",
            "cmd",
            "/C",
            "copy /Y in.txt out.txt >nul",
        ])
    } else {
        run_ant(&[
            "run",
            "--workspace",
            &workspace,
            "--cwd",
            ".",
            "--",
            "sh",
            "-c",
            "cp in.txt out.txt",
        ])
    };
    assert_eq!(code, 0, "stderr was {stderr:?}");
    assert!(stdout.contains("action=cli-local-run exit_code=0"));
    // The single-command form declares no inputs or outputs, so only the
    // action key candidate is reported.
    assert!(stdout.contains("key_candidate=sha256:"));
    assert!(
        stdout.contains("cache_eligible=false"),
        "default must be non-cacheable"
    );
}

#[test]
fn plan_with_duplicate_action_id_is_rejected() {
    let dir = TempDir::new("plan-dup");
    let plan = write_plan(
        &dir,
        &[
            plan_action("dup", "echo a", &[]),
            plan_action("dup", "echo b", &[]),
        ],
    );
    let (code, _, stderr) = run_ant(&["plan", "validate", "--plan", &plan]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("duplicate action ID"),
        "stderr was {stderr:?}"
    );
}

#[test]
fn parallel_jobs_respect_dependencies() {
    let dir = TempDir::new("par-deps");
    // `first` takes about a second before creating its output; `second` copies
    // it. If dependencies were ignored, second would start first and fail.
    let first = if cfg!(windows) {
        plan_action("first", "ping -n 2 127.0.0.1 >nul & echo x> a.out", &[])
    } else {
        plan_action("first", "sleep 1 && echo x > a.out", &[])
    };
    let second = if cfg!(windows) {
        plan_action("second", "copy /Y a.out b.out >nul", &["first"])
    } else {
        plan_action("second", "cp a.out b.out", &["first"])
    };
    let plan = write_plan(&dir, &[first, second]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "4",
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        dir.path().join("b.out").exists(),
        "dependent must run after its dependency: {stdout:?}"
    );
}

#[test]
fn parallel_jobs_run_independent_actions_concurrently() {
    let dir = TempDir::new("par-overlap");
    // `set-marker` creates its marker immediately and keeps running for ~2s.
    // `wait-for-marker` runs for ~2s and fails if the marker never appeared.
    // Sequential execution would run the waiter first and it would fail; the
    // marker can only exist in time if both actions ran concurrently.
    let setter = if cfg!(windows) {
        plan_action(
            "set-marker",
            "echo x> b.started & ping -n 3 127.0.0.1 >nul",
            &[],
        )
    } else {
        plan_action("set-marker", "touch b.started && sleep 2", &[])
    };
    let waiter = if cfg!(windows) {
        plan_action(
            "wait-for-marker",
            "ping -n 3 127.0.0.1 >nul & if not exist b.started exit 1",
            &[],
        )
    } else {
        plan_action("wait-for-marker", "sleep 2 && test -f b.started", &[])
    };
    let plan = write_plan(&dir, &[setter, waiter]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "2",
    ]);
    assert_eq!(
        code, 0,
        "actions did not overlap; stderr was {stderr:?}; stdout was {stdout:?}"
    );
    assert!(
        dir.path().join("b.started").exists(),
        "stdout was {stdout:?}"
    );
}

#[test]
fn parallel_failure_stops_new_starts_and_reports_skipped() {
    let dir = TempDir::new("par-fail");
    let plan = write_plan(
        &dir,
        &[
            plan_action("fails", "exit 3", &[]),
            plan_action("dependent", "echo never> d.txt", &["fails"]),
            plan_action("independent", &slow_command(3), &[]),
        ],
    );
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, _) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "4",
    ]);
    assert_eq!(code, 1);
    assert!(stdout.contains("action=fails status=failed exit_code=3"));
    assert!(
        stdout.contains("action=dependent status=skipped"),
        "stdout was {stdout:?}"
    );
    assert!(
        !stdout.contains("action=dependent status=started"),
        "fail-fast violated: {stdout:?}"
    );
}

#[test]
fn parallel_timeout_reports_exit_124() {
    let dir = TempDir::new("par-timeout");
    let mut slow_a = plan_action("slow-a", &slow_command(6), &[]);
    let mut slow_b = plan_action("slow-b", &slow_command(6), &[]);
    slow_a["action"]["timeout_ms"] = json!(500);
    slow_b["action"]["timeout_ms"] = json!(500);
    let plan = write_plan(&dir, &[slow_a, slow_b]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, _) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "2",
    ]);
    assert_eq!(code, 124, "stdout was {stdout:?}");
    assert!(stdout.contains("status=timed_out"));
}

#[test]
fn parallel_cancel_file_reports_exit_130() {
    let dir = TempDir::new("par-cancel");
    let plan = write_plan(
        &dir,
        &[
            plan_action("slow-a", &slow_command(6), &[]),
            plan_action("slow-b", &slow_command(6), &[]),
        ],
    );
    let cancel_file = dir.path().join("stop.marker");
    let workspace = dir.path().to_string_lossy().into_owned();

    let child = ant()
        .args([
            "run",
            "--workspace",
            &workspace,
            "--plan",
            &plan,
            "--jobs",
            "2",
            "--cancel-file",
        ])
        .arg(&cancel_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ant run");
    std::thread::sleep(Duration::from_millis(700));
    fs::write(&cancel_file, b"stop").expect("create cancel marker");
    let output = child.wait_with_output().expect("wait for ant");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout was {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status=cancelled"), "stdout was {stdout:?}");
}

#[test]
fn jobs_zero_and_single_command_rejections() {
    let dir = TempDir::new("jobs-arg");
    let plan = write_plan(&dir, &[plan_action("a", "echo hi", &[])]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "0",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--jobs requires a positive integer"),
        "stderr was {stderr:?}"
    );

    let (code, _, stderr) = run_ant(&["run", "--jobs", "2", "--", "cmd", "/C", "rem"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--jobs applies only to plan execution"),
        "stderr was {stderr:?}"
    );
}

fn read_events(path: &Path) -> Vec<build_events::Event> {
    let content = fs::read_to_string(path).expect("read events file");
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each line parses as an event"))
        .collect()
}

#[test]
fn events_file_records_plan_lifecycle() {
    let dir = TempDir::new("events-plan");
    let plan = write_plan(
        &dir,
        &[
            plan_action("first", "echo first> a.txt", &[]),
            plan_action("second", "echo second> b.txt", &["first"]),
        ],
    );
    let workspace = dir.path().to_string_lossy().into_owned();
    let events_path = dir.path().join("events.jsonl");
    let (code, _, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "2",
        "--events",
    ]);
    // --events requires its path argument; rerun correctly.
    assert_eq!(code, 1);
    assert!(stderr.contains("--events requires a file path"));

    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "2",
        "--events",
        events_path.to_string_lossy().as_ref(),
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let events = read_events(&events_path);
    let kinds: Vec<String> = events
        .iter()
        .map(|event| {
            serde_json::to_value(event).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    // Order: run started, both actions started, finished, run finished.
    assert_eq!(kinds.first().map(|kind| kind.as_str()), Some("run_started"));
    assert_eq!(kinds.last().map(|kind| kind.as_str()), Some("run_finished"));
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| kind.as_str() == "action_started")
            .count(),
        2
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| kind.as_str() == "action_finished")
            .count(),
        2
    );
    // All events share one run_id.
    let run_ids: std::collections::HashSet<u64> = events.iter().map(|event| event.run_id).collect();
    assert_eq!(run_ids.len(), 1, "run_id must correlate the run");
    // Run facts: plan mode with jobs, successful outcome, no skips.
    let first = &events[0];
    let value = serde_json::to_value(first).unwrap();
    assert_eq!(value["mode"]["type"], "plan");
    assert_eq!(value["mode"]["jobs"], 2);
    let last = events.last().unwrap();
    let value = serde_json::to_value(last).unwrap();
    assert_eq!(value["outcome"], "succeeded");
    assert_eq!(value["executed"], 2);
    assert_eq!(value["skipped"], 0);
    // Action facts: succeeded status with a key candidate.
    let finished = events
        .iter()
        .find(|event| matches!(event.kind, build_events::EventKind::ActionFinished { .. }))
        .expect("action finished event");
    match &finished.kind {
        build_events::EventKind::ActionFinished {
            status,
            key_candidate,
            cache_eligible,
            error,
            ..
        } => {
            assert_eq!(status, &build_events::ActionStatus::Succeeded);
            assert!(
                key_candidate
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("sha256:")
            );
            assert_eq!(cache_eligible, &Some(false));
            assert_eq!(error, &None);
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[test]
fn events_file_records_failures_and_skips() {
    let dir = TempDir::new("events-fail");
    let plan = write_plan(
        &dir,
        &[
            plan_action("fails", "exit 3", &[]),
            plan_action("dependent", "echo never> d.txt", &["fails"]),
        ],
    );
    let workspace = dir.path().to_string_lossy().into_owned();
    let events_path = dir.path().join("events.jsonl");
    let (code, _, _) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--events",
        events_path.to_string_lossy().as_ref(),
    ]);
    assert_eq!(code, 1);
    let events = read_events(&events_path);
    assert!(events.iter().any(|event| matches!(
        event.kind,
        build_events::EventKind::ActionFinished {
            status: build_events::ActionStatus::Failed,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event.kind,
        build_events::EventKind::ActionSkipped { ref action_id } if action_id == "dependent"
    )));
    let last = events.last().unwrap();
    let value = serde_json::to_value(last).unwrap();
    assert_eq!(value["kind"], "run_finished");
    assert_eq!(value["outcome"], "failed");
    assert_eq!(value["executed"], 1);
    assert_eq!(value["skipped"], 1);
}

#[test]
fn plan_explain_reports_key_blockers_and_cache_entry() {
    let dir = TempDir::new("plan-explain");
    let plan = write_plan(&dir, &[plan_action("only", "echo hi", &[])]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let cache_dir = dir.path().join("cache").to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&[
        "plan",
        "explain",
        "--plan",
        &plan,
        "--workspace",
        &workspace,
        "--cache-dir",
        &cache_dir,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(stdout.contains("mode=explain"), "stdout was {stdout:?}");
    assert!(stdout.contains("status=prepared"), "stdout was {stdout:?}");
    assert!(
        stdout.contains("key_candidate=sha256:"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("cache_eligible=false"),
        "stdout was {stdout:?}"
    );
    // The direct local executor declares no isolation capabilities, so these
    // capability-driven blockers must appear in the diagnostics.
    assert!(
        stdout.contains("blocker=policy-never"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("blocker=missing-toolchain-identity"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("blocker=local-executor-unsandboxed"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("blocker=network-isolation-unavailable"),
        "stdout was {stdout:?}"
    );
    // The executor now captures the platform version and digests input
    // metadata, so these two blockers must be gone:
    assert!(
        !stdout.contains("blocker=platform-version-unspecified"),
        "platform version is captured truthfully; stdout was {stdout:?}"
    );
    assert!(
        !stdout.contains("blocker=filesystem-metadata-not-fingerprintable"),
        "input digests include metadata; stdout was {stdout:?}"
    );
    // No reuse is wired into the run path, so a fresh cache has no entry.
    assert!(
        stdout.contains("cache_entry=absent"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("explain=complete result=prepared"),
        "stdout was {stdout:?}"
    );

    // The same plan without --cache-dir omits the cache line entirely.
    let (code, stdout, _) = run_ant(&[
        "plan",
        "explain",
        "--plan",
        &plan,
        "--workspace",
        &workspace,
    ]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("cache_entry="), "stdout was {stdout:?}");
}

#[test]
fn plan_explain_reports_prepare_failure_and_fails() {
    let dir = TempDir::new("plan-explain-missing");
    // The plan declares an input file that does not exist in the workspace.
    let mut action = plan_action("needs-input", "echo hi", &[]);
    action["action"]["inputs"] = json!([{ "path": "ghost.txt", "kind": "file" }]);
    let plan = write_plan(&dir, &[action]);
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&[
        "plan",
        "explain",
        "--plan",
        &plan,
        "--workspace",
        &workspace,
    ]);
    assert_eq!(code, 1, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("status=prepare_failed"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("explain=complete result=prepare_failed"),
        "stdout was {stdout:?}"
    );
}

/// A mtime-only input change must change the reported action key: the run
/// path digests declared inputs with filesystem metadata. A content-only key
/// would be unchanged here and would wrongly allow reuse.
#[test]
fn run_action_keys_respond_to_metadata_only_input_changes() {
    let dir = TempDir::new("key-mtime");
    dir.write("in.txt", "stable content");
    let mut action = plan_action("build", "echo hi", &[]);
    action["action"]["inputs"] = json!([{ "path": "in.txt", "kind": "file" }]);
    let plan = write_plan(&dir, &[action]);
    let workspace = dir.path().to_string_lossy().into_owned();

    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &workspace, "--plan", &plan]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let first_key = extract_key_candidate(&stdout).expect("first key candidate");

    // Advance the file's modification time without touching its content.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("in.txt"))
        .expect("open input");
    file.set_times(
        std::fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(3600)),
    )
    .expect("set modification time");

    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &workspace, "--plan", &plan]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let second_key = extract_key_candidate(&stdout).expect("second key candidate");
    assert_ne!(
        first_key, second_key,
        "a metadata-only input change must produce a different action key"
    );
}

/// Extract the `key_candidate=sha256:...` value from run output.
fn extract_key_candidate(stdout: &str) -> Option<String> {
    let line = stdout
        .lines()
        .find(|line| line.contains("key_candidate="))?;
    let start = line.find("key_candidate=")? + "key_candidate=".len();
    let rest = &line[start..];
    Some(rest.split_whitespace().next().expect("value").to_owned())
}

#[test]
fn plan_explain_rejects_flags_from_other_subcommands() {
    let dir = TempDir::new("plan-explain-flags");
    let plan = write_plan(&dir, &[plan_action("only", "echo hi", &[])]);
    let (code, _, stderr) = run_ant(&["plan", "validate", "--plan", &plan, "--cache-dir", "cache"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--cache-dir applies only to 'ant plan explain'"),
        "stderr was {stderr:?}"
    );
    let (code, _, stderr) = run_ant(&["plan", "describe", "--plan", &plan, "--workspace", "."]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--workspace applies only to 'ant plan explain'"),
        "stderr was {stderr:?}"
    );
    let (code, _, stderr) = run_ant(&["run", "--cache-dir", "cache", "--", "cmd", "/C", "rem"]);
    assert_eq!(code, 1);
    // The single-command action is always cache_policy "never", so cache
    // participation there would be a silent no-op; it is rejected explicitly.
    assert!(
        stderr.contains("--cache-dir applies only to 'ant run --plan'"),
        "stderr was {stderr:?}"
    );
}

#[test]
fn events_file_records_single_command_run() {
    let dir = TempDir::new("events-single");
    let events_path = dir.path().join("events.jsonl");
    let (code, stdout, stderr) = if cfg!(windows) {
        run_ant(&[
            "run",
            "--events",
            events_path.to_string_lossy().as_ref(),
            "--",
            "cmd",
            "/C",
            "echo hi",
        ])
    } else {
        run_ant(&[
            "run",
            "--events",
            events_path.to_string_lossy().as_ref(),
            "--",
            "sh",
            "-c",
            "echo hi",
        ])
    };
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let events = read_events(&events_path);
    let value = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(value["kind"], "run_started");
    assert_eq!(value["mode"]["type"], "single_command");
    let last = events.last().unwrap();
    let value = serde_json::to_value(last).unwrap();
    assert_eq!(value["kind"], "run_finished");
    assert_eq!(value["outcome"], "succeeded");
    assert_eq!(value["executed"], 1);
}

#[cfg(windows)]
#[test]
fn sandbox_run_writes_inside_workspace_succeeds() {
    let dir = TempDir::new("sandbox-inside");
    let workspace = dir.path().to_string_lossy().into_owned();
    // Single-command form: write a marker file inside the workspace.
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--sandbox",
        "--",
        "cmd",
        "/C",
        "echo confined> marker.txt",
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let marker = dir.path().join("marker.txt");
    assert!(
        marker.exists(),
        "sandboxed run should write inside the workspace"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "confined");
}

#[cfg(windows)]
#[test]
fn sandbox_run_write_outside_workspace_fails_closed() {
    let dir = TempDir::new("sandbox-escape");
    let workspace = dir.path().to_string_lossy().into_owned();
    // An absolute path outside the workspace: the restricted token must
    // refuse the write, the action must fail, and no escape file appears.
    let outside = std::env::temp_dir().join(format!(
        "ant-sandbox-escape-e2e-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    assert!(
        !outside.starts_with(dir.path()),
        "escape target must be outside the workspace"
    );
    let command = format!("echo escaped > {}", outside.display());
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--sandbox",
        "--",
        "cmd",
        "/C",
        &command,
    ]);
    assert_ne!(
        code, 0,
        "sandboxed run should fail when writing outside; got code {code}\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        !Path::new(&outside).exists(),
        "no escape file should be created outside the sandboxed workspace"
    );
    let _ = std::fs::remove_file(&outside);
}

#[cfg(not(any(windows, target_os = "linux")))]
#[test]
fn sandbox_run_fails_closed_on_non_windows() {
    let dir = TempDir::new("sandbox-nonwindows");
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--sandbox",
        "--",
        "sh",
        "-c",
        "true",
    ]);
    // The sandbox is Windows-only; the run must fail closed rather than
    // executing unsandboxed.
    assert_ne!(code, 0);
    assert!(
        stderr.contains("sandbox unavailable"),
        "stderr was {stderr:?}"
    );
}

#[test]
fn sandbox_flag_appears_in_usage() {
    let (_, stdout, _) = run_ant(&["--help"]);
    assert!(stdout.contains("--sandbox"), "stdout was {stdout:?}");
}

#[test]
fn cache_flag_appears_in_usage() {
    let (_, stdout, _) = run_ant(&["--help"]);
    assert!(stdout.contains("--cache-dir"), "stdout was {stdout:?}");
}

/// The D-003 gate must stay closed end to end: even a plan action that
/// declares everything it can (eligible policy, toolchain identity) plus an
/// open cache directory gains no cache entries, because network isolation is
/// unavailable for every executor today.
#[test]
fn cache_participation_keeps_the_gate_closed() {
    let dir = TempDir::new("cache-gate");
    let workspace = dir.path().to_string_lossy().into_owned();
    dir.write("in.txt", "input");
    let plan = write_plan(
        &dir,
        &[cacheable_plan_action(
            "gen",
            "echo cached > out.txt",
            "in.txt",
            "out.txt",
        )],
    );
    let cache = dir.path().join("cache");
    let cache_str = cache.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["run", "--workspace", &workspace];
    // On Windows the sandbox removes the unsandboxed blocker so the run
    // proves the *only* remaining blocker is network isolation.
    if cfg!(windows) {
        args.push("--sandbox");
    }
    args.extend(["--plan", &plan, "--cache-dir", &cache_str]);
    let (code, stdout, stderr) = run_ant(&args);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");

    // The cache directory was opened (structure created)...
    assert!(
        cache.join("entries").is_dir(),
        "--cache-dir must open the cache"
    );
    // ...but the gate stayed closed: the network blocker remains...
    assert!(
        stdout.contains("NetworkIsolationUnavailable"),
        "network isolation must remain a blocker: {stdout:?}"
    );
    if cfg!(windows) {
        assert!(
            !stdout.contains("LocalExecutorUnsandboxed"),
            "--sandbox must remove the unsandboxed blocker: {stdout:?}"
        );
    }
    // ...nothing was reused...
    assert!(
        !stdout.contains("status=reused"),
        "no action may be reused while blockers remain: {stdout:?}"
    );
    // ...and no cache entry was written.
    let entries = std::fs::read_dir(cache.join("entries"))
        .expect("entries dir")
        .count();
    assert_eq!(
        entries, 0,
        "no cache entries may be written while blockers remain"
    );
}

#[test]
fn adapt_flag_appears_in_usage() {
    let (_, stdout, _) = run_ant(&["--help"]);
    assert!(stdout.contains("ant adapt"), "stdout was {stdout:?}");
    assert!(stdout.contains("--adapter"), "stdout was {stdout:?}");
}

#[test]
fn adapt_rejects_unknown_adapter_and_unrecognized_projects() {
    let dir = TempDir::new("adapt-errors");
    let out = dir.path().join("plan.json");
    let out_str = out.to_string_lossy().into_owned();

    let (code, _, stderr) = run_ant(&["adapt", "--adapter", "make", "--out", &out_str]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("unknown adapter 'make'") && stderr.contains("available adapters: cargo"),
        "stderr was {stderr:?}"
    );

    // No Cargo.toml here: detection must fail with a readable reason.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let empty_str = empty.to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &empty_str,
        "--out",
        &out_str,
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("does not recognize project") && stderr.contains("no Cargo.toml"),
        "stderr was {stderr:?}"
    );
    assert!(
        !out.exists(),
        "no plan may be written for an unrecognized project"
    );
}

/// The adapter's plan must be a real vertical slice: generate, execute, and
/// produce the declared artifact through the ordinary `ant run --plan` path.
#[test]
fn adapt_generates_and_runs_a_cargo_project() {
    let dir = TempDir::new("adapt-cargo");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"hello-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .expect("write main.rs");
    let project = dir.path().to_string_lossy().into_owned();
    let plan = dir.path().join("plan.json");
    let plan_str = plan.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &project,
        "--out",
        &plan_str,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(stdout.contains("adapter=cargo"), "stdout was {stdout:?}");
    assert!(stdout.contains("actions=1"), "stdout was {stdout:?}");
    assert!(plan.exists(), "plan file must be written");

    // The generated plan is loadable and valid.
    let (code, stdout, stderr) = run_ant(&["plan", "validate", "--plan", &plan_str]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(stdout.contains("action=hello-app"), "stdout was {stdout:?}");

    // Executing it builds the real crate and the declared artifact exists.
    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &project, "--plan", &plan_str]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("action=hello-app status=succeeded"),
        "stdout was {stdout:?}"
    );
    // The toolchain identity is real, so no missing-toolchain blocker.
    assert!(
        !stdout.contains("MissingToolchainIdentity"),
        "a real cargo toolchain identity must be declared: {stdout:?}"
    );
    let artifact = dir
        .path()
        .join("target/debug")
        .join(format!("hello-app{}", std::env::consts::EXE_SUFFIX));
    assert!(
        artifact.is_file(),
        "declared artifact must exist after the run"
    );

    // And re-adapting is deterministic: same plan content.
    let plan2 = dir.path().join("plan2.json");
    let plan2_str = plan2.to_string_lossy().into_owned();
    let (code, _, _) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &project,
        "--out",
        &plan2_str,
    ]);
    assert_eq!(code, 0);
    let first = std::fs::read_to_string(&plan).expect("read plan");
    let second = std::fs::read_to_string(&plan2).expect("read plan2");
    assert_eq!(first, second, "re-adaptation must be deterministic");
}

/// A generated plan must be runnable under `ant run --sandbox`: the adapter
/// redirects toolchain scratch writes (linker temp files, rustc temporaries)
/// into a workspace-relative staging directory, so the confined child can
/// compile and link a real two-member path-dependency workspace end to end.
#[test]
fn adapt_plan_runs_sandboxed_for_a_path_dep_workspace() {
    let dir = TempDir::new("adapt-sandbox");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"2\"\n",
    )
    .expect("write workspace manifest");
    std::fs::create_dir_all(dir.path().join("core/src")).expect("mkdir core/src");
    std::fs::write(
        dir.path().join("core/Cargo.toml"),
        "[package]\nname = \"core-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"core_lib\"\n",
    )
    .expect("write core manifest");
    std::fs::write(
        dir.path().join("core/src/lib.rs"),
        "pub fn core() -> u32 { 1 }\n",
    )
    .expect("write lib.rs");
    std::fs::create_dir_all(dir.path().join("app/src")).expect("mkdir app/src");
    std::fs::write(
        dir.path().join("app/Cargo.toml"),
        "[package]\nname = \"app-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore-lib = { path = \"../core\" }\n",
    )
    .expect("write app manifest");
    std::fs::write(
        dir.path().join("app/src/main.rs"),
        "fn main() { println!(\"core={}\", core_lib::core()); }\n",
    )
    .expect("write main.rs");
    let project = dir.path().to_string_lossy().into_owned();
    let plan = dir.path().join("plan.json");
    let plan_str = plan.to_string_lossy().into_owned();

    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &project,
        "--out",
        &plan_str,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}");

    // Generation prepared the staging directory generated actions point at.
    assert!(
        dir.path().join(".ant/tmp").is_dir(),
        "adapt must prepare the .ant/tmp staging directory"
    );

    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &project,
        "--sandbox",
        "--plan",
        &plan_str,
    ]);
    // Windows: restricted-token sandbox; Linux: bubblewrap sandbox (D-018).
    if cfg!(any(windows, target_os = "linux")) {
        assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
        // Both members built in DAG order under confinement.
        assert!(
            stdout.contains("action=core-lib status=succeeded")
                && stdout.contains("action=app-bin status=succeeded"),
            "both members must succeed sandboxed: {stdout:?}"
        );
        assert!(
            dir.path().join("target/debug/libcore_lib.rlib").is_file(),
            "core lib artifact must exist"
        );
        assert!(
            dir.path()
                .join("target/debug")
                .join(format!("app-bin{}", std::env::consts::EXE_SUFFIX))
                .is_file(),
            "app artifact must exist"
        );
        // The successful confined link is itself the proof that scratch
        // writes were redirected: without TMP/TEMP pointing into the
        // workspace the MSVC linker fails with LNK1104 on its %TEMP%
        // response files. (The linker deletes those files after use, so the
        // staging directory may legitimately be empty afterwards.)
    } else {
        // Other platforms: `--sandbox` fails closed before any action runs.
        assert_ne!(code, 0, "sandbox must fail closed without a sandbox");
        assert!(
            stderr.contains("sandbox unavailable"),
            "stderr was {stderr:?}"
        );
    }
}

/// The npm adapter is a real second ecosystem: `ant adapt --adapter npm`
/// generates a validated plan from a real `package.json`, and `ant run`
/// executes the build script through the real node toolchain and produces a
/// real artifact. This proves the adapter contract generalizes beyond cargo.
#[test]
fn adapt_generates_and_runs_an_npm_project() {
    let dir = TempDir::new("adapt-npm");
    // A real npm project whose build script needs no `npm install` (no
    // registry dependencies), so the test is network-free and deterministic —
    // the same standard as the no-deps cargo crate in the cargo e2e test.
    dir.write(
        "package.json",
        "{\"name\":\"hello-app\",\"version\":\"0.1.0\",\"scripts\":{\"build\":\"node build.js\"}}",
    );
    dir.write("build.js", "const fs = require('fs'); fs.mkdirSync('dist'); fs.writeFileSync('dist/out.txt', 'hello from npm');\n");
    let project = dir.path().to_string_lossy().into_owned();
    let plan = dir.path().join("plan.json");
    let plan_str = plan.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "npm",
        "--project",
        &project,
        "--out",
        &plan_str,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(stdout.contains("adapter=npm"), "stdout was {stdout:?}");
    assert!(stdout.contains("actions=1"), "stdout was {stdout:?}");
    assert!(plan.exists(), "plan file must be written");

    // The generated plan is loadable and valid.
    let (code, stdout, stderr) = run_ant(&["plan", "validate", "--plan", &plan_str]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(stdout.contains("action=hello-app"), "stdout was {stdout:?}");

    // Executing it runs the build through the real node toolchain.
    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &project, "--plan", &plan_str]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("action=hello-app status=succeeded"),
        "stdout was {stdout:?}"
    );
    // The real toolchain identity is declared, so no missing-toolchain blocker.
    assert!(
        !stdout.contains("MissingToolchainIdentity"),
        "a real node toolchain identity must be declared: {stdout:?}"
    );
    // The build script ran and produced the real artifact. (The npm adapter
    // intentionally declares no outputs — npm has no standard layout — so the
    // artifact is not verified by the plan; the test verifies it directly.)
    let artifact = dir.path().join("dist/out.txt");
    assert!(
        artifact.is_file(),
        "build artifact must exist after the run"
    );
    assert_eq!(
        std::fs::read_to_string(&artifact).expect("read artifact"),
        "hello from npm"
    );

    // Re-adapting is deterministic: same plan content.
    let plan2 = dir.path().join("plan2.json");
    let plan2_str = plan2.to_string_lossy().into_owned();
    let (code, _, _) = run_ant(&[
        "adapt",
        "--adapter",
        "npm",
        "--project",
        &project,
        "--out",
        &plan2_str,
    ]);
    assert_eq!(code, 0);
    let first = std::fs::read_to_string(&plan).expect("read plan");
    let second = std::fs::read_to_string(&plan2).expect("read plan2");
    assert_eq!(first, second, "re-adaptation must be deterministic");
}

/// The npm adapter fails honestly when there is no `build` script, and the
/// error message points at the real cause rather than emitting an invalid plan.
#[test]
fn adapt_npm_rejects_project_without_a_build_script() {
    let dir = TempDir::new("adapt-npm-no-build");
    dir.write(
        "package.json",
        "{\"name\":\"no-build\",\"version\":\"0.1.0\",\"scripts\":{\"test\":\"node test.js\"}}",
    );
    let project = dir.path().to_string_lossy().into_owned();
    let out = dir.path().join("plan.json");
    let out_str = out.to_string_lossy().into_owned();

    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "npm",
        "--project",
        &project,
        "--out",
        &out_str,
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("no `build` script"),
        "stderr was {stderr:?}"
    );
    assert!(
        !out.exists(),
        "no plan may be written without a build script"
    );
}

/// Without `--adapter`, `ant adapt` uses the single adapter that recognizes
/// the project, and the generated plan runs through the ordinary path.
#[test]
fn adapt_auto_selects_the_single_matching_adapter() {
    let dir = TempDir::new("adapt-auto");
    dir.write(
        "package.json",
        "{\"name\":\"auto-app\",\"version\":\"0.1.0\",\"scripts\":{\"build\":\"node build.js\"}}",
    );
    dir.write(
        "build.js",
        "require('fs').writeFileSync('auto-out.txt', 'auto');\n",
    );
    let project = dir.path().to_string_lossy().into_owned();
    let plan = dir.path().join("plan.json");
    let plan_str = plan.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run_ant(&["adapt", "--project", &project, "--out", &plan_str]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("adapter=npm") && stdout.contains("selection=auto"),
        "stdout was {stdout:?}"
    );

    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &project, "--plan", &plan_str]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert_eq!(
        fs::read_to_string(dir.path().join("auto-out.txt")).expect("artifact"),
        "auto"
    );
}

/// Auto-selection never guesses: several matches or none fail with the
/// reasons, and no plan file is written.
#[test]
fn adapt_auto_selection_fails_closed_on_ambiguity_and_no_match() {
    let dir = TempDir::new("adapt-auto-errors");
    let out = dir.path().join("plan.json");
    let out_str = out.to_string_lossy().into_owned();

    let both = dir.path().join("both");
    fs::create_dir_all(&both).expect("mkdir");
    fs::write(both.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
    fs::write(both.join("package.json"), "{}").expect("write");
    let both_str = both.to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&["adapt", "--project", &both_str, "--out", &out_str]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("multiple adapters recognize the project (cargo, npm)")
            && stderr.contains("--adapter"),
        "stderr was {stderr:?}"
    );

    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).expect("mkdir");
    let empty_str = empty.to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&["adapt", "--project", &empty_str, "--out", &out_str]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("no adapter recognizes the project")
            && stderr.contains("cargo: no Cargo.toml")
            && stderr.contains("npm: no package.json"),
        "stderr was {stderr:?}"
    );

    let missing = dir.path().join("does-not-exist");
    let missing_str = missing.to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&["adapt", "--project", &missing_str, "--out", &out_str]);
    assert_eq!(code, 1);
    assert!(stderr.contains("does not exist"), "stderr was {stderr:?}");

    assert!(!out.exists(), "no plan may be written when selection fails");
}

/// `ant adapt --detect` reports every adapter's outcome, writes nothing, and
/// exits 1 only when no adapter recognizes the project.
#[test]
fn adapt_detect_reports_every_adapter_without_writing() {
    let dir = TempDir::new("adapt-detect");
    dir.write("Cargo.toml", "[package]\nname = \"x\"\n");
    let project = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&["adapt", "--detect", "--project", &project]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("detect adapter=cargo supported=true"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("detect adapter=npm supported=false reason=no package.json"),
        "stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("supported_adapters=1"),
        "stdout was {stdout:?}"
    );
    assert!(
        !dir.path().join(".ant").exists(),
        "detection must not write anything"
    );

    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).expect("mkdir");
    let empty_str = empty.to_string_lossy().into_owned();
    let (code, stdout, _) = run_ant(&["adapt", "--detect", "--project", &empty_str]);
    assert_eq!(code, 1);
    assert!(
        stdout.contains("supported_adapters=0"),
        "stdout was {stdout:?}"
    );

    let out_str = dir.path().join("plan.json").to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--detect",
        "--project",
        &project,
        "--out",
        &out_str,
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--detect cannot be combined"),
        "stderr was {stderr:?}"
    );
}

/// An action that fails if a sibling ran at the same time: it creates its own
/// marker, holds it ~1s, fails if the other's marker exists, then removes its
/// own. Two of these overlap only if both fail.
fn exclusive_probe(id: &str, mine: &str, other: &str) -> serde_json::Value {
    if cfg!(windows) {
        plan_action(
            id,
            &format!(
                "echo x> {mine} & ping -n 2 127.0.0.1 >nul & if exist {other} (del {mine} & exit 1) else (del {mine})"
            ),
            &[],
        )
    } else {
        plan_action(
            id,
            &format!(
                "touch {mine} && sleep 1 && if [ -e {other} ]; then rm -f {mine}; exit 1; fi; rm -f {mine}"
            ),
            &[],
        )
    }
}

fn with_resources(
    mut action: serde_json::Value,
    resources: serde_json::Value,
) -> serde_json::Value {
    action["action"]["resources"] = resources;
    action
}

/// Resource weights are admission weights: two actions that each request the
/// whole CPU capacity never overlap, while the same plan with default weights
/// does (the control proves the probe detects overlap at all).
#[test]
fn parallel_cpu_weights_serialize_heavy_actions() {
    let dir = TempDir::new("par-cpu-weight");
    let workspace = dir.path().to_string_lossy().into_owned();

    let control = write_plan(
        &dir,
        &[
            exclusive_probe("a", "a.run", "b.run"),
            exclusive_probe("b", "b.run", "a.run"),
        ],
    );
    let (code, stdout, _) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &control,
        "--jobs",
        "2",
    ]);
    assert_eq!(
        code, 1,
        "control: default-weight actions must overlap and be detected; stdout was {stdout:?}"
    );
    let _ = fs::remove_file(dir.path().join("a.run"));
    let _ = fs::remove_file(dir.path().join("b.run"));

    let weighted = write_plan(
        &dir,
        &[
            with_resources(exclusive_probe("a", "a.run", "b.run"), json!({ "cpu": 2 })),
            with_resources(exclusive_probe("b", "b.run", "a.run"), json!({ "cpu": 2 })),
        ],
    );
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &weighted,
        "--jobs",
        "2",
    ]);
    assert_eq!(
        code, 0,
        "cpu=2 actions overlapped under --jobs 2; stderr was {stderr:?}; stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("action=a status=succeeded")
            && stdout.contains("action=b status=succeeded"),
        "{stdout:?}"
    );
}

/// A memory budget serializes actions whose declared memory does not fit
/// together, and an over-capacity request is clamped (with a note) instead of
/// deadlocking the run.
#[test]
fn parallel_memory_budget_and_clamping() {
    let dir = TempDir::new("par-mem-weight");
    let workspace = dir.path().to_string_lossy().into_owned();
    let plan = write_plan(
        &dir,
        &[
            with_resources(
                exclusive_probe("a", "a.run", "b.run"),
                json!({ "memory_mb": 600 }),
            ),
            with_resources(
                exclusive_probe("b", "b.run", "a.run"),
                json!({ "cpu": 8, "memory_mb": 600 }),
            ),
        ],
    );
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--jobs",
        "4",
        "--memory-mb",
        "1000",
    ]);
    assert_eq!(
        code, 0,
        "600+600 MiB must not fit a 1000 MiB budget; stderr was {stderr:?}; stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("ant: note: action=b requests cpu=8 memory_mb=600 exceeding capacity cpu=4 memory_mb=1000; clamped to cpu=4 memory_mb=600"),
        "stdout was {stdout:?}"
    );
}

#[test]
fn resource_requests_and_flags_are_validated() {
    let dir = TempDir::new("res-validate");
    let workspace = dir.path().to_string_lossy().into_owned();
    let plan = write_plan(
        &dir,
        &[with_resources(
            plan_action("zero", "echo hi", &[]),
            json!({ "cpu": 0 }),
        )],
    );
    let (code, _, stderr) = run_ant(&["plan", "validate", "--plan", &plan]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("InvalidResources") || stderr.to_lowercase().contains("resource"),
        "stderr was {stderr:?}"
    );

    let plan = write_plan(
        &dir,
        &[with_resources(
            plan_action("bogus", "echo hi", &[]),
            json!({ "gpu": 1 }),
        )],
    );
    let (code, _, _) = run_ant(&["plan", "validate", "--plan", &plan]);
    assert_eq!(code, 1, "unknown resource fields must be rejected");

    let ok = write_plan(&dir, &[plan_action("ok", "echo hi", &[])]);
    let (code, _, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &ok,
        "--memory-mb",
        "0",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--memory-mb requires a positive integer"),
        "stderr was {stderr:?}"
    );
    let (code, _, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--memory-mb",
        "100",
        "--",
        "cmd",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--memory-mb applies only to plan execution"),
        "stderr was {stderr:?}"
    );
}

fn keep_going_plan(dir: &TempDir) -> String {
    // `bad` fails; `child` and `grandchild` sit downstream of it; `independent`
    // does not depend on it. Ready actions start in action-ID order, so
    // without keep-going `independent` would be skipped after `bad` fails.
    write_plan(
        dir,
        &[
            plan_action("bad", "exit 3", &[]),
            plan_action("child", "echo child> child.txt", &["bad"]),
            plan_action("grandchild", "echo grandchild> grandchild.txt", &["child"]),
            plan_action("independent", "echo ok> independent.txt", &[]),
        ],
    )
}

#[test]
fn keep_going_runs_independent_actions_and_skips_dependents() {
    for jobs in ["1", "2"] {
        let dir = TempDir::new(&format!("keep-going-{jobs}"));
        let plan = keep_going_plan(&dir);
        let workspace = dir.path().to_string_lossy().into_owned();

        // Control: the default stops new starts at the first failure.
        let (code, stdout, _) = run_ant(&[
            "run",
            "--workspace",
            &workspace,
            "--plan",
            &plan,
            "--jobs",
            "1",
        ]);
        assert_eq!(code, 1);
        assert!(
            stdout.contains("action=independent status=skipped"),
            "control stdout was {stdout:?}"
        );
        assert!(!dir.path().join("independent.txt").exists());

        let (code, stdout, stderr) = run_ant(&[
            "run",
            "--workspace",
            &workspace,
            "--plan",
            &plan,
            "--jobs",
            jobs,
            "--keep-going",
        ]);
        assert_eq!(
            code, 1,
            "jobs={jobs}: a failure must still fail the run; stderr was {stderr:?}"
        );
        assert!(
            stdout.contains("action=bad status=failed"),
            "jobs={jobs}: {stdout:?}"
        );
        assert!(
            stdout.contains("action=independent status=succeeded"),
            "jobs={jobs}: {stdout:?}"
        );
        assert!(
            dir.path().join("independent.txt").exists(),
            "jobs={jobs}: independent action must run"
        );
        assert!(
            stdout.contains("action=child status=skipped"),
            "jobs={jobs}: {stdout:?}"
        );
        assert!(
            stdout.contains("action=grandchild status=skipped"),
            "jobs={jobs}: {stdout:?}"
        );
        assert!(
            !dir.path().join("child.txt").exists() && !dir.path().join("grandchild.txt").exists()
        );
        assert!(
            !stdout.contains("action=bad status=skipped"),
            "a failed action is not skipped: {stdout:?}"
        );
    }
}

#[test]
fn keep_going_reports_the_worst_outcome_and_is_plan_only() {
    let dir = TempDir::new("keep-going-worst");
    let mut slow = if cfg!(windows) {
        plan_action("a-slow", "ping -n 5 127.0.0.1 >nul", &[])
    } else {
        plan_action("a-slow", "sleep 4", &[])
    };
    slow["action"]["timeout_ms"] = json!(500);
    let plan = write_plan(
        &dir,
        &[
            slow,
            plan_action("b-fails", "exit 1", &[]),
            plan_action("c-ok", "echo ok> c.txt", &[]),
        ],
    );
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--keep-going",
    ]);
    assert_eq!(
        code, 124,
        "a timeout outranks a plain failure; stderr was {stderr:?}; stdout was {stdout:?}"
    );
    assert!(dir.path().join("c.txt").exists(), "stdout was {stdout:?}");

    let (code, _, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--keep-going",
        "--",
        "cmd",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--keep-going applies only to plan execution"),
        "stderr was {stderr:?}"
    );
}

/// PowerShell action that commits `alloc_mb` MiB on top of its own baseline
/// (~80 MiB) and exits nonzero if the allocation is refused.
#[cfg(windows)]
fn alloc_action(alloc_mb: u64, memory_mb: u64) -> serde_json::Value {
    json!({
        "action": {
            "schema_version": 1,
            "id": "alloc",
            "argv": ["powershell", "-NoProfile", "-NonInteractive", "-Command",
                format!("$ErrorActionPreference='Stop'; $b = New-Object byte[] ({alloc_mb}MB); $b[0]=1; exit 0")],
            "working_directory": ".",
            "inputs": [], "outputs": [], "environment": [],
            "cache_policy": "never",
            // Generous: a fresh CI runner's first PowerShell start can be slow.
            "timeout_ms": 180000,
            "resources": { "memory_mb": memory_mb }
        },
        "dependencies": []
    })
}

/// `--enforce-memory` makes a declared `memory_mb` a real job-wide cap on
/// both executor paths: an allocation that fits succeeds, one that would
/// exceed the cap is refused inside the process, and the same plan without
/// the flag is unaffected (the control).
#[cfg(windows)]
#[test]
fn enforce_memory_caps_committed_memory() {
    // Warm PowerShell up once, uncapped and outside Ant, so first-run module
    // preparation on a fresh machine is not timed inside the capped runs.
    let _ = Command::new("powershell")
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .status();
    let dir = TempDir::new("enforce-memory");
    let workspace = dir.path().to_string_lossy().into_owned();

    let fits = write_plan(&dir, &[alloc_action(20, 400)]);
    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &fits,
        "--enforce-memory",
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("memory_limit_bytes=419430400 memory_limit_hit=false"),
        "stdout was {stdout:?}"
    );

    let too_big = write_plan(&dir, &[alloc_action(600, 300)]);
    let (code, stdout, _) = run_ant(&["run", "--workspace", &workspace, "--plan", &too_big]);
    assert_eq!(
        code, 0,
        "control: without the flag the allocation succeeds; stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("memory_limit_bytes=none"),
        "stdout was {stdout:?}"
    );

    for extra in [&[][..], &["--sandbox"][..]] {
        let mut args = vec![
            "run",
            "--workspace",
            &workspace,
            "--plan",
            &too_big,
            "--enforce-memory",
        ];
        args.extend_from_slice(extra);
        let (code, stdout, _) = run_ant(&args);
        assert_eq!(
            code, 1,
            "{extra:?}: the cap must refuse the allocation; stdout was {stdout:?}"
        );
        assert!(
            stdout.contains("action=alloc status=failed"),
            "{extra:?}: {stdout:?}"
        );
        assert!(
            stdout.contains("memory_limit_hit=true"),
            "{extra:?}: {stdout:?}"
        );
    }
}

#[test]
fn enforce_memory_is_plan_only_and_fails_closed_off_windows() {
    let dir = TempDir::new("enforce-memory-flags");
    let workspace = dir.path().to_string_lossy().into_owned();
    let (code, _, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--enforce-memory",
        "--",
        "cmd",
    ]);
    assert_eq!(code, 1);
    if cfg!(windows) {
        assert!(
            stderr.contains("--enforce-memory applies only to plan execution"),
            "stderr was {stderr:?}"
        );
    } else {
        assert!(
            stderr.contains("Windows job objects only") || stderr.contains("plan execution"),
            "stderr was {stderr:?}"
        );
    }
}

/// Plans written by Windows PowerShell 5 (`Set-Content -Encoding UTF8`)
/// start with a UTF-8 BOM; they must load.
#[test]
fn plan_with_utf8_bom_loads() {
    let dir = TempDir::new("plan-bom");
    let plan = json!({ "schema_version": 1, "actions": [plan_action("bom", "echo ok", &[])] });
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend(serde_json::to_vec(&plan).expect("json"));
    let path = dir.path().join("plan.json");
    fs::write(&path, bytes).expect("write");
    let (code, stdout, stderr) = run_ant(&["plan", "validate", "--plan", &path.to_string_lossy()]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
}

/// Network-isolation tests need the one-time elevated `ant sandbox setup`
/// on this machine. Without it they skip (printing why) rather than fail:
/// they prove nothing on machines without the setup.
#[cfg(windows)]
fn network_isolation_ready() -> bool {
    let (code, stdout, _) = run_ant(&["sandbox", "status"]);
    let ready = code == 0 && stdout.contains("ready=true");
    if !ready {
        eprintln!(
            "skipping: network isolation not set up on this machine (run 'ant sandbox setup' elevated)"
        );
    }
    ready
}

#[cfg(windows)]
fn system_action(id: &str, argv: &[&str], cache_policy: &str) -> serde_json::Value {
    json!({
        "action": {
            "schema_version": 1,
            "id": id,
            "argv": argv,
            "working_directory": ".",
            "inputs": [], "outputs": [],
            "environment": [["TEMP", r".ant\tmp"], ["TMP", r".ant\tmp"]],
            "cache_policy": cache_policy,
            "timeout_ms": 60000
        },
        "dependencies": []
    })
}

/// Runs as the sandbox account, and outbound connections are refused by the
/// account-scoped firewall rules while the same action connects without
/// isolation (the control; needs outbound internet to 1.1.1.1:443).
#[cfg(windows)]
#[test]
fn network_isolated_runs_as_sandbox_account_and_blocks_connections() {
    if !network_isolation_ready() {
        return;
    }
    let dir = TempDir::new("net-iso");
    fs::create_dir_all(dir.path().join(".ant").join("tmp")).expect("staging");
    let workspace = dir.path().to_string_lossy().into_owned();
    let mut who = system_action(
        "a-who",
        &[
            r"C:\Windows\System32\cmd.exe",
            "/C",
            r"C:\Windows\System32\whoami.exe > who.txt",
        ],
        "never",
    );
    who["action"]["outputs"] = json!([{ "path": "who.txt", "kind": "file" }]);
    let curl = system_action(
        "b-curl",
        &[
            r"C:\Windows\System32\curl.exe",
            "-sS",
            "-m",
            "15",
            "-o",
            "NUL",
            "https://1.1.1.1/",
        ],
        "never",
    );
    let plan = write_plan(&dir, &[who, curl]);

    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--keep-going",
    ]);
    assert_eq!(
        code, 0,
        "control: connection must succeed without isolation; stderr {stderr:?} stdout {stdout:?}"
    );
    assert!(
        !fs::read_to_string(dir.path().join("who.txt"))
            .expect("who")
            .to_lowercase()
            .contains("antsandboxoffline")
    );

    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &workspace,
        "--plan",
        &plan,
        "--keep-going",
        "--network-isolated",
    ]);
    assert_eq!(
        code, 1,
        "the connection must fail under isolation; stderr {stderr:?} stdout {stdout:?}"
    );
    assert!(
        stdout.contains("action=a-who status=succeeded"),
        "{stdout:?}"
    );
    assert!(
        stdout.contains("action=b-curl status=failed exit_code=7"),
        "curl exit 7 = connect refused; {stdout:?}"
    );
    assert!(
        fs::read_to_string(dir.path().join("who.txt"))
            .expect("who")
            .to_lowercase()
            .contains("antsandboxoffline"),
        "the action must run as the sandbox account"
    );
    assert!(
        stdout.contains("blockers=[PolicyNever, MissingToolchainIdentity]"),
        "only plan-level blockers remain; {stdout:?}"
    );
}

/// The cache gate opens under network isolation: a fully declared action is
/// stored, reused with its output restored after deletion, and rebuilt when
/// its input changes. The same plan under `--sandbox` alone stays gated.
#[cfg(windows)]
#[test]
fn network_isolated_cache_reuses_and_invalidates() {
    if !network_isolation_ready() {
        return;
    }
    let dir = TempDir::new("net-iso-cache");
    let ws = dir.path().join("ws");
    fs::create_dir_all(ws.join(".ant").join("tmp")).expect("staging");
    fs::write(ws.join("in.txt"), "hello cache").expect("input");
    let cmd = r"C:\Windows\System32\cmd.exe";
    let digest = build_digest::digest_file(std::path::Path::new(cmd)).expect("digest cmd.exe");
    let mut action = system_action(
        "copy",
        &[cmd, "/C", "copy /Y in.txt out.txt >nul"],
        "eligible",
    );
    action["action"]["inputs"] = json!([{ "path": "in.txt", "kind": "file" }]);
    action["action"]["outputs"] = json!([{ "path": "out.txt", "kind": "file" }]);
    // Truthful identity: the real digest of the executable that runs.
    action["action"]["toolchain"] = json!({
        "name": "cmd",
        "version": "system32",
        "executable_digest": digest.to_string()
    });
    let plan_dir = TempDir::new("net-iso-cache-plan");
    let plan = write_plan(&plan_dir, &[action]);
    let workspace = ws.to_string_lossy().into_owned();
    let cache = dir.path().join("cache").to_string_lossy().into_owned();
    let run = |extra: &str| {
        run_ant(&[
            "run",
            "--workspace",
            &workspace,
            "--plan",
            &plan,
            "--cache-dir",
            &cache,
            extra,
        ])
    };

    let (code, stdout, stderr) = run("--sandbox");
    assert_eq!(code, 0, "stderr {stderr:?}");
    assert!(
        stdout.contains("blockers=[NetworkIsolationUnavailable]"),
        "control stays gated: {stdout:?}"
    );

    let (code, stdout, stderr) = run("--network-isolated");
    assert_eq!(code, 0, "stderr {stderr:?}; stdout {stdout:?}");
    assert!(
        stdout.contains("action=copy status=succeeded")
            && stdout.contains("cache_eligible=true blockers=[]"),
        "{stdout:?}"
    );

    fs::remove_file(ws.join("out.txt")).expect("delete output");
    let (code, stdout, stderr) = run("--network-isolated");
    assert_eq!(code, 0, "stderr {stderr:?}");
    assert!(stdout.contains("action=copy status=reused"), "{stdout:?}");
    assert_eq!(
        fs::read_to_string(ws.join("out.txt")).expect("restored"),
        "hello cache"
    );

    fs::write(ws.join("in.txt"), "changed input").expect("change");
    let (code, stdout, _) = run("--network-isolated");
    assert_eq!(code, 0);
    assert!(
        stdout.contains("action=copy status=succeeded"),
        "a changed input must rebuild: {stdout:?}"
    );
    assert_eq!(
        fs::read_to_string(ws.join("out.txt")).expect("rebuilt"),
        "changed input"
    );
}
