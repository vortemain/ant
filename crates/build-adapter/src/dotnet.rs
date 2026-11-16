//! The .NET adapter (roadmap P6): publishes a C#, F#, or VB project with
//! `dotnet publish`.
//!
//! A .NET project is a directory with exactly one `*.sln`/`*.slnx` solution
//! or, without one, exactly one `*.csproj`/`*.fsproj`/`*.vbproj` at its root.
//! The adapter generates one action, `dotnet publish <entry> -c <config> -o
//! .ant/dotnet/<name>`, and collects the published application directory
//! into `dist/<name>/`.
//!
//! Honest limits (decision D-051, recorded in the module tests):
//!
//! - NuGet packages are restored from a feed into the user's global packages
//!   folder, outside the workspace, so they are undeclared inputs. Package
//!   versions are pinned in the project files (and `packages.lock.json` when
//!   used), so this machine may reuse its own result (`cache_policy: local`,
//!   D-028), but results are never shared.
//! - Inputs are the whole project minus `.ant/`, `.git/`, `bin/`, `obj/`
//!   (MSBuild's own output and intermediate directories, at any depth).
//! - With several entry files at the root, detection fails asking the
//!   developer to keep one (or add a solution); Ant does not guess.
//! - The CLI's per-user state (NuGet configuration and packages, the
//!   first-run sentinel) is reached through `HOME` (and, on Windows,
//!   `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`), declared into
//!   the action environment because the executor clears it. Telemetry and
//!   the first-run banner are turned off.
//! - Toolchain identity: the resolved `dotnet` host's digest and `dotnet
//!   --version` (the SDK version that `global.json` selects, run in the
//!   project).

use crate::cargo::declare_project;
use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};
use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct DotnetAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "configuration",
    values: "Release|Debug",
    description: "MSBuild configuration passed to dotnet publish -c",
}];

/// Where `dotnet publish` writes (workspace-relative; `.ant/` is never an
/// input).
const OUTPUT_DIR: &str = ".ant/dotnet";

/// The solution, or the single project, `dotnet publish` should build.
fn entry(root: &Path) -> Result<PathBuf, String> {
    let mut solutions = Vec::new();
    let mut projects = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|error| error.to_string())?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".sln") || lower.ends_with(".slnx") {
            solutions.push(name);
        } else if [".csproj", ".fsproj", ".vbproj"]
            .iter()
            .any(|extension| lower.ends_with(extension))
        {
            projects.push(name);
        }
    }
    solutions.sort();
    projects.sort();
    match (solutions.as_slice(), projects.as_slice()) {
        ([solution], _) => Ok(PathBuf::from(solution)),
        ([], [project]) => Ok(PathBuf::from(project)),
        ([], []) => Err("no *.sln, *.csproj, *.fsproj, or *.vbproj".to_owned()),
        ([], several) => Err(format!(
            "several projects at the root ({}); add a solution or keep one",
            several.join(", ")
        )),
        (several, _) => Err(format!(
            "several solutions at the root ({}); keep one",
            several.join(", ")
        )),
    }
}

/// The deliverable directory's name: the entry file's stem.
fn app_name(entry: &Path) -> String {
    entry
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "app".to_owned())
}

/// Per-user locations the .NET CLI and NuGet need, from this process's
/// environment (the executor clears the child's).
const USER_VARIABLES: &[&str] = if cfg!(windows) {
    &[
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramData",
        "HOMEDRIVE",
        "HOMEPATH",
        "NUGET_PACKAGES",
        "DOTNET_ROOT",
    ]
} else {
    &["HOME", "NUGET_PACKAGES", "DOTNET_ROOT", "XDG_DATA_HOME"]
};

impl BuildAdapter for DotnetAdapter {
    fn name(&self) -> &'static str {
        "dotnet"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        match entry(project_root) {
            Ok(_) => Ok(Detection::supported()),
            Err(reason) => Ok(Detection::unsupported(format!(
                "{reason} at {}",
                project_root.display()
            ))),
        }
    }

    /// The published application directory, collected whole into
    /// `dist/<name>/`.
    fn built_artifacts(&self, project_root: &Path, plan: &PlanFile) -> Vec<PathBuf> {
        plan.actions
            .iter()
            .flat_map(|planned| planned.action.outputs.iter())
            .filter(|output| output.kind == OutputKind::Tree)
            .map(|output| output.path.clone())
            .filter(|path| project_root.join(path).is_dir())
            .collect()
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let configuration = match options.get("configuration") {
            None => "Release",
            Some(value) if value.eq_ignore_ascii_case("release") => "Release",
            Some(value) if value.eq_ignore_ascii_case("debug") => "Debug",
            Some(other) => {
                return Err(AdapterError::Option(format!(
                    "dotnet configuration '{other}' is not supported (supported: Release, Debug)"
                )));
            }
        };
        let root = canonical_root(project_root)?;
        let entry = entry(&root).map_err(AdapterError::Project)?;
        let name = app_name(&entry);
        let dotnet = locate_executable(&format!("dotnet{}", std::env::consts::EXE_SUFFIX))?;
        let version = executable_version(&dotnet, &root)?;
        let digest = crate::toolchain_digest(&dotnet.canonicalize().unwrap_or(dotnet.clone()))
            .map_err(|error| AdapterError::Toolchain(format!("cannot digest dotnet: {error}")))?
            .to_string();

        let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
        let mut has_symlink = false;
        declare_project(
            &root,
            Path::new(""),
            &[PathBuf::from(".ant"), PathBuf::from(".git")],
            &mut inputs,
            &mut has_symlink,
        )?;
        // MSBuild writes bin/ and obj/ next to every project, at any depth.
        inputs.retain(|path, _| {
            !path.components().any(|component| {
                let part = component.as_os_str().to_string_lossy();
                part.eq_ignore_ascii_case("bin") || part.eq_ignore_ascii_case("obj")
            })
        });

        std::fs::create_dir_all(root.join(".ant").join("tmp")).map_err(AdapterError::from)?;
        let staging = Path::new(".ant").join("tmp").to_string_lossy().into_owned();
        let mut environment: Vec<(String, String)> = vec![
            ("DOTNET_CLI_TELEMETRY_OPTOUT".to_owned(), "1".to_owned()),
            ("DOTNET_NOLOGO".to_owned(), "1".to_owned()),
            (
                "DOTNET_SKIP_FIRST_TIME_EXPERIENCE".to_owned(),
                "1".to_owned(),
            ),
            ("TMP".to_owned(), staging.clone()),
            ("TEMP".to_owned(), staging.clone()),
        ];
        if cfg!(not(windows)) {
            environment.push(("TMPDIR".to_owned(), staging));
        }
        for variable in USER_VARIABLES {
            if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
                environment.push(((*variable).to_owned(), value.to_string_lossy().into_owned()));
            }
        }
        environment.sort();
        environment.dedup_by(|a, b| a.0 == b.0);

        let output = Path::new(OUTPUT_DIR).join(&name);
        let argv = vec![
            dotnet.to_string_lossy().into_owned(),
            "publish".to_owned(),
            entry.to_string_lossy().into_owned(),
            "-c".to_owned(),
            configuration.to_owned(),
            "-o".to_owned(),
            output.to_string_lossy().replace('\\', "/"),
            "--nologo".to_owned(),
        ];
        let action = Action {
            schema_version: 1,
            id: "dotnet-publish".to_owned(),
            argv,
            working_directory: PathBuf::from("."),
            inputs: inputs
                .into_iter()
                .map(|(path, kind)| Input { path, kind })
                .collect(),
            outputs: vec![Output {
                path: output,
                kind: OutputKind::Tree,
            }],
            environment,
            cache_policy: if has_symlink {
                CachePolicy::Never
            } else {
                CachePolicy::Local
            },
            toolchain: Some(ToolchainIdentity {
                name: "dotnet".to_owned(),
                version,
                executable_digest: digest,
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
            let path = std::env::temp_dir()
                .join(format!("ant-dotnet-{name}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn touch(path: &Path) {
        std::fs::write(path, "<Project />").expect("write");
    }

    #[test]
    fn detection_prefers_one_solution_and_refuses_to_guess() {
        let dir = TempDir::new("detect");
        assert!(entry(&dir.0).is_err());
        touch(&dir.0.join("App.csproj"));
        assert_eq!(entry(&dir.0), Ok(PathBuf::from("App.csproj")));
        touch(&dir.0.join("Tool.fsproj"));
        let error = entry(&dir.0).expect_err("two projects");
        assert!(error.contains("several projects"), "{error}");
        touch(&dir.0.join("All.sln"));
        assert_eq!(entry(&dir.0), Ok(PathBuf::from("All.sln")));
        touch(&dir.0.join("Other.slnx"));
        assert!(
            entry(&dir.0)
                .expect_err("two solutions")
                .contains("several solutions")
        );
    }

    #[test]
    fn deliverable_is_named_after_the_entry() {
        assert_eq!(app_name(Path::new("Hello.csproj")), "Hello");
        assert_eq!(app_name(Path::new("Shop.sln")), "Shop");
    }

    #[test]
    fn unknown_configurations_fail_closed() {
        let dir = TempDir::new("options");
        touch(&dir.0.join("App.csproj"));
        let mut options = AdapterOptions::new();
        options.insert("configuration", "Profile").expect("option");
        let error = DotnetAdapter
            .generate(&dir.0, &options)
            .expect_err("unsupported configuration");
        assert!(
            matches!(error, AdapterError::Option(_)),
            "fails on the option before looking for dotnet: {error}"
        );
    }

    /// A real publish needs the .NET SDK; skips honestly without one (the
    /// `verify-adapters` workflow runs it on Linux and Windows).
    #[test]
    fn generates_a_publish_action_when_dotnet_is_installed() {
        if locate_executable(&format!("dotnet{}", std::env::consts::EXE_SUFFIX)).is_err() {
            eprintln!("skipping: no dotnet on PATH");
            return;
        }
        let dir = TempDir::new("generate");
        touch(&dir.0.join("Hello.csproj"));
        std::fs::create_dir_all(dir.0.join("obj")).expect("obj");
        std::fs::write(dir.0.join("obj/project.assets.json"), "{}").expect("obj file");
        std::fs::write(dir.0.join("Program.cs"), "").expect("source");
        let plan = DotnetAdapter
            .generate(&dir.0, &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[1..4], ["publish", "Hello.csproj", "-c"]);
        assert_eq!(action.outputs[0].path, Path::new(".ant/dotnet/Hello"));
        assert!(
            action
                .inputs
                .iter()
                .any(|input| input.path == Path::new("Program.cs"))
        );
        assert!(
            !action
                .inputs
                .iter()
                .any(|input| input.path.starts_with("obj")),
            "MSBuild's obj/ is not an input"
        );
        assert_eq!(action.cache_policy, CachePolicy::Local);
    }
}
