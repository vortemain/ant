//! Local build history (roadmap P7, decision D-042).
//!
//! Every `ant build` appends one JSON line to
//! `<user cache>/history/builds.jsonl`: when, which project, which adapter
//! and flavor, the outcome, how long it took, what each step did (ran,
//! reused, failed), and the deliverables it collected. `ant dashboard`
//! reads it. The file lives in the per-user cache directory (never in the
//! project), holds no secrets (no environment, no arguments, no output),
//! and is trimmed to the most recent [`KEEP`] builds. Recording is best
//! effort: a history problem never fails a build.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Builds kept in the history file.
pub const KEEP: usize = 500;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StepRecord {
    pub id: String,
    /// `succeeded`, `reused`, `failed`, `timed_out`, `cancelled`, `skipped`.
    pub status: String,
    pub millis: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct BuildRecord {
    /// Unique per build (time + process id).
    pub id: String,
    /// Milliseconds since the Unix epoch when the build started.
    pub started_ms: u64,
    pub project: PathBuf,
    pub adapter: String,
    pub flavor: String,
    pub succeeded: bool,
    pub millis: u64,
    pub steps: Vec<StepRecord>,
    /// Workspace-relative deliverables collected into the output directory.
    pub deliverables: Vec<PathBuf>,
}

fn history_file() -> Option<PathBuf> {
    Some(
        build_adapter::user_cache_dir()?
            .join("history")
            .join("builds.jsonl"),
    )
}

/// Append `record` (best effort), trimming the file when it grows past
/// twice [`KEEP`] lines.
pub fn record(record: &BuildRecord) {
    let Some(file) = history_file() else {
        return;
    };
    let _ = append_to(&file, record);
}

fn append_to(file: &Path, record: &BuildRecord) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(format!("{line}\n").as_bytes())?;
    let text = fs::read_to_string(file)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > KEEP * 2 {
        let kept = lines[lines.len() - KEEP..].join("\n") + "\n";
        let temp = file.with_extension(format!("jsonl.{}.tmp", std::process::id()));
        fs::write(&temp, kept)?;
        fs::rename(&temp, file)?;
    }
    Ok(())
}

/// The most recent builds, newest first (unreadable lines are skipped).
pub fn recent(limit: usize) -> Vec<BuildRecord> {
    history_file()
        .map(|file| read_from(&file, limit))
        .unwrap_or_default()
}

fn read_from(file: &Path, limit: usize) -> Vec<BuildRecord> {
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str(line).ok())
        .take(limit)
        .collect()
}

/// A build in progress (D-044): rewritten after every step to
/// `<user cache>/history/running/<id>.json`, removed when the build ends.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RunningBuild {
    pub id: String,
    pub started_ms: u64,
    pub project: PathBuf,
    pub adapter: String,
    pub flavor: String,
    pub total_steps: usize,
    /// Steps that finished, in order.
    pub steps: Vec<StepRecord>,
    /// Steps running now.
    pub current: Vec<String>,
    /// When this record was last written (a stale record means the build
    /// was killed before it could clean up).
    pub updated_ms: u64,
}

/// A running record not refreshed for this long belongs to a build that
/// was killed; it is ignored and removed.
pub const STALE_RUNNING_MS: u64 = 6 * 60 * 60 * 1000;

fn running_dir() -> Option<PathBuf> {
    Some(
        build_adapter::user_cache_dir()?
            .join("history")
            .join("running"),
    )
}

fn running_file(dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    dir.join(format!("{safe}.json"))
}

/// Write the in-progress record (best effort, atomic replace).
pub fn update_running(running: &RunningBuild) {
    let Some(dir) = running_dir() else {
        return;
    };
    let _ = write_running(&dir, running);
}

fn write_running(dir: &Path, running: &RunningBuild) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let file = running_file(dir, &running.id);
    let temp = file.with_extension("json.tmp");
    let mut record = running.clone();
    record.updated_ms = now_ms();
    fs::write(
        &temp,
        serde_json::to_vec(&record).map_err(std::io::Error::other)?,
    )?;
    fs::rename(&temp, &file)
}

/// Remove the in-progress record of build `id`.
pub fn finish_running(id: &str) {
    if let Some(dir) = running_dir() {
        let _ = fs::remove_file(running_file(&dir, id));
    }
}

/// Removes a build's in-progress record when dropped, so every way out of
/// `ant build` (success, failure, early error) clears it.
pub struct RunningGuard(pub String);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        finish_running(&self.0);
    }
}

/// Builds in progress, oldest first; stale records are removed.
pub fn running() -> Vec<RunningBuild> {
    running_dir()
        .map(|dir| read_running(&dir, now_ms()))
        .unwrap_or_default()
}

fn read_running(dir: &Path, now: u64) -> Vec<RunningBuild> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<RunningBuild> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let record = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RunningBuild>(&bytes).ok());
        match record {
            Some(record) if now.saturating_sub(record.updated_ms) < STALE_RUNNING_MS => {
                found.push(record);
            }
            _ => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    found.sort_by_key(|record| record.started_ms);
    found
}

/// A fresh build id: start time plus process id.
pub fn new_id(started_ms: u64) -> String {
    format!("{started_ms}-{}", std::process::id())
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: u64) -> BuildRecord {
        BuildRecord {
            id: format!("b{n}"),
            started_ms: n,
            project: PathBuf::from("/work/app"),
            adapter: "cargo".to_owned(),
            flavor: "release".to_owned(),
            succeeded: n.is_multiple_of(2),
            millis: 10 * n,
            steps: vec![StepRecord {
                id: "workspace".to_owned(),
                status: "reused".to_owned(),
                millis: 1,
            }],
            deliverables: vec![PathBuf::from("dist/app")],
        }
    }

    #[test]
    fn running_records_update_list_and_expire() {
        let dir = std::env::temp_dir().join(format!("ant-running-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut build = RunningBuild {
            id: "100-7".to_owned(),
            started_ms: 100,
            project: PathBuf::from("/work/app"),
            adapter: "cargo".to_owned(),
            total_steps: 2,
            current: vec!["configure".to_owned()],
            ..RunningBuild::default()
        };
        write_running(&dir, &build).expect("write");
        build.current = vec!["build".to_owned()];
        build.steps.push(StepRecord {
            id: "configure".to_owned(),
            status: "succeeded".to_owned(),
            millis: 5,
        });
        write_running(&dir, &build).expect("rewrite");
        let now = now_ms();
        let found = read_running(&dir, now);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].current, vec!["build".to_owned()]);
        assert_eq!(found[0].steps.len(), 1);
        // A record nobody refreshed for hours belongs to a killed build.
        assert!(read_running(&dir, now + STALE_RUNNING_MS + 1).is_empty());
        assert!(
            !running_file(&dir, "100-7").exists(),
            "stale record removed"
        );
        assert_eq!(
            running_file(&dir, "../../x").file_name().unwrap(),
            "x.json",
            "ids cannot escape the directory"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_appends_reads_newest_first_and_trims() {
        let dir = std::env::temp_dir().join(format!("ant-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("history").join("builds.jsonl");
        for n in 0..(KEEP as u64 * 2 + 5) {
            append_to(&file, &sample(n)).expect("append");
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .and_then(|mut f| f.write_all(b"not json\n"))
            .expect("garbage line");
        let lines = fs::read_to_string(&file).expect("read").lines().count();
        assert!(lines <= KEEP * 2 + 1, "trimmed: {lines}");
        let recent = read_from(&file, 3);
        let last = KEEP as u64 * 2 + 4;
        assert_eq!(
            recent.iter().map(|r| r.started_ms).collect::<Vec<_>>(),
            vec![last, last - 1, last - 2]
        );
        assert_eq!(recent[0], sample(last));
        assert!(read_from(&dir.join("missing.jsonl"), 5).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
