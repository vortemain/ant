//! The Flutter adapter (roadmap P6): builds a Flutter app for the `web` target.
//!
//! A Flutter project is a `pubspec.yaml` whose dependencies reference the
//! Flutter SDK (`sdk: flutter`). A plain Dart package (CLI, server) has no such
//! reference and is not recognized: Ant does not yet build non-Flutter Dart.
//!
//! The adapter generates one action that runs `flutter build <target>
//! --<mode>`, declaring the workspace inputs it can honestly cover and the
//! deterministic web output files.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Only the `web` target is supported in this phase. Its output layout
//!   (`build/web/index.html`, `build/web/main.dart.js`) is deterministic and
//!   portable, so the declared outputs are real. Other targets (`apk`, `ios`,
//!   desktop) have platform-SDK-dependent layouts and arrive in a later phase;
//!   `--option target=` anything but `web` fails closed.
//! - Flutter resolves packages from the pub registry into the pub cache
//!   (`~/.pub-cache` / `PUB_CACHE`) and runs on the bundled Dart VM, neither of
//!   which is workspace source. Those are undeclared inputs, so generated
//!   actions are `cache_policy: Local` (D-028): this machine may reuse its own
//!   result (pubspec pins versions locally), but it is never shared — the same
//!   stance the Gradle adapter takes when its dependencies are not locally
//!   declared.
//! - Toolchain identity is captured from the real Flutter SDK: `flutter
//!   --version` for the version, and the SHA-256 digest of the bundled Dart
//!   executable the build actually runs (`<flutter_bin>/cache/dart-sdk/bin/
//!   dart<EXE_SUFFIX>`). `flutter` itself is a launcher script
//!   (`flutter.bat` on Windows), not a standalone executable; the bundled Dart
//!   VM is the truthful runtime identity, the way `node` is for npm.
//! - Windows invocation: `flutter` is `flutter.bat` and `CreateProcess` cannot
//!   launch batch scripts directly, so the argv is `cmd /C flutter build ...`
//!   on Windows and `flutter build ...` elsewhere. The executor inherits `PATH`
//!   (and `PATHEXT` on Windows), so `cmd /C flutter` resolves the launcher the
//!   same way a shell would.
//! - Workspace-relative scratch: the action points `TMP`/`TEMP` (plus `TMPDIR`
//!   off Windows) at `.ant/tmp`, the staging pattern shared with the other
//!   adapters.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};

pub struct FlutterAdapter;

const OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        key: "target",
        values: "web",
        description: "the Flutter build target; `web` is the only supported target in this phase (deterministic, portable output layout)",
    },
    OptionSpec {
        key: "mode",
        values: "release|debug|profile",
        description: "Flutter build mode; defaults to release (Ant builds release by default)",
    },
];

/// The launcher name with the platform's executable suffix where one is needed.
/// `flutter.bat` on Windows (a batch shim), `flutter` elsewhere (a shell script).
fn flutter_launcher_name() -> &'static str {
    if cfg!(windows) {
        "flutter.bat"
    } else {
        "flutter"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Web,
}

fn target_from(options: &AdapterOptions) -> Result<Target, AdapterError> {
    match options.get("target") {
        None | Some("web") => Ok(Target::Web),
        Some(other) => Err(AdapterError::Option(format!(
            "flutter target '{other}' is not supported yet (supported: web); apk, ios and desktop \
             targets arrive in a later roadmap phase"
        ))),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Release,
    Debug,
    Profile,
}

impl Mode {
    fn flag(self) -> &'static str {
        match self {
            Mode::Release => "--release",
            Mode::Debug => "--debug",
            Mode::Profile => "--profile",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Release => "release",
            Mode::Debug => "debug",
            Mode::Profile => "profile",
        }
    }
}

fn mode_from(options: &AdapterOptions) -> Result<Mode, AdapterError> {
    match options.get("mode") {
        None | Some("release") => Ok(Mode::Release),
        Some("debug") => Ok(Mode::Debug),
        Some("profile") => Ok(Mode::Profile),
        Some(other) => Err(AdapterError::Option(format!(
            "flutter mode '{other}' is not supported (supported: release, debug, profile)"
        ))),
    }
}

impl BuildAdapter for FlutterAdapter {
    fn name(&self) -> &'static str {
        "flutter"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        let pubspec = project_root.join("pubspec.yaml");
        if !pubspec.is_file() {
            return Ok(Detection::unsupported(format!(
                "no pubspec.yaml at {}",
                project_root.display()
            )));
        }
        let text = std::fs::read_to_string(&pubspec).map_err(|error| {
            AdapterError::Project(format!("cannot read {}: {error}", pubspec.display()))
        })?;
        // A Flutter project references the Flutter SDK as `sdk: flutter`. A
        // plain Dart package has no such reference and is not recognized.
        if text.contains("sdk: flutter") {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "{} is a Dart package without a Flutter SDK reference (`sdk: flutter`); the \
                 Flutter adapter only recognizes Flutter apps and plugins",
                pubspec.display()
            )))
        }
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let target = target_from(options)?;
        let mode = mode_from(options)?;
        let root = canonical_root(project_root)?;
        // Require a Flutter project: detect's marker check, repeated here so a
        // `--adapter flutter` invocation on the wrong project fails at
        // generation with the same honest reason rather than a misleading
        // toolchain error.
        let pubspec = root.join("pubspec.yaml");
        let is_flutter = pubspec.is_file()
            && std::fs::read_to_string(&pubspec)
                .map(|text| text.contains("sdk: flutter"))
                .unwrap_or(false);
        if !is_flutter {
            return Err(AdapterError::Project(format!(
                "{} is not a Flutter project (no `sdk: flutter` in pubspec.yaml)",
                pubspec.display()
            )));
        }

        let inputs = declare_inputs(&root);
        let id = format!("flutter-build-web-{}", mode.label());
        let action = build_action(id, target, mode, inputs, web_outputs());
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = flutter_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Declare the workspace inputs Ant can honestly cover: `pubspec.yaml`
/// (dependencies and the assets manifest), `pubspec.lock` (pinned versions)
/// when present, and the `lib` (Dart sources), `web` (web template) and
/// `assets` trees when present. Pub package sources live outside the
/// workspace and are intentionally not declared (see the module docs).
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    inputs.insert(PathBuf::from("pubspec.yaml"), InputKind::File);
    if root.join("pubspec.lock").is_file() {
        inputs.insert(PathBuf::from("pubspec.lock"), InputKind::File);
    }
    if root.join("lib").is_dir() {
        inputs.insert(PathBuf::from("lib"), InputKind::Tree);
    }
    if root.join("web").is_dir() {
        inputs.insert(PathBuf::from("web"), InputKind::Tree);
    }
    if root.join("assets").is_dir() {
        inputs.insert(PathBuf::from("assets"), InputKind::Tree);
    }
    inputs
}

/// The deterministic web output files `flutter build web --<mode>` produces.
fn web_outputs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("build/web/index.html"),
        PathBuf::from("build/web/main.dart.js"),
    ]
}

/// `flutter build web --<mode>`, wrapped in the Windows batch launcher
/// (`cmd /C`) because `flutter` is a `.bat` shim on Windows.
fn flutter_build_argv(target: Target, mode: Mode) -> Vec<String> {
    let target_name = match target {
        Target::Web => "web",
    };
    let mode_flag = mode.flag();
    let argv: &[&str] = if cfg!(windows) {
        &["cmd", "/C", "flutter", "build", target_name, mode_flag]
    } else {
        &["flutter", "build", target_name, mode_flag]
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

fn build_action(
    id: String,
    target: Target,
    mode: Mode,
    inputs: BTreeMap<PathBuf, InputKind>,
    outputs: Vec<PathBuf>,
) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: flutter_build_argv(target, mode),
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
        environment: staging_environment(),
        // Pub package sources and the bundled Dart VM are undeclared inputs
        // (they live outside the workspace), so these actions never claim
        // shared cache eligibility. Local reuse is allowed: pubspec pins
        // versions and this machine may reuse its own result (D-028).
        cache_policy: CachePolicy::Local,
        toolchain: None,
        timeout_ms: None,
        resources: None,
    }
}

/// Truthful Flutter toolchain identity: the SDK version and the SHA-256 digest
/// of the bundled Dart executable the build runs.
fn flutter_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    let launcher = locate_executable(flutter_launcher_name())?;
    // `flutter` is a launcher script; the build runs on the Dart VM bundled
    // with the Flutter SDK at `<flutter_bin>/cache/dart-sdk/bin/dart<EXE>`.
    let launcher = launcher.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve the flutter launcher {}: {error}",
            launcher.display()
        ))
    })?;
    let bin = launcher.parent().ok_or_else(|| {
        AdapterError::Toolchain(format!(
            "flutter launcher {} has no parent",
            launcher.display()
        ))
    })?;
    let dart = bin
        .join("cache")
        .join("dart-sdk")
        .join("bin")
        .join(format!("dart{}", std::env::consts::EXE_SUFFIX));
    let dart = dart.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve the Flutter SDK's bundled Dart executable {}: {error}; is the \
             Flutter SDK installed?",
            dart.display()
        ))
    })?;
    // `flutter --version` prints a multi-line banner; the first line is the
    // version (e.g. `Flutter 3.41.5 • channel stable …`).
    let version = executable_version(&launcher, root)?;
    let version = version.lines().next().unwrap_or_default().to_owned();
    let executable_digest = crate::toolchain_digest(&dart)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest the Flutter SDK's Dart executable {}: {error}",
                dart.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "flutter".to_owned(),
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
                "ant-flutter-adapter-{name}-{}-{unique}",
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

    fn options(pairs: &[(&str, &str)]) -> AdapterOptions {
        let mut options = AdapterOptions::new();
        for (key, value) in pairs {
            options.insert(key, value).expect("insert option");
        }
        options
    }

    /// A pubspec.yaml that references the Flutter SDK.
    fn pubspec_yaml() -> &'static str {
        "name: example\nenvironment:\n  sdk: '>=3.0.0 <4.0.0'\ndependencies:\n  flutter:\n    sdk: flutter\n"
    }

    #[test]
    fn detect_recognizes_flutter_pubspec_and_rejects_others() {
        let adapter = FlutterAdapter;
        let dir = TempDir::new("detect");
        write(&dir.path().join("pubspec.yaml"), pubspec_yaml());
        assert!(adapter.detect(dir.path()).expect("detect").supported);
        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no pubspec.yaml"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn detect_rejects_plain_dart_package() {
        let adapter = FlutterAdapter;
        let dir = TempDir::new("dart");
        // A Dart package with no Flutter SDK reference.
        write(
            &dir.path().join("pubspec.yaml"),
            "name: cli\nenvironment:\n  sdk: '>=3.0.0 <4.0.0'\n",
        );
        let detection = adapter.detect(dir.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("without a Flutter SDK reference"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn target_rejects_unsupported_targets() {
        let adapter = FlutterAdapter;
        let dir = TempDir::new("target");
        write(&dir.path().join("pubspec.yaml"), pubspec_yaml());
        let error = adapter
            .generate(dir.path(), &options(&[("target", "apk")]))
            .expect_err("apk must fail");
        assert!(error.to_string().contains("supported: web"), "{}", error);
    }

    #[test]
    fn mode_rejects_unknown_values() {
        let adapter = FlutterAdapter;
        let dir = TempDir::new("mode");
        write(&dir.path().join("pubspec.yaml"), pubspec_yaml());
        let error = adapter
            .generate(dir.path(), &options(&[("mode", "nightly")]))
            .expect_err("bad mode must fail");
        assert!(
            error
                .to_string()
                .contains("supported: release, debug, profile"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_flutter_pubspec() {
        let adapter = FlutterAdapter;
        let dir = TempDir::new("not-flutter");
        write(
            &dir.path().join("pubspec.yaml"),
            "name: cli\nenvironment:\n  sdk: '>=3.0.0 <4.0.0'\n",
        );
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(
            error.to_string().contains("not a Flutter project"),
            "{}",
            error
        );
        // No staging directory is written on failure.
        assert!(!dir.path().join(".ant").exists());
    }

    #[test]
    fn flutter_build_argv_uses_cmd_on_windows() {
        if cfg!(windows) {
            assert_eq!(
                flutter_build_argv(Target::Web, Mode::Release),
                vec!["cmd", "/C", "flutter", "build", "web", "--release"]
            );
        } else {
            assert_eq!(
                flutter_build_argv(Target::Web, Mode::Debug),
                vec!["flutter", "build", "web", "--debug"]
            );
        }
    }

    #[test]
    fn web_outputs_are_the_deterministic_files() {
        let outputs = web_outputs();
        assert!(outputs.contains(&PathBuf::from("build/web/index.html")));
        assert!(outputs.contains(&PathBuf::from("build/web/main.dart.js")));
    }

    #[test]
    fn staging_environment_points_scratch_at_ant_tmp() {
        let env = staging_environment();
        let staging = PathBuf::from(".ant")
            .join("tmp")
            .to_string_lossy()
            .into_owned();
        assert!(env.contains(&("TMP".to_owned(), staging.clone())));
        assert!(env.contains(&("TEMP".to_owned(), staging.clone())));
        if cfg!(not(windows)) {
            assert!(env.contains(&("TMPDIR".to_owned(), staging)));
        }
    }

    #[test]
    fn declare_inputs_covers_pubspec_lock_lib_web_and_assets() {
        let dir = TempDir::new("inputs");
        write(&dir.path().join("pubspec.yaml"), pubspec_yaml());
        write(&dir.path().join("pubspec.lock"), "packages: {}\n");
        write(&dir.path().join("lib/main.dart"), "void main() {}\n");
        write(&dir.path().join("web/index.html"), "<html></html>\n");
        write(&dir.path().join("assets/icon.png"), "png");
        let inputs = declare_inputs(dir.path());
        assert_eq!(
            inputs.get(&PathBuf::from("pubspec.yaml")),
            Some(&InputKind::File)
        );
        assert_eq!(
            inputs.get(&PathBuf::from("pubspec.lock")),
            Some(&InputKind::File)
        );
        assert_eq!(inputs.get(&PathBuf::from("lib")), Some(&InputKind::Tree));
        assert_eq!(inputs.get(&PathBuf::from("web")), Some(&InputKind::Tree));
        assert_eq!(inputs.get(&PathBuf::from("assets")), Some(&InputKind::Tree));
    }

    #[test]
    fn generate_sets_cache_policy_local() {
        // Requires the Flutter SDK on PATH; skip honestly where it is absent so
        // this unit suite does not depend on a Flutter install (the e2e stub
        // test covers the full happy path without the real SDK).
        if locate_executable(flutter_launcher_name()).is_err() {
            eprintln!("skipping: flutter not on PATH");
            return;
        }
        let adapter = FlutterAdapter;
        let dir = TempDir::new("cache");
        write(&dir.path().join("pubspec.yaml"), pubspec_yaml());
        write(&dir.path().join("lib/main.dart"), "void main() {}\n");
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Local);
        assert_eq!(action.id, "flutter-build-web-release");
        assert_eq!(action.working_directory, PathBuf::from("."));
        let argv: Vec<&str> = action.argv.iter().map(String::as_str).collect();
        assert!(argv.contains(&"build") && argv.contains(&"web") && argv.contains(&"--release"));
        assert!(
            action
                .outputs
                .iter()
                .any(|out| out.path == Path::new("build/web/index.html"))
        );
        assert_eq!(
            action.toolchain.as_ref().expect("toolchain").name,
            "flutter"
        );
    }
}
