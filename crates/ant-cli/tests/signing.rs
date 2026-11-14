//! Android signing keys (P4, D-033): import, status, and export through the
//! CLI against a real keystore made with the JDK's `keytool`. Building a
//! signed APK needs the Android SDK and is verified by hand (see
//! docs/plan.md, P4.3); these tests cover the key store itself and that
//! passwords never leak into it in plain text.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ant-signing-{name}-{}-{unique}",
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

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

/// A Gradle wrapper project with one Android app module (never built here).
fn android_project(root: &Path) {
    write(
        &root.join("settings.gradle.kts"),
        "rootProject.name = \"Demo\"\ninclude(\":app\")\n",
    );
    write(&root.join("build.gradle.kts"), "plugins { }\n");
    let wrapper = if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    };
    write(&root.join(wrapper), "exit 1\n");
    write(&root.join("gradle/wrapper/gradle-wrapper.jar"), "jar");
    write(
        &root.join("gradle/wrapper/gradle-wrapper.properties"),
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.14.3-bin.zip\n",
    );
    write(
        &root.join("app/build.gradle.kts"),
        "plugins { id(\"com.android.application\") }\nandroid {\n    namespace = \"dev.ant.demo\"\n    defaultConfig { applicationId = \"dev.ant.demo\" }\n}\n",
    );
}

fn ant(dir: &Path, data: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ant"));
    command
        .current_dir(dir)
        .env("ANT_DATA_HOME", data)
        .env("ANT_CACHE_HOME", data.join("cache"))
        .env_remove("ANT_SIGNING_STORE_PASSWORD")
        .env_remove("ANT_SIGNING_KEY_PASSWORD")
        .args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn keytool_available() -> bool {
    Command::new("keytool").arg("-help").output().is_ok()
        && Command::new("java").arg("-version").output().is_ok()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn files_below(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_below(&path, found);
        } else {
            found.push(path);
        }
    }
}

#[test]
fn signing_keys_import_report_and_export_without_leaking_passwords() {
    if !keytool_available() {
        eprintln!("skipping: no JDK (keytool/java) on PATH");
        return;
    }
    let project = TempDir::new("project");
    let data = TempDir::new("data");
    let outside = TempDir::new("outside");
    android_project(project.path());

    let (code, text) = ant(project.path(), data.path(), &["signing", "status"], &[]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("dev.ant.demo: no signing key yet"), "{text}");

    // A keystore made outside Ant, as a developer would bring one.
    let password = "imported-store-password-2026";
    let keystore = outside.path().join("mine.p12");
    let made = Command::new("keytool")
        .args([
            "-genkeypair",
            "-keystore",
            &keystore.to_string_lossy(),
            "-storetype",
            "PKCS12",
            "-alias",
            "mykey",
            "-keyalg",
            "RSA",
            "-keysize",
            "2048",
            "-validity",
            "365",
            "-dname",
            "CN=dev.ant.demo",
            "-storepass:env",
            "TEST_PASSWORD",
            "-keypass:env",
            "TEST_PASSWORD",
            "-noprompt",
        ])
        .env("TEST_PASSWORD", password)
        .output()
        .expect("keytool");
    assert!(made.status.success(), "{made:?}");

    let keystore_arg = keystore.to_string_lossy().into_owned();
    let import = [
        "signing",
        "import",
        "--keystore",
        keystore_arg.as_str(),
        "--alias",
        "mykey",
    ];
    let (code, text) = ant(project.path(), data.path(), &import, &[]);
    assert_ne!(code, 0, "import without a password must fail: {text}");
    let (code, text) = ant(
        project.path(),
        data.path(),
        &import,
        &[("ANT_SIGNING_STORE_PASSWORD", "wrong password")],
    );
    assert_ne!(code, 0, "a wrong password must fail: {text}");
    let (code, text) = ant(
        project.path(),
        data.path(),
        &import,
        &[("ANT_SIGNING_STORE_PASSWORD", password)],
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        !text.contains(password),
        "output never shows the password: {text}"
    );

    let (code, text) = ant(project.path(), data.path(), &["signing", "status"], &[]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("upload key 'mykey'"), "{text}");
    let fingerprint_line = text
        .lines()
        .find(|line| line.contains("certificate SHA-256"))
        .expect("fingerprint line");
    assert_eq!(
        fingerprint_line.split_whitespace().last().map(str::len),
        Some(64),
        "{text}"
    );
    assert!(!text.contains(password), "{text}");

    // The stored key never holds the password in plain text where Ant can
    // encrypt it (Windows DPAPI); elsewhere it is a 0600 file by design.
    let mut stored = Vec::new();
    files_below(&data.path().join("signing"), &mut stored);
    assert!(stored.iter().any(|path| path.ends_with("key.json")));
    if cfg!(windows) {
        for path in &stored {
            let bytes = fs::read(path).expect("read stored file");
            assert!(
                !contains_bytes(&bytes, password.as_bytes()),
                "{} holds the password in plain text",
                path.display()
            );
        }
    }

    // Export is the explicit backup: it does contain the password.
    let backup = outside.path().join("backup");
    let backup_arg = backup.to_string_lossy().into_owned();
    let (code, text) = ant(
        project.path(),
        data.path(),
        &["signing", "export", "--out", backup_arg.as_str()],
        &[],
    );
    assert_eq!(code, 0, "{text}");
    let notes = fs::read_to_string(backup.join("signing.txt")).expect("notes");
    assert!(
        notes.contains(password) && notes.contains("alias: mykey"),
        "{notes}"
    );
    assert!(backup.join("upload.keystore").is_file());
    let (code, _) = ant(
        project.path(),
        data.path(),
        &["signing", "export", "--out", backup_arg.as_str()],
        &[],
    );
    assert_ne!(code, 0, "export never overwrites an existing backup");

    // A second import for the same app is refused.
    let (code, text) = ant(
        project.path(),
        data.path(),
        &import,
        &[("ANT_SIGNING_STORE_PASSWORD", password)],
    );
    assert_ne!(code, 0, "{text}");
}

#[test]
fn release_and_android_target_need_an_android_project() {
    let project = TempDir::new("not-android");
    let data = TempDir::new("not-android-data");
    write(
        &project.path().join("package.json"),
        "{ \"name\": \"web\", \"scripts\": { \"build\": \"node -e 0\" } }\n",
    );
    let (code, text) = ant(project.path(), data.path(), &["build", "android"], &[]);
    assert_ne!(code, 0);
    assert!(text.contains("needs an Android"), "{text}");
    let (code, text) = ant(
        project.path(),
        data.path(),
        &["build", "--dev", "--release"],
        &[],
    );
    assert_ne!(code, 0);
    assert!(text.contains("choose one of --dev and --release"), "{text}");
    let (code, text) = ant(project.path(), data.path(), &["signing", "status"], &[]);
    assert_ne!(code, 0);
    assert!(text.contains("not one"), "{text}");
}
