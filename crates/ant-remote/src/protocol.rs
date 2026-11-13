//! Remote protocol v1: the documents exchanged between client, coordinator,
//! and worker (see `docs/remote/protocol-v1.md`). JSON over HTTP under `/v1`.
//!
//! Every path in these documents is workspace-relative, `/`-separated, and
//! validated with [`validate_relative`] by *every* party before use — the
//! coordinator on submission, the worker before materializing inputs, and the
//! client before writing outputs — so a malicious peer cannot direct a write
//! outside a workspace.

use build_action::Action;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
}

/// One file or directory in a transferred set. Files carry the SHA-256
/// digest (`sha256:<hex>`) of their content, which names the blob.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Unix executable permission of a file (D-022). Without it, binaries
    /// and scripts transferred between machines would not run. Always false
    /// on Windows, which has no such permission. Not part of cache keys.
    #[serde(default, skip_serializing_if = "is_false")]
    pub executable: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Whether `path` is a file with any Unix execute permission bit set.
pub fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Give `path` the conventional executable mode (0o755). No-op off Unix.
pub fn make_executable(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("cannot make {} executable: {error}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// A job: one action plus the exact input set it may see. Undeclared inputs
/// do not exist on the worker, so remote execution enforces input
/// declarations by construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub protocol_version: u32,
    pub action: Action,
    pub inputs: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    /// The worker finished; `result` says whether the action succeeded.
    Completed,
    /// The coordinator gave up (for example the lease expired twice).
    Failed,
}

/// What a worker reports for a job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub worker_id: String,
    /// Process exit code; `None` when the process never ran or was killed.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Executor error (input materialization, missing output, sandbox...).
    pub error: Option<String>,
    /// Every declared output file/directory, workspace-relative. Files are
    /// uploaded as blobs before the result is posted.
    pub outputs: Vec<Entry>,
    /// The outputs came from the shared remote cache; nothing was executed.
    #[serde(default)]
    pub reused: bool,
    /// The action key the worker computed when the run was cache-eligible
    /// (sandboxed Linux worker, fully declared action; D-019).
    #[serde(default)]
    pub cache_key: Option<String>,
}

/// A shared remote cache entry: the outputs a sandboxed worker produced for
/// an action key (`GET /v1/cache/{key}`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEntry {
    pub outputs: Vec<Entry>,
    pub worker_id: String,
    pub created_unix_ms: u64,
}

/// `PUT /v1/cache/{key}` (workers only).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheStoreRequest {
    pub worker_id: String,
    pub outputs: Vec<Entry>,
}

impl JobResult {
    pub fn succeeded(&self) -> bool {
        self.error.is_none() && !self.timed_out && self.exit_code == Some(0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    pub job_id: String,
    pub state: JobState,
    pub attempts: u32,
    pub result: Option<JobResult>,
    /// Coordinator-side failure reason when `state` is `failed`.
    pub error: Option<String>,
}

/// Control-plane read model of one job (`GET /v1/jobs`). Timestamps are the
/// coordinator's wall clock in Unix milliseconds, recorded when the event
/// happened — measured facts, never estimates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    pub action_id: String,
    pub state: JobState,
    pub attempts: u32,
    /// Worker holding the lease (running) or that reported the result.
    pub worker_id: Option<String>,
    pub submitted_unix_ms: u64,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub exit_code: Option<i32>,
    /// `Some(true)` only for a completed job whose action succeeded.
    pub succeeded: Option<bool>,
    pub error: Option<String>,
    pub input_count: usize,
    pub output_count: usize,
    /// Satisfied from the shared remote cache instead of executing.
    #[serde(default)]
    pub reused: bool,
}

/// `GET /v1/jobs`: every job the coordinator holds (in memory since start),
/// newest first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobList {
    pub coordinator_started_unix_ms: u64,
    pub jobs: Vec<JobSummary>,
}

/// Control-plane read model of one worker, as observed by the coordinator.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerSummary {
    pub worker_id: String,
    pub os: String,
    /// Last lease poll or result report.
    pub last_seen_unix_ms: u64,
    pub current_job: Option<String>,
    pub jobs_reported: u64,
}

/// `GET /v1/workers`: workers seen since the coordinator started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerList {
    pub now_unix_ms: u64,
    pub workers: Vec<WorkerSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub job_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub protocol_version: u32,
    pub worker_id: String,
    /// `std::env::consts::OS` of the worker; jobs are not yet routed by it.
    pub os: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub job_id: String,
    pub spec: JobSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MissingRequest {
    pub digests: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MissingResponse {
    pub missing: Vec<String>,
}

/// `POST /v1/blobs/batch-get` request: at most [`MAX_BATCH_BLOBS`] digests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchGetRequest {
    pub digests: Vec<String>,
}

/// Most blobs one batch request may name or carry (P2.6).
pub const MAX_BATCH_BLOBS: usize = 1024;
/// Target payload size for one batch; senders split larger sets.
pub const BATCH_TARGET_BYTES: usize = 32 * 1024 * 1024;

/// Encode blobs as frames: for each, a u32 (big-endian) digest length, the
/// digest text, a u64 data length, then the data. Used by the batched blob
/// routes to move many small files per request (P2.6, D-023).
pub fn encode_frames(frames: &[(&str, &[u8])]) -> Vec<u8> {
    let total: usize = frames.iter().map(|(d, b)| 12 + d.len() + b.len()).sum();
    let mut out = Vec::with_capacity(total);
    for (digest, bytes) in frames {
        out.extend_from_slice(&(digest.len() as u32).to_be_bytes());
        out.extend_from_slice(digest.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// Decode frames produced by [`encode_frames`]; rejects truncated or
/// oversized frames and more than [`MAX_BATCH_BLOBS`] frames. Callers must
/// still verify each blob against its digest.
pub fn decode_frames(mut bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut frames = Vec::new();
    let take = |bytes: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
        if bytes.len() < n {
            return Err("truncated blob frame".to_owned());
        }
        let (head, tail) = bytes.split_at(n);
        *bytes = tail;
        Ok(head.to_vec())
    };
    while !bytes.is_empty() {
        if frames.len() >= MAX_BATCH_BLOBS {
            return Err(format!("more than {MAX_BATCH_BLOBS} blobs in one batch"));
        }
        let digest_len =
            u32::from_be_bytes(take(&mut bytes, 4)?.try_into().expect("4 bytes")) as usize;
        if digest_len > 128 {
            return Err("blob frame digest is too long".to_owned());
        }
        let digest = String::from_utf8(take(&mut bytes, digest_len)?)
            .map_err(|_| "blob frame digest is not UTF-8".to_owned())?;
        let data_len = u64::from_be_bytes(take(&mut bytes, 8)?.try_into().expect("8 bytes"));
        let data_len = usize::try_from(data_len).map_err(|_| "blob frame too large".to_owned())?;
        frames.push((digest, take(&mut bytes, data_len)?));
    }
    Ok(frames)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Health {
    pub protocol_version: u32,
}

/// Accept only a non-empty, `/`-separated, workspace-relative path made of
/// normal components: no absolute or drive paths, no `.`/`..`, no empty
/// components, no backslashes, colons, or control characters.
pub fn validate_relative(path: &str) -> Result<(), String> {
    let reject = || {
        Err(format!(
            "path '{path}' is not a safe workspace-relative path"
        ))
    };
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
        return reject();
    }
    if path
        .chars()
        .any(|c| c == '\\' || c == ':' || c.is_control())
    {
        return reject();
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return reject();
        }
    }
    Ok(())
}

/// Validate a digest string's syntax (`sha256:` + 64 hex).
pub fn validate_digest(digest: &str) -> Result<(), String> {
    digest
        .parse::<build_digest::ContentDigest>()
        .map(|_| ())
        .map_err(|error| format!("invalid digest: {error}"))
}

/// Validate a transferred entry set: safe paths, no duplicates, files carry
/// a well-formed digest, directories carry none.
pub fn validate_entries(entries: &[Entry]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        validate_relative(&entry.path)?;
        if !seen.insert(entry.path.as_str()) {
            return Err(format!("duplicate entry '{}'", entry.path));
        }
        match (entry.kind, &entry.digest) {
            (EntryKind::File, Some(digest)) => validate_digest(digest)?,
            (EntryKind::Dir, None) => {}
            _ => {
                return Err(format!(
                    "entry '{}' has an inconsistent kind/digest",
                    entry.path
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_reject_every_escape() {
        for good in ["a", "src/main.rs", "a/b/c.txt", ".ant/tmp/x"] {
            assert_eq!(validate_relative(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a//b",
            "a/",
            "C:/x",
            "C:x",
            r"a\b",
            "a/\0",
            ".",
            "..",
        ] {
            assert!(validate_relative(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn frames_roundtrip_and_reject_malformed_input() {
        let frames = encode_frames(&[("sha256:aa", b"hello"), ("sha256:bb", b"")]);
        let decoded = decode_frames(&frames).expect("decode");
        assert_eq!(
            decoded,
            vec![
                ("sha256:aa".to_owned(), b"hello".to_vec()),
                ("sha256:bb".to_owned(), Vec::new())
            ]
        );
        assert!(
            decode_frames(&frames[..frames.len() - 1]).is_err(),
            "truncated data"
        );
        assert!(decode_frames(&[0, 0, 0]).is_err(), "truncated header");
        let mut huge = (200u32).to_be_bytes().to_vec();
        huge.extend(vec![b'x'; 200]);
        assert!(decode_frames(&huge).is_err(), "oversized digest");
        assert_eq!(decode_frames(&[]).expect("empty"), Vec::new());
    }

    #[test]
    fn entries_require_consistent_kind_and_digest() {
        let digest = format!("sha256:{}", "0".repeat(64));
        let file = |p: &str| Entry {
            path: p.into(),
            kind: EntryKind::File,
            digest: Some(digest.clone()),
            executable: false,
        };
        assert!(validate_entries(&[file("a"), file("b/c")]).is_ok());
        assert!(
            validate_entries(&[file("a"), file("a")]).is_err(),
            "duplicates"
        );
        assert!(
            validate_entries(&[Entry {
                path: "a".into(),
                kind: EntryKind::File,
                digest: None,
                executable: false
            }])
            .is_err()
        );
        assert!(
            validate_entries(&[Entry {
                path: "d".into(),
                kind: EntryKind::Dir,
                digest: Some(digest.clone()),
                executable: false
            }])
            .is_err()
        );
        assert!(
            validate_entries(&[Entry {
                path: "a".into(),
                kind: EntryKind::File,
                digest: Some("sha256:zz".into()),
                executable: false
            }])
            .is_err()
        );
    }
}
