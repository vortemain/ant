//! Contract tests for the `--events` JSONL stream as it is written by the
//! real binary (schema v3): one correlated run, and `action_finished`
//! carrying the measured peak memory of the spawned process.

use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn run_ant(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .env("RUST_BACKTRACE", "0")
        .args(args)
        .output()
        .expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn plan_action(id: &str, command: &str) -> serde_json::Value {
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
        "dependencies": [],
    })
}

fn write_plan(dir: &TempDir, actions: &[serde_json::Value]) -> String {
    let plan = json!({ "schema_version": 1, "actions": actions });
    let path = dir.write(
        "plan.json",
        &serde_json::to_string_pretty(&plan).expect("plan json"),
    );
    path.to_string_lossy().into_owned()
}

fn read_events(path: &Path) -> Vec<build_events::Event> {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each line is one event"))
        .collect()
}

#[test]
fn events_stream_is_schema_v3_with_measured_peak_memory() {
    let dir = TempDir::new("events-peak");
    let plan = write_plan(&dir, &[plan_action("mem", "echo hi> out.txt")]);
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
    assert_eq!(code, 0);
    let events = read_events(&events_path);

    // The stream is correlated and stamped with the current schema version.
    assert!(
        events
            .iter()
            .all(|event| event.schema_version == build_events::EVENT_SCHEMA_VERSION),
        "every event must carry the current schema version"
    );
    let run_ids: std::collections::HashSet<u64> = events.iter().map(|event| event.run_id).collect();
    assert_eq!(run_ids.len(), 1, "run_id must correlate the run");

    // The spawned action reports its measured peak memory. The local
    // executor measures it through the job object on Windows and cannot
    // elsewhere today, which surfaces as `null` rather than a guess.
    let finished = events
        .iter()
        .find(|event| matches!(event.kind, build_events::EventKind::ActionFinished { .. }))
        .expect("action finished event");
    let peak = match &finished.kind {
        build_events::EventKind::ActionFinished {
            peak_memory_bytes, ..
        } => *peak_memory_bytes,
        other => panic!("unexpected event {other:?}"),
    };
    let value = serde_json::to_value(finished).unwrap();
    assert!(
        value.get("peak_memory_bytes").is_some(),
        "the field must be serialized, was {value}"
    );
    #[cfg(windows)]
    assert!(peak.is_some(), "peak memory must be measured on Windows");
    #[cfg(not(windows))]
    assert_eq!(peak, None, "non-Windows executors report null today");
}
