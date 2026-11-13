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
//! - Outputs are declared only for `bin` and `lib` targets, under
//!   `<target-dir>/<profile-dir>`: the target directory is the one `cargo
//!   metadata` reports (`target_directory`, which already reflects
//!   `CARGO_TARGET_DIR` and `build.target-dir`), and the profile directory is
//!   `debug` for the default `dev` profile or `release` for `--option
//!   profile=release`. A target directory outside the project fails closed
//!   (outputs must be workspace-relative); a non-default one inside the
//!   project is pinned with a workspace-relative `CARGO_TARGET_DIR` on every
//!   action, because the executor clears the child environment and would
//!   otherwise drop an adapt-time variable. Other target kinds declare no
//!   outputs. A configured default `build.target` triple changes the layout
//!   (`<target-dir>/<triple>/<profile-dir>`) and is not reported by `cargo
//!   metadata`; the executor then fails the action on output coverage
//!   instead of guessing.
//! - Dependency sources that live inside the project are declared: any
//!   non-member package whose manifest is inside the workspace (vendored
//!   crates under `cargo vendor` source replacement, path dependencies that
//!   are not workspace members) becomes a tree input of every member that
//!   transitively depends on it, from the `cargo metadata` resolve graph. The
//!   project's `.cargo/config.toml` (or legacy `.cargo/config`) is declared
//!   too, because it can change the build (source replacement, flags,
//!   target dir). Configuration outside the project (`~/.cargo`) and
//!   registry sources outside the project stay undeclared, so policy stays
//!   `never` regardless.
//! - Toolchain identity is captured from the real cargo: the resolved PATH
//!   executable's SHA-256 digest and `cargo --version`. The adapter refuses
//!   to generate when cargo cannot be resolved.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    canonical_root, check_option_keys, executable_version, locate_executable, relative_to,
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec,
};

pub struct CargoAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "profile",
    values: "dev|release",
    description: "cargo build profile; release adds --release and maps outputs to <target-dir>/release",
}];

/// A supported cargo build profile: the extra argv and the profile's output
/// directory name under the target directory.
struct Profile {
    flags: &'static [&'static str],
    directory: &'static str,
}

fn profile(options: &AdapterOptions) -> Result<Profile, AdapterError> {
    match options.get("profile") {
        // The default dev profile keeps the pre-options argv exactly.
        None | Some("dev") => Ok(Profile { flags: &[], directory: "debug" }),
        Some("release") => Ok(Profile { flags: &["--release"], directory: "release" }),
        // Custom profiles write to `<target-dir>/<name>` but whether one is
        // defined cannot be checked through `cargo metadata`; refuse rather
        // than emit a plan that may name a nonexistent profile.
        Some(other) => Err(AdapterError::Option(format!(
            "cargo profile '{other}' is not supported (supported: dev, release)"
        ))),
    }
}

impl BuildAdapter for CargoAdapter {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
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

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        // Options are validated before any side effect.
        check_option_keys(self, options)?;
        let profile = profile(options)?;
        let root = canonical_root(project_root)?;
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
        // Every declared path is relative to the project root, so the project
        // root must be the workspace root; a member directory would make the
        // workspace manifest and sibling members unreachable.
        let canonical_workspace = workspace_root.canonicalize().map_err(|error| {
            bad_metadata(&format!("cannot resolve workspace_root {}: {error}", workspace_root.display()))
        })?;
        if canonical_workspace != root {
            return Err(AdapterError::Project(format!(
                "project {} is inside the cargo workspace rooted at {}; point --project at the workspace root",
                root.display(),
                workspace_root.display()
            )));
        }
        // Outputs live under the target directory cargo reports, which must
        // be inside the project: outputs are workspace-relative by contract.
        let target_directory = PathBuf::from(
            metadata["target_directory"]
                .as_str()
                .ok_or_else(|| bad_metadata("target_directory is missing"))?,
        );
        let target_rel = relative_to(&workspace_root, &target_directory).map_err(|_| {
            AdapterError::Project(format!(
                "cargo target directory {} is outside the project root {}; generated outputs must be workspace-relative, so point CARGO_TARGET_DIR / build.target-dir inside the project or unset it",
                target_directory.display(),
                workspace_root.display()
            ))
        })?;
        if target_rel.as_os_str().is_empty() {
            return Err(AdapterError::Project(
                "cargo target directory is the project root itself; outputs would overlap sources".to_owned(),
            ));
        }
        let output_dir = target_rel.join(profile.directory);
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
        let cargo_config = ["config.toml", "config"]
            .iter()
            .map(|name| PathBuf::from(".cargo").join(name))
            .filter(|path| root.join(path).is_file())
            .collect::<Vec<_>>();
        let local_sources = local_dependency_sources(&metadata, &member_ids, &workspace_root)?;

        // Prepare the staging directory generated actions use as TMP/TEMP
        // (see the crate-level contract): toolchain scratch writes land
        // inside the workspace, which is what makes these plans runnable
        // under `ant run --sandbox`. Idempotent; the one allowed side effect,
        // performed only after every check above passed.
        std::fs::create_dir_all(root.join(".ant").join("tmp")).map_err(AdapterError::from)?;
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
            for config in &cargo_config {
                declare_input(&mut inputs, config.clone(), InputKind::File);
            }
            let package_id = package["id"].as_str().ok_or_else(|| bad_metadata("package has no id"))?;
            if let Some(sources) = local_sources.get(package_id) {
                for source in sources {
                    declare_input(&mut inputs, source.clone(), InputKind::Tree);
                }
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
                    outputs.insert(output_dir
                        .join(format!("{target_name}{}", std::env::consts::EXE_SUFFIX)));
                } else if is_lib {
                    // Crate names in artifact file names use underscores.
                    let crate_name = target_name.replace('-', "_");
                    outputs.insert(output_dir.join(format!("lib{crate_name}.rlib")));
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
            // The executor clears the child environment, so an adapt-time
            // CARGO_TARGET_DIR would not reach the build; pin a non-default
            // target directory explicitly (workspace-relative, resolved
            // against the action's cwd = workspace root). The default
            // `target` needs nothing, keeping default plans unchanged.
            if target_rel != Path::new("target") {
                environment.push((
                    "CARGO_TARGET_DIR".to_owned(),
                    target_rel.to_string_lossy().into_owned(),
                ));
            }
            environment.sort();

            let mut argv = vec![
                "cargo".to_owned(),
                "build".to_owned(),
                "--package".to_owned(),
                (*name).to_owned(),
            ];
            argv.extend(profile.flags.iter().map(|flag| (*flag).to_owned()));

            let action = Action {
                schema_version: 1,
                id: (*name).to_owned(),
                argv,
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

/// For each workspace member package id, the workspace-relative directories
/// of non-member packages it transitively depends on whose manifests live
/// inside the workspace (vendored crates, non-member path dependencies).
/// Packages outside the workspace (the registry cache, git checkouts, path
/// dependencies elsewhere) cannot be declared and are skipped.
fn local_dependency_sources(
    metadata: &serde_json::Value,
    member_ids: &BTreeSet<&str>,
    workspace_root: &Path,
) -> Result<BTreeMap<String, BTreeSet<PathBuf>>, AdapterError> {
    let mut local_dirs: BTreeMap<&str, PathBuf> = BTreeMap::new();
    for package in metadata_value(metadata, "packages")?
        .as_array()
        .ok_or_else(|| bad_metadata("packages is not an array"))?
    {
        let id = package["id"].as_str().ok_or_else(|| bad_metadata("package has no id"))?;
        if member_ids.contains(id) {
            continue;
        }
        let manifest = PathBuf::from(
            package["manifest_path"]
                .as_str()
                .ok_or_else(|| bad_metadata("package manifest_path is missing"))?,
        );
        let dir = manifest.parent().ok_or_else(|| bad_metadata("manifest has no parent"))?;
        if let Ok(relative) = relative_to(workspace_root, dir)
            && !relative.as_os_str().is_empty()
        {
            local_dirs.insert(id, relative);
        }
    }

    let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let resolve = metadata_value(metadata, "resolve")?;
    for node in resolve["nodes"]
        .as_array()
        .ok_or_else(|| bad_metadata("resolve.nodes is missing"))?
    {
        let id = node["id"].as_str().ok_or_else(|| bad_metadata("resolve node has no id"))?;
        let deps = node["dependencies"]
            .as_array()
            .ok_or_else(|| bad_metadata("resolve node has no dependencies"))?
            .iter()
            .map(|dep| dep.as_str().ok_or_else(|| bad_metadata("resolve dependency is not a string")))
            .collect::<Result<Vec<_>, _>>()?;
        edges.insert(id, deps);
    }

    let mut result = BTreeMap::new();
    for member in member_ids {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut stack = vec![*member];
        while let Some(id) = stack.pop() {
            for dep in edges.get(id).map(Vec::as_slice).unwrap_or_default() {
                if seen.insert(dep) {
                    stack.push(dep);
                }
            }
        }
        let dirs: BTreeSet<PathBuf> = seen
            .iter()
            .filter_map(|id| local_dirs.get(id).cloned())
            .collect();
        result.insert((*member).to_owned(), dirs);
    }
    Ok(result)
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
        let first = adapter.generate(dir.path(), &AdapterOptions::new()).expect("generate");
        let second = adapter.generate(dir.path(), &AdapterOptions::new()).expect("generate again");
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

        let plan = adapter.generate(dir.path(), &AdapterOptions::new()).expect("generate");
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

        let plan = adapter.generate(dir.path(), &AdapterOptions::new()).expect("generate");
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
        assert!(adapter.generate(dir.path(), &AdapterOptions::new()).is_err());
    }

    fn options(pairs: &[(&str, &str)]) -> AdapterOptions {
        let mut options = AdapterOptions::new();
        for (key, value) in pairs {
            options.insert(key, value).expect("insert option");
        }
        options
    }

    fn single_bin(name: &str) -> TempDir {
        let dir = TempDir::new(name);
        write(
            &dir.path().join("Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
        write(&dir.path().join("src/main.rs"), "fn main() {}\n");
        dir
    }

    fn output_paths(action: &Action) -> Vec<PathBuf> {
        action.outputs.iter().map(|output| output.path.clone()).collect()
    }

    #[test]
    fn release_profile_adds_flag_and_maps_outputs() {
        let adapter = CargoAdapter;
        let dir = workspace();
        let plan = adapter
            .generate(dir.path(), &options(&[("profile", "release")]))
            .expect("generate");
        let app = &plan.actions[0].action;
        assert_eq!(app.argv, vec!["cargo", "build", "--package", "app-bin", "--release"]);
        assert_eq!(
            output_paths(app),
            vec![PathBuf::from("target")
                .join("release")
                .join(format!("app-bin{}", std::env::consts::EXE_SUFFIX))]
        );
        assert_eq!(
            output_paths(&plan.actions[1].action),
            vec![PathBuf::from("target").join("release").join("libcore_lib.rlib")]
        );
        // An explicit dev profile is byte-identical to the default plan.
        let default = adapter.generate(dir.path(), &AdapterOptions::new()).expect("default");
        let dev = adapter.generate(dir.path(), &options(&[("profile", "dev")])).expect("dev");
        assert_eq!(default, dev);
    }

    #[test]
    fn unsupported_options_fail_before_any_side_effect() {
        let adapter = CargoAdapter;
        let dir = single_bin("opt-errors");
        let error = adapter
            .generate(dir.path(), &options(&[("profile", "bench")]))
            .expect_err("custom profile rejected");
        assert!(matches!(error, AdapterError::Option(_)), "{error:?}");
        assert!(error.to_string().contains("supported: dev, release"), "{error}");
        let error = adapter
            .generate(dir.path(), &options(&[("outputs", "entry-points")]))
            .expect_err("foreign option rejected");
        assert!(error.to_string().contains("does not accept option 'outputs'"), "{error}");
        assert!(!dir.path().join(".ant").exists(), "no staging directory on option failure");
    }

    #[test]
    fn config_target_dir_inside_the_project_is_mapped_and_pinned() {
        let adapter = CargoAdapter;
        let dir = single_bin("cfg-target");
        write(
            &dir.path().join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"build/out\"\n",
        );
        let plan = adapter
            .generate(dir.path(), &options(&[("profile", "release")]))
            .expect("generate");
        let action = &plan.actions[0].action;
        let target = PathBuf::from("build").join("out");
        assert_eq!(
            output_paths(action),
            vec![target
                .join("release")
                .join(format!("cfg-target{}", std::env::consts::EXE_SUFFIX))]
        );
        // The executor clears the child environment, so the target dir is
        // pinned explicitly (workspace-relative, platform separators).
        assert!(
            action
                .environment
                .contains(&("CARGO_TARGET_DIR".to_owned(), target.to_string_lossy().into_owned())),
            "{:?}",
            action.environment
        );
        // The project config can change the build, so it is a declared input.
        assert!(action
            .inputs
            .iter()
            .any(|input| input.path == Path::new(".cargo").join("config.toml")));
        // The default layout does not pin anything (default plans unchanged).
        let plain = single_bin("plain-target");
        let plan = adapter.generate(plain.path(), &AdapterOptions::new()).expect("generate");
        assert!(plan.actions[0]
            .action
            .environment
            .iter()
            .all(|(key, _)| key != "CARGO_TARGET_DIR"));
    }

    #[test]
    fn target_dir_outside_the_project_fails_closed() {
        let adapter = CargoAdapter;
        let dir = single_bin("escape-target");
        write(
            &dir.path().join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"../escaped-target\"\n",
        );
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("outside target dir rejected");
        assert!(
            error.to_string().contains("is outside the project root"),
            "{error}"
        );
        assert!(!dir.path().join(".ant").exists(), "no staging directory on failure");
    }

    #[test]
    fn member_directory_as_project_root_is_rejected() {
        let adapter = CargoAdapter;
        let dir = workspace();
        let error = adapter
            .generate(&dir.path().join("app"), &AdapterOptions::new())
            .expect_err("member root rejected");
        assert!(error.to_string().contains("point --project at the workspace root"), "{error}");
    }

    /// A crates.io dependency served from a vendored directory source (the
    /// layout `cargo vendor` produces, built by hand so the test needs no
    /// registry cache or network) plus a non-member path dependency: both
    /// live inside the project, so both are declared as tree inputs of the
    /// members that transitively depend on them — and only those.
    #[test]
    fn in_project_dependency_sources_are_declared_for_dependents() {
        let adapter = CargoAdapter;
        let dir = TempDir::new("vendored");
        write(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"other\"]\nexclude = [\"helper\"]\nresolver = \"2\"\n",
        );
        write(
            &dir.path().join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nhelper = { path = \"../helper\" }\n",
        );
        write(&dir.path().join("app/src/main.rs"), "fn main() { helper::h(); }\n");
        // `helper` is an excluded (non-member) path dependency — in-workspace
        // path dependencies are otherwise auto-promoted to members — and
        // it depends on the vendored registry crate.
        write(
            &dir.path().join("helper/Cargo.toml"),
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ntinydep = \"1\"\n",
        );
        write(&dir.path().join("helper/src/lib.rs"), "pub fn h() -> u32 { tinydep::value() }\n");
        write(
            &dir.path().join("other/Cargo.toml"),
            "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&dir.path().join("other/src/lib.rs"), "pub fn o() {}\n");
        write_vendored_tinydep(dir.path());

        let plan = adapter.generate(dir.path(), &AdapterOptions::new()).expect("generate");
        let app = &plan.actions[0].action;
        assert_eq!(app.id, "app");
        let tree_inputs: Vec<&Path> = app
            .inputs
            .iter()
            .filter(|input| input.kind == InputKind::Tree)
            .map(|input| input.path.as_path())
            .collect();
        assert!(tree_inputs.contains(&Path::new("helper")), "{tree_inputs:?}");
        assert!(
            tree_inputs.contains(&Path::new("vendor").join("tinydep").as_path()),
            "transitive vendored source declared: {tree_inputs:?}"
        );
        let other = &plan.actions[1].action;
        assert_eq!(other.id, "other");
        assert!(
            other.inputs.iter().all(|input| !input.path.starts_with("vendor")
                && !input.path.starts_with("helper")),
            "non-dependents declare no foreign sources: {:?}",
            other.inputs
        );
        // Undeclared external configuration still keeps policy never.
        assert_eq!(app.cache_policy, CachePolicy::Never);
    }

    /// Write `vendor/tinydep` as a `cargo vendor` directory source and the
    /// `.cargo/config.toml` source replacement that points crates.io at it.
    fn write_vendored_tinydep(root: &Path) {
        write(
            &root.join("vendor/tinydep/Cargo.toml"),
            "[package]\nname = \"tinydep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
        );
        write(&root.join("vendor/tinydep/src/lib.rs"), "pub fn value() -> u32 { 7 }\n");
        write(
            &root.join("vendor/tinydep/.cargo-checksum.json"),
            "{\"files\":{},\"package\":\"0000000000000000000000000000000000000000000000000000000000000000\"}",
        );
        write(
            &root.join(".cargo/config.toml"),
            "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"vendor\"\n",
        );
    }
}
