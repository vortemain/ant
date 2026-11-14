//! The CMake adapter (decision D-010, T3.11).
//!
//! It generates two actions for a CMake project rooted at `CMakeLists.txt`:
//! `configure` (`cmake -S . -B build -DCMAKE_BUILD_TYPE=<config>`) and
//! `build` (`cmake --build build --config <config>`, depending on
//! `configure`). Generation is inspection-only apart from the staging
//! directory: it never configures the project.
//!
//! Honest v1 boundaries:
//! - The generator is CMake's own default for the machine (the executor
//!   clears `CMAKE_GENERATOR` along with the rest of the environment); on
//!   Windows with Visual Studio installed that is the newest Visual Studio
//!   generator. `--option config=Debug|Release` selects the configuration
//!   for both multi-config generators (`--config`) and single-config ones
//!   (`CMAKE_BUILD_TYPE`).
//! - Inputs declared: every `CMakeLists.txt` and `*.cmake` file in the
//!   project (skipping the `build` directory, hidden directories,
//!   `node_modules`, and any directory holding a `CMakeCache.txt`, i.e. other
//!   build trees). Source files are **not** declared: which files a target
//!   compiles is only known after configuring (CMake's File API), which
//!   generation deliberately does not do. Actions therefore use
//!   `cache_policy: never` (D-003, D-010).
//! - Outputs declared: only `build/CMakeCache.txt` for `configure` (CMake
//!   always writes it). Build artifacts are not declared: their location
//!   depends on the generator (multi-config `build/<Config>/` versus
//!   single-config `build/`) and on target properties
//!   (`RUNTIME_OUTPUT_DIRECTORY`, `OUTPUT_NAME`, ...) that only a configured
//!   code model reports. Unknown layouts are omitted, not guessed.
//! - Toolchain identity is the resolved PATH `cmake` executable's SHA-256
//!   digest and the first line of `cmake --version`. The compiler CMake
//!   selects is not captured (another reason policy stays `never`).
//! - Not runnable under `ant run --sandbox` with the Visual Studio generator
//!   on Windows: MSBuild writes `%LOCALAPPDATA%\Temp\MSBuildTemp` regardless
//!   of `TMP`/`TEMP`, which confinement denies (see `docs/cli/adapters.md`).

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::path::{Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};

pub struct CmakeAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "config",
    values: "Debug|Release",
    description: "CMake configuration (CMAKE_BUILD_TYPE for single-config generators, --config for multi-config)",
}];

/// The build directory generated actions configure into.
const BUILD_DIR: &str = "build";

fn config(options: &AdapterOptions) -> Result<&'static str, AdapterError> {
    match options.get("config") {
        None | Some("Debug") => Ok("Debug"),
        Some("Release") => Ok("Release"),
        Some(other) => Err(AdapterError::Option(format!(
            "cmake config '{other}' is not supported (supported: Debug, Release)"
        ))),
    }
}

impl BuildAdapter for CmakeAdapter {
    fn name(&self) -> &'static str {
        "cmake"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root.join("CMakeLists.txt").is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no CMakeLists.txt at {}",
                project_root.display()
            )))
        }
    }

    /// CMake's built targets are only known after configuring (File API is
    /// future work); the one declared output, `CMakeCache.txt`, is not a
    /// deliverable. No artifacts are claimed.
    fn artifacts(&self, _plan: &PlanFile) -> Vec<PathBuf> {
        Vec::new()
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let config = config(options)?;
        let root = canonical_root(project_root)?;
        if !root.join("CMakeLists.txt").is_file() {
            return Err(AdapterError::Project(format!(
                "no CMakeLists.txt at {}",
                root.display()
            )));
        }
        let build_dir = root.join(BUILD_DIR);
        if build_dir.exists() && !build_dir.is_dir() {
            return Err(AdapterError::Project(format!(
                "{} exists and is not a directory; the cmake adapter configures into it",
                build_dir.display()
            )));
        }
        let mut cmake_files = Vec::new();
        collect_cmake_files(&root, Path::new(""), &mut cmake_files)?;
        cmake_files.sort();
        let inputs: Vec<Input> = cmake_files
            .into_iter()
            .map(|path| Input {
                path,
                kind: InputKind::File,
            })
            .collect();

        let (cmake, program) = locate_cmake()?;
        let digest_target = cmake.canonicalize().map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot resolve cmake executable {}: {error}",
                cmake.display()
            ))
        })?;
        let version = executable_version(&cmake, &root)?
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let executable_digest = crate::toolchain_digest(&digest_target)
            .map_err(|error| {
                AdapterError::Toolchain(format!(
                    "cannot digest cmake executable {}: {error}",
                    digest_target.display()
                ))
            })?
            .to_string();
        let toolchain = ToolchainIdentity {
            name: "cmake".to_owned(),
            version,
            executable_digest,
        };

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

        let action = |id: &str, argv: Vec<String>, outputs: Vec<Output>| Action {
            schema_version: 1,
            id: id.to_owned(),
            argv,
            working_directory: PathBuf::from("."),
            inputs: inputs.clone(),
            outputs,
            environment: environment.clone(),
            // Source files are undeclared (see module docs).
            cache_policy: CachePolicy::Never,
            toolchain: Some(toolchain.clone()),
            timeout_ms: None,
            resources: None,
        };
        let strings = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>()
        };
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action: action(
                "configure",
                strings(&[
                    &program,
                    "-S",
                    ".",
                    "-B",
                    BUILD_DIR,
                    &format!("-DCMAKE_BUILD_TYPE={config}"),
                ]),
                vec![Output {
                    path: PathBuf::from(BUILD_DIR).join("CMakeCache.txt"),
                    kind: OutputKind::File,
                }],
            ),
            dependencies: Vec::new(),
        });
        file.actions.push(PlanAction {
            action: action(
                "build",
                strings(&[&program, "--build", BUILD_DIR, "--config", config]),
                Vec::new(),
            ),
            dependencies: vec!["configure".to_owned()],
        });
        file.to_graph()?;
        // Prepare the staging directory (crate contract exception) last.
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Collect workspace-relative `CMakeLists.txt` and `*.cmake` files under
/// `root/relative`, skipping build trees, hidden directories, and
/// `node_modules`. Symlinks are not followed.
fn collect_cmake_files(
    root: &Path,
    relative: &Path,
    found: &mut Vec<PathBuf>,
) -> Result<(), AdapterError> {
    let dir = root.join(relative);
    if !relative.as_os_str().is_empty() && dir.join("CMakeCache.txt").is_file() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = relative.join(name);
        if file_type.is_dir() {
            let skipped = name.starts_with('.')
                || name == "node_modules"
                || (relative.as_os_str().is_empty() && name == BUILD_DIR);
            if !skipped {
                collect_cmake_files(root, &path, found)?;
            }
        } else if file_type.is_file() && (name == "CMakeLists.txt" || name.ends_with(".cmake")) {
            found.push(path);
        }
    }
    Ok(())
}

/// Find `cmake`: PATH first (the plan then calls plain `cmake`, exactly as
/// before); otherwise, on Windows, the copy Visual Studio bundles, located
/// through `vswhere` — the plan then records that absolute path so the build
/// runs without the user editing PATH. Returns the executable and the argv[0]
/// to put in the plan.
fn locate_cmake() -> Result<(PathBuf, String), AdapterError> {
    let name = format!("cmake{}", std::env::consts::EXE_SUFFIX);
    match locate_executable(&name) {
        Ok(path) => Ok((path, "cmake".to_owned())),
        Err(path_error) => match visual_studio_cmake() {
            Some(path) => {
                let program = path.to_string_lossy().into_owned();
                Ok((path, program))
            }
            None => Err(path_error),
        },
    }
}

fn visual_studio_cmake() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let vswhere = PathBuf::from(std::env::var_os("ProgramFiles(x86)")?)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    let output = std::process::Command::new(vswhere)
        .args(["-latest", "-products", "*", "-property", "installationPath"])
        .output()
        .ok()?;
    let install = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if install.is_empty() {
        return None;
    }
    let candidate = Path::new(&install)
        .join("Common7")
        .join("IDE")
        .join("CommonExtensions")
        .join("Microsoft")
        .join("CMake")
        .join("CMake")
        .join("bin")
        .join("cmake.exe");
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ant-cmake-adapter-{name}-{}-{unique}",
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

    #[test]
    fn detect_requires_a_top_level_cmakelists() {
        let dir = TempDir::new("detect");
        assert!(!CmakeAdapter.detect(dir.path()).expect("detect").supported);
        write(&dir.path().join("CMakeLists.txt"), "project(x C)\n");
        assert!(CmakeAdapter.detect(dir.path()).expect("detect").supported);
    }

    #[test]
    fn cmake_files_are_collected_outside_build_trees() {
        let dir = TempDir::new("collect");
        write(&dir.path().join("CMakeLists.txt"), "\n");
        write(&dir.path().join("lib/CMakeLists.txt"), "\n");
        write(&dir.path().join("cmake/Helpers.cmake"), "\n");
        write(&dir.path().join("lib/main.c"), "\n");
        write(&dir.path().join("build/CMakeLists.txt"), "\n");
        write(&dir.path().join(".git/x.cmake"), "\n");
        write(&dir.path().join("node_modules/y.cmake"), "\n");
        write(&dir.path().join("out/CMakeCache.txt"), "\n");
        write(&dir.path().join("out/z.cmake"), "\n");
        let mut found = Vec::new();
        collect_cmake_files(dir.path(), Path::new(""), &mut found).expect("collect");
        found.sort();
        assert_eq!(
            found,
            vec![
                PathBuf::from("CMakeLists.txt"),
                PathBuf::from("cmake").join("Helpers.cmake"),
                PathBuf::from("lib").join("CMakeLists.txt"),
            ]
        );
    }

    #[test]
    fn options_and_project_shape_are_validated_before_side_effects() {
        let dir = TempDir::new("invalid");
        write(&dir.path().join("CMakeLists.txt"), "\n");
        let mut options = AdapterOptions::new();
        options.insert("config", "RelWithDebInfo").expect("insert");
        let error = CmakeAdapter
            .generate(dir.path(), &options)
            .expect_err("bad config");
        assert!(
            error.to_string().contains("supported: Debug, Release"),
            "{error}"
        );
        write(&dir.path().join("build"), "not a dir");
        let error = CmakeAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("build is a file");
        assert!(error.to_string().contains("is not a directory"), "{error}");
        assert!(
            !dir.path().join(".ant").exists(),
            "no staging directory on failure"
        );
    }

    #[test]
    fn cmake_actions_stay_path_dependent_without_the_marker() {
        // The configure action's declared output is CMakeCache.txt, which
        // embeds the absolute build path, so CMake must NOT claim path
        // independence: no PATH_INDEPENDENT_MARKER, keys keep the workspace
        // path (documented in docs/cli/adapters.md). Adding the marker here
        // would silently let two clones share results whose CMakeCache.txt
        // points at the other clone's absolute paths.
        let dir = TempDir::new("path-dep");
        write(&dir.path().join("CMakeLists.txt"), "\n");
        let plan = CmakeAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        assert!(plan.actions.len() >= 2, "configure and build actions");
        for entry in &plan.actions {
            assert!(
                entry
                    .action
                    .environment
                    .iter()
                    .all(|(key, _)| key != build_digest::PATH_INDEPENDENT_MARKER),
                "cmake actions must stay path-dependent: {}",
                entry.action.id
            );
        }
    }
}
