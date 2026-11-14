//! Structured, correlated build event contract (schema version 1).
//!
//! Events carry measured facts only: what started, what finished with which
//! exit status, which digests were observed, and how the run ended. They do
//! not carry estimates, interpretations, or secret values. Consumers receive
//! one JSON object per line so streams append safely.

use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bump when the event contract changes shape or meaning.
/// Version of the JSONL event contract. History:
/// - 1: initial contract (`run_started`/`action_started`/`action_finished`/
///   `action_skipped`/`run_finished`).
/// - 2: `action_finished` gained the `reused` status for actions satisfied
///   from a verified cache entry instead of a spawned process. Readers that
///   must accept every v1 file unchanged should treat `reused` as an
///   additional terminal status.
/// - 3: `action_finished` gained `peak_memory_bytes`, the measured peak
///   memory of the spawned action process, `null` when no process was
///   spawned (`reused`) or the executor cannot measure it (non-Windows local
///   execution today). Older files simply lack the field.
pub const EVENT_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    /// Stable identifier for one invocation, shared by every event it emits.
    pub run_id: u64,
    /// Wall-clock time of the event; measured fact, not an estimate.
    pub timestamp_unix_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(run_id: u64, kind: EventKind) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            run_id,
            timestamp_unix_ms: unix_time_ms(),
            kind,
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    RunStarted {
        mode: RunMode,
    },
    ActionStarted {
        action_id: String,
    },
    ActionFinished {
        action_id: String,
        status: ActionStatus,
        exit_code: Option<i32>,
        #[serde(default)]
        input_digests: Vec<NamedDigest>,
        #[serde(default)]
        output_digests: Vec<NamedDigest>,
        #[serde(default)]
        key_candidate: Option<String>,
        #[serde(default)]
        cache_eligible: Option<bool>,
        /// Measured peak memory (RSS/commit) of the spawned action process in
        /// bytes; `None` when no process was spawned (`reused`) or the executor
        /// cannot measure it (schema v3, M1).
        #[serde(default)]
        peak_memory_bytes: Option<u64>,
        #[serde(default)]
        error: Option<String>,
    },
    ActionSkipped {
        action_id: String,
    },
    RunFinished {
        outcome: RunOutcome,
        executed: usize,
        skipped: usize,
    },
}

/// How the invocation was launched; `Plan` records its parallelism.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunMode {
    SingleCommand,
    Plan { jobs: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    /// The action's declared outputs were restored from a verified cache
    /// entry; no process was spawned. Only possible when the action key
    /// candidate reported no blockers.
    Reused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

/// A path and its observed content digest at event time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NamedDigest {
    pub path: String,
    pub digest: String,
}

/// Destination for a stream of events. Implementations must be safe to call
/// from multiple threads when wrapped in a mutex and must not panic on IO
/// failure; callers decide how write failures are reported.
pub trait EventSink: Send {
    fn emit(&mut self, event: Event) -> io::Result<()>;
}

/// Writes one JSON event per line, flushing after each event so consumers can
/// tail the file while the run is in progress.
pub struct JsonLinesFile {
    writer: io::BufWriter<std::fs::File>,
}

impl JsonLinesFile {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = std::fs::File::create(path)?;
        Ok(Self {
            writer: io::BufWriter::new(file),
        })
    }
}

impl EventSink for JsonLinesFile {
    fn emit(&mut self, event: Event) -> io::Result<()> {
        serde_json::to_writer(&mut self.writer, &event)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started_event() -> Event {
        Event::new(
            7,
            EventKind::RunStarted {
                mode: RunMode::Plan { jobs: 4 },
            },
        )
    }

    #[test]
    fn event_serializes_with_tagged_kind_and_facts() {
        let value = serde_json::to_value(started_event()).expect("serialize");
        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["run_id"], 7);
        assert!(value["timestamp_unix_ms"].as_u64().is_some());
        assert_eq!(value["kind"], "run_started");
        assert_eq!(value["mode"]["type"], "plan");
        assert_eq!(value["mode"]["jobs"], 4);
    }

    #[test]
    fn action_finished_serializes_digests_and_key_facts() {
        let event = Event::new(
            1,
            EventKind::ActionFinished {
                action_id: "a".to_owned(),
                status: ActionStatus::Succeeded,
                exit_code: Some(0),
                input_digests: vec![NamedDigest {
                    path: "in.txt".to_owned(),
                    digest: "sha256:aa".to_owned(),
                }],
                output_digests: Vec::new(),
                key_candidate: Some("sha256:bb".to_owned()),
                cache_eligible: Some(false),
                peak_memory_bytes: Some(4096),
                error: None,
            },
        );
        let text = serde_json::to_string(&event).expect("serialize");
        assert!(text.contains(r#""kind":"action_finished""#), "was {text}");
        assert!(text.contains(r#""status":"succeeded""#), "was {text}");
        assert!(text.contains(r#""path":"in.txt""#), "was {text}");
        assert!(text.contains(r#""cache_eligible":false"#), "was {text}");
        assert!(text.contains(r#""peak_memory_bytes":4096"#), "was {text}");
        let parsed: Event = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(parsed, event);
    }

    #[test]
    fn v2_action_finished_without_peak_memory_still_parses() {
        // Schema history: v2 files have no `peak_memory_bytes` field. The
        // field is `#[serde(default)]`, so old files must parse unchanged.
        let text = r#"{"schema_version":2,"run_id":1,"timestamp_unix_ms":5,"kind":"action_finished","action_id":"a","status":"reused","exit_code":null,"input_digests":[],"output_digests":[],"key_candidate":"sha256:bb","cache_eligible":true,"error":null}"#;
        let parsed: Event = serde_json::from_str(text).expect("deserialize");
        let EventKind::ActionFinished {
            peak_memory_bytes, ..
        } = parsed.kind
        else {
            panic!("wrong kind {parsed:?}");
        };
        assert_eq!(peak_memory_bytes, None, "no process was spawned");
    }

    #[test]
    fn statuses_and_outcomes_use_snake_case() {
        assert_eq!(
            serde_json::to_string(&ActionStatus::TimedOut).unwrap(),
            r#""timed_out""#
        );
        assert_eq!(
            serde_json::to_string(&ActionStatus::Reused).unwrap(),
            r#""reused""#
        );
        assert_eq!(
            serde_json::to_string(&RunOutcome::Cancelled).unwrap(),
            r#""cancelled""#
        );
        assert_eq!(
            serde_json::to_string(&RunMode::SingleCommand).unwrap(),
            r#"{"type":"single_command"}"#
        );
    }

    #[test]
    fn run_finished_carries_counts() {
        let event = Event::new(
            3,
            EventKind::RunFinished {
                outcome: RunOutcome::Failed,
                executed: 2,
                skipped: 1,
            },
        );
        let text = serde_json::to_string(&event).expect("serialize");
        assert!(text.contains(r#""outcome":"failed""#), "was {text}");
        assert!(text.contains(r#""executed":2"#), "was {text}");
        assert!(text.contains(r#""skipped":1"#), "was {text}");
    }

    #[test]
    fn round_trips_through_jsonl_lines() {
        let events = vec![
            started_event(),
            Event::new(
                7,
                EventKind::ActionStarted {
                    action_id: "a".to_owned(),
                },
            ),
            Event::new(
                7,
                EventKind::RunFinished {
                    outcome: RunOutcome::Succeeded,
                    executed: 1,
                    skipped: 0,
                },
            ),
        ];
        let jsonl = events
            .iter()
            .map(|event| serde_json::to_string(event).expect("serialize"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Vec<Event> = jsonl
            .lines()
            .map(|line| serde_json::from_str(line).expect("deserialize"))
            .collect();
        assert_eq!(parsed, events);
    }

    #[test]
    fn json_lines_file_writes_one_event_per_line() {
        let dir = std::env::temp_dir().join(format!(
            "ant-events-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("events.jsonl");
        {
            let mut sink = JsonLinesFile::create(&path).expect("create file");
            sink.emit(started_event()).expect("emit");
            sink.emit(Event::new(
                7,
                EventKind::ActionStarted {
                    action_id: "a".to_owned(),
                },
            ))
            .expect("emit");
        }
        let content = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2, "content was {content:?}");
        for line in lines {
            serde_json::from_str::<Event>(line).expect("each line parses as an event");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
