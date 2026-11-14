//! End-to-end test for `ant build` on an Android-shaped Gradle project
//! (P4, D-024): the front door detects the Gradle wrapper, runs
//! `assembleDebug` through the platform launcher (Windows: `cmd /C
//! gradlew.bat`, Unix: `sh gradlew`), collects the debug APK into `dist/`,
//! and reuses its own local result on a no-change rebuild (D-028) instead
//! of running Gradle again.
//!
//! The wrapper is a stub script, so the test needs no Gradle distribution,
//! Android SDK, or network: it fabricates the APK the adapter declares from
//! a real source file, which proves the collected artifact really flows from
//! the declared inputs to `dist/`. The adapter still requires `java` on PATH
//! for toolchain identity (CI runners have it).

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
            std::env::temp_dir().join(format!("ant-gradle-{name}-{}-{unique}", std::process::id()));
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

fn ant_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .current_dir(dir)
        .env("RUST_BACKTRACE", "0")
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

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

/// How many times the wrapper stub ran: every run appends one line to
/// `.ant/wrapper-runs`, so local reuse that skips Gradle stays visible.
fn wrapper_runs(dir: &Path) -> usize {
    fs::read_to_string(dir.join(".ant").join("wrapper-runs"))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// A minimal Android-shaped Gradle project (JetNews's shape): one
/// application module addressed by a version-catalog plugin alias, a local
/// distribution, and a stub wrapper that builds the APK path the adapter
/// declares.
fn android_project(root: &Path) {
    write(
        &root.join("settings.gradle.kts"),
        "rootProject.name = \"stub\"\ninclude(\":app\")\n",
    );
    write(&root.join("build.gradle.kts"), "plugins { }\n");
    write(
        &root.join("app/build.gradle.kts"),
        "plugins { alias(libs.plugins.android.application) }\n",
    );
    write(
        &root.join("app/src/main/AndroidManifest.xml"),
        "<manifest/>\n",
    );
    write(
        &root.join("gradle/wrapper/gradle-wrapper.properties"),
        "distributionUrl=gradle-9.5.0-bin.zip\n",
    );
    // Identity file for the wrapper digest (never executed by the stub).
    write(&root.join("gradle/wrapper/gradle-wrapper.jar"), "stub-jar");
    write_wrapper_script(root);
}

/// The wrapper stub: fabricates `app/build/outputs/apk/debug/app-debug.apk`
/// (the deliverable the adapter declares) from the manifest and records the
/// run so the test can tell execution from reuse.
#[cfg(windows)]
fn write_wrapper_script(root: &Path) {
    write(
        &root.join("gradlew.bat"),
        "@echo off\r\n\
         mkdir app\\build\\outputs\\apk\\debug 2>nul\r\n\
         >>.ant\\wrapper-runs echo run\r\n\
         copy /Y app\\src\\main\\AndroidManifest.xml \
         app\\build\\outputs\\apk\\debug\\app-debug.apk >nul\r\n",
    );
}

#[cfg(unix)]
fn write_wrapper_script(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("gradlew");
    write(
        &script,
        "#!/bin/sh\n\
         mkdir -p app/build/outputs/apk/debug\n\
         cp app/src/main/AndroidManifest.xml \
         app/build/outputs/apk/debug/app-debug.apk\n\
         echo run >> .ant/wrapper-runs\n",
    );
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod gradlew");
}

#[test]
fn ant_build_runs_a_gradle_android_project_and_reuses_it_locally() {
    let dir = TempDir::new("build");
    android_project(dir.path());
    let apk = dir.path().join("dist").join("app-debug.apk");
    let manifest = dir.path().join("app/src/main/AndroidManifest.xml");

    // The first build runs the wrapper and collects the declared APK.
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");
    assert!(stdout.contains("ant build · gradle · 1 step"), "{stdout:?}");
    assert!(stdout.contains("✓ assembleDebug"), "{stdout:?}");
    assert!(
        !stdout.contains("reused"),
        "the first build executes: {stdout:?}"
    );
    assert_eq!(fs::read_to_string(&apk).expect("apk"), "<manifest/>\n");
    assert_eq!(wrapper_runs(dir.path()), 1);
    assert!(dir.path().join(".ant").join("plan.json").is_file());

    // No change: Ant reuses its own local result (D-028); the wrapper does
    // not run again, and the restored APK lands in dist/ again.
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert!(stdout.contains("reused from cache"), "{stdout:?}");
    assert_eq!(wrapper_runs(dir.path()), 1);
    assert_eq!(fs::read_to_string(&apk).expect("apk"), "<manifest/>\n");

    // A source change rebuilds, and the new content reaches dist/.
    write(&manifest, "<manifest version=\"2\"/>\n");
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert!(
        !stdout.contains("reused from cache"),
        "a changed project rebuilds: {stdout:?}"
    );
    assert_eq!(wrapper_runs(dir.path()), 2);
    assert_eq!(
        fs::read_to_string(&apk).expect("apk"),
        "<manifest version=\"2\"/>\n"
    );

    // --fresh rebuilds even when nothing changed.
    let (code, stdout, stderr) = ant_in(dir.path(), &["build", "--fresh"]);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert_eq!(wrapper_runs(dir.path()), 3);
    assert_eq!(
        fs::read_to_string(&apk).expect("apk"),
        "<manifest version=\"2\"/>\n"
    );
}
