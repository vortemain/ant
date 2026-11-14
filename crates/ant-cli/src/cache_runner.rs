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
use build_adapter::gradle::SIGNING_MARKER;
use build_cache::Cache;
use build_digest::{ActionKeyCandidate, ContentDigest, ExecutionFingerprint};
use local_executor::IsolatedAccount;
use local_executor::{
    self, ActionContentDigest, CancellationToken, ExecutionError, ExecutionResult, PreparedAction,
};
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
/// How actions execute: direct, write-confined (`--sandbox`), or
/// write-confined and network-isolated as the sandbox account
/// (`--network-isolated`, which implies `sandbox`).
#[derive(Clone, Copy)]
pub struct ExecConfig {
    pub sandbox: bool,
    pub enforce_memory: bool,
    pub isolated: Option<&'static IsolatedAccount>,
    /// Local trusted reuse (D-028, `ant build`): unsandboxed runs key by
    /// content-only digests through [`build_digest::trusted_local_key`],
    /// against a store that is never shared.
    pub local_reuse: bool,
    /// Skip lookups (results are still stored): `ant build --fresh`.
    pub fresh: bool,
}

pub fn run_action(
    participant: &CacheParticipant,
    workspace: &Path,
    action: &Action,
    cancellation: &CancellationToken,
    exec: ExecConfig,
    secrets: Option<&crate::signing::SigningSecrets>,
) -> Result<ActionRun, ExecutionError> {
    let ExecConfig {
        sandbox,
        enforce_memory,
        isolated,
        local_reuse,
        fresh,
    } = exec;
    let local_reuse = local_reuse && !sandbox && isolated.is_none();
    // Local reuse keys by content only, so a branch switch or re-checkout
    // that rewrites timestamps still hits.
    let mut prepared = if local_reuse {
        local_executor::prepare(workspace, action)?
    } else {
        local_executor::prepare_with_metadata(workspace, action)?
    };
    if enforce_memory {
        prepared.enforce_memory_limit();
    }
    // Only the executor that runs as the verified, firewall-scoped sandbox
    // account may declare network isolation (D-013).
    let fingerprint = if isolated.is_some() {
        ExecutionFingerprint::local_network_isolated(workspace)
    } else if sandbox {
        local_executor::sandboxed_fingerprint(workspace)
    } else if local_reuse {
        ExecutionFingerprint::local(workspace)
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
    // Proven key, or (local reuse only) the domain-separated trusted key.
    // Actions without declared outputs have nothing to restore.
    let key = if candidate.cache_eligible {
        Some(candidate.digest)
    } else if local_reuse && !action.outputs.is_empty() {
        build_digest::trusted_local_key(&candidate)
    } else {
        None
    };

    if !fresh
        && let Some(key) = &key
        && let Some(cache) = &participant.cache
        && let Some(output_digests) = try_reuse(cache, key, action, workspace)
    {
        return Ok(ActionRun::Reused {
            candidate,
            input_digests: prepared.input_digests.clone(),
            output_digests,
        });
    }

    // A previous cache hit may have restored these outputs as hard links into
    // the blob store. Before re-running the action, drop them so the action
    // regenerates fresh files. This both (1) lets a sandboxed action overwrite
    // an output whose inode lives in the user-owned cache — which otherwise
    // fails with "Access is denied" — and (2) keeps an in-place rewrite from
    // corrupting the shared, content-addressed blob. Only the cache path hard
    // links, so without a cache we leave outputs untouched.
    if participant.cache.is_some() {
        clear_declared_outputs(workspace, action);
    }

    // Build-time secrets (D-036), injected into the prepared action's
    // environment here — after key computation above — so the values never
    // touch the plan, the action key, the cache, or the events. A plan that
    // declares signing must be matched by the secrets it was built with:
    // a stale plan (the keystore was re-imported) fails closed instead of
    // signing with the wrong key.
    if action
        .environment
        .iter()
        .any(|(key, _)| key == SIGNING_MARKER)
    {
        let secrets = secrets.ok_or_else(|| {
            ExecutionError::Secrets(
                "this plan signs its build, but this run has no secrets to sign \
                 with; run 'ant build' from this project's front door"
                    .to_owned(),
            )
        })?;
        if !secrets.matches_action(action) {
            return Err(ExecutionError::Secrets(format!(
                "the plan expects keystore {} but the registered keystore is \
                 {}; re-run 'ant build' (or 'ant keystore import' if that is the \
                 keystore you meant)",
                action
                    .environment
                    .iter()
                    .find_map(|(key, value)| (key == SIGNING_MARKER).then_some(value.clone()))
                    .unwrap_or_default(),
                secrets.reference
            )));
        }
        prepared
            .action
            .environment
            .extend(secrets.values.iter().cloned());
    }

    let result = execute_prepared_choice(&prepared, cancellation, sandbox, isolated)?;
    let mut store_warning = None;
    if result.success()
        && !result.cancelled
        && !result.timed_out
        && let Some(key) = &key
        && let Some(cache) = &participant.cache
    {
        let outputs: Vec<(std::path::PathBuf, OutputKind)> = action
            .outputs
            .iter()
            .map(|output| (output.path.clone(), output.kind))
            .collect();
        // On success the executor re-verified the inputs unchanged, so
        // the pre-run candidate is the key these outputs belong to.
        if let Err(error) = cache.store(key, workspace, &outputs) {
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
    isolated: Option<&IsolatedAccount>,
) -> Result<ExecutionResult, ExecutionError> {
    if let Some(account) = isolated {
        local_executor::execute_network_isolated_prepared(prepared, cancellation, account)
    } else if sandbox {
        local_executor::execute_sandboxed_prepared(prepared, cancellation)
    } else {
        local_executor::execute_prepared(prepared, cancellation)
    }
}

/// Remove the action's declared outputs from the workspace before re-running
/// it, to break hard links restored from the cache (see `run_action`). Errors
/// are ignored: a missing output or a busy file simply means the action
/// regenerates or overwrites it.
fn clear_declared_outputs(workspace: &Path, action: &Action) {
    for output in &action.outputs {
        let path = workspace.join(&output.path);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
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
        let cache = workspace
            .canonicalize()
            .ok()
            .and_then(|root| build_digest::workspace_cache(&root));
        let digest = match (output.kind, &cache) {
            (OutputKind::File, Some(cache)) => {
                build_digest::digest_file_cached(&path, false, cache)
            }
            (OutputKind::Tree, Some(cache)) => {
                build_digest::digest_tree_cached(&path, false, cache)
            }
            (OutputKind::File, None) => build_digest::digest_file(&path),
            (OutputKind::Tree, None) => build_digest::digest_tree(&path),
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
            argv: vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                "echo hi > out.txt".to_owned(),
            ],
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

    /// Cache participation off: this test exercises signing, not the cache.
    fn plain_exec() -> ExecConfig {
        ExecConfig {
            sandbox: false,
            enforce_memory: false,
            isolated: None,
            local_reuse: false,
            fresh: false,
        }
    }

    /// A stub "release build": a plan whose environment declares the signing
    /// marker (as `ant build --release` plans it), whose argv is a script that
    /// records the four signing variables it received into a declared output.
    /// This is the P4 e2e: secrets must reach exactly this spawned process,
    /// only when the plan references the keystore they unlock.
    fn signing_stub_action(reference: &str) -> Action {
        use build_adapter::gradle::{
            KEY_ALIAS_ENV, KEY_PASSWORD_ENV, KEYSTORE_ENV, SIGNING_MARKER, STORE_PASSWORD_ENV,
        };
        let argv = if cfg!(windows) {
            vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                format!(
                    "echo %{KEYSTORE_ENV}% %{STORE_PASSWORD_ENV}% %{KEY_ALIAS_ENV}% %{KEY_PASSWORD_ENV}%> probe.txt"
                ),
            ]
        } else {
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "echo \"${KEYSTORE_ENV} ${STORE_PASSWORD_ENV} ${KEY_ALIAS_ENV} ${KEY_PASSWORD_ENV}\" > probe.txt"
                ),
            ]
        };
        Action {
            schema_version: 1,
            id: "assembleRelease".to_owned(),
            argv,
            working_directory: std::path::PathBuf::from("."),
            inputs: vec![Input {
                path: std::path::PathBuf::from("project.txt"),
                kind: InputKind::File,
            }],
            outputs: vec![Output {
                path: std::path::PathBuf::from("probe.txt"),
                kind: OutputKind::File,
            }],
            environment: vec![(SIGNING_MARKER.to_owned(), reference.to_owned())],
            cache_policy: CachePolicy::Local,
            toolchain: None,
            timeout_ms: None,
            resources: None,
        }
    }

    #[test]
    fn signing_secrets_reach_only_a_matching_plan_and_never_a_cache() {
        let workspace = TempDir::new("sign-ws");
        workspace.write("project.txt", b"project");
        let participant = CacheParticipant::new(None).expect("no cache participation");
        let cancellation = CancellationToken::new();
        let reference = format!("sha256:{}", "c".repeat(64));
        let secrets = crate::signing::SigningSecrets {
            reference: reference.clone(),
            values: vec![
                (
                    build_adapter::gradle::KEYSTORE_ENV.to_owned(),
                    "keystore-bytes".to_owned(),
                ),
                (
                    build_adapter::gradle::STORE_PASSWORD_ENV.to_owned(),
                    "store-pass".to_owned(),
                ),
                (
                    build_adapter::gradle::KEY_ALIAS_ENV.to_owned(),
                    "app".to_owned(),
                ),
                (
                    build_adapter::gradle::KEY_PASSWORD_ENV.to_owned(),
                    "key-pass".to_owned(),
                ),
            ],
        };
        let action = signing_stub_action(&reference);

        // No secrets at all: a signing plan refuses to run unsigned.
        let err = match run_action(
            &participant,
            workspace.path(),
            &action,
            &cancellation,
            plain_exec(),
            None,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a signing plan without secrets must fail closed"),
        };
        assert!(
            matches!(err, ExecutionError::Secrets(_)),
            "a signing plan without secrets fails closed: {err}"
        );

        // The wrong keystore: the plan's marker names a different digest.
        let other = crate::signing::SigningSecrets {
            reference: format!("sha256:{}", "d".repeat(64)),
            values: secrets.values.clone(),
        };
        let err = match run_action(
            &participant,
            workspace.path(),
            &action,
            &cancellation,
            plain_exec(),
            Some(&other),
        ) {
            Err(error) => error,
            Ok(_) => panic!("the plan and the keystore must agree before anything runs"),
        };
        assert!(
            matches!(err, ExecutionError::Secrets(_)),
            "the plan and the keystore must agree before anything runs: {err}"
        );

        // The matching keystore: the stub build received the secret values.
        let run = run_action(
            &participant,
            workspace.path(),
            &action,
            &cancellation,
            plain_exec(),
            Some(&secrets),
        )
        .expect("run the signing stub");
        let result = match run {
            ActionRun::Executed { result, .. } => result,
            ActionRun::Reused { .. } => panic!("no cache participates; the stub must execute"),
        };
        assert!(result.success(), "the stub build must succeed");
        let probe =
            fs::read_to_string(workspace.path().join("probe.txt")).expect("stub wrote its probe");
        assert!(
            probe.contains("keystore-bytes") && probe.contains("store-pass"),
            "the secrets must reach the spawned process: {probe}"
        );
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
        cache
            .store(&key, workspace.path(), &outputs)
            .expect("store");

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
        cache
            .store(&key, workspace.path(), &outputs)
            .expect("store");

        // Corrupt the stored blob in place.
        let blob_dir = cache.root().join("blobs");
        let blob = fs::read_dir(&blob_dir)
            .expect("read blobs")
            .next()
            .expect("one blob")
            .expect("dir entry")
            .path();
        fs::write(&blob, b"tampered").expect("corrupt blob");
        // The output must come from the blob (an output already in place
        // with the right content is kept without reading the cache).
        fs::remove_file(workspace.path().join("out.txt")).expect("remove output");

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
        assert!(
            error.contains("could not open cache directory"),
            "was {error}"
        );
    }
}
