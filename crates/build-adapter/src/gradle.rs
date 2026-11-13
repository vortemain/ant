//! Gradle / Android adapter v1 (roadmap P4, decision D-024).
//!
//! Generates one `./gradlew <task>` action for the project root (default
//! `assembleDebug`). Declared inputs are the whole project except Gradle's
//! outputs and caches, walked so the *shape* of the input list depends only
//! on source-controlled files: every Gradle module directory (one holding a
//! `build.gradle[.kts]`) is listed child by child without its `build/`
//! directory, directories that contain modules deeper down are descended,
//! and everything else is a single tree input. `.gradle/`, `.kotlin/`,
//! `.idea/`, `.cxx/`, `.ant/`, and `.git/` are never inputs. That keeps keys
//! identical whether or not a previous build left `build/` directories.
//!
//! Outputs: for `assembleDebug`, every Android application module's debug
//! APK at Android Gradle Plugin's default path
//! `<module>/build/outputs/apk/debug/<module>-debug.apk` (application modules
//! are recognized by the `android.application` plugin id in the build file;
//! a project with product flavors or renamed outputs fails loudly with
//! "declared output was not created"). Other tasks declare no outputs.
//!
//! Hermeticity and caching: Gradle downloads dependencies and its own
//! distribution from the network, which a sandboxed worker does not have.
//! A project is cache-eligible only when both are inside the project:
//! a Gradle read-only dependency cache at `.gradle-deps/` (Gradle's
//! `GRADLE_RO_DEP_CACHE` layout, i.e. a copy of `caches/modules-2`) and a
//! local wrapper distribution (`distributionUrl` not http/https). Then the
//! action runs `--offline`. Gradle's writable home and Android's user home
//! are redirected into the workspace staging area (`.ant/`), so nothing is
//! written outside the workspace. Known gap: the JDK and Android SDK are
//! host installations identified only through the environment allowlist
//! (`JAVA_HOME`, `ANDROID_HOME`, `ANDROID_SDK_ROOT` values), not by digest.

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};
use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct GradleAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "task",
    values: "assembleDebug|assembleRelease",
    description: "Gradle task to run; assembleDebug declares each application module's debug APK as an output",
}];

/// The in-project read-only dependency cache directory (Gradle
/// `GRADLE_RO_DEP_CACHE` layout).
pub const DEPENDENCY_CACHE_DIR: &str = ".gradle-deps";

/// Directory names that are never inputs wherever they appear.
const ALWAYS_EXCLUDED: &[&str] = &[".gradle", ".kotlin", ".idea", ".cxx", ".ant", ".git"];

/// Unix launcher: absolutize the workspace-relative homes, then run the
/// wrapper with the remaining arguments.
const UNIX_LAUNCHER: &str = r#"for v in GRADLE_USER_HOME ANDROID_USER_HOME GRADLE_RO_DEP_CACHE TMPDIR TMP TEMP; do eval "x=\${$v:-}"; case "$x" in ""|/*) ;; *) export "$v=$PWD/$x" ;; esac; done; exec ./gradlew "$@""#;

fn build_file(dir: &Path) -> Option<PathBuf> {
    ["build.gradle.kts", "build.gradle"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

fn settings_file(dir: &Path) -> Option<PathBuf> {
    ["settings.gradle.kts", "settings.gradle"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

fn wrapper_script() -> &'static str {
    if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    }
}

impl BuildAdapter for GradleAdapter {
    fn name(&self) -> &'static str {
        "gradle"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if settings_file(project_root).is_none() {
            return Ok(Detection::unsupported(format!(
                "no settings.gradle(.kts) at {}",
                project_root.display()
            )));
        }
        if !project_root.join(wrapper_script()).is_file() {
            return Ok(Detection::unsupported(format!(
                "no {} at {} (the Gradle wrapper is required)",
                wrapper_script(),
                project_root.display()
            )));
        }
        Ok(Detection::supported())
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let task = match options.get("task") {
            None | Some("assembleDebug") => "assembleDebug",
            Some("assembleRelease") => "assembleRelease",
            Some(other) => {
                return Err(AdapterError::Option(format!(
                    "gradle task '{other}' is not supported (supported: assembleDebug, assembleRelease)"
                )));
            }
        };
        let root = canonical_root(project_root)?;
        if settings_file(&root).is_none() || !root.join(wrapper_script()).is_file() {
            return Err(AdapterError::Project(
                "not a Gradle wrapper project (settings.gradle(.kts) and the wrapper script are required)"
                    .to_owned(),
            ));
        }

        // Toolchain identity: the wrapper (its jar pins the bootstrap) plus
        // the distribution it selects, and the JDK on PATH.
        let wrapper_props = root
            .join("gradle")
            .join("wrapper")
            .join("gradle-wrapper.properties");
        let distribution = std::fs::read_to_string(&wrapper_props)
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.trim().strip_prefix("distributionUrl="))
                    .map(|url| url.replace("\\:", ":").trim().to_owned())
            })
            .ok_or_else(|| {
                AdapterError::Project(format!(
                    "{} has no distributionUrl",
                    wrapper_props.display()
                ))
            })?;
        let local_distribution =
            !(distribution.starts_with("http://") || distribution.starts_with("https://"));
        let java = locate_executable(&format!("java{}", std::env::consts::EXE_SUFFIX))?;
        let java_version = executable_version(&java, &root)?;
        let wrapper_jar = root
            .join("gradle")
            .join("wrapper")
            .join("gradle-wrapper.jar");
        let identity_file = if wrapper_jar.is_file() {
            wrapper_jar
        } else {
            root.join(wrapper_script())
        };
        let executable_digest = crate::toolchain_digest(&identity_file)
            .map_err(|error| {
                AdapterError::Toolchain(format!("cannot digest the Gradle wrapper: {error}"))
            })?
            .to_string();
        let toolchain = ToolchainIdentity {
            name: "gradle-wrapper".to_owned(),
            version: format!(
                "distribution {distribution}; {}",
                java_version.lines().next().unwrap_or_default()
            ),
            executable_digest,
        };

        let dependency_cache = root.join(DEPENDENCY_CACHE_DIR).is_dir();
        let fully_declared = dependency_cache && local_distribution;

        let mut inputs: BTreeMap<PathBuf, InputKind> = BTreeMap::new();
        let mut has_symlink = false;
        declare_gradle_project(&root, Path::new(""), &mut inputs, &mut has_symlink)?;
        let fully_declared = fully_declared && !has_symlink;

        let mut outputs: Vec<PathBuf> = Vec::new();
        if task == "assembleDebug" {
            for module in application_modules(&root, Path::new(""))? {
                let name = module
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| {
                        root.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    });
                outputs.push(
                    module
                        .join("build")
                        .join("outputs")
                        .join("apk")
                        .join("debug")
                        .join(format!("{name}-debug.apk")),
                );
            }
        }

        std::fs::create_dir_all(root.join(".ant").join("tmp")).map_err(AdapterError::from)?;
        let staging = PathBuf::from(".ant");
        let rel = |path: PathBuf| path.to_string_lossy().into_owned();
        let mut environment = vec![
            ("TMP".to_owned(), rel(staging.join("tmp"))),
            ("TEMP".to_owned(), rel(staging.join("tmp"))),
            // Gradle's and Android's writable homes stay inside the
            // workspace (never inputs: `.ant/` is excluded).
            (
                "GRADLE_USER_HOME".to_owned(),
                rel(staging.join("gradle-home")),
            ),
            (
                "ANDROID_USER_HOME".to_owned(),
                rel(staging.join("android-home")),
            ),
        ];
        if cfg!(not(windows)) {
            environment.push(("TMPDIR".to_owned(), rel(staging.join("tmp"))));
        }
        if dependency_cache {
            environment.push((
                "GRADLE_RO_DEP_CACHE".to_owned(),
                DEPENDENCY_CACHE_DIR.to_owned(),
            ));
        }
        if fully_declared {
            environment.push(("ANT_GRADLE_TASK_CACHE".to_owned(), "1".to_owned()));
        }
        environment.sort();

        let mut argv: Vec<String> = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "gradlew.bat".into()]
        } else {
            // Gradle and Android tools want absolute homes; the declared
            // values are workspace-relative so keys stay path-independent.
            // `sh` makes them absolute against the working directory.
            vec![
                "sh".into(),
                "-c".into(),
                UNIX_LAUNCHER.into(),
                "gradlew".into(),
            ]
        };
        argv.extend(["--no-daemon".to_owned(), "--console=plain".to_owned()]);
        if fully_declared {
            argv.push("--offline".to_owned());
            // No Kotlin daemon: it would write under the (read-only) user
            // home and outlive the action.
            argv.push("-Pkotlin.compiler.execution.strategy=in-process".to_owned());
            // Task outputs may come from (and go to) the shared Gradle
            // task cache when a worker serves it (D-025).
            argv.push("--build-cache".to_owned());
        }
        argv.push(task.to_owned());

        let action = Action {
            schema_version: 1,
            id: task.to_owned(),
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
            // Eligible only when dependencies and the distribution are
            // declared inputs (D-024); otherwise the network supplies
            // undeclared content.
            cache_policy: if fully_declared {
                CachePolicy::Eligible
            } else {
                CachePolicy::Never
            },
            toolchain: Some(toolchain),
            timeout_ms: None,
            resources: None,
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

/// Whether `dir` (absolute) contains a Gradle build file anywhere below it.
fn contains_module(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ALWAYS_EXCLUDED.contains(&name.as_ref()) {
            return false;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => build_file(&path).is_some() || contains_module(&path),
            _ => false,
        }
    })
}

/// Declare the project's inputs under `dir_rel` (see the module docs).
fn declare_gradle_project(
    root: &Path,
    dir_rel: &Path,
    inputs: &mut BTreeMap<PathBuf, InputKind>,
    has_symlink: &mut bool,
) -> Result<(), AdapterError> {
    let dir_abs = root.join(dir_rel);
    let is_module = build_file(&dir_abs).is_some() || dir_rel.as_os_str().is_empty();
    let mut children: Vec<_> = std::fs::read_dir(&dir_abs)
        .map_err(AdapterError::from)?
        .collect::<Result<_, _>>()
        .map_err(AdapterError::from)?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child.file_name();
        let name = name.to_string_lossy();
        if ALWAYS_EXCLUDED.contains(&name.as_ref()) || (is_module && name == "build") {
            continue;
        }
        let rel = dir_rel.join(child.file_name());
        let kind = child.file_type().map_err(AdapterError::from)?;
        if kind.is_symlink() {
            *has_symlink = true;
            continue;
        }
        if kind.is_dir() {
            let abs = root.join(&rel);
            if abs.join(".ant-artifacts").is_file() {
                continue;
            }
            if build_file(&abs).is_some() || contains_module(&abs) {
                declare_gradle_project(root, &rel, inputs, has_symlink)?;
            } else {
                inputs.entry(rel).or_insert(InputKind::Tree);
            }
        } else {
            inputs.entry(rel).or_insert(InputKind::File);
        }
    }
    Ok(())
}

/// Workspace-relative directories of Android application modules (build
/// file applies the `android.application` plugin, by id or version-catalog
/// alias).
fn application_modules(root: &Path, dir_rel: &Path) -> Result<Vec<PathBuf>, AdapterError> {
    let mut modules = Vec::new();
    let dir_abs = root.join(dir_rel);
    if let Some(file) = build_file(&dir_abs) {
        let text = std::fs::read_to_string(&file).map_err(AdapterError::from)?;
        if text.contains("android.application") && !dir_rel.as_os_str().is_empty() {
            modules.push(dir_rel.to_path_buf());
        }
    }
    let mut children: Vec<_> = std::fs::read_dir(&dir_abs)
        .map_err(AdapterError::from)?
        .flatten()
        .collect();
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child.file_name();
        let name = name.to_string_lossy();
        if ALWAYS_EXCLUDED.contains(&name.as_ref()) || name == "build" {
            continue;
        }
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            let rel = dir_rel.join(child.file_name());
            let abs = root.join(&rel);
            if build_file(&abs).is_some() || contains_module(&abs) {
                modules.extend(application_modules(root, &rel)?);
            }
        }
    }
    Ok(modules)
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
                .join(format!("ant-gradle-{name}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp");
            TempDir(path)
        }
        fn path(&self) -> &Path {
            &self.0
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

    /// A JetNews-shaped project: root build, one application module.
    fn android_project(online: bool) -> TempDir {
        let dir = TempDir::new("android");
        let root = dir.path();
        write(
            &root.join("settings.gradle.kts"),
            "rootProject.name = \"Demo\"\ninclude(\":app\")\n",
        );
        write(&root.join("build.gradle.kts"), "plugins { }\n");
        write(&root.join(wrapper_script()), "#!/bin/sh\n");
        write(&root.join("gradle/wrapper/gradle-wrapper.jar"), "jar");
        let url = if online {
            "https\\://services.gradle.org/distributions/gradle-9.5.0-bin.zip"
        } else {
            "gradle-9.5.0-bin.zip"
        };
        write(
            &root.join("gradle/wrapper/gradle-wrapper.properties"),
            &format!("distributionUrl={url}\n"),
        );
        write(
            &root.join("app/build.gradle.kts"),
            "plugins { alias(libs.plugins.android.application) }\n",
        );
        write(
            &root.join("app/src/main/AndroidManifest.xml"),
            "<manifest/>\n",
        );
        dir
    }

    #[test]
    fn detects_wrapper_projects_only() {
        let dir = android_project(true);
        assert!(GradleAdapter.detect(dir.path()).expect("detect").supported);
        let other = TempDir::new("plain");
        write(&other.path().join("build.gradle"), "");
        assert!(
            !GradleAdapter
                .detect(other.path())
                .expect("detect")
                .supported
        );
    }

    #[test]
    fn online_projects_are_never_cached_and_declare_the_debug_apk() {
        let dir = android_project(true);
        let plan = GradleAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &plan.actions[0].action;
        assert_eq!(action.id, "assembleDebug");
        assert_eq!(action.cache_policy, CachePolicy::Never);
        assert!(!action.argv.iter().any(|arg| arg == "--offline"));
        assert_eq!(
            action.outputs[0].path,
            PathBuf::from("app/build/outputs/apk/debug/app-debug.apk")
        );
        assert!(
            action
                .toolchain
                .as_ref()
                .expect("toolchain")
                .version
                .contains("gradle-9.5.0")
        );
    }

    #[test]
    fn vendored_projects_are_cacheable_offline_and_keys_ignore_build_dirs() {
        let dir = android_project(false);
        std::fs::create_dir_all(dir.path().join(DEPENDENCY_CACHE_DIR).join("modules-2"))
            .expect("deps");
        let before = GradleAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &before.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Eligible);
        assert!(action.argv.iter().any(|arg| arg == "--offline"));
        assert!(action.environment.contains(&(
            "GRADLE_RO_DEP_CACHE".to_owned(),
            DEPENDENCY_CACHE_DIR.to_owned()
        )));
        // Simulate a previous build: outputs and caches appear.
        write(&dir.path().join("app/build/intermediates/x"), "x");
        write(&dir.path().join("build/reports/y"), "y");
        write(&dir.path().join(".gradle/8.0/z"), "z");
        let after = GradleAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        assert_eq!(
            before.actions[0].action.inputs, after.actions[0].action.inputs,
            "build outputs must not change the declared inputs"
        );
        let paths: Vec<_> = action.inputs.iter().map(|i| i.path.clone()).collect();
        assert!(paths.contains(&PathBuf::from("app/src")), "{paths:?}");
        assert!(
            paths.contains(&PathBuf::from(DEPENDENCY_CACHE_DIR)),
            "{paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("app/build")),
            "{paths:?}"
        );
    }
}
