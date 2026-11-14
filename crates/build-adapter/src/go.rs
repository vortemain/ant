//! The Go adapter (roadmap P6): builds a Go module's main package.
//!
//! A Go module is a directory with a `go.mod` (the module manifest). The
//! adapter reads the module path from `go.mod`, derives the binary name from
//! its last path segment, and generates one action that runs
//! `go build -o dist/<name><EXE_SUFFIX> .`, declaring the workspace inputs it
//! can honestly cover and the binary it produces.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Only the root main package is built: `go build -o dist/<name> .`. A
//!   library module (`package foo`, no `func main`) compiles but produces no
//!   deliverable; `go build` with `-o` still writes a file, but the action
//!   fails at execution time when the package has no `main` — the adapter
//!   produces a valid plan regardless, and the build's failure is the honest
//!   signal that the module is not a command. A `cmd/` subdirectory layout
//!   arrives in a later phase.
//! - Go resolves modules from the module proxy / VCS into `~/go/pkg/mod`
//!   (or `$GOPATH/pkg/mod`); those sources live outside the workspace and are
//!   undeclared inputs, so generated actions are `cache_policy: never`
//!   (D-003, D-010) — the same stance the cargo and npm adapters take on
//!   registry dependency sources. `go.sum` is a declared input (pinned
//!   hashes); `vendor/` is a declared tree when present (vendored deps are
//!   workspace source).
//! - The output binary path is deterministic: `dist/<name><EXE_SUFFIX>`,
//!   where `<name>` is the last segment of the module path from `go.mod`
//!   (e.g. `github.com/user/myapp` → `myapp` → `dist/myapp` on Linux,
//!   `dist/myapp.exe` on Windows). It is declared as a `File` output and
//!   verified to exist by the executor after the run.
//! - Toolchain identity is captured from the real `go`: the version from
//!   `go version` (e.g. `go version go1.22.2 windows/amd64`) and the SHA-256
//!   digest of the resolved `go` executable. `go` is a standalone executable
//!   (`go.exe` on Windows), not a batch shim, so no `cmd /C` wrapper is
//!   needed.
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

pub struct GoAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "goal",
    values: "build|vet",
    description: "the Go build goal; `build` (default) compiles the root main package into \
                      dist/<name>, `vet` runs `go vet ./...` (checks all packages, no binary)",
}];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Build,
    Vet,
}

fn mode_from(options: &AdapterOptions) -> Result<Mode, AdapterError> {
    match options.get("goal") {
        None | Some("build") => Ok(Mode::Build),
        Some("vet") => Ok(Mode::Vet),
        Some(other) => Err(AdapterError::Option(format!(
            "go goal '{other}' is not supported (supported: build, vet)"
        ))),
    }
}

impl BuildAdapter for GoAdapter {
    fn name(&self) -> &'static str {
        "go"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root.join("go.mod").is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no go.mod at {}",
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
        let mode = mode_from(options)?;
        let root = canonical_root(project_root)?;
        // Require a Go module: detect's marker check, repeated here so a
        // `--adapter go` invocation on the wrong project fails at generation
        // with the same honest reason rather than a misleading toolchain
        // error.
        let go_mod = root.join("go.mod");
        if !go_mod.is_file() {
            return Err(AdapterError::Project(format!(
                "no go.mod at {}",
                root.display()
            )));
        }

        let module_name = read_module_name(&go_mod)?;
        let binary_name = derive_binary_name(&module_name);
        let inputs = declare_inputs(&root);
        let (_, action) = match mode {
            Mode::Build => {
                let id = "go-build".to_owned();
                let outputs = vec![Output {
                    path: PathBuf::from("dist")
                        .join(format!("{binary_name}{}", std::env::consts::EXE_SUFFIX)),
                    kind: OutputKind::File,
                }];
                (
                    id.clone(),
                    build_action(id, mode, &binary_name, inputs, outputs),
                )
            }
            Mode::Vet => {
                let id = "go-vet".to_owned();
                (
                    id.clone(),
                    build_action(id, mode, &binary_name, inputs, Vec::new()),
                )
            }
        };
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = go_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Read the module path from `go.mod`: the first non-comment line starting
/// with `module `.
fn read_module_name(go_mod: &Path) -> Result<String, AdapterError> {
    let text = std::fs::read_to_string(go_mod).map_err(|error| {
        AdapterError::Project(format!("cannot read {}: {error}", go_mod.display()))
    })?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("module ") {
            return Ok(rest.trim().trim_matches('"').to_owned());
        }
    }
    Err(AdapterError::Project(format!(
        "go.mod at {} has no `module` directive",
        go_mod.parent().unwrap_or(go_mod).display()
    )))
}

/// Derive the binary name from the module path: the last segment after the
/// final `/` (e.g. `github.com/user/myapp` → `myapp`).
fn derive_binary_name(module_path: &str) -> String {
    module_path
        .rsplit('/')
        .next()
        .unwrap_or(module_path)
        .trim_matches('"')
        .to_owned()
}

/// Declare the workspace inputs Ant can honestly cover: `go.mod` (the module
/// manifest), `go.sum` (pinned hashes) when present, `vendor/` (vendored
/// deps) when present, and `*.go` source files at the root. The Go module
/// cache (`~/go/pkg/mod`) is outside the workspace and intentionally not
/// declared (see the module docs).
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    inputs.insert(PathBuf::from("go.mod"), InputKind::File);
    if root.join("go.sum").is_file() {
        inputs.insert(PathBuf::from("go.sum"), InputKind::File);
    }
    if root.join("vendor").is_dir() {
        inputs.insert(PathBuf::from("vendor"), InputKind::Tree);
    }
    // Go source files at the root package.
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".go"))
            {
                inputs.insert(PathBuf::from(entry.file_name()), InputKind::File);
            }
        }
    }
    inputs
}

/// `go build -o dist/<name><EXE_SUFFIX> .` or `go vet ./...`.
fn go_build_argv(mode: Mode, binary_name: &str) -> Vec<String> {
    match mode {
        Mode::Build => {
            let output = format!("dist/{binary_name}{}", std::env::consts::EXE_SUFFIX);
            vec![
                "go".to_owned(),
                "build".to_owned(),
                "-o".to_owned(),
                output,
                ".".to_owned(),
            ]
        }
        Mode::Vet => vec!["go".to_owned(), "vet".to_owned(), "./...".to_owned()],
    }
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

fn build_action(
    id: String,
    mode: Mode,
    binary_name: &str,
    inputs: BTreeMap<PathBuf, InputKind>,
    outputs: Vec<Output>,
) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: go_build_argv(mode, binary_name),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs,
        environment: staging_environment(),
        // The Go module proxy and the module cache are undeclared inputs
        // (they live outside the workspace), so these actions never claim
        // shared cache eligibility (D-003, D-010).
        cache_policy: CachePolicy::Never,
        toolchain: None,
        timeout_ms: None,
        resources: None,
    }
}

/// Truthful Go toolchain identity: the version from `go version` and the
/// SHA-256 digest of the resolved `go` executable.
fn go_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    let go = locate_executable(&format!("go{}", std::env::consts::EXE_SUFFIX))?;
    let digest_target = go.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve go executable {}: {error}",
            go.display()
        ))
    })?;
    let version = executable_version(&go, root)?;
    // `go version` prints e.g. `go version go1.22.2 windows/amd64`; the full
    // line is the version.
    let version = version.lines().next().unwrap_or_default().trim().to_owned();
    let executable_digest = crate::toolchain_digest(&digest_target)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest go executable {}: {error}",
                digest_target.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "go".to_owned(),
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
                "ant-go-adapter-{name}-{}-{unique}",
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

    fn go_mod() -> &'static str {
        "module github.com/example/myapp\n\ngo 1.22\n"
    }

    #[test]
    fn detect_recognizes_go_mod_and_rejects_others() {
        let adapter = GoAdapter;
        let dir = TempDir::new("detect");
        write(&dir.path().join("go.mod"), go_mod());
        assert!(adapter.detect(dir.path()).expect("detect").supported);

        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no go.mod"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn goal_rejects_unknown_values() {
        let adapter = GoAdapter;
        let dir = TempDir::new("goal");
        write(&dir.path().join("go.mod"), go_mod());
        let error = adapter
            .generate(dir.path(), &options(&[("goal", "test")]))
            .expect_err("bad goal must fail");
        assert!(
            error.to_string().contains("supported: build, vet"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_go_project() {
        let adapter = GoAdapter;
        let dir = TempDir::new("not-go");
        write(&dir.path().join("README.md"), "not a go module");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("no go.mod"), "{}", error);
        // No staging directory is written on failure.
        assert!(!dir.path().join(".ant").exists());
    }

    #[test]
    fn reject_unknown_option_keys() {
        let adapter = GoAdapter;
        let dir = TempDir::new("bad-option");
        write(&dir.path().join("go.mod"), go_mod());
        let error = adapter
            .generate(dir.path(), &options(&[("format", "wheel")]))
            .expect_err("unknown option must fail");
        assert!(error.to_string().contains("format"), "{}", error);
    }

    #[test]
    fn read_module_name_parses_module_directive() {
        let dir = TempDir::new("module");
        write(&dir.path().join("go.mod"), go_mod());
        assert_eq!(
            read_module_name(&dir.path().join("go.mod")).expect("read"),
            "github.com/example/myapp"
        );
    }

    #[test]
    fn read_module_name_skips_comments() {
        let dir = TempDir::new("comment");
        write(
            &dir.path().join("go.mod"),
            "// a comment\nmodule github.com/user/project\n\ngo 1.22\n",
        );
        assert_eq!(
            read_module_name(&dir.path().join("go.mod")).expect("read"),
            "github.com/user/project"
        );
    }

    #[test]
    fn read_module_name_fails_without_directive() {
        let dir = TempDir::new("no-module");
        write(&dir.path().join("go.mod"), "go 1.22\n");
        assert!(read_module_name(&dir.path().join("go.mod")).is_err());
    }

    #[test]
    fn derive_binary_name_from_module_path() {
        assert_eq!(derive_binary_name("github.com/user/myapp"), "myapp");
        assert_eq!(derive_binary_name("example.com/simple"), "simple");
    }

    #[test]
    fn go_build_argv_build_mode() {
        let argv = go_build_argv(Mode::Build, "myapp");
        assert!(argv.contains(&"go".to_owned()));
        assert!(argv.contains(&"build".to_owned()));
        assert!(argv.contains(&"-o".to_owned()));
        assert!(argv.contains(&".".to_owned()));
        let output_arg = argv.iter().find(|a| a.starts_with("dist/"));
        assert!(
            output_arg.is_some(),
            "expected a dist/<name> output arg, got {argv:?}"
        );
    }

    #[test]
    fn go_build_argv_vet_mode() {
        let argv = go_build_argv(Mode::Vet, "myapp");
        assert_eq!(argv, vec!["go", "vet", "./..."]);
    }

    #[test]
    fn declare_inputs_covers_go_mod_and_sources() {
        let dir = TempDir::new("inputs");
        write(&dir.path().join("go.mod"), go_mod());
        write(
            &dir.path().join("go.sum"),
            "github.com/example/dep v1.0.0\n",
        );
        write(
            &dir.path().join("main.go"),
            "package main\n\nfunc main() {}\n",
        );
        fs::create_dir_all(dir.path().join("vendor")).expect("mkdir vendor");
        let inputs = declare_inputs(dir.path());
        assert_eq!(inputs.get(&PathBuf::from("go.mod")), Some(&InputKind::File));
        assert_eq!(inputs.get(&PathBuf::from("go.sum")), Some(&InputKind::File));
        assert_eq!(inputs.get(&PathBuf::from("vendor")), Some(&InputKind::Tree));
        assert_eq!(
            inputs.get(&PathBuf::from("main.go")),
            Some(&InputKind::File)
        );
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
    fn generate_assembles_go_build_argv() {
        // Requires go on PATH; skip honestly where it is absent (Go may not be
        // installed on every CI runner). The ubuntu-latest CI runner has go
        // preinstalled.
        if locate_executable(&format!("go{}", std::env::consts::EXE_SUFFIX)).is_err() {
            eprintln!("skipping: go not on PATH");
            return;
        }
        let adapter = GoAdapter;
        let dir = TempDir::new("argv");
        write(&dir.path().join("go.mod"), go_mod());
        write(
            &dir.path().join("main.go"),
            "package main\n\nfunc main() {}\n",
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
        assert_eq!(action.id, "go-build");
        assert_eq!(action.working_directory, PathBuf::from("."));
        let argv: Vec<&str> = action.argv.iter().map(String::as_str).collect();
        assert!(argv.contains(&"go"));
        assert!(argv.contains(&"build"));
        assert!(argv.contains(&"-o"));
        assert!(argv.contains(&"."));
        assert!(action.outputs.iter().any(|out| {
            out.path
                .to_str()
                .is_some_and(|s| s.starts_with("dist/myapp"))
        }));
        assert_eq!(action.toolchain.as_ref().expect("toolchain").name, "go");
    }

    #[test]
    fn generate_vet_goal_has_no_outputs() {
        if locate_executable(&format!("go{}", std::env::consts::EXE_SUFFIX)).is_err() {
            eprintln!("skipping: go not on PATH");
            return;
        }
        let adapter = GoAdapter;
        let dir = TempDir::new("vet");
        write(&dir.path().join("go.mod"), go_mod());
        write(
            &dir.path().join("main.go"),
            "package main\n\nfunc main() {}\n",
        );
        let plan = adapter
            .generate(dir.path(), &options(&[("goal", "vet")]))
            .expect("generate");
        let action = &plan.actions[0].action;
        assert_eq!(action.id, "go-vet");
        assert!(action.outputs.is_empty());
        let argv: Vec<&str> = action.argv.iter().map(String::as_str).collect();
        assert!(argv.contains(&"vet"));
        assert!(argv.contains(&"./..."));
    }
}
