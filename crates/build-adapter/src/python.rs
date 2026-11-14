//! The Python adapter (roadmap P6): builds a Python package with PEP 517.
//!
//! A Python project has a `pyproject.toml` (PEP 518/621, with a
//! `[build-system]` table) or a legacy `setup.py`. The adapter generates one
//! action that runs `python -m build`, declaring the workspace inputs it can
//! honestly cover and the `dist/` tree it produces.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Only `python -m build` is supported. It calls the PEP 517 backend
//!   declared in `pyproject.toml`'s `[build-system]` (or the legacy
//!   `setup.py`/`setup.cfg` metadata) and writes a wheel and an sdist into
//!   `dist/`. The `build` package must be installed (`pip install build`);
//!   if it is not, the action fails at execution time — the adapter produces a
//!   valid plan regardless, the way the npm adapter produces `npm run build`
//!   whether `npm` is configured or not.
//! - PEP 517 build dependencies (the `requires` in `[build-system]`) are
//!   resolved from the pip registry into an isolated environment; those sources
//!   live outside the workspace and are undeclared inputs, so generated actions
//!   are `cache_policy: never` (D-003, D-010) — the same stance the cargo and
//!   npm adapters take on registry dependency sources.
//! - Outputs are declared as `dist/` (`OutputKind::Tree`). A wheel's filename
//!   encodes the version, platform, and Python tag — all deterministic for a
//!   given project, but not a fixed name the adapter can hardcode. The tree
//!   output covers the wheel (`dist/*.whl`) and the sdist (`dist/*.tar.gz`)
//!   without guessing either name.
//! - Toolchain identity is captured from the real `python` runtime: the
//!   resolved PATH executable's SHA-256 digest and `python --version`
//!   (e.g. `Python 3.13.6`). `python` is a standalone executable
//!   (`python3` on Linux, `python.exe` on Windows), not a batch shim, so no
//!   `cmd /C` wrapper is needed on Windows.
//! - Workspace-relative scratch: `TMP`/`TEMP` (plus `TMPDIR` off Windows)
//!   point at `.ant/tmp`, the staging pattern shared with the other adapters.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};

pub struct PythonAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "format",
    values: "wheel|sdist|all",
    description: "the build artifact format; `wheel` (default) builds only the wheel, `sdist` \
                      only the source distribution, `all` both",
}];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Wheel,
    Sdist,
    All,
}

impl Format {
    fn flag(self) -> Option<&'static str> {
        match self {
            Format::Wheel => Some("--wheel"),
            Format::Sdist => Some("--sdist"),
            Format::All => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Format::Wheel => "wheel",
            Format::Sdist => "sdist",
            Format::All => "all",
        }
    }
}

fn format_from(options: &AdapterOptions) -> Result<Format, AdapterError> {
    match options.get("format") {
        None | Some("wheel") => Ok(Format::Wheel),
        Some("sdist") => Ok(Format::Sdist),
        Some("all") => Ok(Format::All),
        Some(other) => Err(AdapterError::Option(format!(
            "python format '{other}' is not supported (supported: wheel, sdist, all)"
        ))),
    }
}

/// Find the Python executable: `python` on Windows, `python3` on Linux,
/// falling back to the other if the first is not found.
fn python_executable() -> Result<PathBuf, AdapterError> {
    if cfg!(windows) {
        locate_executable(&format!("python{}", std::env::consts::EXE_SUFFIX))
            .or_else(|_| locate_executable(&format!("python3{}", std::env::consts::EXE_SUFFIX)))
    } else {
        locate_executable(&format!("python3{}", std::env::consts::EXE_SUFFIX))
            .or_else(|_| locate_executable(&format!("python{}", std::env::consts::EXE_SUFFIX)))
    }
}

impl BuildAdapter for PythonAdapter {
    fn name(&self) -> &'static str {
        "python"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        let pyproject = project_root.join("pyproject.toml");
        let setup_py = project_root.join("setup.py");
        if pyproject.is_file() {
            // A `pyproject.toml` with a `[build-system]` table is a buildable
            // package (PEP 518). One without it may be tooling-only config
            // (ruff, black, etc.) and is not recognized unless `setup.py` is
            // also present.
            let text = std::fs::read_to_string(&pyproject).map_err(|error| {
                AdapterError::Project(format!("cannot read {}: {error}", pyproject.display()))
            })?;
            if text.contains("[build-system]") || setup_py.is_file() {
                Ok(Detection::supported())
            } else {
                Ok(Detection::unsupported(format!(
                    "{} has no [build-system] table; it may be tooling-only configuration. \
                     Either add a [build-system] table or provide a setup.py",
                    pyproject.display()
                )))
            }
        } else if setup_py.is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no pyproject.toml or setup.py at {}",
                project_root.display()
            )))
        }
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let format = format_from(options)?;
        let root = canonical_root(project_root)?;
        // Require a Python project: detect's marker check, repeated here so a
        // `--adapter python` invocation on the wrong project fails at
        // generation with the same honest reason rather than a misleading
        // toolchain error.
        let pyproject = root.join("pyproject.toml");
        let setup_py = root.join("setup.py");
        let has_pyproject = pyproject.is_file()
            && std::fs::read_to_string(&pyproject)
                .map(|text| text.contains("[build-system]"))
                .unwrap_or(false);
        let has_setup = setup_py.is_file();
        if !has_pyproject && !has_setup {
            return Err(AdapterError::Project(format!(
                "{} is not a buildable Python project (no pyproject.toml with [build-system] or \
                 setup.py)",
                root.display()
            )));
        }

        let inputs = declare_inputs(&root);
        let id = format!("python-build-{}", format.label());
        let action = build_action(id, format, inputs);
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = python_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Declare the workspace inputs Ant can honestly cover: `pyproject.toml`
/// (the build config), `setup.py` (legacy build script), `setup.cfg` (static
/// metadata), `requirements.txt` (pinned runtime deps), and the `src/`
/// source tree when present. PEP 517 build dependencies and the pip registry
/// are outside the workspace and intentionally not declared (see the module
/// docs).
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    if root.join("pyproject.toml").is_file() {
        inputs.insert(PathBuf::from("pyproject.toml"), InputKind::File);
    }
    if root.join("setup.py").is_file() {
        inputs.insert(PathBuf::from("setup.py"), InputKind::File);
    }
    if root.join("setup.cfg").is_file() {
        inputs.insert(PathBuf::from("setup.cfg"), InputKind::File);
    }
    if root.join("requirements.txt").is_file() {
        inputs.insert(PathBuf::from("requirements.txt"), InputKind::File);
    }
    if root.join("src").is_dir() {
        inputs.insert(PathBuf::from("src"), InputKind::Tree);
    }
    inputs
}

/// `python -m build [--wheel | --sdist]`, with no flag when `format` is
/// `all`.
fn python_build_argv(format: Format) -> Vec<String> {
    let mut argv = vec!["python".to_owned(), "-m".to_owned(), "build".to_owned()];
    if let Some(flag) = format.flag() {
        argv.push(flag.to_owned());
    }
    argv
}

/// `TMP`/`TEMP` (plus `TMPDIR` off Windows) pointing at `.ant/tmp` relative to
/// the action's working directory (the shared staging pattern).
fn staging_environment() -> Vec<(String, String)> {
    let staging_value = PathBuf::from(".ant")
        .join("tmp")
        .to_string_lossy()
        .into_owned();
    let mut environment = vec![
        ("TMP".to_owned(), staging_value.clone()),
        ("TEMP".to_owned(), staging_value.clone()),
    ];
    if cfg!(not(windows)) {
        environment.push(("TMPDIR".to_owned(), staging_value));
    }
    environment.sort();
    environment
}

fn build_action(id: String, format: Format, inputs: BTreeMap<PathBuf, InputKind>) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: python_build_argv(format),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs: vec![Output {
            path: PathBuf::from("dist"),
            kind: OutputKind::Tree,
        }],
        environment: staging_environment(),
        // PEP 517 build dependencies and the pip registry are undeclared
        // inputs (they live outside the workspace), so these actions never
        // claim shared cache eligibility (D-003, D-010).
        cache_policy: CachePolicy::Never,
        toolchain: None,
        timeout_ms: None,
        resources: None,
    }
}

/// Truthful Python toolchain identity: the version from `python --version`
/// and the SHA-256 digest of the resolved `python` executable.
fn python_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    let python = python_executable()?;
    let digest_target = python.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve python executable {}: {error}",
            python.display()
        ))
    })?;
    let version = executable_version(&python, root)?;
    // `python --version` prints `Python 3.13.6` (3.12+: single line; earlier
    // versions print to stderr). `executable_version` captures stdout; the
    // first line is the version.
    let version = version.lines().next().unwrap_or_default().trim().to_owned();
    let executable_digest = crate::toolchain_digest(&digest_target)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest python executable {}: {error}",
                digest_target.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "python".to_owned(),
        version,
        executable_digest,
    })
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
                "ant-python-adapter-{name}-{}-{unique}",
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

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdirs");
        }
        fs::write(path, contents).expect("write");
    }

    fn options(pairs: &[(&str, &str)]) -> AdapterOptions {
        let mut options = AdapterOptions::new();
        for (key, value) in pairs {
            options.insert(key, value).expect("insert option");
        }
        options
    }

    fn pyproject_toml() -> &'static str {
        "[build-system]\nrequires = [\"setuptools>=61.0\"]\nbuild-backend = \"setuptools.build_meta\"\n\n\
[project]\nname = \"example\"\nversion = \"0.1.0\"\n"
    }

    #[test]
    fn detect_recognizes_pyproject_and_rejects_others() {
        let adapter = PythonAdapter;
        let dir = TempDir::new("detect");
        write(&dir.path().join("pyproject.toml"), pyproject_toml());
        assert!(adapter.detect(dir.path()).expect("detect").supported);

        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no pyproject.toml"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn detect_recognizes_setup_py() {
        let adapter = PythonAdapter;
        let dir = TempDir::new("setup");
        write(
            &dir.path().join("setup.py"),
            "from setuptools import setup\nsetup(name='example')\n",
        );
        assert!(adapter.detect(dir.path()).expect("detect").supported);
    }

    #[test]
    fn detect_rejects_pyproject_without_build_system() {
        let adapter = PythonAdapter;
        let dir = TempDir::new("tooling-only");
        write(
            &dir.path().join("pyproject.toml"),
            "[tool.ruff]\nline-length = 100\n",
        );
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("[build-system]"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn format_rejects_unknown_values() {
        let adapter = PythonAdapter;
        let dir = TempDir::new("format");
        write(&dir.path().join("pyproject.toml"), pyproject_toml());
        let error = adapter
            .generate(dir.path(), &options(&[("format", "egg")]))
            .expect_err("bad format must fail");
        assert!(
            error.to_string().contains("supported: wheel, sdist, all"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_python_project() {
        let adapter = PythonAdapter;
        let dir = TempDir::new("not-python");
        write(&dir.path().join("README.md"), "not a python package");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(
            error.to_string().contains("not a buildable Python project"),
            "{}",
            error
        );
        // No staging directory is written on failure.
        assert!(!dir.path().join(".ant").exists());
    }

    #[test]
    fn reject_unknown_option_keys() {
        let adapter = PythonAdapter;
        let dir = TempDir::new("bad-option");
        write(&dir.path().join("pyproject.toml"), pyproject_toml());
        let error = adapter
            .generate(dir.path(), &options(&[("profile", "release")]))
            .expect_err("unknown option must fail");
        assert!(error.to_string().contains("profile"), "{}", error);
    }

    #[test]
    fn python_build_argv_wheel_only() {
        let argv = python_build_argv(Format::Wheel);
        assert_eq!(argv, vec!["python", "-m", "build", "--wheel"]);
    }

    #[test]
    fn python_build_argv_sdist_only() {
        let argv = python_build_argv(Format::Sdist);
        assert_eq!(argv, vec!["python", "-m", "build", "--sdist"]);
    }

    #[test]
    fn python_build_argv_all_is_unflagged() {
        let argv = python_build_argv(Format::All);
        assert_eq!(argv, vec!["python", "-m", "build"]);
    }

    #[test]
    fn declare_inputs_covers_pyproject_and_src() {
        let dir = TempDir::new("inputs");
        write(&dir.path().join("pyproject.toml"), pyproject_toml());
        write(
            &dir.path().join("setup.cfg"),
            "[metadata]\nname = example\n",
        );
        write(&dir.path().join("src/example/__init__.py"), "");
        let inputs = declare_inputs(dir.path());
        assert_eq!(
            inputs.get(&PathBuf::from("pyproject.toml")),
            Some(&InputKind::File)
        );
        assert_eq!(
            inputs.get(&PathBuf::from("setup.cfg")),
            Some(&InputKind::File)
        );
        assert_eq!(inputs.get(&PathBuf::from("src")), Some(&InputKind::Tree));
    }

    #[test]
    fn staging_environment_points_scratch_at_ant_tmp() {
        let env = staging_environment();
        let staging = PathBuf::from(".ant")
            .join("tmp")
            .to_string_lossy()
            .into_owned();
        assert!(env.contains(&("TMP".to_owned(), staging.clone())));
        assert!(env.contains(&("TEMP".to_owned(), staging)));
    }

    #[test]
    fn generate_assembles_python_build_argv() {
        // Requires python on PATH; skip honestly where it is absent.
        if python_executable().is_err() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let adapter = PythonAdapter;
        let dir = TempDir::new("argv");
        write(&dir.path().join("pyproject.toml"), pyproject_toml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
        assert_eq!(action.id, "python-build-wheel");
        assert_eq!(action.working_directory, PathBuf::from("."));
        let argv: Vec<&str> = action.argv.iter().map(String::as_str).collect();
        assert!(argv.contains(&"python"));
        assert!(argv.contains(&"-m"));
        assert!(argv.contains(&"build"));
        assert!(argv.contains(&"--wheel"));
        assert!(
            action
                .outputs
                .iter()
                .any(|out| out.path == Path::new("dist") && out.kind == OutputKind::Tree)
        );
        assert_eq!(action.toolchain.as_ref().expect("toolchain").name, "python");
    }

    #[test]
    fn generate_sets_cache_policy_never() {
        if python_executable().is_err() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let adapter = PythonAdapter;
        let dir = TempDir::new("cache");
        write(&dir.path().join("pyproject.toml"), pyproject_toml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
    }
}
