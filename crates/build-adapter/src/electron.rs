//! The Electron desktop adapter (roadmap P6): builds an Electron app's
//! desktop installers via `electron-builder`.
//!
//! An Electron project is an npm project whose `package.json` declares
//! `electron` in `dependencies` or `devDependencies`. The adapter generates
//! one action that runs `npx electron-builder`, which bundles the app and
//! produces platform installers in `dist/`.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Electron builds pull npm dependencies (into `node_modules/`) from the
//!   npm registry; those sources live outside the workspace and are undeclared
//!   inputs, so generated actions are `cache_policy: never` (D-003, D-010).
//! - The output directory `dist/` contains platform-specific installers
//!   (`.exe`/`.msi` on Windows, `.AppImage`/`.deb` on Linux, `.dmg` on macOS).
//!   The filenames include the app name and version, so `dist/` is declared as
//!   a `Tree` output (like Tauri's `bundle/` and Maven's `target/`).
//! - `npx` is `npx.cmd` (a batch shim) on Windows and a script elsewhere, so
//!   the action runs `cmd /C npx electron-builder` on Windows and
//!   `npx electron-builder` elsewhere (same pattern as the npm adapter).
//! - Toolchain identity is captured from the real `node`: the version from
//!   `node --version` and the SHA-256 digest of the resolved `node`
//!   executable. Same capture as the npm and Tauri adapters.
//! - An Electron project also has a `package.json` at its root, so the npm
//!   adapter also matches it. Ant reports the ambiguity (`npm`, `electron`)
//!   and the user picks `--adapter electron` (or sets it in `ant.toml`). This
//!   is the same behavior as a project with both `Cargo.toml` and
//!   `package.json`.
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

pub struct ElectronAdapter;

impl BuildAdapter for ElectronAdapter {
    /// An Electron app is also an npm project; Electron wins (D-036).
    fn supersedes(&self) -> &'static [&'static str] {
        &["npm"]
    }

    fn name(&self) -> &'static str {
        "electron"
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        let package_json = project_root.join("package.json");
        if !package_json.is_file() {
            return Ok(Detection::unsupported(format!(
                "no package.json at {}",
                project_root.display()
            )));
        }
        if has_electron(&package_json)? {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "package.json at {} does not declare `electron` in dependencies or devDependencies",
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
        let package_json = root.join("package.json");
        if !package_json.is_file() {
            return Err(AdapterError::Project(format!(
                "no package.json at {}",
                root.display()
            )));
        }
        // Require electron in package.json: detect's check, repeated here so
        // a `--adapter electron` invocation on a plain npm project fails at
        // generation with the same honest reason.
        if !has_electron(&package_json)? {
            return Err(AdapterError::Project(format!(
                "package.json at {} does not declare `electron` in dependencies or devDependencies",
                root.display()
            )));
        }

        let inputs = declare_inputs(&root);
        let outputs = vec![Output {
            path: PathBuf::from("dist"),
            kind: OutputKind::Tree,
        }];
        let action = build_action("electron-build".to_owned(), inputs, outputs);
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
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

/// Read `package.json` and return true when `electron` appears in
/// `dependencies` or `devDependencies`.
fn has_electron(package_json: &Path) -> Result<bool, AdapterError> {
    let text = std::fs::read_to_string(package_json).map_err(|error| {
        AdapterError::Project(format!("cannot read {}: {error}", package_json.display()))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        AdapterError::Project(format!("cannot parse {}: {error}", package_json.display()))
    })?;
    Ok(value
        .get("dependencies")
        .and_then(|d| d.get("electron"))
        .is_some()
        || value
            .get("devDependencies")
            .and_then(|d| d.get("electron"))
            .is_some())
}

/// Declare `package.json`, `src/` (frontend source, tree), and
/// `electron-builder.yml`/`electron-builder.json` (builder config, if
/// present). npm dependencies are outside the workspace and intentionally
/// not declared.
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    if root.join("package.json").is_file() {
        inputs.insert(PathBuf::from("package.json"), InputKind::File);
    }
    if root.join("src").is_dir() {
        inputs.insert(PathBuf::from("src"), InputKind::Tree);
    }
    for name in &["electron-builder.yml", "electron-builder.json"] {
        if root.join(name).is_file() {
            inputs.insert(PathBuf::from(name), InputKind::File);
        }
    }
    inputs
}

/// `npx electron-builder`, wrapped in `cmd /C` on Windows because `npx` is
/// `npx.cmd` (a batch shim, like the npm adapter).
fn electron_build_argv() -> Vec<String> {
    let argv: &[&str] = if cfg!(windows) {
        &["cmd", "/C", "npx", "electron-builder"]
    } else {
        &["npx", "electron-builder"]
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
        argv: electron_build_argv(),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs,
        environment: staging_environment(),
        // npm dependencies are pulled from the npm registry (undeclared
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
/// npm and Tauri adapters.
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
                "ant-electron-adapter-{name}-{}-{unique}",
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

    fn electron_package_json() -> &'static str {
        "{\n  \"name\": \"myapp\",\n  \"version\": \"1.0.0\",\n  \"main\": \"main.js\",\n  \"devDependencies\": {\n    \"electron\": \"^31.0.0\",\n    \"electron-builder\": \"^24.0.0\"\n  }\n}"
    }

    fn plain_npm_package_json() -> &'static str {
        "{\n  \"name\": \"myapp\",\n  \"version\": \"1.0.0\",\n  \"devDependencies\": {\n    \"typescript\": \"^5.0.0\"\n  }\n}"
    }

    #[test]
    fn detect_recognizes_electron_in_devdeps() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("devdep");
        write(&dir.path().join("package.json"), electron_package_json());
        assert!(adapter.detect(dir.path()).expect("detect").supported);
    }

    #[test]
    fn detect_recognizes_electron_in_deps() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("dep");
        write(
            &dir.path().join("package.json"),
            "{\n  \"name\": \"myapp\",\n  \"dependencies\": {\n    \"electron\": \"^31.0.0\"\n  }\n}\n",
        );
        assert!(adapter.detect(dir.path()).expect("detect").supported);
    }

    #[test]
    fn detect_rejects_plain_npm() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("plain");
        write(&dir.path().join("package.json"), plain_npm_package_json());
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("does not declare `electron`"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn detect_rejects_no_package_json() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("none");
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(!detection.supported);
        assert!(detection.reason.contains("no package.json"));
    }

    #[test]
    fn unknown_option_rejected() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("unknown");
        write(&dir.path().join("package.json"), electron_package_json());
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
    fn generate_rejects_plain_npm_project() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("not-electron");
        write(&dir.path().join("package.json"), plain_npm_package_json());
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(
            error.to_string().contains("does not declare `electron`"),
            "{}",
            error
        );
    }

    #[test]
    fn plan_has_exactly_one_action() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("one");
        write(&dir.path().join("package.json"), electron_package_json());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action.id, "electron-build");
        assert!(plan.actions[0].dependencies.is_empty());
    }

    #[test]
    fn dist_declared_as_tree_output() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("output");
        write(&dir.path().join("package.json"), electron_package_json());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.outputs.len(), 1);
        assert_eq!(action.outputs[0].path, PathBuf::from("dist"));
        assert_eq!(action.outputs[0].kind, OutputKind::Tree);
    }

    #[test]
    fn package_json_and_src_declared_as_inputs() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("inputs");
        write(&dir.path().join("package.json"), electron_package_json());
        write(&dir.path().join("src").join("main.js"), "console.log(1)");
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
        assert!(
            action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("src"))
        );
    }

    #[test]
    fn electron_builder_config_declared_when_present() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("config");
        write(&dir.path().join("package.json"), electron_package_json());
        write(
            &dir.path().join("electron-builder.yml"),
            "appId: com.example.myapp\n",
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(
            action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("electron-builder.yml"))
        );
    }

    #[test]
    fn cache_policy_is_never() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("cache");
        write(&dir.path().join("package.json"), electron_package_json());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
    }

    #[cfg(windows)]
    #[test]
    fn windows_argv_uses_cmd_c() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("win");
        write(&dir.path().join("package.json"), electron_package_json());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "cmd");
        assert_eq!(action.argv[1], "/C");
        assert_eq!(action.argv[2], "npx");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_argv_runs_npx_directly() {
        let adapter = ElectronAdapter;
        let dir = TempDir::new("unix");
        write(&dir.path().join("package.json"), electron_package_json());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "npx");
    }
}
