//! End-to-end test for `ant build` on a Flutter web project (P6, D-038):
//! the front door detects `pubspec.yaml` with a Flutter SDK reference, runs
//! `flutter build web --release` through the platform launcher (Windows:
//! `cmd /C flutter`, Unix: `flutter`), collects the deterministic web files
//! into `dist/`, and reuses its own local result on a no-change rebuild
//! (D-028) instead of running Flutter again.
//!
//! The Flutter SDK is a stub script on PATH, so the test needs no Flutter
//! install, Dart SDK, or network: it fabricates the web files the adapter
//! declares from a real source file, which proves the collected artifact
//! really flows from the declared `lib/` input to `dist/`. The bundled Dart
//! executable the adapter digests is a fixed dummy file, so toolchain identity
//! is stable across runs.

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
        let path = std::env::temp_dir().join(format!(
            "ant-flutter-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir");
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

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

/// How many times the flutter stub ran a build: every build appends one line
/// to `.ant/flutter-runs`, so local reuse that skips Flutter stays visible.
fn flutter_runs(dir: &Path) -> usize {
    fs::read_to_string(dir.join(".ant").join("flutter-runs"))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// A minimal Flutter web project: a pubspec that references the Flutter SDK,
/// a Dart entry point, and the web target template the adapter declares.
fn flutter_project(root: &Path) {
    write(
        &root.join("pubspec.yaml"),
        "name: example\nenvironment:\n  sdk: '>=3.0.0 <4.0.0'\ndependencies:\n  flutter:\n    sdk: flutter\n",
    );
    write(&root.join("lib/main.dart"), "void main() {}\n");
    write(
        &root.join("web/index.html"),
        "<!DOCTYPE html><html></html>\n",
    );
    write_flutter_stub(root);
    // The bundled Dart executable the adapter digests (never executed by the
    // stub): a fixed dummy file so toolchain identity is stable across runs.
    let dart_name = if cfg!(windows) { "dart.exe" } else { "dart" };
    write(
        &root
            .join(".ant")
            .join("bin")
            .join("cache")
            .join("dart-sdk")
            .join("bin")
            .join(dart_name),
        "stub-dart-vm",
    );
}

/// Prepend the stub SDK's bin directory to PATH so both the adapter's
/// `flutter --version` toolchain probe and the spawned `cmd /C flutter`
/// (Windows) / `flutter` (Unix) action resolve the stub launcher.
fn path_with_stub_bin(dir: &Path) -> PathBuf {
    let bin = dir.join(".ant").join("bin");
    let sep = if cfg!(windows) { ";" } else { ":" };
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut combined = std::ffi::OsString::new();
    combined.push(bin.as_os_str());
    combined.push(sep);
    combined.push(existing);
    PathBuf::from(combined)
}

/// The stub Flutter launcher: `--version` prints the version banner; `build
/// web --<mode>` fabricates the declared web files from `lib/main.dart` and
/// records the run so the test can tell execution from reuse.
#[cfg(windows)]
fn write_flutter_stub(root: &Path) {
    write(
        &root.join(".ant").join("bin").join("flutter.bat"),
        "@echo off\r\n\
         if \"%~1\"==\"--version\" (\r\n\
         echo Flutter 3.41.5 - channel stable\r\n\
         echo Framework - revision stub - 2024-01-01 00:00:00\r\n\
         echo Engine - revision stub\r\n\
         exit /b 0\r\n\
         )\r\n\
         if \"%~1\"==\"build\" (\r\n\
         mkdir build\\web 2>nul\r\n\
         >>.ant\\flutter-runs echo run\r\n\
         copy /Y lib\\main.dart build\\web\\main.dart.js >nul\r\n\
         echo ^<html^>^<head^>^</head^>^<body^>app^</body^>^</html^> > build\\web\\index.html\r\n\
         exit /b 0\r\n\
         )\r\n\
         exit /b 0\r\n",
    );
}

/// The stub Flutter launcher (Unix): a shell script.
#[cfg(unix)]
fn write_flutter_stub(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join(".ant").join("bin").join("flutter");
    write(
        &script,
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then\n\
         echo \"Flutter 3.41.5 - channel stable\"\n\
         echo \"Framework - revision stub - 2024-01-01 00:00:00\"\n\
         echo \"Engine - revision stub\"\n\
         exit 0\n\
         fi\n\
         if [ \"$1\" = \"build\" ]; then\n\
         mkdir -p build/web\n\
         cp lib/main.dart build/web/main.dart.js\n\
         printf '%s\\n' '<html><head></head><body>app</body></html>' > build/web/index.html\n\
         echo run >> .ant/flutter-runs\n\
         exit 0\n\
         fi\n\
         exit 0\n",
    );
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod flutter");
}

fn ant_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .current_dir(dir)
        .env("RUST_BACKTRACE", "0")
        .env("PATH", path_with_stub_bin(dir))
        // Keep per-user caches (change detection, local results) inside the
        // test project's own `.ant/`, which is never a build input.
        .env("ANT_CACHE_HOME", dir.join(".ant").join("cache-home"))
        .args(args)
        .output()
        .expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn flutter_build_runs_a_web_release_and_reuses_it_locally() {
    let dir = TempDir::new("build");
    flutter_project(dir.path());
    let index = dir.path().join("dist").join("index.html");
    let main_js = dir.path().join("dist").join("main.dart.js");
    let source = dir.path().join("lib").join("main.dart");

    // The first build runs the stub and collects the declared web files.
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");
    assert!(
        stdout.contains("ant build · flutter · release · 1 step"),
        "{stdout:?}"
    );
    assert!(stdout.contains("✓ flutter-build-web-release"), "{stdout:?}");
    assert!(
        !stdout.contains("reused"),
        "the first build executes: {stdout:?}"
    );
    assert!(index.is_file(), "index.html collected: {stdout:?}");
    // The stub copies lib/main.dart into main.dart.js, proving the declared
    // `lib` input flows through to the collected deliverable.
    assert_eq!(
        fs::read_to_string(&main_js).expect("main.dart.js"),
        "void main() {}\n"
    );
    assert_eq!(flutter_runs(dir.path()), 1);
    assert!(dir.path().join(".ant").join("plan.json").is_file());

    // No change: Ant reuses its own local result (D-028); the stub does not run
    // again, and the restored web files land in dist/ again.
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert!(stdout.contains("reused from cache"), "{stdout:?}");
    assert_eq!(flutter_runs(dir.path()), 1);
    assert_eq!(
        fs::read_to_string(&main_js).expect("main.dart.js"),
        "void main() {}\n"
    );

    // A source change rebuilds, and the new content reaches dist/.
    write(&source, "void main() { print(2); }\n");
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert!(
        !stdout.contains("reused from cache"),
        "a changed project rebuilds: {stdout:?}"
    );
    assert_eq!(flutter_runs(dir.path()), 2);
    assert_eq!(
        fs::read_to_string(&main_js).expect("main.dart.js"),
        "void main() { print(2); }\n"
    );

    // --fresh rebuilds even when nothing changed.
    let (code, stdout, stderr) = ant_in(dir.path(), &["build", "--fresh"]);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert_eq!(flutter_runs(dir.path()), 3);
    assert_eq!(
        fs::read_to_string(&main_js).expect("main.dart.js"),
        "void main() { print(2); }\n"
    );
}
