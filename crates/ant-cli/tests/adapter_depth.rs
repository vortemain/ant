//! End-to-end validation of adapter depth (T3.6–T3.11): cargo profiles,
//! custom target directories, vendored registry dependencies under
//! `--sandbox`, npm workspaces, npm entry-point outputs, and the CMake
//! adapter. Every test runs
//! the real `ant` binary and the real toolchain; nothing touches the network
//! (vendored sources are a hand-built `cargo vendor` directory source; npm
//! builds need no `npm install`).
//!
//! Kept separate from `end_to_end.rs` so adapter work and run-path work can
//! evolve without editing the same file.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ant-depth-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdirs");
        }
        fs::write(&path, contents).expect("write file");
    }

    fn arg(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    fn file_arg(&self, relative: &str) -> String {
        self.0.join(relative).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_ant(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .env("RUST_BACKTRACE", "0")
        // An inherited CARGO_TARGET_DIR would change what `cargo metadata`
        // reports; these tests control the target directory explicitly.
        .env_remove("CARGO_TARGET_DIR")
        .args(args)
        .output()
        .expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn bin_crate(dir: &TempDir, name: &str) {
    dir.write(
        "Cargo.toml",
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    );
    dir.write("src/main.rs", "fn main() { println!(\"built\"); }\n");
}

/// `--option profile=release` builds with `--release` and the declared
/// artifact lives under `target/release`; the run verifies it exists.
#[test]
fn cargo_release_profile_adapts_runs_and_produces_the_release_artifact() {
    let dir = TempDir::new("cargo-release");
    bin_crate(&dir, "rel-app");
    let plan = dir.file_arg("plan.json");
    let (code, stdout, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--option",
        "profile=release",
        "--project",
        &dir.arg(),
        "--out",
        &plan,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let text = fs::read_to_string(&plan).expect("plan");
    assert!(text.contains("\"--release\""), "{text}");

    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &dir.arg(), "--plan", &plan]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("action=rel-app status=succeeded"),
        "{stdout:?}"
    );
    let artifact = dir
        .path()
        .join("target")
        .join("release")
        .join(exe("rel-app"));
    assert!(artifact.is_file(), "release artifact must exist");
    assert!(
        !dir.path()
            .join("target")
            .join("debug")
            .join(exe("rel-app"))
            .exists(),
        "a release plan must not build the debug artifact"
    );
}

/// Option errors fail before anything is written, with actionable messages.
#[test]
fn adapt_option_errors_fail_closed_without_writing() {
    let dir = TempDir::new("adapt-options");
    bin_crate(&dir, "opt-app");
    let plan = dir.file_arg("plan.json");
    let project = dir.arg();
    let cases: &[(&[&str], &str)] = &[
        (
            &["--option", "profile=bench"],
            "cargo profile 'bench' is not supported",
        ),
        (
            &["--option", "outputs=entry-points"],
            "does not accept option 'outputs' (accepted: profile, granularity)",
        ),
        (&["--option", "profile"], "is not KEY=VALUE"),
        (
            &["--option", "profile=dev", "--option", "profile=release"],
            "given more than once",
        ),
    ];
    for (extra, expected) in cases {
        let mut args = vec![
            "adapt",
            "--adapter",
            "cargo",
            "--project",
            &project,
            "--out",
            &plan,
        ];
        args.extend_from_slice(extra);
        let (code, _, stderr) = run_ant(&args);
        assert_eq!(code, 1, "{extra:?} must fail");
        assert!(
            stderr.contains(expected),
            "{extra:?}: stderr was {stderr:?}"
        );
        assert!(!Path::new(&plan).exists(), "{extra:?}: no plan written");
        assert!(
            !dir.path().join(".ant").exists(),
            "{extra:?}: no staging directory"
        );
    }
    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--detect",
        "--project",
        &project,
        "--option",
        "profile=release",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--detect cannot be combined"),
        "stderr was {stderr:?}"
    );
}

/// A `build.target-dir` inside the project is mapped (outputs) and pinned
/// (`CARGO_TARGET_DIR`, because the executor clears the child environment);
/// one outside the project fails closed with no plan written.
#[test]
fn cargo_custom_target_dir_is_honored_inside_and_rejected_outside() {
    let dir = TempDir::new("cargo-target-dir");
    bin_crate(&dir, "td-app");
    dir.write(
        ".cargo/config.toml",
        "[build]\ntarget-dir = \"out/cargo\"\n",
    );
    let plan = dir.file_arg("plan.json");
    let (code, stdout, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &dir.arg(),
        "--out",
        &plan,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let text = fs::read_to_string(&plan).expect("plan");
    assert!(text.contains("CARGO_TARGET_DIR"), "{text}");

    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &dir.arg(), "--plan", &plan]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    let artifact = dir
        .path()
        .join("out")
        .join("cargo")
        .join("debug")
        .join(exe("td-app"));
    assert!(
        artifact.is_file(),
        "artifact must land in the custom target dir"
    );
    assert!(
        !dir.path().join("target").exists(),
        "the default target dir must not be used"
    );

    let outside = TempDir::new("cargo-target-outside");
    bin_crate(&outside, "out-app");
    outside.write(
        ".cargo/config.toml",
        "[build]\ntarget-dir = \"../shared-target\"\n",
    );
    let plan = outside.file_arg("plan.json");
    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &outside.arg(),
        "--out",
        &plan,
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("is outside the project root"),
        "stderr was {stderr:?}"
    );
    assert!(!Path::new(&plan).exists(), "no plan written");
}

/// A crates.io dependency vendored the way `cargo vendor` lays it out (a
/// directory source plus `.cargo/config.toml` source replacement) builds
/// under `ant run --sandbox`: the vendored source is a declared input, and
/// the confined build never needs to write the cargo home. Built by hand so
/// the test needs neither network nor a warm registry cache.
#[test]
fn cargo_vendored_registry_dependency_builds_sandboxed() {
    let dir = TempDir::new("cargo-vendored");
    dir.write(
        "Cargo.toml",
        "[package]\nname = \"vend-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ntinydep = \"1\"\n",
    );
    dir.write(
        "src/main.rs",
        "fn main() { println!(\"v={}\", tinydep::value()); }\n",
    );
    dir.write(
        "vendor/tinydep/Cargo.toml",
        "[package]\nname = \"tinydep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    );
    dir.write("vendor/tinydep/src/lib.rs", "pub fn value() -> u32 { 7 }\n");
    dir.write(
        "vendor/tinydep/.cargo-checksum.json",
        "{\"files\":{},\"package\":\"0000000000000000000000000000000000000000000000000000000000000000\"}",
    );
    dir.write(
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"vendor\"\n",
    );
    let plan = dir.file_arg("plan.json");
    let (code, stdout, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "cargo",
        "--project",
        &dir.arg(),
        "--out",
        &plan,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    // Cargo resolved it as a crates.io package (source replacement), and
    // the lock file records the registry source.
    let lock = fs::read_to_string(dir.path().join("Cargo.lock")).expect("lock");
    assert!(
        lock.contains("registry+https://github.com/rust-lang/crates.io-index"),
        "{lock}"
    );

    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &dir.arg(),
        "--sandbox",
        "--plan",
        &plan,
    ]);
    // Windows: restricted-token sandbox; Linux: bubblewrap sandbox (D-018).
    if cfg!(any(windows, target_os = "linux")) {
        assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
        assert!(
            stdout.contains("action=vend-app status=succeeded"),
            "{stdout:?}"
        );
        // The whole project is declared top-down (D-021): the vendored
        // sources are digested as part of the `vendor` tree input.
        let vendored_input = "input=vendor digest=".to_owned();
        assert!(
            stdout.contains(&vendored_input),
            "vendored source must be digested: {stdout:?}"
        );
        let config_input = "input=.cargo digest=".to_owned();
        assert!(
            stdout.contains(&config_input),
            "project cargo config must be digested: {stdout:?}"
        );
        let artifact = dir
            .path()
            .join("target")
            .join("debug")
            .join(exe("vend-app"));
        let output = Command::new(&artifact)
            .output()
            .expect("run built artifact");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "v=7");
    } else {
        assert_ne!(code, 0, "sandbox must fail closed without a sandbox");
        assert!(
            stderr.contains("sandbox unavailable"),
            "stderr was {stderr:?}"
        );
    }
}

/// An npm monorepo: one action per workspace package with a build script,
/// a DAG edge found through a non-building intermediate package, per-package
/// working directories, entry-point outputs verified by the executor, all
/// under `--sandbox`.
#[test]
fn npm_workspaces_adapt_and_run_in_dependency_order() {
    let dir = TempDir::new("npm-workspaces");
    dir.write(
        "package.json",
        "{\"name\":\"mono\",\"private\":true,\"workspaces\":[\"packages/*\"],\"scripts\":{\"build\":\"npm run build --workspaces\"}}",
    );
    dir.write(
        "packages/core/package.json",
        "{\"name\":\"@demo/core\",\"version\":\"1.0.0\",\"main\":\"dist/core.js\",\"scripts\":{\"build\":\"node build.js\"}}",
    );
    dir.write(
        "packages/core/build.js",
        "const fs = require('fs'); fs.mkdirSync('dist', { recursive: true }); fs.writeFileSync('dist/core.js', 'module.exports = 41;');\n",
    );
    dir.write("packages/core/src/marker.txt", "core source\n");
    dir.write(
        "packages/util/package.json",
        "{\"name\":\"@demo/util\",\"version\":\"1.0.0\",\"dependencies\":{\"@demo/core\":\"*\"}}",
    );
    dir.write("packages/util/src/util.txt", "util source\n");
    // app reads core's build output, so it only succeeds after core ran.
    dir.write(
        "packages/app/package.json",
        "{\"name\":\"@demo/app\",\"version\":\"1.0.0\",\"bin\":{\"demo-app\":\"dist/app.js\"},\"dependencies\":{\"@demo/util\":\"*\"},\"scripts\":{\"build\":\"node build.js\"}}",
    );
    dir.write(
        "packages/app/build.js",
        "const fs = require('fs'); const v = require('../core/dist/core.js'); fs.mkdirSync('dist', { recursive: true }); fs.writeFileSync('dist/app.js', 'console.log(' + (v + 1) + ');');\n",
    );
    let plan = dir.file_arg("plan.json");
    let (code, stdout, stderr) = run_ant(&[
        "adapt",
        "--project",
        &dir.arg(),
        "--option",
        "outputs=entry-points",
        "--out",
        &plan,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
    assert!(
        stdout.contains("adapter=npm") && stdout.contains("actions=2"),
        "{stdout:?}"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan).expect("plan")).expect("json");
    let app = &parsed["actions"][0];
    assert_eq!(app["action"]["id"], "@demo/app");
    assert_eq!(app["dependencies"], serde_json::json!(["@demo/core"]));

    let (code, stdout, stderr) = run_ant(&[
        "run",
        "--workspace",
        &dir.arg(),
        "--sandbox",
        "--plan",
        &plan,
    ]);
    // Windows: restricted-token sandbox; Linux: bubblewrap sandbox (D-018).
    if cfg!(any(windows, target_os = "linux")) {
        assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
        let core_done = stdout
            .find("action=@demo/core status=succeeded")
            .expect("core ran");
        let app_start = stdout
            .find("action=@demo/app status=started")
            .expect("app ran");
        assert!(
            core_done < app_start,
            "core must finish before app starts: {stdout:?}"
        );
        let app_out = Path::new("packages")
            .join("app")
            .join("dist")
            .join("app.js");
        assert!(
            stdout.contains(&format!("output={} digest=", app_out.display())),
            "entry-point output verified by the executor: {stdout:?}"
        );
        let built = fs::read_to_string(dir.path().join(&app_out)).expect("app artifact");
        assert_eq!(built, "console.log(42);");
    } else {
        assert_ne!(code, 0, "sandbox must fail closed without a sandbox");
    }

    // Deterministic re-adaptation.
    let plan2 = dir.file_arg("plan2.json");
    let (code, _, _) = run_ant(&[
        "adapt",
        "--project",
        &dir.arg(),
        "--option",
        "outputs=entry-points",
        "--out",
        &plan2,
    ]);
    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(&plan).expect("plan"),
        fs::read_to_string(&plan2).expect("plan2"),
        "re-adaptation must be deterministic"
    );
}

/// A declared entry point the build does not produce fails the action on
/// output coverage — the user's `entry-points` assertion is checked, not
/// trusted.
#[test]
fn npm_entry_point_outputs_are_verified_not_trusted() {
    let dir = TempDir::new("npm-entry-missing");
    dir.write(
        "package.json",
        "{\"name\":\"liar\",\"version\":\"1.0.0\",\"main\":\"dist/index.js\",\"scripts\":{\"build\":\"node build.js\"}}",
    );
    dir.write("build.js", "require('fs').writeFileSync('other.js', '');\n");
    let plan = dir.file_arg("plan.json");
    let (code, _, stderr) = run_ant(&[
        "adapt",
        "--adapter",
        "npm",
        "--project",
        &dir.arg(),
        "--option",
        "outputs=entry-points",
        "--out",
        &plan,
    ]);
    assert_eq!(code, 0, "stderr was {stderr:?}");
    let (code, stdout, stderr) = run_ant(&["run", "--workspace", &dir.arg(), "--plan", &plan]);
    assert_ne!(
        code, 0,
        "a missing declared output must fail: stdout was {stdout:?}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("declared output was not created"),
        "{combined:?}"
    );
}

/// The directory holding a usable `cmake` executable: PATH first, then the
/// copy Visual Studio bundles (found through `vswhere`). `None` means this
/// machine cannot run the CMake test; the caller prints a skip notice.
fn cmake_dir() -> Option<PathBuf> {
    let name = exe("cmake");
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join(&name).is_file() {
                return Some(dir);
            }
        }
    }
    if !cfg!(windows) {
        return None;
    }
    let vswhere = PathBuf::from(std::env::var_os("ProgramFiles(x86)")?)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    let output = Command::new(vswhere)
        .args(["-latest", "-products", "*", "-property", "installationPath"])
        .output()
        .ok()?;
    let install = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let dir = Path::new(&install).join("Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin");
    dir.join(&name).is_file().then_some(dir)
}

fn run_ant_with_path(extra: &Path, args: &[&str]) -> (i32, String, String) {
    let mut paths = vec![extra.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .env("RUST_BACKTRACE", "0")
        .env("PATH", std::env::join_paths(paths).expect("join PATH"))
        .args(args)
        .output()
        .expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The CMake adapter on a real C project: adapt → validate → run builds the
/// executable through the machine's default generator, for both Debug and
/// Release, and the built program runs. The artifact location is checked
/// for both generator families because the adapter deliberately does not
/// declare it.
#[test]
fn cmake_project_adapts_runs_and_builds_a_real_executable() {
    let Some(cmake) = cmake_dir() else {
        eprintln!("skipping: no cmake on PATH or bundled with Visual Studio");
        return;
    };
    for config in ["Debug", "Release"] {
        let dir = TempDir::new(&format!("cmake-{config}"));
        dir.write(
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 3.20)\nproject(hello C)\nadd_subdirectory(app)\n",
        );
        dir.write("app/CMakeLists.txt", "add_executable(hello main.c)\n");
        dir.write(
            "app/main.c",
            "#include <stdio.h>\nint main(void) { puts(\"hello cmake\"); return 0; }\n",
        );
        let plan = dir.file_arg("plan.json");
        let option = format!("config={config}");
        let (code, stdout, stderr) = run_ant_with_path(
            &cmake,
            &[
                "adapt",
                "--project",
                &dir.arg(),
                "--option",
                &option,
                "--out",
                &plan,
            ],
        );
        assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
        assert!(
            stdout.contains("adapter=cmake") && stdout.contains("actions=2"),
            "{stdout:?}"
        );

        let (code, stdout, stderr) =
            run_ant_with_path(&cmake, &["plan", "validate", "--plan", &plan]);
        assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");

        let (code, stdout, stderr) =
            run_ant_with_path(&cmake, &["run", "--workspace", &dir.arg(), "--plan", &plan]);
        assert_eq!(code, 0, "stderr was {stderr:?}; stdout was {stdout:?}");
        assert!(
            stdout.contains("action=configure status=succeeded"),
            "{stdout:?}"
        );
        assert!(
            stdout.contains("action=build status=succeeded"),
            "{stdout:?}"
        );
        let cache = format!(
            "output={} digest=",
            Path::new("build").join("CMakeCache.txt").display()
        );
        assert!(
            stdout.contains(&cache),
            "CMakeCache.txt verified: {stdout:?}"
        );
        let build = dir.path().join("build").join("app");
        let artifact = [
            build.join(config).join(exe("hello")),
            build.join(exe("hello")),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("no hello executable under {}", build.display()));
        let output = Command::new(&artifact).output().expect("run artifact");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello cmake"
        );
    }
}
