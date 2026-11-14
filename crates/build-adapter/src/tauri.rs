//! The Tauri desktop adapter (roadmap P6): builds a Tauri app's desktop
//! installers.
//!
//! A Tauri project is a directory with `src-tauri/tauri.conf.json` (the Tauri
//! manifest) at its root. The adapter generates one action that runs
//! `npm run tauri build`, which compiles the Rust backend (via cargo under the
//! hood), bundles the frontend, and produces platform installers in
//! `src-tauri/target/release/bundle/`.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Tauri builds pull npm dependencies (into `node_modules/`) and Rust crate
//!   dependencies (into `~/.cargo/registry` and `~/.cargo/git`); those sources
//!   live outside the workspace and are undeclared inputs, so generated actions
//!   are `cache_policy: never` (D-003, D-010) — the same stance the cargo, npm,
//!   Go, Python, Maven, and Docker adapters take on registry dependency
//!   sources.
//! - The output directory `src-tauri/target/release/bundle/` contains
//!   platform-specific installers (`.msi`/`.exe` on Windows, `.dmg`/`.AppImage`
//!   on Linux, `.dmg`/`.app` on macOS). The filenames include the app name and
//!   version, so they are not fixed; `bundle/` is declared as a `Tree` output
//!   (like Maven's `target/`).
//! - `npm` is `npm.cmd` (a batch shim) on Windows and a script elsewhere, so
//!   the action runs `cmd /C npm run tauri build` on Windows and
//!   `npm run tauri build` elsewhere (same pattern as the npm adapter).
//! - Toolchain identity is captured from the real `node`: the version from
//!   `node --version` and the SHA-256 digest of the resolved `node`
//!   executable. Node is the top-level executable that launches the build;
//!   the Rust toolchain is an implicit dependency of the Tauri CLI, not
//!   captured separately (same trust model as npm capturing node but not
//!   every transitive tool).
//! - A Tauri project also has a `package.json` at its root (the frontend), so
//!   the npm adapter also matches it. Ant reports the ambiguity
//!   (`npm`, `tauri`) and the user picks `--adapter tauri` (or sets it in
//!   `ant.toml`). This is the same behavior as a project with both
//!   `Cargo.toml` and `package.json`.
//! - Workspace-relative scratch: `TMP`/`TEMP` (plus `TMPDIR` off Windows)
//!   point at `.ant/tmp`, the staging pattern shared with the other adapters.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, canonical_root, check_option_keys,
    executable_version, locate_executable,
};

pub struct TauriAdapter;

impl BuildAdapter for TauriAdapter {
    /// A Tauri app is also an npm project and contains a cargo crate; Tauri
    /// builds both, so it wins (D-036).
    fn supersedes(&self) -> &'static [&'static str] {
        &["npm", "cargo"]
    }

    fn name(&self) -> &'static str {
        "tauri"
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root
            .join("src-tauri")
            .join("tauri.conf.json")
            .is_file()
        {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no src-tauri/tauri.conf.json at {}",
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
        let root = canonical_root(project_root)?;
        // Require the Tauri manifest: detect's marker check, repeated here so
        // a `--adapter tauri` invocation on the wrong project fails at
        // generation with the same honest reason rather than a misleading
        // toolchain error.
        let tauri_conf = root.join("src-tauri").join("tauri.conf.json");
        if !tauri_conf.is_file() {
            return Err(AdapterError::Project(format!(
                "no src-tauri/tauri.conf.json at {}",
                root.display()
            )));
        }

        let inputs = declare_inputs(&root);
        let outputs = vec![Output {
            path: PathBuf::from("src-tauri")
                .join("target")
                .join("release")
                .join("bundle"),
            kind: OutputKind::Tree,
        }];
        let action = build_action("tauri-build".to_owned(), inputs, outputs);
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = node_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Declare the workspace inputs Ant can honestly cover: `package.json`
/// (frontend manifest), `src-tauri/tauri.conf.json` (Tauri config),
/// `src-tauri/Cargo.toml` (Rust backend manifest), `src/` (frontend source),
/// and `src-tauri/src/` (Rust backend source), each when present. npm and
/// cargo dependency registries are outside the workspace and intentionally
/// not declared (see the module docs).
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    if root.join("package.json").is_file() {
        inputs.insert(PathBuf::from("package.json"), InputKind::File);
    }
    inputs.insert(
        PathBuf::from("src-tauri").join("tauri.conf.json"),
        InputKind::File,
    );
    let cargo_toml = PathBuf::from("src-tauri").join("Cargo.toml");
    if root.join(&cargo_toml).is_file() {
        inputs.insert(cargo_toml, InputKind::File);
    }
    if root.join("src").is_dir() {
        inputs.insert(PathBuf::from("src"), InputKind::Tree);
    }
    let tauri_src = PathBuf::from("src-tauri").join("src");
    if root.join(&tauri_src).is_dir() {
        inputs.insert(tauri_src, InputKind::Tree);
    }
    inputs
}

/// `npm run tauri build`, wrapped in `cmd /C` on Windows because `npm` is
/// `npm.cmd` (a batch shim, like the npm adapter).
fn tauri_build_argv() -> Vec<String> {
    let argv: &[&str] = if cfg!(windows) {
        &["cmd", "/C", "npm", "run", "tauri", "build"]
    } else {
        &["npm", "run", "tauri", "build"]
    };
    argv.iter().map(|arg| (*arg).to_owned()).collect()
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

fn build_action(id: String, inputs: BTreeMap<PathBuf, InputKind>, outputs: Vec<Output>) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: tauri_build_argv(),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs,
        environment: staging_environment(),
        // npm and cargo dependencies are pulled from registries (undeclared
        // inputs outside the workspace), so these actions never claim shared
        // cache eligibility (D-003, D-010).
        cache_policy: CachePolicy::Never,
        toolchain: None,
        timeout_ms: None,
        resources: None,
        background_services: false,
    }
}

/// Truthful Node toolchain identity: the version from `node --version` and
/// the SHA-256 digest of the resolved `node` executable. Same capture as the
/// npm adapter; Node is the top-level executable that launches the Tauri
/// build.
fn node_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    let node = locate_executable(&format!("node{}", std::env::consts::EXE_SUFFIX))?;
    let node_digest_target = node.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve node executable {}: {error}",
            node.display()
        ))
    })?;
    let version = executable_version(&node, root)?;
    let executable_digest = crate::toolchain_digest(&node_digest_target)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest node executable {}: {error}",
                node_digest_target.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "node".to_owned(),
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
                "ant-tauri-adapter-{name}-{}-{unique}",
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

    fn tauri_conf() -> &'static str {
        "{\n  \"productName\": \"myapp\",\n  \"version\": \"1.0.0\",\n  \"identifier\": \"com.example.myapp\",\n  \"build\": {\n    \"beforeBuildCommand\": \"npm run build\",\n    \"frontendDist\": \"../dist\"\n  }\n}"
    }

    #[test]
    fn detect_recognizes_tauri_conf_and_rejects_others() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("detect");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        assert!(adapter.detect(dir.path()).expect("detect").supported);

        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no src-tauri/tauri.conf.json"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn unknown_option_rejected() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("unknown");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        let error = adapter
            .generate(dir.path(), &{
                let mut o = AdapterOptions::new();
                o.insert("debug", "true").expect("insert");
                o
            })
            .expect_err("unknown option must fail");
        assert!(
            error.to_string().contains("does not accept option 'debug'"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_tauri_project() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("not-tauri");
        write(&dir.path().join("README.md"), "not a tauri project");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(
            error.to_string().contains("no src-tauri/tauri.conf.json"),
            "{}",
            error
        );
    }

    #[test]
    fn plan_has_exactly_one_action() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("one");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action.id, "tauri-build");
        assert!(plan.actions[0].dependencies.is_empty());
    }

    #[test]
    fn bundle_declared_as_tree_output() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("output");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.outputs.len(), 1);
        assert_eq!(
            action.outputs[0].path,
            PathBuf::from("src-tauri/target/release/bundle")
        );
        assert_eq!(action.outputs[0].kind, OutputKind::Tree);
    }

    #[test]
    fn tauri_conf_and_cargo_toml_declared_as_inputs() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("inputs");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        write(
            &dir.path().join("src-tauri").join("Cargo.toml"),
            "[package]\nname = \"myapp\"\n",
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        let input_paths: Vec<&Path> = action.inputs.iter().map(|i| i.path.as_path()).collect();
        assert!(input_paths.contains(&Path::new("src-tauri/tauri.conf.json")));
        assert!(input_paths.contains(&Path::new("src-tauri/Cargo.toml")));
    }

    #[test]
    fn package_json_declared_when_present() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("pkg");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        write(&dir.path().join("package.json"), "{\"name\":\"myapp\"}");
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(
            action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("package.json"))
        );
    }

    #[test]
    fn src_dirs_declared_when_present() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("srcs");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        write(&dir.path().join("src").join("main.ts"), "console.log(1)");
        write(
            &dir.path().join("src-tauri").join("src").join("main.rs"),
            "fn main() {}",
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(
            action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("src"))
        );
        assert!(
            action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("src-tauri/src"))
        );
    }

    #[test]
    fn cache_policy_is_never() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("cache");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
    }

    #[cfg(windows)]
    #[test]
    fn windows_argv_uses_cmd_c() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("win");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "cmd");
        assert_eq!(action.argv[1], "/C");
        assert_eq!(action.argv[2], "npm");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_argv_runs_npm_directly() {
        let adapter = TauriAdapter;
        let dir = TempDir::new("unix");
        write(
            &dir.path().join("src-tauri").join("tauri.conf.json"),
            tauri_conf(),
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "npm");
    }
}
