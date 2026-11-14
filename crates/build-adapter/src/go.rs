//! Go adapter v1 (roadmap P6, decision D-035).
//!
//! Detects a Go module (`go.mod` at the project root) and generates one
//! action: `go build -o .ant/go/bin/ ./...`, which compiles every package
//! and writes one executable per `main` package into that directory.
//!
//! - **Profiles:** `profile=release` (what `ant build` uses) adds
//!   `-trimpath -ldflags=-s -w`: no absolute paths and no symbol tables in
//!   the deliverables. `profile=dev` is a plain `go build` (debuggable).
//! - **Inputs:** the whole project except `.ant/`, `.git/`, and output
//!   directories Ant created (same walk as the cargo adapter).
//! - **Outputs:** `.ant/go/bin/<name>[.exe]` for each `main` package, named
//!   the way `go build` names it (last import-path element; the module path
//!   for the root package), from `go list`.
//! - **Environment:** the executor clears the child environment, so the
//!   plan declares Go's effective settings, read once with `go env -json`:
//!   `GOCACHE`, `GOMODCACHE`, `GOPATH`, `GOPROXY`, `GOSUMDB`, `GOPRIVATE`,
//!   `GONOPROXY`, `GONOSUMDB`, `GOFLAGS`, `GOTOOLCHAIN`, `CGO_ENABLED`,
//!   `GOOS`, `GOARCH`, plus `GOENV=off` (those values already include the
//!   user's `go env -w` settings) and workspace-relative temp directories.
//!   They are key material, so results are specific to this machine.
//! - **Reuse:** module dependencies are pinned by `go.sum` and fetched into
//!   the module cache outside the project, so plans are `cache_policy:
//!   local` (this machine reuses its own results, D-028). A `replace`
//!   directive pointing outside the project, or a symlink in the project,
//!   makes them `never`. Cross-machine reuse would need the module cache
//!   inside the project (vendoring) and a sandbox; not offered yet.
//! - **Toolchain identity:** the `go` binary's digest plus `go env
//!   GOVERSION` (which reflects `GOTOOLCHAIN` switching).

use crate::cargo::declare_project;
use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, locate_executable,
};
use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub struct GoAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "profile",
    values: "dev|release",
    description: "release adds -trimpath -ldflags=-s -w (no paths or symbols in binaries); dev is a plain go build",
}];

/// Where executables go (workspace-relative; `.ant/` is never an input).
const OUTPUT_DIR: &str = ".ant/go/bin";

/// Go settings declared into the action environment (see the module docs).
const DECLARED_SETTINGS: &[&str] = &[
    "GOCACHE",
    "GOMODCACHE",
    "GOPATH",
    "GOPROXY",
    "GOSUMDB",
    "GOPRIVATE",
    "GONOPROXY",
    "GONOSUMDB",
    "GOFLAGS",
    "GOTOOLCHAIN",
    "CGO_ENABLED",
    "GOOS",
    "GOARCH",
];

fn go_executable() -> Result<PathBuf, AdapterError> {
    locate_executable(&format!("go{}", std::env::consts::EXE_SUFFIX))
}

fn run_go(go: &Path, root: &Path, args: &[&str]) -> Result<String, AdapterError> {
    let output = Command::new(go)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| AdapterError::Toolchain(format!("cannot run go: {error}")))?;
    if !output.status.success() {
        return Err(AdapterError::Toolchain(format!(
            "go {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a `replace` target path (relative to the module root, or
/// absolute) stays inside the project.
fn replace_target_inside(root: &Path, target: &str) -> bool {
    let path = Path::new(target);
    if path.is_absolute() || path.has_root() {
        return path.starts_with(root);
    }
    let mut depth: i32 = 0;
    for component in path.components() {
        match component {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

/// Whether `go.mod` replaces a module with a directory outside the project
/// (an undeclared input). Module-path replacements (`=> other/mod v1.2.3`)
/// are pinned like any dependency.
fn replaces_outside(root: &Path, go_mod: &str) -> bool {
    go_mod.lines().any(|line| {
        let Some((_, target)) = line.split_once("=>") else {
            return false;
        };
        let target = target.split_whitespace().next().unwrap_or("");
        let local = target.starts_with('.')
            || target.starts_with('/')
            || target.starts_with('\\')
            || target.chars().nth(1) == Some(':');
        local && !replace_target_inside(root, target)
    })
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

    /// Plans read only project files (declared inputs plus the top-level
    /// listing the plan cache adds) and, outside the project, the `go`
    /// binary, `GO*`/`CGO*` environment and `PATH`, and the user's `go env
    /// -w` file (D-029).
    fn plan_cache_probe(&self, _project_root: &Path) -> Option<Vec<String>> {
        let mut probe = vec!["go plan probe v1".to_owned()];
        probe.extend(crate::environment_probe(&[
            "GO",
            "CGO",
            "APPDATA",
            "LOCALAPPDATA",
            "HOME",
            "USERPROFILE",
            "XDG_",
        ]));
        let go = go_executable().ok()?;
        probe.push(crate::stamp_line(&go));
        if let Ok(real) = go.canonicalize() {
            probe.push(crate::stamp_line(&real));
        }
        let config = if cfg!(windows) {
            std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("go").join("env"))
        } else {
            crate::home_dir().map(|home| home.join(".config").join("go").join("env"))
        };
        if let Some(config) = config {
            probe.push(crate::stamp_line(&config));
        }
        Some(probe)
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let release = match options.get("profile") {
            None | Some("dev") => false,
            Some("release") => true,
            Some(other) => {
                return Err(AdapterError::Option(format!(
                    "go profile '{other}' is not supported (supported: dev, release)"
                )));
            }
        };
        let root = canonical_root(project_root)?;
        let go_mod = std::fs::read_to_string(root.join("go.mod"))
            .map_err(|error| AdapterError::Project(format!("cannot read go.mod: {error}")))?;
        let go = go_executable()?;

        // One `go env` call gives the toolchain version and every setting
        // the child needs (the executor clears its environment).
        let mut query = vec!["env", "-json", "GOVERSION", "GOEXE"];
        query.extend(DECLARED_SETTINGS);
        let settings: BTreeMap<String, String> = serde_json::from_str(&run_go(&go, &root, &query)?)
            .map_err(|error| {
                AdapterError::Toolchain(format!("cannot parse go env output: {error}"))
            })?;
        let version = settings
            .get("GOVERSION")
            .filter(|version| !version.is_empty())
            .cloned()
            .ok_or_else(|| AdapterError::Toolchain("go env reported no GOVERSION".to_owned()))?;
        let exe_suffix = settings.get("GOEXE").cloned().unwrap_or_default();

        // Executables: one per main package, named as `go build -o dir/`
        // names them (the last element of the import path).
        let listing = run_go(
            &go,
            &root,
            &[
                "list",
                "-f",
                "{{if eq .Name \"main\"}}{{.ImportPath}}{{end}}",
                "./...",
            ],
        )?;
        let mut outputs: Vec<PathBuf> = Vec::new();
        for import_path in listing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let name = import_path.rsplit('/').next().unwrap_or(import_path);
            outputs.push(Path::new(OUTPUT_DIR).join(format!("{name}{exe_suffix}")));
        }
        outputs.sort();
        outputs.dedup();

        let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
        let mut has_symlink = false;
        declare_project(
            &root,
            Path::new(""),
            &[PathBuf::from(".ant"), PathBuf::from(".git")],
            &mut inputs,
            &mut has_symlink,
        )?;

        let toolchain_digest = crate::toolchain_digest(&go.canonicalize().unwrap_or(go.clone()))
            .map_err(|error| AdapterError::Toolchain(format!("cannot digest go: {error}")))?
            .to_string();

        std::fs::create_dir_all(root.join(".ant").join("tmp")).map_err(AdapterError::from)?;
        let staging = Path::new(".ant").join("tmp").to_string_lossy().into_owned();
        let mut environment: Vec<(String, String)> = vec![
            ("GOENV".to_owned(), "off".to_owned()),
            ("TMP".to_owned(), staging.clone()),
            ("TEMP".to_owned(), staging.clone()),
        ];
        if cfg!(not(windows)) {
            environment.push(("TMPDIR".to_owned(), staging));
        }
        for name in DECLARED_SETTINGS {
            if let Some(value) = settings.get(*name).filter(|value| !value.is_empty()) {
                environment.push(((*name).to_owned(), value.clone()));
            }
        }
        environment.sort();

        let mut argv = vec![go.to_string_lossy().into_owned(), "build".to_owned()];
        if release {
            argv.extend(["-trimpath".to_owned(), "-ldflags=-s -w".to_owned()]);
        }
        argv.extend([
            "-o".to_owned(),
            format!("{OUTPUT_DIR}/"),
            "./...".to_owned(),
        ]);

        let action = Action {
            schema_version: 1,
            id: "go-build".to_owned(),
            argv,
            working_directory: PathBuf::from("."),
            inputs: inputs
                .into_iter()
                .map(|(path, kind)| Input { path, kind })
                .collect(),
            outputs: outputs
                .into_iter()
                .map(|path| Output {
                    path,
                    kind: OutputKind::File,
                })
                .collect(),
            environment,
            cache_policy: if has_symlink || replaces_outside(&root, &go_mod) {
                CachePolicy::Never
            } else {
                CachePolicy::Local
            },
            toolchain: Some(ToolchainIdentity {
                name: "go".to_owned(),
                version,
                executable_digest: toolchain_digest,
            }),
            timeout_ms: None,
            resources: None,
            background_services: false,
        };
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        file.to_graph()?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

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
            std::fs::create_dir_all(&path).expect("mkdir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn replace_directives_outside_the_project_disable_reuse() {
        let root = Path::new("/work/app");
        assert!(!replaces_outside(
            root,
            "module x\nreplace a => ./internal/a\n"
        ));
        assert!(!replaces_outside(
            root,
            "replace a => example.com/a v1.2.3\n"
        ));
        assert!(replaces_outside(root, "replace a => ../sibling\n"));
        assert!(replaces_outside(root, "replace (\n\ta v1 => ../../a\n)\n"));
        assert!(replaces_outside(root, "replace a => /opt/a\n"));
        assert!(!replaces_outside(root, "replace a => ./x/../y\n"));
    }

    #[test]
    fn plans_one_build_with_an_executable_per_main_package() {
        if go_executable().is_err() {
            eprintln!("skipping: go is not on PATH");
            return;
        }
        let dir = TempDir::new("mains");
        let root = &dir.0;
        write(&root.join("go.mod"), "module example.com/tool\n\ngo 1.21\n");
        write(&root.join("main.go"), "package main\n\nfunc main() {}\n");
        write(
            &root.join("cmd/helper/main.go"),
            "package main\n\nfunc main() {}\n",
        );
        write(
            &root.join("lib/lib.go"),
            "package lib\n\nfunc X() int { return 1 }\n",
        );
        assert!(GoAdapter.detect(root).expect("detect").supported);

        let mut options = AdapterOptions::new();
        options.insert("profile", "release").expect("option");
        let plan = GoAdapter.generate(root, &options).expect("generate");
        let action = &plan.actions[0].action;
        let suffix = std::env::consts::EXE_SUFFIX;
        let outputs: Vec<_> = action.outputs.iter().map(|o| o.path.clone()).collect();
        assert_eq!(
            outputs,
            vec![
                Path::new(OUTPUT_DIR).join(format!("helper{suffix}")),
                Path::new(OUTPUT_DIR).join(format!("tool{suffix}")),
            ]
        );
        assert!(action.argv.iter().any(|arg| arg == "-trimpath"));
        assert_eq!(action.cache_policy, CachePolicy::Local);
        let names: Vec<_> = action.environment.iter().map(|(n, _)| n.as_str()).collect();
        for required in ["GOENV", "GOCACHE", "GOMODCACHE", "TMP"] {
            assert!(names.contains(&required), "{names:?}");
        }
        assert!(
            action
                .inputs
                .iter()
                .all(|input| !input.path.starts_with(".ant")),
            "Ant's own directory is never an input"
        );

        let dev = GoAdapter
            .generate(root, &AdapterOptions::new())
            .expect("generate");
        assert!(
            !dev.actions[0]
                .action
                .argv
                .iter()
                .any(|arg| arg == "-trimpath")
        );
    }
}
