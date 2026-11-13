//! The Cargo adapter: the first real build-system adapter (decision D-010).
//!
//! It inspects a Cargo project or workspace through `cargo metadata` and
//! generates one `cargo build --package <name>` action per workspace member
//! package, with deterministic dependency edges between member packages.
//!
//! Honest v1 boundaries:
//! - Inputs declared per member package: its manifest, its `src` tree, its
//!   `build.rs` when present, plus the workspace `Cargo.toml` (when distinct)
//!   and `Cargo.lock` when present. Registry dependency sources live outside
//!   the workspace and cannot be declared, so generated actions use
//!   `cache_policy: never` — under-declared inputs must never claim cache
//!   eligibility (D-003, D-010). Member-to-member invalidation comes from the
//!   DAG edges, not from key material.
//! - Outputs are declared only for `bin` and `lib` targets under the default
//!   `target/debug` layout; other target kinds declare no outputs. A custom
//!   `CARGO_TARGET_DIR` or build config makes the declared outputs wrong, and
//!   the executor will then fail the action honestly instead of guessing.
//! - Toolchain identity is captured from the real cargo: the resolved PATH
//!   executable's SHA-256 digest and `cargo --version`. The adapter refuses
//!   to generate when cargo cannot be resolved.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{canonical_root, executable_version, locate_executable, relative_to, AdapterError, BuildAdapter, Detection};

pub struct CargoAdapter;

impl BuildAdapter for CargoAdapter {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root.join("Cargo.toml").is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no Cargo.toml at {}",
                project_root.display()
            )))
        }
    }

    fn generate(&self, project_root: &Path) -> Result<PlanFile, AdapterError> {
        let root = canonical_root(project_root)?;
        // Prepare the staging directory generated actions use as TMP/TEMP
        // (see the crate-level contract): toolchain scratch writes land
        // inside the workspace, which is what makes these plans runnable
        // under `ant run --sandbox`. Idempotent; the one allowed side effect.
        let staging = root.join(".ant").join("tmp");
        std::fs::create_dir_all(&staging).map_err(AdapterError::from)?;
        let staging_value = PathBuf::from(".ant").join("tmp").to_string_lossy().into_owned();
        let cargo = locate_executable(&format!("cargo{}", std::env::consts::EXE_SUFFIX))?;
        // Digest the real backing file: PATH entries may be symlinks (e.g.
        // rustup shims), and digests reject symlinks by design.
        let cargo_digest_target = cargo
            .canonicalize()
            .map_err(|error| AdapterError::Toolchain(format!(
                "cannot resolve cargo executable {}: {error}",
                cargo.display()
            )))?;
        let version = executable_version(&cargo, &root)?;
        let toolchain_digest = build_digest::digest_file(&cargo_digest_target)
            .map_err(|error| AdapterError::Toolchain(format!(
                "cannot digest cargo executable {}: {error}",
                cargo_digest_target.display()
            )))?
            .to_string();
        let metadata = cargo_metadata(&cargo, &root)?;
        let packages = metadata_value(&metadata, "packages")?
            .as_array()
            .ok_or_else(|| bad_metadata("packages is not an array"))?;

        let workspace_root = PathBuf::from(
            metadata["workspace_root"]
                .as_str()
                .ok_or_else(|| bad_metadata("workspace_root is missing"))?,
        );
        let member_ids: BTreeSet<&str> = metadata_value(&metadata, "workspace_members")?
            .as_array()
            .ok_or_else(|| bad_metadata("workspace_members is not an array"))?
            .iter()
            .map(|id| id.as_str().ok_or_else(|| bad_metadata("member id is not a string")))
            .collect::<Result<_, _>>()?;

        // Member packages sorted by name for a deterministic plan.
        let mut members: BTreeMap<&str, &serde_json::Value> = BTreeMap::new();
        for package in packages {
            let id = package["id"].as_str().ok_or_else(|| bad_metadata("package has no id"))?;
            if member_ids.contains(id) {
                let name = package["name"].as_str().ok_or_else(|| bad_metadata("package has no name"))?;
                members.insert(name, package);
            }
        }
        if members.is_empty() {
            return Err(bad_metadata("no workspace member packages found"));
        }

        let member_names: BTreeSet<&str> = members.keys().copied().collect();
        let lock_exists = root.join("Cargo.lock").is_file();
        let mut file = PlanFile::new();
        for (name, package) in &members {
            let manifest = PathBuf::from(
                package["manifest_path"]
                    .as_str()
                    .ok_or_else(|| bad_metadata("package manifest_path is missing"))?,
            );
            let package_dir = relative_to(&workspace_root, manifest.parent().expect("manifest has a parent"))?;
            let manifest_rel = join_rel(&package_dir, "Cargo.toml");

            let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
            declare_input(&mut inputs, manifest_rel.clone(), InputKind::File);
            let src = join_rel(&package_dir, "src");
            if root.join(&src).is_dir() {
                declare_input(&mut inputs, src, InputKind::Tree);
            }
            let build_script = join_rel(&package_dir, "build.rs");
            if root.join(&build_script).is_file() {
                declare_input(&mut inputs, build_script, InputKind::File);
            }
            // The workspace manifest drives profiles and inheritance; declare
            // it whenever it is not this package's own manifest.
            if manifest_rel != Path::new("Cargo.toml") {
                declare_input(&mut inputs, PathBuf::from("Cargo.toml"), InputKind::File);
            }
            if lock_exists {
                declare_input(&mut inputs, PathBuf::from("Cargo.lock"), InputKind::File);
            }

            let mut outputs: BTreeSet<PathBuf> = BTreeSet::new();
            let targets = package["targets"]
                .as_array()
                .ok_or_else(|| bad_metadata("package targets are missing"))?;
            for target in targets {
                let target_name =
                    target["name"].as_str().ok_or_else(|| bad_metadata("target has no name"))?;
                let kinds = target["kind"]
                    .as_array()
                    .ok_or_else(|| bad_metadata("target has no kind"))?;
                let is_bin = kinds.iter().any(|kind| kind.as_str() == Some("bin"));
                let is_lib = kinds.iter().any(|kind| kind.as_str() == Some("lib"));
                if is_bin {
                    outputs.insert(PathBuf::from("target")
                        .join("debug")
                        .join(format!("{target_name}{}", std::env::consts::EXE_SUFFIX)));
                } else if is_lib {
                    // Crate names in artifact file names use underscores.
                    let crate_name = target_name.replace('-', "_");
                    outputs.insert(PathBuf::from("target")
                        .join("debug")
                        .join(format!("lib{crate_name}.rlib")));
                }
                // Other target kinds (proc-macro, test, bench, example,
                // custom-build) declare no outputs in v1.
            }

            // Dependency edges: only edges to other member packages, sorted
            // and deduplicated. Dev- and build-dependencies are included
            // conservatively (rerunning dependents on any dependency change
            // is never wrong, only sometimes redundant).
            let mut dependencies: BTreeSet<String> = BTreeSet::new();
            if let Some(deps) = package["dependencies"].as_array() {
                for dep in deps {
                    if let Some(dep_name) = dep["name"].as_str()
                        && member_names.contains(dep_name)
                    {
                        dependencies.insert(dep_name.to_owned());
                    }
                }
            }

            // Toolchain scratch writes (the MSVC linker's response files,
            // rustc temp files) go to the platform temp directory, which is
            // outside the workspace; pointing TMP/TEMP (and TMPDIR for
            // Unix-style tools, unverified) at the workspace staging
            // directory keeps those writes inside the workspace, so generated
            // plans run under `ant run --sandbox`. Values are
            // workspace-relative, so the plan stays portable.
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
                id: (*name).to_owned(),
                argv: vec![
                    "cargo".to_owned(),
                    "build".to_owned(),
                    "--package".to_owned(),
                    (*name).to_owned(),
                ],
                working_directory: PathBuf::from("."),
                inputs: inputs
                    .into_iter()
                    .map(|(path, kind)| Input { path, kind })
                    .collect(),
                outputs: outputs
                    .into_iter()
                    .map(|path| Output { path, kind: OutputKind::File })
                    .collect(),
                environment,
                // Dependency source content is undeclared, so these actions
                // must never claim cache eligibility.
                cache_policy: CachePolicy::Never,
                toolchain: Some(ToolchainIdentity {
                    name: "cargo".to_owned(),
                    version: version.clone(),
                    executable_digest: toolchain_digest.clone(),
                }),
                timeout_ms: None,
                resources: None,
            };
            file.actions.push(PlanAction {
                action,
                dependencies: dependencies.into_iter().collect(),
            });
        }
        // Validate the full plan through the canonical graph conversion; an
        // adapter must never emit an invalid plan.
        file.to_graph()?;
        Ok(file)
    }
}

fn bad_metadata(detail: &str) -> AdapterError {
    AdapterError::Project(format!("cargo metadata unusable: {detail}"))
}

fn metadata_value<'a>(
    metadata: &'a serde_json::Value,
    key: &str,
) -> Result<&'a serde_json::Value, AdapterError> {
    metadata
        .get(key)
        .ok_or_else(|| bad_metadata(&format!("{key} is missing")))
}

/// Workspace-relative path join that stays relative.
fn join_rel(base: &Path, leaf: &str) -> PathBuf {
    base.join(leaf)
}

/// Insert an input declaration without overwriting an existing kind for the
/// same path.
fn declare_input(inputs: &mut BTreeMap<PathBuf, InputKind>, path: PathBuf, kind: InputKind) {
    inputs.entry(path).or_insert(kind);
}

fn cargo_metadata(cargo: &Path, root: &Path) -> Result<serde_json::Value, AdapterError> {
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|error| AdapterError::Toolchain(format!("cannot run cargo: {error}")))?;
    if !output.status.success() {
        return Err(AdapterError::Toolchain(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| bad_metadata(&format!("cannot parse cargo metadata: {error}")))
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
                "ant-cargo-adapter-{name}-{}-{unique}",
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

    /// A two-member workspace: a lib with a bin and a dependent lib.
    fn workspace() -> TempDir {
        let dir = TempDir::new("ws");
        write(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"2\"\n",
        );
        write(
            &dir.path().join("core/Cargo.toml"),
            "[package]\nname = \"core-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"core_lib\"\n",
        );
        write(&dir.path().join("core/src/lib.rs"), "pub fn core() {}\n");
        write(
            &dir.path().join("app/Cargo.toml"),
            "[package]\nname = \"app-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore-lib = { path = \"../core\" }\n",
        );
        write(&dir.path().join("app/src/main.rs"), "fn main() {}\n");
        dir
    }

    #[test]
    fn detect_recognizes_manifests_and_rejects_other_projects() {
        let adapter = CargoAdapter;
        let dir = workspace();
        assert!(adapter.detect(dir.path()).expect("detect").supported);
        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(detection.reason.contains("no Cargo.toml"));
    }

    #[test]
    fn generate_produces_deterministic_validated_member_actions() {
        let adapter = CargoAdapter;
        let dir = workspace();
        let first = adapter.generate(dir.path()).expect("generate");
        let second = adapter.generate(dir.path()).expect("generate again");
        assert_eq!(first, second, "generation must be deterministic");

        let ids: Vec<&str> = first.actions.iter().map(|entry| entry.action.id.as_str()).collect();
        assert_eq!(ids, vec!["app-bin", "core-lib"], "members sorted by package name");

        let app = &first.actions[0].action;
        // Sorted, deduplicated member dependency edges.
        assert_eq!(first.actions[0].dependencies, vec!["core-lib".to_owned()]);
        // The bin output maps into the default target layout.
        assert_eq!(
            app.outputs.iter().map(|output| output.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from(format!(
                "target/debug/app-bin{}",
                std::env::consts::EXE_SUFFIX
            ))]
        );
        // Member inputs: manifest, src tree, and the workspace manifest.
        let input_paths: Vec<&Path> = app.inputs.iter().map(|input| input.path.as_path()).collect();
        assert!(input_paths.contains(&Path::new("app/Cargo.toml")));
        assert!(input_paths.contains(&Path::new("app/src")));
        assert!(input_paths.contains(&Path::new("Cargo.toml")), "workspace manifest declared");
        // Tree-kind inputs are marked as trees.
        let src_input = app.inputs.iter().find(|input| input.path == Path::new("app/src")).expect("src input");
        assert_eq!(src_input.kind, InputKind::Tree);
        // Undeclared dependency sources force policy never.
        assert_eq!(app.cache_policy, CachePolicy::Never);
        // Scratch writes are redirected into the workspace staging dir so the
        // plan is runnable under `ant run --sandbox`.
        let staging = PathBuf::from(".ant").join("tmp").to_string_lossy().into_owned();
        assert_eq!(
            app.environment,
            vec![
                ("TEMP".to_owned(), staging.clone()),
                ("TMP".to_owned(), staging.clone()),
            ]
        );
        // The staging directory was prepared by generation.
        assert!(dir.path().join(".ant").join("tmp").is_dir());
        // Truthful toolchain identity from the real cargo.
        let toolchain = app.toolchain.as_ref().expect("toolchain identity");
        assert!(toolchain.version.starts_with("cargo "), "was {:?}", toolchain.version);
        assert!(toolchain.executable_digest.starts_with("sha256:"));

        let core = &first.actions[1].action;
        assert_eq!(
            core.outputs.iter().map(|output| output.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("target/debug/libcore_lib.rlib")]
        );
        assert!(first.actions[1].dependencies.is_empty());
    }

    #[test]
    fn generate_declares_build_script_input() {
        let adapter = CargoAdapter;
        let dir = TempDir::new("ws-build-rs");
        write(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"gen\"]\nresolver = \"2\"\n",
        );
        write(
            &dir.path().join("gen/Cargo.toml"),
            "[package]\nname = \"gen-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&dir.path().join("gen/src/lib.rs"), "pub fn x() {}\n");
        write(&dir.path().join("gen/build.rs"), "fn main() {}\n");

        let plan = adapter.generate(dir.path()).expect("generate");
        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0].action;
        let paths: Vec<&Path> = action.inputs.iter().map(|input| input.path.as_path()).collect();
        // The build script can change the build result, so it must be declared.
        let build_script = paths
            .iter()
            .copied()
            .find(|path| path.file_name() == Some(std::ffi::OsStr::new("build.rs")))
            .expect("build.rs declared as input");
        assert_eq!(build_script, &Path::new("gen").join("build.rs"));
        let script_input = action
            .inputs
            .iter()
            .find(|input| input.path == Path::new("gen").join("build.rs"))
            .expect("build.rs entry");
        assert_eq!(script_input.kind, InputKind::File);
        // The generated package has no bin or lib output name mismatch: it is
        // a lib, so the rlib output is declared.
        assert_eq!(
            action.outputs.iter().map(|output| output.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("target/debug/libgen_lib.rlib")]
        );
    }

    #[test]
    fn generate_handles_virtual_workspace() {
        let adapter = CargoAdapter;
        let dir = TempDir::new("ws-virtual");
        // A virtual workspace: the root manifest declares members but is not
        // itself a package.
        write(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"svc\"]\nresolver = \"2\"\n",
        );
        write(
            &dir.path().join("svc/Cargo.toml"),
            "[package]\nname = \"svc-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&dir.path().join("svc/src/main.rs"), "fn main() {}\n");

        let plan = adapter.generate(dir.path()).expect("generate");
        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0].action;
        assert_eq!(action.id, "svc-bin");
        // The virtual root's manifest is not the package's own manifest, so
        // it must still be declared as an input (it drives inheritance).
        let paths: Vec<&Path> = action.inputs.iter().map(|input| input.path.as_path()).collect();
        assert!(paths.contains(&Path::new("svc/Cargo.toml")));
        assert!(paths.contains(&Path::new("Cargo.toml")), "workspace manifest declared");
        assert!(paths.contains(&Path::new("svc/src")));
        assert!(plan.actions[0].dependencies.is_empty());
    }

    #[test]
    fn generate_rejects_projects_without_a_workspace() {
        let adapter = CargoAdapter;
        let dir = TempDir::new("not-a-project");
        write(&dir.path().join("readme.txt"), "not rust\n");
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(!detection.supported);
        // Generation without detection is still honest: cargo metadata fails.
        assert!(adapter.generate(dir.path()).is_err());
    }
}
