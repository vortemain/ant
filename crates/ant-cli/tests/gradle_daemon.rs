//! End-to-end test for the Gradle daemon on the local front door (D-033):
//! `ant build` in an Android-shaped project keeps Gradle's daemon warm
//! between invocations — the wrapper runs without `--no-daemon` and the
//! adapter's `_ANT_DAEMON` marker reaches the action's environment — while
//! `ant.toml` (`[options] daemon = "0"`) restores the hermetic one-shot
//! daemon.
//!
//! The wrapper is a stub script (the same trick as `gradle_build.rs`): it
//! records the command line and the daemon marker it was given, fabricates
//! the APK the adapter declares, and emulates the daemon's home under
//! `.ant/` (where the adapter points `GRADLE_USER_HOME`). No Gradle
//! distribution, Android SDK, or network is needed; `java` on PATH is still
//! required for toolchain identity.

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
            "ant-gradle-daemon-{name}-{}-{unique}",
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

/// The recorded lines of one of the stub wrapper's log files, one line per
/// wrapper run, in order.
fn recorded_lines(root: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(root.join(".ant").join(name))
        .expect("the wrapper must have recorded its run")
        .lines()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// A minimal Android-shaped Gradle project (JetNews's shape): one
/// application module addressed by a version-catalog plugin alias, a local
/// distribution, and a stub wrapper that records what the front door passed.
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

/// The wrapper stub: records its arguments and the `_ANT_DAEMON` marker it
/// received, emulates the daemon's home where `GRADLE_USER_HOME` points,
/// and fabricates the APK the adapter declares.
#[cfg(windows)]
fn write_wrapper_script(root: &Path) {
    write(
        &root.join("gradlew.bat"),
        "@echo off\r\n\
         mkdir app\\build\\outputs\\apk\\debug 2>nul\r\n\
         mkdir .ant\\gradle-home 2>nul\r\n\
         >>.ant\\wrapper-runs echo run\r\n\
         >>.ant\\wrapper-args echo %*\r\n\
         >>.ant\\daemon-marker echo %_ANT_DAEMON%\r\n\
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
         mkdir -p app/build/outputs/apk/debug .ant/gradle-home\n\
         printf 'run\\n' >> .ant/wrapper-runs\n\
         printf '%s\\n' \"$*\" >> .ant/wrapper-args\n\
         printf '%s\\n' \"${_ANT_DAEMON-unset}\" >> .ant/daemon-marker\n\
         cp app/src/main/AndroidManifest.xml app/build/outputs/apk/debug/app-debug.apk\n",
    );
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Build the stub project, assert the wrapper ran with the Gradle daemon
/// (no `--no-daemon`, the `_ANT_DAEMON` marker present, the daemon's home
/// under `.ant/`), then opt out through `ant.toml` and assert the hermetic
/// one-shot daemon comes back — with the APK still collected both times.
#[test]
fn local_gradle_builds_keep_the_daemon_and_ant_toml_can_opt_out() {
    let dir = TempDir::new("daemon");
    android_project(dir.path());

    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(dir.path().join("dist/app-debug.apk").exists());

    let args = recorded_lines(dir.path(), "wrapper-args");
    assert_eq!(args.len(), 1, "the wrapper ran once: {args:?}");
    assert!(
        args[0].contains("assembleDebug"),
        "the daemon variant still runs the task: {}",
        args[0]
    );
    assert!(
        !args[0].contains("--no-daemon"),
        "the front door must let Gradle keep its daemon: {}",
        args[0]
    );
    let markers = recorded_lines(dir.path(), "daemon-marker");
    assert_eq!(
        markers.len(),
        1,
        "the wrapper records its marker per run: {markers:?}"
    );
    assert_eq!(markers[0], "1", "the daemon marker reached the action");
    assert!(
        dir.path().join(".ant/gradle-home").exists(),
        "the daemon's home lives under .ant/ and survives the build"
    );

    // Opt out through ant.toml. The manifest changes too, so local reuse
    // (D-028) misses and Gradle really runs again.
    write(&dir.path().join("ant.toml"), "[options]\ndaemon = \"0\"\n");
    write(
        &dir.path().join("app/src/main/AndroidManifest.xml"),
        "<manifest package=\"stub\"/>\n",
    );
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(dir.path().join("dist/app-debug.apk").exists());

    let args = recorded_lines(dir.path(), "wrapper-args");
    assert_eq!(args.len(), 2, "the second build re-ran Gradle: {args:?}");
    assert!(
        args[1].contains("--no-daemon"),
        "daemon = \"0\" must restore the hermetic one-shot daemon: {}",
        args[1]
    );
    let markers = recorded_lines(dir.path(), "daemon-marker");
    assert_ne!(
        markers[1], "1",
        "the opt-out build must not carry the daemon marker"
    );
}
