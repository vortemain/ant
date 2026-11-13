//! The Ant adapter contract: turning a real build-system project into a
//! validated Ant plan.
//!
//! An adapter answers two questions for one project root: does it recognize
//! the project (`detect`), and what actions would build it (`generate`)?
//! Generation must produce a fully validated [`build_graph::PlanFile`] — the
//! same versioned document `ant run --plan` accepts — so adapter output is
//! never a second, weaker action model. Contracts live below adapters; an
//! adapter depends on domain crates, never the reverse (see
//! `docs/architecture.md`).
//!
//! Honesty rules for adapters (decision D-010):
//! - Declared inputs must cover everything that can change the result. When a
//!   dependency's source content cannot be declared (it lives outside the
//!   workspace), the generated actions must use `cache_policy: never` rather
//!   than claim eligibility.
//! - Toolchain identity is captured from the real toolchain, never invented.
//! - Outputs are declared only where the mapping is deterministic; unknown
//!   artifact layouts are omitted rather than guessed.
//!
//! One deliberate exception to inspection-only generation: an adapter may
//! prepare its staging directory (`.ant/tmp` inside the project) so generated
//! actions can point `TMP`/`TEMP` at a workspace-relative path. Toolchains
//! write scratch files to the system temp directory during compilation and
//! linking; redirecting those writes into the workspace is what makes
//! generated plans runnable under `ant run --sandbox`, where writes outside
//! the workspace are confined. The directory is idempotently created, is not
//! a declared input or output, and is safe to delete at any time.

pub mod cargo;
pub mod npm;

use build_graph::{GraphError, PlanFile};
use std::fmt;
use std::path::{Path, PathBuf};

/// Why an adapter does or does not recognize a project root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Detection {
    pub supported: bool,
    /// When unsupported, a human-readable reason for diagnostics.
    pub reason: String,
}

impl Detection {
    pub fn supported() -> Self {
        Self { supported: true, reason: String::new() }
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self { supported: false, reason: reason.into() }
    }
}

/// Errors raised while generating a plan for a recognized project.
#[derive(Debug)]
pub enum AdapterError {
    /// The project exists but its build-system metadata is unusable.
    Project(String),
    /// The toolchain required by the adapter is missing or failed.
    Toolchain(String),
    /// The generated plan failed validation (never expected for a correct
    /// adapter; kept explicit so generated output is always re-checked).
    Plan(GraphError),
    Io(std::io::Error),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(detail) => write!(f, "project metadata unusable: {detail}"),
            Self::Toolchain(detail) => write!(f, "toolchain problem: {detail}"),
            Self::Plan(error) => write!(f, "generated plan is invalid: {error}"),
            Self::Io(error) => write!(f, "io error: {error}"),
        }
    }
}

impl std::error::Error for AdapterError {}

impl From<GraphError> for AdapterError {
    fn from(error: GraphError) -> Self {
        Self::Plan(error)
    }
}

impl From<std::io::Error> for AdapterError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// One build-system adapter. Implementations must be side-effect free with
/// respect to the project (inspection only) and deterministic: two calls on
/// the same unchanged project produce the same plan.
pub trait BuildAdapter {
    /// Stable adapter name, matching the CLI `--adapter` value.
    fn name(&self) -> &'static str;

    /// Non-destructive compatibility check for one project root.
    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError>;

    /// Generate a validated plan for one recognized project root.
    fn generate(&self, project_root: &Path) -> Result<PlanFile, AdapterError>;
}

/// The adapters compiled into this build, in registry order.
pub fn available() -> Vec<&'static dyn BuildAdapter> {
    vec![&cargo::CargoAdapter, &npm::NpmAdapter]
}

/// Look up an adapter by its stable name.
pub fn adapter(name: &str) -> Option<&'static dyn BuildAdapter> {
    available().into_iter().find(|candidate| candidate.name() == name)
}

/// One adapter's detection outcome for a project root, for diagnostics.
#[derive(Debug)]
pub struct DetectionReport {
    pub adapter: &'static str,
    pub outcome: Result<Detection, AdapterError>,
}

/// Run every registered adapter's `detect` against `project_root`, in
/// registry order. Detection is inspection-only, so this never writes.
pub fn detect_all(project_root: &Path) -> Vec<DetectionReport> {
    available()
        .into_iter()
        .map(|adapter| DetectionReport {
            adapter: adapter.name(),
            outcome: adapter.detect(project_root),
        })
        .collect()
}

/// Why automatic adapter selection could not pick exactly one adapter.
#[derive(Debug)]
pub enum SelectionError {
    /// No adapter recognized the project; each adapter's reason is kept.
    NoMatch(Vec<(&'static str, String)>),
    /// More than one adapter recognized the project. Selection never guesses
    /// a priority between ecosystems; the caller must name one.
    Ambiguous(Vec<&'static str>),
    /// An adapter's detection itself failed, so the outcome is unknown and
    /// selection fails closed rather than skipping it.
    Detect { adapter: &'static str, error: AdapterError },
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMatch(reasons) => {
                write!(f, "no adapter recognizes the project")?;
                for (adapter, reason) in reasons {
                    write!(f, "; {adapter}: {reason}")?;
                }
                Ok(())
            }
            Self::Ambiguous(names) => write!(
                f,
                "multiple adapters recognize the project ({}); choose one with --adapter",
                names.join(", ")
            ),
            Self::Detect { adapter, error } => {
                write!(f, "adapter '{adapter}' detection failed: {error}")
            }
        }
    }
}

impl std::error::Error for SelectionError {}

/// Pick the single adapter that recognizes `project_root`. Fails when none or
/// several match (no ecosystem priority is invented — D-010 honesty rules),
/// and fails closed when any adapter's detection errors.
pub fn select_adapter(project_root: &Path) -> Result<&'static dyn BuildAdapter, SelectionError> {
    let mut matches = Vec::new();
    let mut reasons = Vec::new();
    for report in detect_all(project_root) {
        match report.outcome {
            Err(error) => return Err(SelectionError::Detect { adapter: report.adapter, error }),
            Ok(detection) if detection.supported => matches.push(report.adapter),
            Ok(detection) => reasons.push((report.adapter, detection.reason)),
        }
    }
    match matches.as_slice() {
        [] => Err(SelectionError::NoMatch(reasons)),
        [only] => Ok(adapter(only).expect("registered adapter name resolves")),
        _ => Err(SelectionError::Ambiguous(matches)),
    }
}

/// Canonicalize a project root for adapter work and return both the canonical
/// root and a helper that makes absolute paths workspace-relative.
pub(crate) fn canonical_root(project_root: &Path) -> Result<PathBuf, AdapterError> {
    project_root
        .canonicalize()
        .map_err(|error| AdapterError::Project(format!(
            "cannot resolve project root {}: {error}",
            project_root.display()
        )))
}

/// Convert an absolute path under `root` into a workspace-relative path,
/// failing closed when the path is outside the root.
pub(crate) fn relative_to(root: &Path, path: &Path) -> Result<PathBuf, AdapterError> {
    path.strip_prefix(root)
        .map(|relative| {
            // Plain path separators, matching the workspace-relative contract.
            relative.to_path_buf()
        })
        .map_err(|_| AdapterError::Project(format!(
            "path {} is not inside the project root {}",
            path.display(),
            root.display()
        )))
}

/// Resolve an executable `name` on PATH the same way shells do, so the digest
/// recorded as toolchain identity belongs to the exact executable that will
/// run. `name` should already include the platform executable suffix where
/// the platform needs one (callers pass `format!("node{}", EXE_SUFFIX)` etc.).
pub(crate) fn locate_executable(name: &str) -> Result<PathBuf, AdapterError> {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(AdapterError::Toolchain(format!(
        "{name} was not found on PATH; the adapter cannot generate a plan without it"
    )))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    // Windows treats files as executable by extension, which the candidate
    // name already satisfies; Unix checks the mode bits.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Run `executable --version` in `root` and return its trimmed stdout, for
/// truthful toolchain identity capture.
pub(crate) fn executable_version(executable: &Path, root: &Path) -> Result<String, AdapterError> {
    let output = std::process::Command::new(executable)
        .arg("--version")
        .current_dir(root)
        .output()
        .map_err(|error| AdapterError::Toolchain(format!(
            "cannot run {executable}: {error}",
            executable = executable.display()
        )))?;
    if !output.status.success() {
        return Err(AdapterError::Toolchain(format!(
            "{exe} --version failed: {stderr}",
            exe = executable.display(),
            stderr = String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        return Err(AdapterError::Toolchain(format!(
            "{exe} --version produced no output",
            exe = executable.display()
        )));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ant-adapter-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registry_finds_adapters_by_name() {
        assert!(adapter("cargo").is_some(), "the cargo adapter must be registered");
        assert!(adapter("no-such-adapter").is_none());
        assert!(available().iter().any(|candidate| candidate.name() == "cargo"));
        assert!(adapter("npm").is_some(), "the npm adapter must be registered");
    }

    #[test]
    fn selection_picks_the_single_matching_adapter() {
        let dir = TempDir::new("select-one");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        let selected = select_adapter(dir.path()).expect("select");
        assert_eq!(selected.name(), "cargo");

        let npm_dir = TempDir::new("select-npm");
        fs::write(npm_dir.path().join("package.json"), "{}").expect("write");
        assert_eq!(select_adapter(npm_dir.path()).expect("select").name(), "npm");
    }

    #[test]
    fn selection_reports_every_reason_when_nothing_matches() {
        let dir = TempDir::new("select-none");
        let error = select_adapter(dir.path()).err().expect("no match");
        let SelectionError::NoMatch(reasons) = &error else {
            panic!("expected NoMatch, got {error:?}");
        };
        let names: Vec<_> = reasons.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["cargo", "npm"], "one reason per registered adapter");
        let text = error.to_string();
        assert!(text.contains("no Cargo.toml") && text.contains("no package.json"), "{text}");
    }

    #[test]
    fn selection_refuses_to_guess_between_multiple_matches() {
        let dir = TempDir::new("select-many");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        fs::write(dir.path().join("package.json"), "{}").expect("write");
        let error = select_adapter(dir.path()).err().expect("ambiguous");
        assert!(
            matches!(&error, SelectionError::Ambiguous(names) if names == &vec!["cargo", "npm"]),
            "{error:?}"
        );
        assert!(error.to_string().contains("--adapter"));

        let reports = detect_all(dir.path());
        assert!(reports.iter().all(|report| report.outcome.as_ref().is_ok_and(|d| d.supported)));
    }

    #[test]
    fn relative_to_fails_closed_outside_root() {
        let dir = TempDir::new("rel");
        let inner = dir.path().join("sub");
        fs::create_dir_all(&inner).expect("mkdir");
        assert_eq!(
            relative_to(dir.path(), &inner.join("file.txt")).expect("inside root"),
            PathBuf::from("sub").join("file.txt")
        );
        let outside = TempDir::new("elsewhere");
        assert!(
            relative_to(dir.path(), &outside.path().join("f")).is_err(),
            "paths outside the root must be rejected"
        );
    }

    #[test]
    fn canonical_root_rejects_missing_directories() {
        let dir = TempDir::new("missing");
        let missing = dir.path().join("does-not-exist");
        assert!(canonical_root(&missing).is_err());
    }
}
