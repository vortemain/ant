//! Per-build output logs (D-045): what the build's tools printed, teed from
//! the terminal into `<user cache>/history/logs/<build id>.log` so the
//! dashboard can show it live and afterwards.
//!
//! - Secret values Ant injected (D-033) are replaced with `****` before
//!   anything is written. Output is processed line by line (a line ends at
//!   `\n` or `\r`), so a secret split across two reads is still caught.
//! - Each log stops at [`MAX_LOG_BYTES`] with a note; the newest [`KEEP`]
//!   logs are kept.
//! - Failures never affect the build: a log that cannot be written is
//!   simply missing.

use local_executor::OutputSink;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Largest log kept per build.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Logs kept on disk.
pub const KEEP: usize = 100;
/// A partial line longer than this is written without waiting for its end.
const MAX_PENDING: usize = 64 * 1024;

fn logs_dir() -> Option<PathBuf> {
    Some(
        build_adapter::user_cache_dir()?
            .join("history")
            .join("logs"),
    )
}

/// The log file for build `id` (ids are sanitized; they cannot escape).
fn log_file(dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    dir.join(format!("{safe}.log"))
}

struct State {
    file: Option<fs::File>,
    written: u64,
    pending: Vec<u8>,
    truncated: bool,
}

pub struct BuildLog {
    state: Mutex<State>,
    secrets: Vec<Vec<u8>>,
}

impl BuildLog {
    /// Start the log for build `id` (best effort) and prune old logs.
    pub fn create(id: &str, secrets: Vec<String>) -> Self {
        let file = logs_dir().and_then(|dir| {
            fs::create_dir_all(&dir).ok()?;
            prune(&dir, KEEP.saturating_sub(1));
            fs::File::create(log_file(&dir, id)).ok()
        });
        Self::with_file(file, secrets)
    }

    fn with_file(file: Option<fs::File>, secrets: Vec<String>) -> Self {
        let mut secrets: Vec<Vec<u8>> = secrets
            .into_iter()
            .filter(|secret| secret.len() >= 4)
            .map(String::into_bytes)
            .collect();
        // Longest first, so a secret containing another is masked whole.
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self {
            state: Mutex::new(State {
                file,
                written: 0,
                pending: Vec::new(),
                truncated: false,
            }),
            secrets,
        }
    }

    fn scrub(&self, line: &[u8]) -> Vec<u8> {
        let mut out = line.to_vec();
        for secret in &self.secrets {
            let mut result = Vec::with_capacity(out.len());
            let mut i = 0;
            while i < out.len() {
                if out[i..].starts_with(secret) {
                    result.extend_from_slice(b"****");
                    i += secret.len();
                } else {
                    result.push(out[i]);
                    i += 1;
                }
            }
            out = result;
        }
        out
    }

    fn emit(&self, state: &mut State, bytes: &[u8]) {
        if state.truncated || bytes.is_empty() {
            return;
        }
        let clean = self.scrub(bytes);
        let Some(file) = state.file.as_mut() else {
            return;
        };
        let room = MAX_LOG_BYTES.saturating_sub(state.written) as usize;
        let take = clean.len().min(room);
        let _ = file.write_all(&clean[..take]);
        state.written += take as u64;
        if take < clean.len() {
            let _ = file.write_all(b"\n[ant: output truncated at 5 MB]\n");
            state.truncated = true;
        }
        let _ = file.flush();
    }

    /// Write whatever partial line is still buffered (call when the build
    /// ends).
    pub fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            let pending = std::mem::take(&mut state.pending);
            self.emit(&mut state, &pending);
        }
    }
}

impl OutputSink for BuildLog {
    fn write(&self, bytes: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.pending.extend_from_slice(bytes);
        // Emit every complete line (ending in \n or \r), keep the rest.
        if let Some(end) = state
            .pending
            .iter()
            .rposition(|b| *b == b'\n' || *b == b'\r')
        {
            let complete: Vec<u8> = state.pending.drain(..=end).collect();
            self.emit(&mut state, &complete);
        }
        if state.pending.len() > MAX_PENDING {
            let pending = std::mem::take(&mut state.pending);
            self.emit(&mut state, &pending);
        }
    }
}

/// Keep only the newest `keep` logs in `dir`.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    logs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in logs.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

/// Bytes of build `id`'s log from offset `from`, at most `limit` of them,
/// plus the log's current size. `None` when there is no log.
pub fn read(id: &str, from: u64, limit: usize) -> Option<(Vec<u8>, u64)> {
    let path = log_file(&logs_dir()?, id);
    read_path(&path, from, limit)
}

fn read_path(path: &Path, from: u64, limit: usize) -> Option<(Vec<u8>, u64)> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let start = from.min(size);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes).ok()?;
    Some((bytes, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> (PathBuf, fs::File) {
        let dir = std::env::temp_dir().join(format!("ant-log-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("build.log");
        let file = fs::File::create(&path).expect("file");
        (path, file)
    }

    #[test]
    fn logs_mask_secrets_even_across_chunks_and_read_from_offsets() {
        let (path, file) = temp_file("mask");
        let log = BuildLog::with_file(
            Some(file),
            vec!["hunter2-secret".to_owned(), "abc".to_owned()],
        );
        log.write(b"Compiling app\nsigning with hunt");
        log.write(b"er2-secret now\r\x1b[32mdone\x1b[0m");
        log.finish();
        let text = fs::read_to_string(&path).expect("read");
        assert!(!text.contains("hunter2-secret"), "{text:?}");
        assert!(text.contains("signing with **** now"), "{text:?}");
        assert!(
            text.contains("Compiling app") && text.contains("done"),
            "{text:?}"
        );
        let (tail, size) = read_path(&path, 14, 1024).expect("read tail");
        assert_eq!(size, text.len() as u64);
        assert!(String::from_utf8_lossy(&tail).starts_with("signing"));
        assert!(read_path(&path.with_extension("missing"), 0, 10).is_none());
        let _ = fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn logs_stop_at_the_size_limit() {
        let (path, file) = temp_file("cap");
        let log = BuildLog::with_file(Some(file), Vec::new());
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..7 {
            log.write(&line);
            log.write(b"\n");
        }
        log.finish();
        let size = fs::metadata(&path).expect("meta").len();
        assert!(size <= MAX_LOG_BYTES + 64, "{size}");
        assert!(
            fs::read_to_string(&path)
                .expect("read")
                .ends_with("[ant: output truncated at 5 MB]\n")
        );
        let _ = fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn ids_cannot_escape_the_log_directory() {
        assert_eq!(
            log_file(Path::new("/logs"), "../../etc/passwd")
                .file_name()
                .unwrap(),
            "etcpasswd.log"
        );
    }
}
