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
//! Outputs: for `assembleDebug`/`assembleRelease`, every Android application
//! module's APK at Android Gradle Plugin's default path
//! `<module>/build/outputs/apk/<debug|release>/<module>-<debug|release>.apk`;
//! for `bundleRelease`, its AAB at `<module>/build/outputs/bundle/release/
//! <module>-release.aab` (application modules are recognized by the
//! `android.application` plugin id in the build file; a project with product
//! flavors or renamed outputs fails loudly with "declared output was not
//! created"). Other tasks declare no outputs.
//!
//! Hermeticity and caching: Gradle downloads dependencies and its own
//! distribution from the network, which a sandboxed worker does not have.
//! A project is cache-eligible only when both are inside the project:
//! a Gradle read-only dependency cache at `.gradle-deps/` (Gradle's
//! `GRADLE_RO_DEP_CACHE` layout, i.e. a copy of `caches/modules-2`) and a
//! local wrapper distribution (`distributionUrl` not http/https). Then the
//! action may run in the no-network sandbox (not `--offline`, which would
//! disable Gradle's remote task cache, D-025). Gradle's writable home and Android's user home
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

const OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        key: "task",
        values: "assembleDebug|assembleRelease|bundleRelease",
        description: "Gradle task to run; assembleDebug/assembleRelease declare each application module's APK, bundleRelease its AAB, as outputs",
    },
    OptionSpec {
        key: "daemon",
        values: "0|1",
        description: "1 keeps the Gradle daemon between local builds (drops --no-daemon and marks the action daemon-keeping); 0 runs the hermetic default",
    },
    OptionSpec {
        key: "sign",
        values: "0|sha256:<hex>",
        description: "the registered keystore's content digest configures release signing through the Ant init script (local front door only); 0 (default) signs nothing",
    },
];

/// The in-project read-only dependency cache directory (Gradle
/// `GRADLE_RO_DEP_CACHE` layout).
pub const DEPENDENCY_CACHE_DIR: &str = ".gradle-deps";

// ---- Release signing (P4, D-036) -----------------------------------------
// The four environment names below are the whole build-time signing
// contract: the plan's action references the keystore only by content
// digest under `SIGNING_MARKER`, and the local front door injects the
// secrets as these variables into the spawned process only. They never
// appear in the plan file, the action key, events, or any cache.

/// Environment the plan declares, whose value is the keystore's digest.
pub const SIGNING_MARKER: &str = "ANT_ANDROID_SIGNING";
/// Build-time-only environment: path of the registered keystore.
pub const KEYSTORE_ENV: &str = "ANT_ANDROID_KEYSTORE";
/// Build-time-only environment: the keystore (store) password.
pub const STORE_PASSWORD_ENV: &str = "ANT_KEYSTORE_PASSWORD";
/// Build-time-only environment: the signing key alias.
pub const KEY_ALIAS_ENV: &str = "ANT_KEY_ALIAS";
/// Build-time-only environment: the key's own password.
pub const KEY_PASSWORD_ENV: &str = "ANT_KEY_PASSWORD";

/// Where Ant stages the signing init script (`.ant/` is never a build
/// input, and the script itself contains no secrets).
pub const SIGNING_INIT_REL: &str = ".ant/android-signing.gradle";

/// Which Gradle tasks are release tasks (they produce signed-eligible
/// artifacts and are the only ones that may carry signing).
pub fn release_task(task: &str) -> bool {
    matches!(task, "assembleRelease" | "bundleRelease")
}

/// Whether the project has at least one Android application module (the
/// front door uses this to validate the `ant build android` target).
pub fn is_android_project(root: &Path) -> bool {
    application_modules(root, Path::new(""))
        .map(|modules| !modules.is_empty())
        .unwrap_or(false)
}

/// The init script Ant passes to Gradle with `-I` when signing is active:
/// without the four variables (any non-signing run) it does nothing at all.
/// Static, no secrets; rewritten by every signed build.
pub const SIGNING_INIT_SCRIPT: &str = r#""// Generated by Ant: release signing from build-time environment only (D-036).
// The passwords live only in the build process's environment; nothing is
// written here, into the plan, or into any cache.
def antKeystore = System.getenv('ANT_ANDROID_KEYSTORE')
def antStorePass = System.getenv('ANT_KEYSTORE_PASSWORD')
def antAlias = System.getenv('ANT_KEY_ALIAS')
def antKeyPass = System.getenv('ANT_KEY_PASSWORD')
if (antKeystore != null && antStorePass != null && antAlias != null) {
    if (antKeyPass == null) {
        antKeyPass = antStorePass
    }
    allprojects { project ->
        project.afterEvaluate {
            def android = project.extensions.findByName('android')
            if (android != null && project.plugins.hasPlugin('com.android.application')) {
                android.signingConfigs {
                    antRelease {
                        storeFile project.file(antKeystore)
                        storePassword antStorePass
                        keyAlias antAlias
                        keyPassword antKeyPass
                    }
                }
                android.buildTypes.release.signingConfig android.signingConfigs.antRelease
            }
        }
    }
}
""#;

/// Write the signing init script into `.ant/` so the `-I` argument in the
/// plan's argv resolves. Called by the local front door only; the script is
/// inert without the four environment values it reads.
pub fn write_signing_init_script(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join(".ant"))?;
    std::fs::write(root.join(SIGNING_INIT_REL), SIGNING_INIT_SCRIPT)
}

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
            Some("bundleRelease") => "bundleRelease",
            Some(other) => {
                return Err(AdapterError::Option(format!(
                    "gradle task '{other}' is not supported (supported: assembleDebug, assembleRelease, bundleRelease)"
                )));
            }
        };
        // The daemon option trades hermetic one-shot Gradle processes for a
        // warm daemon that survives the action. It is off by default so
        // `ant adapt` plans (remote and sandbox runs) stay hermetic; the
        // local front door turns it on and `ant.toml` can turn it back off.
        let daemon = match options.get("daemon") {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => {
                return Err(AdapterError::Option(format!(
                    "gradle daemon '{other}' is not supported (supported: 0, 1)"
                )));
            }
        };
        // Signing is a local front-door feature (D-036): the secrets it
        // injects exist only in the spawned process, so remote and sandbox
        // runs never see it. It applies to release tasks only. The option
        // value is the registered keystore's digest, which lands verbatim in
        // the action environment: a re-registered keystore can never reuse
        // a cache entry signed by the old one.
        let sign_digest = match options.get("sign") {
            None | Some("0") => None,
            Some("1") => {
                return Err(AdapterError::Option(
                    "gradle sign needs the keystore's sha256 digest (the front door sets it), not '1'"
                        .to_owned(),
                ));
            }
            Some(value)
                if value.len() == 71
                    && value.starts_with("sha256:")
                    && value[7..].chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                Some(value.to_owned())
            }
            Some(other) => {
                return Err(AdapterError::Option(format!(
                    "gradle sign '{other}' is not supported (0 or the keystore's sha256 digest)"
                )));
            }
        };
        if sign_digest.is_some() && !release_task(task) {
            return Err(AdapterError::Option(format!(
                "gradle sign needs a release task (assembleRelease or bundleRelease), not '{task}'"
            )));
        }
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
        for module in application_modules(&root, Path::new(""))? {
            let name = module
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    root.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            let (kind_dir, extension) = if task == "assembleDebug" {
                ("debug", "apk")
            } else if task == "assembleRelease" {
                ("release", "apk")
            } else {
                ("release", "aab")
            };
            outputs.push(
                module
                    .join("build")
                    .join("outputs")
                    .join(if extension == "apk" { "apk" } else { "bundle" })
                    .join(kind_dir)
                    .join(format!("{name}-{kind_dir}.{extension}")),
            );
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
        if daemon {
            // Marks the action daemon-keeping for executors (see
            // `build_digest::DAEMON_MARKER`); hashed into the action key as
            // part of the environment, so daemon and hermetic variants never
            // share a cache entry.
            environment.push((build_digest::DAEMON_MARKER.to_owned(), "1".to_owned()));
        }
        if let Some(digest) = &sign_digest {
            // The reference a signed build used — by digest, never by secret.
            environment.push((SIGNING_MARKER.to_owned(), digest.clone()));
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
        if !daemon {
            argv.push("--no-daemon".to_owned());
        }
        argv.push("--console=plain".to_owned());
        if fully_declared {
            // Not `--offline`: Gradle disables its remote build cache in
            // offline mode, and the sandbox already has no network, so a
            // missing dependency fails the build either way (D-025).
            // No Kotlin daemon: it would write under the (read-only) user
            // home and outlive the action.
            argv.push("-Pkotlin.compiler.execution.strategy=in-process".to_owned());
            // Task outputs may come from (and go to) the shared Gradle
            // task cache when a worker serves it (D-025).
            argv.push("--build-cache".to_owned());
        }
        if sign_digest.is_some() {
            // The init script is written by the local front door (it is inert
            // without the secrets, which remote and sandbox runs never have).
            argv.push("-I".to_owned());
            argv.push(SIGNING_INIT_REL.to_owned());
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
            // Local reuse otherwise (D-028): dependencies resolve from the
            // versions in the build files, so this machine may reuse its own
            // result; it is never shared.
            cache_policy: if fully_declared {
                CachePolicy::Eligible
            } else if has_symlink {
                CachePolicy::Never
            } else {
                CachePolicy::Local
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
    fn online_projects_are_reused_only_locally_and_declare_the_debug_apk() {
        let dir = android_project(true);
        let plan = GradleAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &plan.actions[0].action;
        assert_eq!(action.id, "assembleDebug");
        assert_eq!(action.cache_policy, CachePolicy::Local);
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
    fn daemon_option_drops_no_daemon_and_marks_the_action() {
        let dir = android_project(true);
        let mut options = AdapterOptions::new();
        options.insert("daemon", "1").expect("option");
        let plan = GradleAdapter
            .generate(dir.path(), &options)
            .expect("generate");
        let action = &plan.actions[0].action;
        assert!(
            !action.argv.contains(&"--no-daemon".to_owned()),
            "the daemon variant must let Gradle start or reuse its daemon"
        );
        assert!(
            action
                .environment
                .contains(&(build_digest::DAEMON_MARKER.to_owned(), "1".to_owned()))
        );
    }

    #[test]
    fn release_signing_marks_the_action_and_uses_the_init_script() {
        // P4 (D-036): a signed release build references the keystore by
        // digest in the environment (never a secret), stages the init
        // script with -I, and declares the release APK output.
        let dir = android_project(true);
        let digest = format!("sha256:{}", "a".repeat(64));
        let mut options = AdapterOptions::new();
        options.insert("task", "assembleRelease").expect("option");
        options.insert("sign", digest.as_str()).expect("option");
        let plan = GradleAdapter
            .generate(dir.path(), &options)
            .expect("generate");
        let action = &plan.actions[0].action;
        assert_eq!(
            action.id, "assembleRelease",
            "the release task is the action identity"
        );
        assert!(
            action
                .environment
                .contains(&(SIGNING_MARKER.to_owned(), digest.clone())),
            "the keystore reference must travel by digest"
        );
        let init = action
            .argv
            .iter()
            .position(|arg| arg == SIGNING_INIT_REL)
            .expect("the init script must be staged with -I");
        assert_eq!(action.argv[init - 1], "-I");
        assert_eq!(
            action.outputs[0].path,
            PathBuf::from("app/build/outputs/apk/release/app-release.apk")
        );
        assert!(
            action.environment.iter().all(|(key, _)| {
                key != STORE_PASSWORD_ENV && key != KEY_PASSWORD_ENV && key != KEYSTORE_ENV
            }),
            "no secret or path may appear in the plan, only the digest marker"
        );
    }

    #[test]
    fn signing_is_refused_without_a_release_task_or_a_real_digest() {
        let dir = android_project(true);
        let digest = format!("sha256:{}", "a".repeat(64));
        let mut options = AdapterOptions::new();
        options.insert("sign", digest.as_str()).expect("option");
        let err = GradleAdapter.generate(dir.path(), &options).unwrap_err();
        assert!(
            err.to_string().contains("release task"),
            "signing must not attach to the default debug build: {err}"
        );
        let mut options = AdapterOptions::new();
        options.insert("task", "assembleRelease").expect("option");
        options.insert("sign", "1").expect("option");
        let err = GradleAdapter.generate(dir.path(), &options).unwrap_err();
        assert!(
            err.to_string().contains("digest"),
            "the sign option needs the digest, not a flag: {err}"
        );
    }

    #[test]
    fn daemon_defaults_to_hermetic_and_validates_its_value() {
        let dir = android_project(true);
        let plan = GradleAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &plan.actions[0].action;
        assert!(action.argv.contains(&"--no-daemon".to_owned()));
        assert!(
            !action
                .environment
                .iter()
                .any(|(key, _)| key == build_digest::DAEMON_MARKER),
            "the hermetic default must not carry the marker"
        );

        let mut off = AdapterOptions::new();
        off.insert("daemon", "0").expect("option");
        let plan = GradleAdapter.generate(dir.path(), &off).expect("generate");
        assert!(
            plan.actions[0]
                .action
                .argv
                .contains(&"--no-daemon".to_owned())
        );

        let mut bad = AdapterOptions::new();
        bad.insert("daemon", "yes").expect("option");
        assert!(
            GradleAdapter.generate(dir.path(), &bad).is_err(),
            "only 0 and 1 are valid daemon values"
        );
    }

    #[test]
    fn vendored_projects_are_cacheable_and_keys_ignore_build_dirs() {
        let dir = android_project(false);
        std::fs::create_dir_all(dir.path().join(DEPENDENCY_CACHE_DIR).join("modules-2"))
            .expect("deps");
        let before = GradleAdapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &before.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Eligible);
        assert!(
            !action.argv.iter().any(|arg| arg == "--offline"),
            "offline disables the task cache"
        );
        assert!(action.argv.iter().any(|arg| arg == "--build-cache"));
        assert!(
            action
                .environment
                .contains(&("ANT_GRADLE_TASK_CACHE".to_owned(), "1".to_owned()))
        );
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
