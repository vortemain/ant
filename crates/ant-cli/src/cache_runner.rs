//! Cache participation for the run path.
//!
//! The mechanics of lookup, restore, and store are wired here so that reuse
//! works as soon as an executor can produce a blocker-free action key. The
//! gate itself is unchanged (decision D-003): a key may be stored or looked
//! up only when its candidate reported no blockers. No executor can produce
//! such a key yet — `network-isolation-unavailable` always applies (decision
//! D-009) — so today this module computes keys, proves the gate closed, and
//! never reuses; the mechanics are exercised by unit tests through the
//! `try_reuse` seam with a synthetic eligible key.

use build_action::{Action, OutputKind};
use build_cache::Cache;
use build_digest::{ActionKeyCandidate, ContentDigest, ExecutionFingerprint};
use local_executor::{self, ActionContentDigest, CancellationToken, ExecutionError, ExecutionResult, PreparedAction};
use std::path::Path;

/// One action's outcome on the run path, cache-aware.
pub enum ActionRun {
    /// The declared outputs were restored from a verified cache entry; no
    /// process was spawned. Only produced for a blocker-free key.
    Reused {
        candidate: ActionKeyCandidate,
        input_digests: Vec<ActionContentDigest>,
        output_digests: Vec<ActionContentDigest>,
    },
    /// The action executed through the normal executor path.
    Executed {
        result: ExecutionResult,
        /// The action-key candidate computed from the pre-run input digests.
        /// `cache_eligible` is false for every executor today.
        candidate: ActionKeyCandidate,
        /// A non-fatal cache store failure, reported by the caller without
        /// failing the build.
        store_warning: Option<String>,
    },
}

/// Opt-in cache participation for one run. `None` when `--cache-dir` was not
/// given.
pub struct CacheParticipant {
    cache: Option<Cache>,
}

impl std::fmt::Debug for CacheParticipant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheParticipant")
            .field("enabled", &self.cache.is_some())
            .finish()
    }
}

impl CacheParticipant {
    /// Open the cache when a directory was requested. Opening fails hard:
    /// the user asked for cache participation, and silently running without
    /// it would hide the mistake.
    pub fn new(cache_dir: Option<&Path>) -> Result<Self, String> {
        let cache = match cache_dir {
            None => None,
            Some(dir) => {
                let cache = Cache::open(dir).map_err(|error| {
                    format!("could not open cache directory {}: {error}", dir.display())
                })?;
                Some(cache)
            }
        };
        Ok(Self { cache })
    }
}

/// Run one action with cache participation.
///
/// Flow: prepare once (validates inputs and digests them with metadata), then
/// compute the action-key candidate. When the cache is enabled *and* the
/// candidate reported no blockers, try lookup + restore; on a hit the action
/// reports `Reused`. Otherwise execute the prepared action through the
/// normal executor path and, on success with an eligible key, store the
/// verified outputs.
///
/// Cache failures never fail the build: a failed lookup or restore is a miss
/// and the action executes; a failed store is reported as a warning. Only
/// preparation, key computation, and execution errors are fatal.
pub fn run_action(
    participant: &CacheParticipant,
    workspace: &Path,
    action: &Action,
    cancellation: &CancellationToken,
    sandbox: bool,
) -> Result<ActionRun, ExecutionError> {
    let prepared = local_executor::prepare_with_metadata(workspace, action)?;
    let fingerprint = if sandbox {
        ExecutionFingerprint::local_sandboxed(workspace)
    } else {
        ExecutionFingerprint::local_metadata_fingerprinting(workspace)
    };
    // The candidate must use the same input digests the key reporter prints,
    // so the printed key_candidate is the key a future lookup would use.
    let inputs: Vec<_> = prepared
        .input_digests
        .iter()
        .map(|entry| (entry.path.clone(), entry.digest))
        .collect();
    let candidate = build_digest::action_key_candidate(action, &inputs, &fingerprint)
        .map_err(ExecutionError::InputDigest)?;

    if candidate.cache_eligible
        && let Some(cache) = &participant.cache
        && let Some(output_digests) = try_reuse(cache, &candidate.digest, action, workspace)
    {
        return Ok(ActionRun::Reused {
            candidate,
            input_digests: prepared.input_digests.clone(),
            output_digests,
        });
    }

    let result = execute_prepared_choice(&prepared, cancellation, sandbox)?;
    let mut store_warning = None;
    if result.success()
        && !result.cancelled
        && !result.timed_out
        && candidate.cache_eligible
        && let Some(cache) = &participant.cache
    {
        let outputs: Vec<(std::path::PathBuf, OutputKind)> = action
            .outputs
            .iter()
            .map(|output| (output.path.clone(), output.kind))
            .collect();
        // On success the executor re-verified the inputs unchanged, so
        // the pre-run candidate is the key these outputs belong to.
        if let Err(error) = cache.store(&candidate.digest, workspace, &outputs) {
            store_warning = Some(format!("cache store failed: {error}"));
        }
    }
    Ok(ActionRun::Executed {
        result,
        candidate,
        store_warning,
    })
}

fn execute_prepared_choice(
    prepared: &PreparedAction,
    cancellation: &CancellationToken,
    sandbox: bool,
) -> Result<ExecutionResult, ExecutionError> {
    if sandbox {
        local_executor::execute_sandboxed_prepared(prepared, cancellation)
    } else {
        local_executor::execute_prepared(prepared, cancellation)
    }
}

/// Look up `key`, restore the declared outputs into `workspace`, and digest
/// them for reporting. Any cache problem (miss, corruption, integrity
/// failure, IO error) is a miss: returns `None` and the caller executes the
/// action. This seam is also exercised directly by unit tests with a
/// synthetic eligible key, because no real key is eligible today.
fn try_reuse(
    cache: &Cache,
    key: &ContentDigest,
    action: &Action,
    workspace: &Path,
) -> Option<Vec<ActionContentDigest>> {
    let entry = cache.lookup(key).ok()??;
    let declared: Vec<(std::path::PathBuf, OutputKind)> = action
        .outputs
        .iter()
        .map(|output| (output.path.clone(), output.kind))
        .collect();
    // Restore re-digests every file it writes and removes partial output on
    // integrity failure, so a corrupted cache never reaches the build.
    cache.restore(&entry, workspace, &declared).ok()?;
    let mut output_digests = Vec::with_capacity(action.outputs.len());
    for output in &action.outputs {
        let path = workspace.join(&output.path);
        let digest = match output.kind {
            OutputKind::File => build_digest::digest_file(&path),
            OutputKind::Tree => build_digest::digest_tree(&path),
        }
        .ok()?;
        output_digests.push(ActionContentDigest {
            path: output.path.clone(),
            digest,
        });
    }
    Some(output_digests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_action::{CachePolicy, Input, InputKind, Output, ToolchainIdentity};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ant-cache-runner-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, contents).expect("write file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An action whose declared shape matches the stored outputs; the
    /// toolchain identity is well-formed so the only remaining blockers are
    /// the capability ones (irrelevant here because `try_reuse` takes the
    /// key directly, bypassing the closed gate).
    fn reuse_action() -> Action {
        Action {
            schema_version: 1,
            id: "gen".to_owned(),
            argv: vec!["cmd".to_owned(), "/C".to_owned(), "echo hi > out.txt".to_owned()],
            working_directory: std::path::PathBuf::from("."),
            inputs: vec![Input {
                path: std::path::PathBuf::from("in.txt"),
                kind: InputKind::File,
            }],
            outputs: vec![Output {
                path: std::path::PathBuf::from("out.txt"),
                kind: OutputKind::File,
            }],
            environment: Vec::new(),
            cache_policy: CachePolicy::Eligible,
            toolchain: Some(ToolchainIdentity {
                name: "test".to_owned(),
                version: "1".to_owned(),
                executable_digest: format!("sha256:{}", "0".repeat(64)),
            }),
            timeout_ms: None,
            resources: None,
        }
    }

    #[test]
    fn try_reuse_restores_outputs_and_reports_digests() {
        let workspace = TempDir::new("hit-ws");
        let cache_dir = TempDir::new("hit-cache");
        workspace.write("in.txt", b"input");
        workspace.write("out.txt", b"artifact");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let outputs = vec![(std::path::PathBuf::from("out.txt"), OutputKind::File)];
        // The key is synthetic: no executor can produce an eligible key yet.
        let key = build_digest::digest_file(&workspace.path().join("in.txt")).expect("digest");
        cache.store(&key, workspace.path(), &outputs).expect("store");

        // Remove the output to prove it came back from the cache.
        fs::remove_file(workspace.path().join("out.txt")).expect("remove output");

        let action = reuse_action();
        let digests = try_reuse(&cache, &key, &action, workspace.path())
            .expect("reuse must succeed for a stored key");
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].path, std::path::PathBuf::from("out.txt"));
        assert_eq!(
            digests[0].digest,
            build_digest::digest_file(&workspace.path().join("out.txt")).expect("digest"),
            "reported digest must describe the restored output"
        );
        assert_eq!(
            fs::read(workspace.path().join("out.txt")).expect("restored"),
            b"artifact"
        );
    }

    #[test]
    fn try_reuse_unknown_key_is_a_miss() {
        let workspace = TempDir::new("miss-ws");
        let cache_dir = TempDir::new("miss-cache");
        workspace.write("in.txt", b"input");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let key = build_digest::digest_file(&workspace.path().join("in.txt")).expect("digest");
        let action = reuse_action();
        assert!(try_reuse(&cache, &key, &action, workspace.path()).is_none());
    }

    #[test]
    fn try_reuse_integrity_failure_is_a_miss() {
        let workspace = TempDir::new("corrupt-ws");
        let cache_dir = TempDir::new("corrupt-cache");
        workspace.write("in.txt", b"input");
        workspace.write("out.txt", b"artifact");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let outputs = vec![(std::path::PathBuf::from("out.txt"), OutputKind::File)];
        let key = build_digest::digest_file(&workspace.path().join("in.txt")).expect("digest");
        cache.store(&key, workspace.path(), &outputs).expect("store");

        // Corrupt the stored blob in place.
        let blob_dir = cache.root().join("blobs");
        let blob = fs::read_dir(&blob_dir)
            .expect("read blobs")
            .next()
            .expect("one blob")
            .expect("dir entry")
            .path();
        fs::write(&blob, b"tampered").expect("corrupt blob");

        let action = reuse_action();
        assert!(
            try_reuse(&cache, &key, &action, workspace.path()).is_none(),
            "an integrity failure must be treated as a miss, never as a hit"
        );
        assert!(
            !workspace.path().join("out.txt").exists(),
            "a failed restore must not leave partial output behind"
        );
    }

    #[test]
    fn participant_without_directory_is_disabled() {
        // `new(None)` succeeds and carries no cache; participation is off.
        let participant = CacheParticipant::new(None).expect("no dir");
        assert!(participant.cache.is_none());
    }

    #[test]
    fn participant_fails_hard_on_unusable_directory() {
        let file = TempDir::new("not-a-dir");
        file.write("occupied", b"x");
        let error = CacheParticipant::new(Some(&file.path().join("occupied")))
            .expect_err("a file cannot be a cache root");
        assert!(error.contains("could not open cache directory"), "was {error}");
    }
}
