//! The npm adapter: a second build-system adapter (decision D-010).
//!
//! It reads `package.json` and generates a single `npm run build` action for
//! the project. This is deliberately the simplest honest npm adapter: it
//! proves the adapter contract generalizes beyond cargo without inventing a
//! second, weaker action model.
//!
//! Honest v1 boundaries:
//! - The action runs the project's `scripts.build`. If `package.json` has no
//!   `build` script, generation fails honestly rather than guessing a command.
//! - Inputs declared: `package.json`, `package-lock.json` when present, the
//!   `src` tree when present, and `tsconfig.json` when present (a common
//!   build-driver config). The installed `node_modules` tree is a build input
//!   whose content is derived (it is the product of `npm install` against the
//!   registry), not source; declaring the whole tree is out of scope for v1,
//!   so generated actions use `cache_policy: never` — under-declared inputs
//!   must never claim cache eligibility (D-003, D-010). This mirrors the
//!   cargo adapter's stance on registry dependency sources.
//! - Outputs are **not declared** in v1. npm has no standard output layout
//!   (`dist/` is conventional but not universal, and bundler config decides
//!   the real location); guessing would be dishonest. The executor's output
//!   coverage is therefore trivially satisfied — the build still runs and
//!   produces real artifacts, they are just not verified by the plan. This
//!   follows the honesty rule that unknown artifact layouts are omitted
//!   rather than guessed.
//! - Toolchain identity is captured from the real `node` runtime: the resolved
//!   PATH executable's SHA-256 digest and `node --version`. The build is
//!   driven by `npm`, but `npm` is a script (`npm.cmd` on Windows, a JS file
//!   elsewhere) not a standalone executable; `node` is the truthful runtime
//!   identity. Generation fails when `node` cannot be resolved.
//! - Windows invocation: `npm` is `npm.cmd` (a batch shim) on Windows, and
//!   `CreateProcess` cannot execute batch scripts directly, so the generated
//!   argv is `cmd /C npm run build` on Windows and `npm run build` elsewhere.
//!
//! npm workspaces (monorepos) and per-script selection are future work; v1
//! generates one action for the root package.

use build_action::{Action, CachePolicy, Input, InputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{canonical_root, executable_version, locate_executable, AdapterError, BuildAdapter, Detection};

pub struct NpmAdapter;

impl BuildAdapter for NpmAdapter {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root.join("package.json").is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no package.json at {}",
                project_root.display()
            )))
        }
    }

    fn generate(&self, project_root: &Path) -> Result<PlanFile, AdapterError> {
        let root = canonical_root(project_root)?;
        // Prepare the staging directory generated actions use as TMP/TEMP
        // (see the crate-level contract); idempotent, the one allowed side
        // effect.
        let staging = root.join(".ant").join("tmp");
        std::fs::create_dir_all(&staging).map_err(AdapterError::from)?;
        let staging_value = PathBuf::from(".ant").join("tmp").to_string_lossy().into_owned();

        let node = locate_executable(&format!("node{}", std::env::consts::EXE_SUFFIX))?;
        let node_digest_target = node
            .canonicalize()
            .map_err(|error| AdapterError::Toolchain(format!(
                "cannot resolve node executable {}: {error}",
                node.display()
            )))?;
        let version = executable_version(&node, &root)?;
        let toolchain_digest = build_digest::digest_file(&node_digest_target)
            .map_err(|error| AdapterError::Toolchain(format!(
                "cannot digest node executable {}: {error}",
                node_digest_target.display()
            )))?
            .to_string();

        let package_json_path = root.join("package.json");
        let package_json_text = std::fs::read_to_string(&package_json_path).map_err(|error| {
            AdapterError::Project(format!(
                "cannot read package.json at {}: {error}",
                package_json_path.display()
            ))
        })?;
        let package: serde_json::Value = serde_json::from_str(&package_json_text).map_err(
            |error| AdapterError::Project(format!("package.json is not valid JSON: {error}")),
        )?;

        // Require a `build` script: guessing a command would be dishonest.
        let build_script = package
            .get("scripts")
            .and_then(|scripts| scripts.get("build"))
            .and_then(|build| build.as_str())
            .ok_or_else(|| AdapterError::Project(
                "no `build` script in package.json; the npm adapter generates a `npm run build` action and requires a `scripts.build` entry"
                    .to_owned(),
            ))?;
        if build_script.trim().is_empty() {
            return Err(AdapterError::Project(
                "the `build` script in package.json is empty; the npm adapter cannot generate a meaningful action"
                    .to_owned(),
            ));
        }

        // argv: `npm` is `npm.cmd` (a batch shim) on Windows and cannot be
        // launched directly by CreateProcess; `cmd /C` resolves it. Elsewhere
        // `npm` is on PATH as an executable script.
        let argv: Vec<String> = if cfg!(windows) {
            vec!["cmd".to_owned(), "/C".to_owned(), "npm".to_owned(), "run".to_owned(), "build".to_owned()]
        } else {
            vec!["npm".to_owned(), "run".to_owned(), "build".to_owned()]
        };

        // Declared inputs: the project files that can change the build. The
        // installed node_modules tree is intentionally NOT declared (derived,
        // not source) — see the module docs and cache_policy below.
        let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
        inputs.insert(PathBuf::from("package.json"), InputKind::File);
        if root.join("package-lock.json").is_file() {
            inputs.insert(PathBuf::from("package-lock.json"), InputKind::File);
        }
        if root.join("src").is_dir() {
            inputs.insert(PathBuf::from("src"), InputKind::Tree);
        }
        if root.join("tsconfig.json").is_file() {
            inputs.insert(PathBuf::from("tsconfig.json"), InputKind::File);
        }

        // Toolchain scratch writes are redirected into the workspace staging
        // dir so the plan is runnable under `ant run --sandbox` (T3.3 pattern).
        let mut environment = vec![
            ("TMP".to_owned(), staging_value.clone()),
            ("TEMP".to_owned(), staging_value.clone()),
        ];
        if cfg!(not(windows)) {
            environment.push(("TMPDIR".to_owned(), staging_value.clone()));
        }
        environment.sort();

        let action = Action {
            schema_version: 1,
            id: package
                .get("name")
                .and_then(|name| name.as_str())
                .map(|name| name.to_owned())
                .unwrap_or_else(|| "build".to_owned()),
            argv,
            working_directory: PathBuf::from("."),
            inputs: inputs
                .into_iter()
                .map(|(path, kind)| Input { path, kind })
                .collect(),
            // Outputs are intentionally omitted: npm has no standard output
            // layout, and guessing one would be dishonest (D-010). The build
            // still runs and produces real artifacts; they are just not
            // verified by the plan.
            outputs: Vec::new(),
            environment,
            // node_modules content is an undeclared input, so these actions
            // must never claim cache eligibility.
            cache_policy: CachePolicy::Never,
            toolchain: Some(ToolchainIdentity {
                name: "node".to_owned(),
                version,
                executable_digest: toolchain_digest,
            }),
            timeout_ms: None,
            resources: None,
        };
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the full plan through the canonical graph conversion; an
        // adapter must never emit an invalid plan.
        file.to_graph()?;
        Ok(file)
    }
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
                "ant-npm-adapter-{name}-{}-{unique}",
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

    /// A minimal package.json with a build script.
    fn package_json(name: &str) -> String {
        format!(
            "{{\"name\":\"{name}\",\"version\":\"0.1.0\",\"scripts\":{{\"build\":\"node build.js\"}}}}"
        )
    }

    #[test]
    fn detect_recognizes_package_json_and_rejects_other_projects() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("detect");
        write(&dir.path().join("package.json"), &package_json("detect-app"));
        assert!(adapter.detect(dir.path()).expect("detect").supported);
        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(detection.reason.contains("no package.json"));
    }

    #[test]
    fn generate_produces_one_validated_build_action() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("gen");
        write(&dir.path().join("package.json"), &package_json("gen-app"));
        // A src tree and a lock file are declared when present.
        write(&dir.path().join("src/index.js"), "module.exports = 1;\n");
        write(&dir.path().join("package-lock.json"), "{\"lockfileVersion\":3}");
        // tsconfig is a common build driver; declare it when present.
        write(&dir.path().join("tsconfig.json"), "{\"compilerOptions\":{}}");

        let first = adapter.generate(dir.path()).expect("generate");
        let second = adapter.generate(dir.path()).expect("generate again");
        assert_eq!(first, second, "generation must be deterministic");

        assert_eq!(first.actions.len(), 1);
        let action = &first.actions[0].action;
        assert_eq!(action.id, "gen-app");
        assert!(first.actions[0].dependencies.is_empty());

        // argv runs the build script via npm (cmd /C on Windows).
        if cfg!(windows) {
            assert_eq!(action.argv, vec!["cmd", "/C", "npm", "run", "build"]);
        } else {
            assert_eq!(action.argv, vec!["npm", "run", "build"]);
        }
        assert_eq!(action.working_directory, PathBuf::from("."));

        // Declared inputs: package.json, package-lock.json, src tree, tsconfig.
        let paths: Vec<&Path> = action.inputs.iter().map(|input| input.path.as_path()).collect();
        assert!(paths.contains(&Path::new("package.json")));
        assert!(paths.contains(&Path::new("package-lock.json")));
        assert!(paths.contains(&Path::new("tsconfig.json")));
        assert!(paths.contains(&Path::new("src")));
        let src_input = action
            .inputs
            .iter()
            .find(|input| input.path == Path::new("src"))
            .expect("src input");
        assert_eq!(src_input.kind, InputKind::Tree);
        let manifest_input = action
            .inputs
            .iter()
            .find(|input| input.path == Path::new("package.json"))
            .expect("package.json input");
        assert_eq!(manifest_input.kind, InputKind::File);

        // Outputs are intentionally omitted (npm has no standard layout).
        assert!(action.outputs.is_empty(), "npm adapter must not guess outputs");

        // node_modules is an undeclared input, so policy is never.
        assert_eq!(action.cache_policy, CachePolicy::Never);

        // Scratch writes are redirected into the workspace staging dir.
        let staging = PathBuf::from(".ant").join("tmp").to_string_lossy().into_owned();
        assert_eq!(
            action.environment,
            vec![
                ("TEMP".to_owned(), staging.clone()),
                ("TMP".to_owned(), staging.clone()),
            ]
        );
        assert!(dir.path().join(".ant").join("tmp").is_dir());

        // Truthful node toolchain identity.
        let toolchain = action.toolchain.as_ref().expect("toolchain identity");
        assert_eq!(toolchain.name, "node");
        assert!(
            toolchain.version.starts_with("v"),
            "node --version starts with v: was {:?}",
            toolchain.version
        );
        assert!(toolchain.executable_digest.starts_with("sha256:"));
    }

    #[test]
    fn generate_minimal_inputs_without_src_or_lock() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("minimal");
        // package.json with a build script but no src, no lock, no tsconfig.
        write(&dir.path().join("package.json"), &package_json("minimal-app"));
        let plan = adapter.generate(dir.path()).expect("generate");
        let action = &plan.actions[0].action;
        let paths: Vec<&Path> = action.inputs.iter().map(|input| input.path.as_path()).collect();
        // Only package.json is declared; nothing else is invented.
        assert_eq!(paths, vec![Path::new("package.json")]);
    }

    #[test]
    fn generate_rejects_projects_without_a_build_script() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("no-build");
        write(&dir.path().join("package.json"), "{\"name\":\"no-build\",\"version\":\"0.1.0\",\"scripts\":{\"test\":\"node test.js\"}}");
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(detection.supported, "package.json present means supported");
        let error = adapter.generate(dir.path()).expect_err("must fail without a build script");
        assert!(
            error.to_string().contains("no `build` script"),
            "error was {error}"
        );
    }

    #[test]
    fn generate_rejects_invalid_package_json() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("bad-json");
        write(&dir.path().join("package.json"), "{ not valid json");
        let error = adapter.generate(dir.path()).expect_err("must fail on invalid JSON");
        assert!(
            error.to_string().contains("package.json is not valid JSON"),
            "error was {error}"
        );
    }

    #[test]
    fn generate_rejects_empty_build_script() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("empty-build");
        write(&dir.path().join("package.json"), "{\"name\":\"empty\",\"version\":\"0.1.0\",\"scripts\":{\"build\":\"   \"}}");
        let error = adapter.generate(dir.path()).expect_err("must fail on empty build script");
        assert!(
            error.to_string().contains("`build` script in package.json is empty"),
            "error was {error}"
        );
    }
}
