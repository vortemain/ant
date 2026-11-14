//! The Xcode adapter (roadmap P5): builds an iOS app with `xcodebuild`.
//!
//! An Xcode project is a directory containing a `*.xcodeproj` package (a
//! directory, despite the extension) or a `*.xcworkspace`. A pure iOS app has
//! one at the project root; a Flutter or React Native project keeps its iOS
//! project in an `ios/` subfolder, so root-level detection does not collide
//! with the Flutter adapter (which keys on `pubspec.yaml` at the root).
//!
//! The adapter generates one action that runs `xcodebuild -scheme <scheme>
//! -configuration <config> -sdk <sdk> -derivedDataPath .ant/build build`,
//! declaring the project package and the dependency manifests it can honestly
//! cover, and the isolated derived-data output directory.
//!
//! Honest limits (recorded in the module tests):
//!
//! - Only the **build** verb is supported in this phase. The output layout
//!   under `DerivedData` is deterministic in shape (`Build/Products/<config>-<sdk>/MyApp.app`) but the `.app` product name is not known without parsing
//!   the project, so — like the CMake adapter — no deliverable is claimed
//!   (`artifacts()` is empty) and the build directory is the only declared
//!   output. An export to a signed `.ipa` (the P5 success criterion) needs an
//!   Apple Developer identity and a provisioning profile; those are signing
//!   secrets the owner must supply, so a signed export arrives in a later
//!   step. Unsigned simulator and development builds need no credentials.
//! - Swift Package Manager and CocoaPods resolve dependencies from outside
//!   the workspace (the SPM clone cache, the Pods cache) and those sources are
//!   undeclared inputs, so generated actions are `cache_policy: Local` (D-028):
//!   this machine may reuse its own result (Package.resolved / Podfile.lock pin
//!   versions locally) but it is never shared — the same stance the Flutter
//!   and Gradle adapters take when their dependencies are not locally declared.
//! - Toolchain identity is captured from the real `xcodebuild`: the version
//!   from `xcodebuild -version` (first line, e.g. `Xcode 15.2`) and the SHA-256
//!   digest of the resolved `xcodebuild` executable. `xcodebuild` is a real
//!   executable (an `xcrun` shim on macOS), not a batch script, so no `cmd /C`
//!   wrapper is needed; on a non-Mac it is simply absent and `generate` fails
//!   closed with a toolchain error.
//! - Workspace-relative scratch: `TMP`/`TEMP` (plus `TMPDIR` off Windows) point
//!   at `.ant/tmp`, and the build's derived data is isolated under `.ant/build`
//!   via `-derivedDataPath` so it never pollutes the user's global DerivedData.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version_with_args, locate_executable,
};

pub struct XcodeAdapter;

const OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        key: "scheme",
        values: "any scheme name",
        description: "the Xcode scheme to build; when omitted xcodebuild uses the project's default scheme \
             (pass one for multi-scheme projects)",
    },
    OptionSpec {
        key: "configuration",
        values: "Debug|Release",
        description: "the Xcode build configuration; Release by default (Ant builds release by \
                      default)",
    },
    OptionSpec {
        key: "sdk",
        values: "iphoneos|iphonesimulator",
        description: "the target SDK; `iphoneos` (the device, the deliverable) by default. Use \
             `iphonesimulator` for a credential-free simulator build that needs no signing identity",
    },
];

/// The isolated derived-data directory generated actions build into.
const BUILD_DIR: &str = ".ant/build";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Configuration {
    Debug,
    Release,
}

impl Configuration {
    fn as_str(self) -> &'static str {
        match self {
            Configuration::Debug => "Debug",
            Configuration::Release => "Release",
        }
    }
}

fn configuration_from(options: &AdapterOptions) -> Result<Configuration, AdapterError> {
    match options.get("configuration") {
        None | Some("Release") => Ok(Configuration::Release),
        Some("Debug") => Ok(Configuration::Debug),
        Some(other) => Err(AdapterError::Option(format!(
            "xcode configuration '{other}' is not supported (supported: Debug, Release)"
        ))),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Sdk {
    Iphoneos,
    Iphonesimulator,
}

impl Sdk {
    fn as_str(self) -> &'static str {
        match self {
            Sdk::Iphoneos => "iphoneos",
            Sdk::Iphonesimulator => "iphonesimulator",
        }
    }
}

fn sdk_from(options: &AdapterOptions) -> Result<Sdk, AdapterError> {
    match options.get("sdk") {
        None | Some("iphoneos") => Ok(Sdk::Iphoneos),
        Some("iphonesimulator") => Ok(Sdk::Iphonesimulator),
        Some(other) => Err(AdapterError::Option(format!(
            "xcode sdk '{other}' is not supported (supported: iphoneos, iphonesimulator)"
        ))),
    }
}

/// Find the project package at the root: a `*.xcworkspace` (preferred — Pods
/// and SPM generate one) falling back to a `*.xcodeproj`. Returns the
/// workspace-relative path, or `None` when neither is present.
fn find_project(root: &Path) -> Option<PathBuf> {
    let mut xcodeproj = None;
    let mut xcworkspace = None;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.ends_with(".xcworkspace") {
                xcworkspace = Some(PathBuf::from(name));
            } else if name.ends_with(".xcodeproj") {
                xcodeproj = Some(PathBuf::from(name));
            }
        }
    }
    xcworkspace.or(xcodeproj)
}

impl BuildAdapter for XcodeAdapter {
    fn name(&self) -> &'static str {
        "xcode"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if find_project(project_root).is_some() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no *.xcodeproj or *.xcworkspace at {}",
                project_root.display()
            )))
        }
    }

    /// The `.app` product name is not known without parsing the project, so —
    /// like CMake — no deliverable is claimed. The build directory is the only
    /// declared output.
    fn artifacts(&self, _plan: &PlanFile) -> Vec<PathBuf> {
        Vec::new()
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let configuration = configuration_from(options)?;
        let sdk = sdk_from(options)?;
        let scheme = options.get("scheme").map(str::to_owned);
        let root = canonical_root(project_root)?;
        // Require an Xcode project: detect's marker check, repeated here so a
        // `--adapter xcode` invocation on the wrong project fails at generation
        // with the same honest reason rather than a misleading toolchain error.
        let project = find_project(&root).ok_or_else(|| {
            AdapterError::Project(format!(
                "no *.xcodeproj or *.xcworkspace at {}",
                root.display()
            ))
        })?;

        let inputs = declare_inputs(&root, &project);
        let id = format!(
            "xcode-build-{}-{}",
            configuration.as_str().to_lowercase(),
            sdk.as_str()
        );
        let action = build_action(id, &project, scheme.as_deref(), configuration, sdk, inputs);
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = xcode_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Declare the workspace inputs Ant can honestly cover: the project package
/// (`.xcworkspace`/`.xcodeproj`, a directory tree) and the dependency manifests
/// (`Package.swift`, `Podfile`, `Podfile.lock`) when present, plus common
/// source trees when present. SPM clone sources and the Pods cache live outside
/// the workspace and are intentionally not declared (see the module docs).
fn declare_inputs(root: &Path, project: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    inputs.insert(project.to_owned(), InputKind::Tree);
    if root.join("Package.swift").is_file() {
        inputs.insert(PathBuf::from("Package.swift"), InputKind::File);
    }
    if root.join("Podfile").is_file() {
        inputs.insert(PathBuf::from("Podfile"), InputKind::File);
    }
    if root.join("Podfile.lock").is_file() {
        inputs.insert(PathBuf::from("Podfile.lock"), InputKind::File);
    }
    if root.join("Sources").is_dir() {
        inputs.insert(PathBuf::from("Sources"), InputKind::Tree);
    }
    if root.join("App").is_dir() {
        inputs.insert(PathBuf::from("App"), InputKind::Tree);
    }
    inputs
}

/// `xcodebuild -scheme <scheme> -configuration <config> -sdk <sdk>
/// -derivedDataPath .ant/build build`, with `-scheme` omitted when unspecified.
fn xcodebuild_argv(
    project: &Path,
    scheme: Option<&str>,
    configuration: Configuration,
    sdk: Sdk,
) -> Vec<String> {
    let mut argv = vec!["xcodebuild".to_owned()];
    if let Some(scheme) = scheme {
        argv.push("-scheme".to_owned());
        argv.push(scheme.to_owned());
    }
    argv.push("-project".to_owned());
    argv.push(project.to_string_lossy().into_owned());
    argv.push("-configuration".to_owned());
    argv.push(configuration.as_str().to_owned());
    argv.push("-sdk".to_owned());
    argv.push(sdk.as_str().to_owned());
    argv.push("-derivedDataPath".to_owned());
    argv.push(BUILD_DIR.to_owned());
    argv.push("build".to_owned());
    argv
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
    project: &Path,
    scheme: Option<&str>,
    configuration: Configuration,
    sdk: Sdk,
    inputs: BTreeMap<PathBuf, InputKind>,
) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: xcodebuild_argv(project, scheme, configuration, sdk),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs: vec![Output {
            path: PathBuf::from(BUILD_DIR),
            kind: OutputKind::Tree,
        }],
        environment: staging_environment(),
        // SPM clone sources and the Pods cache are undeclared inputs (they live
        // outside the workspace), so these actions never claim shared cache
        // eligibility. Local reuse is allowed: Package.resolved / Podfile.lock
        // pin versions and this machine may reuse its own result (D-028).
        cache_policy: CachePolicy::Local,
        toolchain: None,
        timeout_ms: None,
        resources: None,
    }
}

/// Truthful Xcode toolchain identity: the version from `xcodebuild -version`
/// and the SHA-256 digest of the resolved `xcodebuild` executable.
fn xcode_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    let xcodebuild = locate_executable("xcodebuild")?;
    let digest_target = xcodebuild.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve xcodebuild executable {}: {error}",
            xcodebuild.display()
        ))
    })?;
    // `xcodebuild -version` (single dash, not `--version`) prints two lines; the
    // first is the Xcode version (e.g. `Xcode 15.2`).
    let version = executable_version_with_args(&xcodebuild, root, &["-version"])?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let executable_digest = crate::toolchain_digest(&digest_target)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest xcodebuild executable {}: {error}",
                digest_target.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "xcode".to_owned(),
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
                "ant-xcode-adapter-{name}-{}-{unique}",
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

    /// A bare `.xcodeproj` package (a directory with the extension).
    fn make_xcodeproj(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("mkdir xcodeproj");
        write(&dir.join("project.pbxproj"), "// stub pbxproj\n");
    }

    #[test]
    fn detect_recognizes_xcodeproj_and_rejects_others() {
        let adapter = XcodeAdapter;
        let dir = TempDir::new("detect");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        assert!(adapter.detect(dir.path()).expect("detect").supported);
        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no *.xcodeproj"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn detect_prefers_xcworkspace_over_xcodeproj() {
        let dir = TempDir::new("workspace");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        fs::create_dir_all(dir.path().join("Example.xcworkspace")).expect("mkdir xcworkspace");
        let project = find_project(dir.path()).expect("found a project");
        assert_eq!(project, PathBuf::from("Example.xcworkspace"));
    }

    #[test]
    fn configuration_rejects_unknown_values() {
        let adapter = XcodeAdapter;
        let dir = TempDir::new("config");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        let error = adapter
            .generate(dir.path(), &options(&[("configuration", "nightly")]))
            .expect_err("bad config must fail");
        assert!(
            error.to_string().contains("supported: Debug, Release"),
            "{}",
            error
        );
    }

    #[test]
    fn sdk_rejects_unknown_values() {
        let adapter = XcodeAdapter;
        let dir = TempDir::new("sdk");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        let error = adapter
            .generate(dir.path(), &options(&[("sdk", "macosx")]))
            .expect_err("bad sdk must fail");
        assert!(
            error
                .to_string()
                .contains("supported: iphoneos, iphonesimulator"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_xcode_project() {
        let adapter = XcodeAdapter;
        let dir = TempDir::new("not-xcode");
        write(&dir.path().join("README.md"), "not an app");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("no *.xcodeproj"), "{}", error);
        // No staging directory is written on failure.
        assert!(!dir.path().join(".ant").exists());
    }

    #[test]
    fn xcodebuild_argv_includes_scheme_configuration_and_sdk() {
        let argv = xcodebuild_argv(
            Path::new("Example.xcodeproj"),
            Some("Example"),
            Configuration::Release,
            Sdk::Iphoneos,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("-scheme Example"));
        assert!(joined.contains("-configuration Release"));
        assert!(joined.contains("-sdk iphoneos"));
        assert!(joined.contains("-derivedDataPath .ant/build"));
        assert!(joined.contains("-project Example.xcodeproj"));
        assert!(argv.last().is_some_and(|last| last == "build"));
    }

    #[test]
    fn xcodebuild_argv_omits_scheme_when_unspecified() {
        let argv = xcodebuild_argv(
            Path::new("Example.xcodeproj"),
            None,
            Configuration::Debug,
            Sdk::Iphonesimulator,
        );
        assert!(!argv.iter().any(|arg| arg == "-scheme"));
        let joined = argv.join(" ");
        assert!(joined.contains("-configuration Debug"));
        assert!(joined.contains("-sdk iphonesimulator"));
    }

    #[test]
    fn declare_inputs_covers_project_and_manifests() {
        let dir = TempDir::new("inputs");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        write(
            &dir.path().join("Package.swift"),
            "// swift-tools-version:5.7\n",
        );
        write(&dir.path().join("Podfile"), "platform :ios, '16.0'\n");
        write(&dir.path().join("Podfile.lock"), "PODS: []\n");
        write(&dir.path().join("Sources/App/main.swift"), "print(1)\n");
        let inputs = declare_inputs(dir.path(), Path::new("Example.xcodeproj"));
        assert_eq!(
            inputs.get(&PathBuf::from("Example.xcodeproj")),
            Some(&InputKind::Tree)
        );
        assert_eq!(
            inputs.get(&PathBuf::from("Package.swift")),
            Some(&InputKind::File)
        );
        assert_eq!(
            inputs.get(&PathBuf::from("Podfile")),
            Some(&InputKind::File)
        );
        assert_eq!(
            inputs.get(&PathBuf::from("Podfile.lock")),
            Some(&InputKind::File)
        );
        assert_eq!(
            inputs.get(&PathBuf::from("Sources")),
            Some(&InputKind::Tree)
        );
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
    fn generate_assembles_xcodebuild_argv() {
        // Requires xcodebuild on PATH; skip honestly where it is absent (no
        // Xcode on Linux/Windows CI) so this unit suite does not depend on a
        // Mac. The macOS verify-ios workflow runs this test for real on Apple
        // hardware, exercising the genuine toolchain capture.
        if locate_executable("xcodebuild").is_err() {
            eprintln!("skipping: xcodebuild not on PATH");
            return;
        }
        let adapter = XcodeAdapter;
        let dir = TempDir::new("argv");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        let plan = adapter
            .generate(
                dir.path(),
                &options(&[("scheme", "Example"), ("configuration", "Release")]),
            )
            .expect("generate");
        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Local);
        assert_eq!(action.id, "xcode-build-release-iphoneos");
        assert_eq!(action.working_directory, PathBuf::from("."));
        let argv: Vec<&str> = action.argv.iter().map(String::as_str).collect();
        assert!(argv.contains(&"-scheme"));
        assert!(argv.contains(&"Example"));
        assert!(argv.contains(&"-configuration"));
        assert!(argv.contains(&"Release"));
        assert!(argv.contains(&"build"));
        assert!(
            action
                .outputs
                .iter()
                .any(|out| out.path == Path::new(".ant/build"))
        );
        assert_eq!(action.toolchain.as_ref().expect("toolchain").name, "xcode");
    }

    #[test]
    fn generate_sets_cache_policy_local() {
        if locate_executable("xcodebuild").is_err() {
            eprintln!("skipping: xcodebuild not on PATH");
            return;
        }
        let adapter = XcodeAdapter;
        let dir = TempDir::new("cache");
        make_xcodeproj(dir.path(), "Example.xcodeproj");
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("generate");
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Local);
        // No scheme option passed, so -scheme is absent.
        assert!(!action.argv.iter().any(|arg| arg == "-scheme"));
    }
}
