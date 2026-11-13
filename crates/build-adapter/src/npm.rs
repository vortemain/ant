//! The npm adapter: a second build-system adapter (decision D-010).
//!
//! It reads `package.json` directly (no `npm` invocation for metadata) and
//! generates `npm run build` actions: one for a single-package project, or
//! one per npm-workspace package that has a `build` script.
//!
//! Honest boundaries:
//! - An action runs a package's own `scripts.build`. A single-package
//!   project without a `build` script fails generation rather than guessing
//!   a command; in a workspace, packages without a `build` script get no
//!   action (the same set `npm run build --workspaces --if-present` would
//!   run), and generation fails only when no workspace package has one.
//! - Workspaces (`"workspaces": [...]` in the root `package.json`, npm's
//!   documented array form) are expanded with a deliberately small pattern
//!   language: literal paths and `*` wildcards inside a path segment
//!   (`packages/*`, `apps/web-*`). `**`, negation, character classes, braces,
//!   absolute paths, and `..` fail closed instead of being approximated;
//!   hidden directories and `node_modules` never match. A wildcard match
//!   without a `package.json` is skipped (it is not a package); a literal
//!   entry without one fails. Every workspace package needs a unique `name`.
//! - Edges come from inter-package dependencies (`dependencies`,
//!   `devDependencies`, `peerDependencies`, `optionalDependencies` naming
//!   another workspace package): a dependency with a build action is a DAG
//!   edge; a dependency without one contributes its `package.json` and `src`
//!   tree as inputs, and the walk continues through it so a building package
//!   behind it still becomes an edge. Dependency cycles fail plan validation.
//! - The root package's own `build` script is not run in workspace mode: in
//!   monorepos it conventionally orchestrates the workspaces (`npm run build
//!   --workspaces`), so running it too would build everything twice.
//! - Inputs declared per action: the package's `package.json`, its `src` tree
//!   and `tsconfig.json` when present; in workspace mode also the root
//!   `package.json`, `package-lock.json`, and `tsconfig.json` when present
//!   (shared configuration and the single workspace lockfile). The installed
//!   `node_modules` tree is derived (the product of `npm install` against the
//!   registry), not source, and stays undeclared, so actions use
//!   `cache_policy: never` (D-003, D-010), mirroring the cargo adapter's
//!   stance on registry dependency sources.
//! - Outputs are not declared by default. npm has no standard output layout,
//!   and no `package.json` field states what a build *produces*: `main`,
//!   `types`, and `bin` state where consumers load the package from — often a
//!   committed source file that no build writes — and `files` / `exports`
//!   describe publish contents and resolution maps that routinely name
//!   sources and globs. With `--option outputs=entry-points` the user asserts
//!   that the package's `main`, `types`/`typings`, and `bin` entries are build
//!   products; they are then declared as outputs (and verified to exist after
//!   the run by the executor). Entries that fall under a declared input are
//!   rejected (a source cannot be an output), and asking for entry points on a
//!   package that declares none fails rather than silently declaring nothing.
//!   `files` and `exports` are never used.
//! - Toolchain identity is captured from the real `node` runtime: the resolved
//!   PATH executable's SHA-256 digest and `node --version`. `npm` is a script
//!   (`npm.cmd` on Windows, a JS file elsewhere), not a standalone
//!   executable; `node` is the truthful runtime identity.
//! - Windows invocation: `npm` is `npm.cmd` (a batch shim) and
//!   `CreateProcess` cannot execute batch scripts directly, so the argv is
//!   `cmd /C npm run build` on Windows and `npm run build` elsewhere.
//! - Workspace actions run with `working_directory` set to the package
//!   directory (npm runs scripts there anyway), so the workspace-relative
//!   `TMP`/`TEMP` value `.ant/tmp` resolves inside the package: generation
//!   prepares `.ant/tmp` in each building package (the staging-directory
//!   exception in the crate contract).

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};

pub struct NpmAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "outputs",
    values: "none|entry-points",
    description: "entry-points declares each package's main/types/bin entries as build outputs (user assertion, executor-verified)",
}];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputsMode {
    None,
    EntryPoints,
}

fn outputs_mode(options: &AdapterOptions) -> Result<OutputsMode, AdapterError> {
    match options.get("outputs") {
        None | Some("none") => Ok(OutputsMode::None),
        Some("entry-points") => Ok(OutputsMode::EntryPoints),
        Some(other) => Err(AdapterError::Option(format!(
            "npm outputs mode '{other}' is not supported (supported: none, entry-points)"
        ))),
    }
}

impl BuildAdapter for NpmAdapter {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
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

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        // Options are validated before any side effect.
        check_option_keys(self, options)?;
        let mode = outputs_mode(options)?;
        let root = canonical_root(project_root)?;
        let package = read_package_json(&root, Path::new(""))?;

        let (mut file, staging_dirs) = match package.get("workspaces") {
            None => single_package_plan(&root, &package, mode)?,
            Some(patterns) => workspace_plan(&root, patterns, mode)?,
        };
        // Validate the plan shape (cycles included) before the expensive
        // toolchain digest and before any side effect.
        file.to_graph()?;
        let toolchain = node_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        // Re-validate the final plan through the canonical graph conversion;
        // an adapter must never emit an invalid plan.
        file.to_graph()?;
        // Prepare the staging directories (crate contract exception), only
        // after every check passed.
        for dir in staging_dirs {
            std::fs::create_dir_all(root.join(dir))?;
        }
        Ok(file)
    }
}

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

/// Read and parse `<root>/<package_dir>/package.json`.
fn read_package_json(root: &Path, package_dir: &Path) -> Result<serde_json::Value, AdapterError> {
    let path = root.join(package_dir).join("package.json");
    let text = std::fs::read_to_string(&path).map_err(|error| {
        AdapterError::Project(format!(
            "cannot read package.json at {}: {error}",
            path.display()
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        if package_dir.as_os_str().is_empty() {
            AdapterError::Project(format!("package.json is not valid JSON: {error}"))
        } else {
            AdapterError::Project(format!(
                "{} is not valid JSON: {error}",
                package_dir.join("package.json").display()
            ))
        }
    })?;
    if !value.is_object() {
        return Err(AdapterError::Project(format!(
            "{} is not a JSON object",
            path.display()
        )));
    }
    Ok(value)
}

/// The package's `build` script: `Ok(None)` when absent, an error when
/// present but empty or not a string.
fn build_script<'a>(
    package: &'a serde_json::Value,
    label: &str,
) -> Result<Option<&'a str>, AdapterError> {
    let Some(build) = package
        .get("scripts")
        .and_then(|scripts| scripts.get("build"))
    else {
        return Ok(None);
    };
    let Some(script) = build.as_str() else {
        return Err(AdapterError::Project(format!(
            "the `build` script in {label} is not a string"
        )));
    };
    if script.trim().is_empty() {
        return Err(AdapterError::Project(format!(
            "the `build` script in {label} is empty; the npm adapter cannot generate a meaningful action"
        )));
    }
    Ok(Some(script))
}

fn npm_build_argv() -> Vec<String> {
    // `npm` is `npm.cmd` (a batch shim) on Windows and cannot be launched
    // directly by CreateProcess; `cmd /C` resolves it. Elsewhere `npm` is on
    // PATH as an executable script.
    let argv: &[&str] = if cfg!(windows) {
        &["cmd", "/C", "npm", "run", "build"]
    } else {
        &["npm", "run", "build"]
    };
    argv.iter().map(|arg| (*arg).to_owned()).collect()
}

/// `TMP`/`TEMP` (plus `TMPDIR` off Windows) pointing at `.ant/tmp` relative
/// to the action's working directory (T3.3 staging pattern).
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

/// Declare `<package_dir>/package.json`, `<package_dir>/src` (tree) and
/// `<package_dir>/tsconfig.json` when present.
fn declare_package_inputs(
    root: &Path,
    package_dir: &Path,
    inputs: &mut BTreeMap<PathBuf, InputKind>,
) {
    inputs
        .entry(package_dir.join("package.json"))
        .or_insert(InputKind::File);
    let src = package_dir.join("src");
    if root.join(&src).is_dir() {
        inputs.entry(src).or_insert(InputKind::Tree);
    }
    let tsconfig = package_dir.join("tsconfig.json");
    if root.join(&tsconfig).is_file() {
        inputs.entry(tsconfig).or_insert(InputKind::File);
    }
}

fn build_action(
    id: String,
    working_directory: PathBuf,
    inputs: BTreeMap<PathBuf, InputKind>,
    outputs: BTreeSet<PathBuf>,
) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: npm_build_argv(),
        working_directory,
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        // Outputs only when the user asserted entry points (see module docs).
        outputs: outputs
            .into_iter()
            .map(|path| Output {
                path,
                kind: OutputKind::File,
            })
            .collect(),
        environment: staging_environment(),
        // node_modules content is an undeclared input, so these actions must
        // never claim cache eligibility.
        cache_policy: CachePolicy::Never,
        // Filled in by `generate` once the plan shape is validated.
        toolchain: None,
        timeout_ms: None,
        resources: None,
    }
}

fn single_package_plan(
    root: &Path,
    package: &serde_json::Value,
    mode: OutputsMode,
) -> Result<(PlanFile, Vec<PathBuf>), AdapterError> {
    // Require a `build` script: guessing a command would be dishonest.
    if build_script(package, "package.json")?.is_none() {
        return Err(AdapterError::Project(
            "no `build` script in package.json; the npm adapter generates a `npm run build` action and requires a `scripts.build` entry"
                .to_owned(),
        ));
    }
    let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
    inputs.insert(PathBuf::from("package.json"), InputKind::File);
    if root.join("package-lock.json").is_file() {
        inputs.insert(PathBuf::from("package-lock.json"), InputKind::File);
    }
    declare_package_inputs(root, Path::new(""), &mut inputs);
    let outputs = match mode {
        OutputsMode::None => BTreeSet::new(),
        OutputsMode::EntryPoints => {
            entry_point_outputs(package, Path::new(""), "package.json", &inputs)?
        }
    };
    let id = package
        .get("name")
        .and_then(|name| name.as_str())
        .map(|name| name.to_owned())
        .unwrap_or_else(|| "build".to_owned());
    let mut file = PlanFile::new();
    file.actions.push(PlanAction {
        action: build_action(id, PathBuf::from("."), inputs, outputs),
        dependencies: Vec::new(),
    });
    Ok((file, vec![PathBuf::from(".ant").join("tmp")]))
}

/// One discovered workspace package.
struct WorkspacePackage {
    dir: PathBuf,
    manifest: serde_json::Value,
    builds: bool,
}

fn workspace_plan(
    root: &Path,
    patterns: &serde_json::Value,
    mode: OutputsMode,
) -> Result<(PlanFile, Vec<PathBuf>), AdapterError> {
    let Some(patterns) = patterns.as_array() else {
        return Err(AdapterError::Project(
            "`workspaces` in package.json must be an array of paths/patterns (npm's documented form); other forms are not supported"
                .to_owned(),
        ));
    };
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for pattern in patterns {
        let Some(pattern) = pattern.as_str() else {
            return Err(AdapterError::Project(
                "`workspaces` entries must be strings".to_owned(),
            ));
        };
        dirs.extend(expand_workspace_pattern(root, pattern)?);
    }
    if dirs.is_empty() {
        return Err(AdapterError::Project(
            "`workspaces` in package.json matched no packages".to_owned(),
        ));
    }

    let mut packages: BTreeMap<String, WorkspacePackage> = BTreeMap::new();
    for dir in dirs {
        let manifest = read_package_json(root, &dir)?;
        let label = dir.join("package.json").display().to_string();
        let Some(name) = manifest.get("name").and_then(|name| name.as_str()) else {
            return Err(AdapterError::Project(format!(
                "workspace package {label} has no `name`; npm workspaces require one"
            )));
        };
        let name = name.to_owned();
        let builds = build_script(&manifest, &label)?.is_some();
        if let Some(existing) = packages.get(&name) {
            return Err(AdapterError::Project(format!(
                "workspace packages {} and {} share the name '{name}'",
                existing.dir.display(),
                dir.display()
            )));
        }
        packages.insert(
            name,
            WorkspacePackage {
                dir,
                manifest,
                builds,
            },
        );
    }
    if !packages.values().any(|package| package.builds) {
        return Err(AdapterError::Project(
            "no workspace package has a `build` script; the npm adapter generates `npm run build` actions and requires at least one"
                .to_owned(),
        ));
    }

    let root_inputs: Vec<PathBuf> = ["package.json", "package-lock.json", "tsconfig.json"]
        .iter()
        .map(PathBuf::from)
        .filter(|path| root.join(path).is_file())
        .collect();

    let mut file = PlanFile::new();
    let mut staging_dirs = Vec::new();
    for (name, package) in &packages {
        if !package.builds {
            continue;
        }
        let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
        for path in &root_inputs {
            inputs.insert(path.clone(), InputKind::File);
        }
        declare_package_inputs(root, &package.dir, &mut inputs);

        // Walk workspace-internal dependencies: building packages become
        // edges (their own edges cover what lies behind them); non-building
        // packages contribute their sources as inputs and are walked
        // through.
        let mut edges: BTreeSet<String> = BTreeSet::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = workspace_dependencies(&package.manifest, &packages, name);
        while let Some(dep) = stack.pop() {
            if !seen.insert(dep) {
                continue;
            }
            let dep_package = &packages[dep];
            if dep_package.builds {
                edges.insert(dep.to_owned());
            } else {
                declare_package_inputs(root, &dep_package.dir, &mut inputs);
                stack.extend(workspace_dependencies(
                    &dep_package.manifest,
                    &packages,
                    dep,
                ));
            }
        }

        let outputs = match mode {
            OutputsMode::None => BTreeSet::new(),
            OutputsMode::EntryPoints => entry_point_outputs(
                &package.manifest,
                &package.dir,
                &package.dir.join("package.json").display().to_string(),
                &inputs,
            )?,
        };
        staging_dirs.push(package.dir.join(".ant").join("tmp"));
        file.actions.push(PlanAction {
            action: build_action(name.clone(), package.dir.clone(), inputs, outputs),
            dependencies: edges.into_iter().collect(),
        });
    }
    Ok((file, staging_dirs))
}

/// Names of other workspace packages `manifest` depends on, sorted.
fn workspace_dependencies<'a>(
    manifest: &serde_json::Value,
    packages: &'a BTreeMap<String, WorkspacePackage>,
    own_name: &str,
) -> Vec<&'a str> {
    let mut names: BTreeSet<&'a str> = BTreeSet::new();
    for field in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(deps) = manifest.get(field).and_then(|deps| deps.as_object()) {
            for dep in deps.keys() {
                if dep != own_name
                    && let Some((key, _)) = packages.get_key_value(dep)
                {
                    names.insert(key.as_str());
                }
            }
        }
    }
    names.into_iter().collect()
}

/// Expand one `workspaces` entry into workspace-relative package
/// directories. See the module docs for the supported pattern language.
fn expand_workspace_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, AdapterError> {
    let unsupported = |why: &str| {
        AdapterError::Project(format!(
            "workspace pattern '{pattern}' is not supported ({why}); supported: literal paths and `*` within a path segment"
        ))
    };
    let trimmed = pattern
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(unsupported("empty"));
    }
    if trimmed.starts_with('!') {
        return Err(unsupported("negation"));
    }
    if trimmed.contains("**") {
        return Err(unsupported("`**` recursion"));
    }
    if trimmed.contains(['?', '[', ']', '{', '}', '\\']) {
        return Err(unsupported(
            "character classes, braces, `?`, or backslashes",
        ));
    }
    if Path::new(trimmed).is_absolute() || trimmed.starts_with('/') {
        return Err(unsupported("absolute path"));
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(unsupported("empty, `.`, or `..` segment"));
    }
    let has_wildcard = segments.iter().any(|segment| segment.contains('*'));

    let mut current: Vec<PathBuf> = vec![PathBuf::new()];
    for segment in &segments {
        let mut next = Vec::new();
        for base in &current {
            if segment.contains('*') {
                let Ok(entries) = std::fs::read_dir(root.join(base)) else {
                    continue;
                };
                for entry in entries {
                    let entry = entry?;
                    // Real directories only (symlinks are not followed: the
                    // executor rejects symlinked input paths anyway).
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if name.starts_with('.') || name == "node_modules" {
                        continue;
                    }
                    if wildcard_match(segment, name) {
                        next.push(base.join(name));
                    }
                }
            } else {
                next.push(base.join(segment));
            }
        }
        current = next;
    }

    let mut matched = Vec::new();
    for dir in current {
        if root.join(&dir).join("package.json").is_file() {
            matched.push(dir);
        } else if !has_wildcard {
            return Err(AdapterError::Project(format!(
                "workspace entry '{pattern}' has no package.json at {}",
                dir.display()
            )));
        }
    }
    matched.sort();
    Ok(matched)
}

/// `*`-only glob match within one path segment.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    true
}

/// Declared outputs for `--option outputs=entry-points`: the package's
/// `main`, `types`/`typings`, and `bin` entries, workspace-relative. Fails
/// when an entry is malformed, escapes the package, falls under a declared
/// input, or when the package declares no entry points at all.
fn entry_point_outputs(
    manifest: &serde_json::Value,
    package_dir: &Path,
    label: &str,
    inputs: &BTreeMap<PathBuf, InputKind>,
) -> Result<BTreeSet<PathBuf>, AdapterError> {
    let mut entries: Vec<(String, &str)> = Vec::new();
    for field in ["main", "types", "typings"] {
        if let Some(value) = manifest.get(field) {
            let text = value.as_str().ok_or_else(|| {
                AdapterError::Project(format!("`{field}` in {label} is not a string"))
            })?;
            entries.push((field.to_owned(), text));
        }
    }
    match manifest.get("bin") {
        None => {}
        Some(serde_json::Value::String(text)) => entries.push(("bin".to_owned(), text)),
        Some(serde_json::Value::Object(map)) => {
            for (command, value) in map {
                let text = value.as_str().ok_or_else(|| {
                    AdapterError::Project(format!("`bin.{command}` in {label} is not a string"))
                })?;
                entries.push((format!("bin.{command}"), text));
            }
        }
        Some(_) => {
            return Err(AdapterError::Project(format!(
                "`bin` in {label} must be a string or an object of strings"
            )));
        }
    }
    if entries.is_empty() {
        return Err(AdapterError::Option(format!(
            "outputs=entry-points was requested but {label} declares no main, types, typings, or bin entries"
        )));
    }

    let mut outputs = BTreeSet::new();
    for (field, text) in entries {
        let relative = Path::new(text.trim().trim_start_matches("./"));
        let plain = !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !plain {
            return Err(AdapterError::Project(format!(
                "`{field}` entry '{text}' in {label} is not a plain path inside the package"
            )));
        }
        // Rebuild from components so separators are platform-native.
        let output: PathBuf = package_dir.join(relative).components().collect();
        if let Some(input) = inputs.keys().find(|input| output.starts_with(input)) {
            return Err(AdapterError::Project(format!(
                "`{field}` entry '{text}' in {label} lies under the declared input {}; a source cannot also be a build output",
                input.display()
            )));
        }
        outputs.insert(output);
    }
    Ok(outputs)
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
        write(
            &dir.path().join("package.json"),
            &package_json("detect-app"),
        );
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
        write(
            &dir.path().join("package-lock.json"),
            "{\"lockfileVersion\":3}",
        );
        // tsconfig is a common build driver; declare it when present.
        write(
            &dir.path().join("tsconfig.json"),
            "{\"compilerOptions\":{}}",
        );

        let first = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let second = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate again");
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
        let paths: Vec<&Path> = action
            .inputs
            .iter()
            .map(|input| input.path.as_path())
            .collect();
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
        assert!(
            action.outputs.is_empty(),
            "npm adapter must not guess outputs"
        );

        // node_modules is an undeclared input, so policy is never.
        assert_eq!(action.cache_policy, CachePolicy::Never);

        // Scratch writes are redirected into the workspace staging dir.
        let staging = PathBuf::from(".ant")
            .join("tmp")
            .to_string_lossy()
            .into_owned();
        // TMPDIR is added off Windows (the Unix convention), as documented.
        let mut expected = vec![
            ("TEMP".to_owned(), staging.clone()),
            ("TMP".to_owned(), staging.clone()),
        ];
        if cfg!(not(windows)) {
            expected.push(("TMPDIR".to_owned(), staging.clone()));
        }
        assert_eq!(action.environment, expected);
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
        write(
            &dir.path().join("package.json"),
            &package_json("minimal-app"),
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &plan.actions[0].action;
        let paths: Vec<&Path> = action
            .inputs
            .iter()
            .map(|input| input.path.as_path())
            .collect();
        // Only package.json is declared; nothing else is invented.
        assert_eq!(paths, vec![Path::new("package.json")]);
    }

    #[test]
    fn generate_rejects_projects_without_a_build_script() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("no-build");
        write(
            &dir.path().join("package.json"),
            "{\"name\":\"no-build\",\"version\":\"0.1.0\",\"scripts\":{\"test\":\"node test.js\"}}",
        );
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(detection.supported, "package.json present means supported");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail without a build script");
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
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail on invalid JSON");
        assert!(
            error.to_string().contains("package.json is not valid JSON"),
            "error was {error}"
        );
    }

    #[test]
    fn generate_rejects_empty_build_script() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("empty-build");
        write(
            &dir.path().join("package.json"),
            "{\"name\":\"empty\",\"version\":\"0.1.0\",\"scripts\":{\"build\":\"   \"}}",
        );
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail on empty build script");
        assert!(
            error
                .to_string()
                .contains("`build` script in package.json is empty"),
            "error was {error}"
        );
    }

    fn options(pairs: &[(&str, &str)]) -> AdapterOptions {
        let mut options = AdapterOptions::new();
        for (key, value) in pairs {
            options.insert(key, value).expect("insert option");
        }
        options
    }

    fn paths(action: &Action) -> Vec<PathBuf> {
        action
            .inputs
            .iter()
            .map(|input| input.path.clone())
            .collect()
    }

    /// Root with `packages/*`: `core` builds, `util` (no build script)
    /// depends on `core`, `app` builds and depends on `util`, and `docs`
    /// has no build script and no dependents.
    fn monorepo() -> TempDir {
        let dir = TempDir::new("mono");
        write(
            &dir.path().join("package.json"),
            "{\"name\":\"root\",\"private\":true,\"workspaces\":[\"packages/*\"],\"scripts\":{\"build\":\"npm run build --workspaces\"}}",
        );
        write(
            &dir.path().join("package-lock.json"),
            "{\"lockfileVersion\":3}",
        );
        write(
            &dir.path().join("packages/core/package.json"),
            "{\"name\":\"@demo/core\",\"main\":\"dist/core.js\",\"scripts\":{\"build\":\"node build.js\"}}",
        );
        write(
            &dir.path().join("packages/core/src/index.js"),
            "module.exports = 1;\n",
        );
        write(
            &dir.path().join("packages/util/package.json"),
            "{\"name\":\"@demo/util\",\"dependencies\":{\"@demo/core\":\"*\"}}",
        );
        write(&dir.path().join("packages/util/src/u.js"), "\n");
        write(
            &dir.path().join("packages/app/package.json"),
            "{\"name\":\"@demo/app\",\"bin\":{\"demo\":\"./dist/app.js\"},\"devDependencies\":{\"@demo/util\":\"*\",\"left-pad\":\"1\"},\"scripts\":{\"build\":\"node build.js\"}}",
        );
        write(
            &dir.path().join("packages/docs/package.json"),
            "{\"name\":\"docs\"}",
        );
        // Not packages: a hidden dir, node_modules, and a dir without a
        // package.json are never matched by the wildcard.
        write(
            &dir.path().join("packages/.hidden/package.json"),
            "{\"name\":\"hidden\",\"scripts\":{\"build\":\"x\"}}",
        );
        write(
            &dir.path().join("packages/node_modules/package.json"),
            "{\"name\":\"nm\",\"scripts\":{\"build\":\"x\"}}",
        );
        fs::create_dir_all(dir.path().join("packages/empty")).expect("mkdir");
        dir
    }

    #[test]
    fn workspaces_generate_one_action_per_building_package_with_edges() {
        let adapter = NpmAdapter;
        let dir = monorepo();
        let first = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let second = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("again");
        assert_eq!(first, second, "generation must be deterministic");

        let ids: Vec<&str> = first
            .actions
            .iter()
            .map(|entry| entry.action.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["@demo/app", "@demo/core"],
            "building packages only, sorted"
        );

        // app reaches core through the non-building util: core is an edge,
        // util's sources are inputs; external deps (left-pad) are ignored.
        let app = &first.actions[0];
        assert_eq!(app.dependencies, vec!["@demo/core".to_owned()]);
        let app_dir = PathBuf::from("packages").join("app");
        assert_eq!(app.action.working_directory, app_dir);
        let util = PathBuf::from("packages").join("util");
        assert_eq!(
            paths(&app.action),
            vec![
                PathBuf::from("package-lock.json"),
                PathBuf::from("package.json"),
                app_dir.join("package.json"),
                util.join("package.json"),
                util.join("src"),
            ]
        );
        assert!(app.action.outputs.is_empty(), "no outputs by default");
        assert_eq!(app.action.cache_policy, CachePolicy::Never);

        let core = &first.actions[1];
        assert!(core.dependencies.is_empty());
        let core_dir = PathBuf::from("packages").join("core");
        assert!(paths(&core.action).contains(&core_dir.join("src")));

        // Each building package gets its own staging dir, because TMP/TEMP
        // resolve against the action's working directory.
        assert!(dir.path().join(&app_dir).join(".ant/tmp").is_dir());
        assert!(dir.path().join(&core_dir).join(".ant/tmp").is_dir());
        assert!(!dir.path().join("packages/util/.ant").exists());
    }

    #[test]
    fn entry_point_outputs_are_declared_only_on_request() {
        let adapter = NpmAdapter;
        let dir = monorepo();
        // app declares only `bin`, core only `main`.
        let plan = adapter
            .generate(dir.path(), &options(&[("outputs", "entry-points")]))
            .expect("generate");
        let output = |index: usize| -> Vec<PathBuf> {
            plan.actions[index]
                .action
                .outputs
                .iter()
                .map(|o| o.path.clone())
                .collect()
        };
        assert_eq!(
            output(0),
            vec![
                PathBuf::from("packages")
                    .join("app")
                    .join("dist")
                    .join("app.js")
            ]
        );
        assert_eq!(
            output(1),
            vec![
                PathBuf::from("packages")
                    .join("core")
                    .join("dist")
                    .join("core.js")
            ]
        );

        let single = TempDir::new("entry-single");
        write(
            &single.path().join("package.json"),
            "{\"name\":\"s\",\"main\":\"lib/index.js\",\"types\":\"lib/index.d.ts\",\"bin\":\"lib/cli.js\",\"files\":[\"lib\",\"src\"],\"scripts\":{\"build\":\"tsc\"}}",
        );
        let plan = adapter
            .generate(single.path(), &options(&[("outputs", "entry-points")]))
            .expect("generate");
        let lib = PathBuf::from("lib");
        assert_eq!(
            plan.actions[0]
                .action
                .outputs
                .iter()
                .map(|o| o.path.clone())
                .collect::<Vec<_>>(),
            vec![
                lib.join("cli.js"),
                lib.join("index.d.ts"),
                lib.join("index.js")
            ],
            "`files` is never used"
        );
    }

    #[test]
    fn entry_points_that_are_sources_or_missing_fail() {
        let adapter = NpmAdapter;
        let dir = TempDir::new("entry-src");
        write(
            &dir.path().join("package.json"),
            "{\"name\":\"s\",\"main\":\"src/index.js\",\"scripts\":{\"build\":\"node b.js\"}}",
        );
        write(&dir.path().join("src/index.js"), "\n");
        let error = adapter
            .generate(dir.path(), &options(&[("outputs", "entry-points")]))
            .expect_err("source entry rejected");
        assert!(
            error
                .to_string()
                .contains("a source cannot also be a build output"),
            "{error}"
        );
        assert!(
            !dir.path().join(".ant").exists(),
            "no staging directory on failure"
        );

        let none = TempDir::new("entry-none");
        write(&none.path().join("package.json"), &package_json("n"));
        let error = adapter
            .generate(none.path(), &options(&[("outputs", "entry-points")]))
            .expect_err("no entry points");
        assert!(
            error
                .to_string()
                .contains("declares no main, types, typings, or bin"),
            "{error}"
        );

        let escape = TempDir::new("entry-escape");
        write(
            &escape.path().join("package.json"),
            "{\"name\":\"e\",\"main\":\"../x.js\",\"scripts\":{\"build\":\"node b.js\"}}",
        );
        assert!(
            adapter
                .generate(escape.path(), &options(&[("outputs", "entry-points")]))
                .is_err()
        );

        let error = adapter
            .generate(none.path(), &options(&[("outputs", "dist")]))
            .expect_err("bad mode");
        assert!(matches!(error, AdapterError::Option(_)), "{error:?}");
    }

    #[test]
    fn workspace_patterns_support_literals_and_segment_wildcards_only() {
        let dir = TempDir::new("patterns");
        for name in ["apps/web-a", "apps/web-b", "apps/api", "tools/cli"] {
            write(&dir.path().join(name).join("package.json"), "{}");
        }
        let expand = |pattern: &str| expand_workspace_pattern(dir.path(), pattern);
        assert_eq!(
            expand("apps/web-*").expect("wildcard"),
            vec![
                PathBuf::from("apps").join("web-a"),
                PathBuf::from("apps").join("web-b")
            ]
        );
        assert_eq!(
            expand("./tools/cli/").expect("literal"),
            vec![PathBuf::from("tools").join("cli")]
        );
        assert_eq!(
            expand("*/cli").expect("leading wildcard"),
            vec![PathBuf::from("tools").join("cli")]
        );
        for bad in [
            "apps/**",
            "!apps/api",
            "../x",
            "apps/{a,b}",
            "apps/web-?",
            "/abs",
            "",
        ] {
            assert!(expand(bad).is_err(), "pattern {bad:?} must be rejected");
        }
        let error = expand("apps/missing").expect_err("literal without package.json");
        assert!(error.to_string().contains("has no package.json"), "{error}");
        assert!(wildcard_match("a*b*c", "aXbYc"));
        assert!(!wildcard_match("a*b", "ab-c"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("web-*", "api"));
    }

    #[test]
    fn invalid_workspaces_fail_honestly() {
        let adapter = NpmAdapter;
        let generate = |root: &str, members: &[(&str, &str)]| {
            let dir = TempDir::new("ws-invalid");
            write(&dir.path().join("package.json"), root);
            for (path, manifest) in members {
                write(&dir.path().join(path).join("package.json"), manifest);
            }
            let result = adapter.generate(dir.path(), &AdapterOptions::new());
            let staged = dir.path().join(".ant").exists()
                || members
                    .iter()
                    .any(|(path, _)| dir.path().join(path).join(".ant").exists());
            assert!(!staged, "no staging directories on failure");
            result.expect_err("must fail").to_string()
        };
        let build = "{\"build\":\"node b.js\"}";
        let error = generate("{\"workspaces\":{\"packages\":[\"p/*\"]}}", &[]);
        assert!(error.contains("must be an array"), "{error}");
        let error = generate("{\"workspaces\":[\"p/*\"]}", &[("p/a", "{\"name\":\"a\"}")]);
        assert!(
            error.contains("no workspace package has a `build` script"),
            "{error}"
        );
        let error = generate(
            "{\"workspaces\":[\"p/*\"]}",
            &[
                ("p/a", &format!("{{\"name\":\"x\",\"scripts\":{build}}}")),
                ("p/b", "{\"name\":\"x\"}"),
            ],
        );
        assert!(error.contains("share the name 'x'"), "{error}");
        let error = generate(
            "{\"workspaces\":[\"p/*\"]}",
            &[("p/a", &format!("{{\"scripts\":{build}}}"))],
        );
        assert!(error.contains("has no `name`"), "{error}");
        let error = generate("{\"workspaces\":[\"p/*\"]}", &[]);
        assert!(error.contains("matched no packages"), "{error}");
        // A dependency cycle between building packages fails plan validation.
        let error = generate(
            "{\"workspaces\":[\"p/*\"]}",
            &[
                (
                    "p/a",
                    &format!(
                        "{{\"name\":\"a\",\"dependencies\":{{\"b\":\"*\"}},\"scripts\":{build}}}"
                    ),
                ),
                (
                    "p/b",
                    &format!(
                        "{{\"name\":\"b\",\"dependencies\":{{\"a\":\"*\"}},\"scripts\":{build}}}"
                    ),
                ),
            ],
        );
        assert!(error.contains("generated plan is invalid"), "{error}");
    }
}
