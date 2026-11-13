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
    fn entries_require_consistent_kind_and_digest() {
        let digest = format!("sha256:{}", "0".repeat(64));
        let file = |p: &str| Entry {
            path: p.into(),
            kind: EntryKind::File,
            digest: Some(digest.clone()),
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
                digest: None
            }])
            .is_err()
        );
        assert!(
            validate_entries(&[Entry {
                path: "d".into(),
                kind: EntryKind::Dir,
                digest: Some(digest.clone())
            }])
            .is_err()
        );
        assert!(
            validate_entries(&[Entry {
                path: "a".into(),
                kind: EntryKind::File,
                digest: Some("sha256:zz".into())
            }])
            .is_err()
        );
    }
}
