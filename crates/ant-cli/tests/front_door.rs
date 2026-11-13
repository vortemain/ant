//! End-to-end tests for the product front door: `ant build` and `ant init`
//! (roadmap phase P1, decision D-016) on real projects built by real
//! toolchains.

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
            std::env::temp_dir().join(format!("ant-front-{name}-{}-{unique}", std::process::id()));
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

/// A two-member cargo workspace: a library and a binary that uses it.
fn rust_workspace(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"2\"\n",
    );
    write(
        &root.join("core/Cargo.toml"),
        "[package]\nname = \"greet-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &root.join("core/src/lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hello, {name}\") }\n",
    );
    write(
        &root.join("app/Cargo.toml"),
        "[package]\nname = \"greeter\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ngreet-core = { path = \"../core\" }\n",
    );
    write(
        &root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", greet_core::greet(\"ant\")); }\n",
    );
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[test]
fn ant_build_needs_no_configuration_and_collects_runnable_artifacts() {
    let dir = TempDir::new("zero-config");
    rust_workspace(dir.path());
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");
    assert!(
        stdout.contains("ant build · cargo · release · 1 step "),
        "{stdout:?}"
    );
    assert!(stdout.contains("✓ workspace"), "{stdout:?}");
    assert!(
        !stdout.contains("action="),
        "machine lines are hidden by default: {stdout:?}"
    );
    let artifact = dir.path().join("dist").join(exe("greeter"));
    assert!(stdout.contains("Built in"), "{stdout:?}");
    let ran = Command::new(&artifact).output().expect("run artifact");
    assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "hello, ant");
    // Intermediates (rlibs) are not collected; the plan is kept for transparency.
    let collected: Vec<_> = fs::read_dir(dir.path().join("dist"))
        .expect("dist")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(
        collected
            .iter()
            .all(|name| !name.to_string_lossy().ends_with(".rlib")),
        "{collected:?}"
    );
    assert!(dir.path().join(".ant").join("plan.json").is_file());
    // Rebuilding replaces Ant's own artifacts without complaint.
    let (code, _, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "{stderr:?}");
    // --verbose shows the engine's machine lines too.
    let (code, stdout, _) = ant_in(dir.path(), &["build", "--verbose"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("action=workspace status=succeeded"),
        "{stdout:?}"
    );
}

#[test]
fn ant_init_records_the_setup_and_dev_builds_debug() {
    let dir = TempDir::new("init");
    rust_workspace(dir.path());
    fs::create_dir_all(dir.path().join(".git")).expect("git dir");
    write(&dir.path().join(".gitignore"), "target/\n");
    let (code, stdout, stderr) = ant_in(dir.path(), &["init"]);
    assert_eq!(code, 0, "stderr {stderr:?}");
    assert!(
        stdout.contains("Created ant.toml for a cargo project"),
        "{stdout:?}"
    );
    let config = fs::read_to_string(dir.path().join("ant.toml")).expect("ant.toml");
    assert!(
        config.contains("adapter = \"cargo\"") && config.contains("profile = \"release\""),
        "{config}"
    );
    let ignore = fs::read_to_string(dir.path().join(".gitignore")).expect(".gitignore");
    assert!(
        ignore.starts_with("target/\n") && ignore.contains(".ant/") && ignore.contains("dist/"),
        "{ignore}"
    );

    let (code, _, stderr) = ant_in(dir.path(), &["init"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("already exists"), "{stderr:?}");

    let (code, stdout, stderr) = ant_in(dir.path(), &["build", "--dev"]);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");
    assert!(stdout.contains("· debug ·"), "{stdout:?}");
    assert!(
        dir.path()
            .join("target")
            .join("debug")
            .join(exe("greeter"))
            .is_file()
    );
    assert!(dir.path().join("dist").join(exe("greeter")).is_file());
}

#[test]
fn ant_build_never_overwrites_a_directory_it_does_not_own() {
    let dir = TempDir::new("owned");
    rust_workspace(dir.path());
    write(&dir.path().join("dist/notes.txt"), "mine");
    let (code, _, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("was not created by Ant") && stderr.contains("out_dir"),
        "{stderr:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("dist/notes.txt")).expect("kept"),
        "mine"
    );
    // Pointing out_dir elsewhere resolves it.
    write(&dir.path().join("ant.toml"), "out_dir = \"ant-out\"\n");
    let (code, _, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "{stderr:?}");
    assert!(dir.path().join("ant-out").join(exe("greeter")).is_file());
}

#[test]
fn ant_build_explains_what_it_cannot_do() {
    let dir = TempDir::new("errors");
    // Nothing recognizable.
    let (code, _, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no supported project found"), "{stderr:?}");
    // Ambiguous: point at the exact command.
    write(
        &dir.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    );
    write(&dir.path().join("package.json"), "{}");
    let (code, _, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("ant init --adapter cargo"), "{stderr:?}");
    // Targets that do not exist yet say when they arrive.
    let (code, _, stderr) = ant_in(dir.path(), &["build", "android"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("P4"), "{stderr:?}");
    // Typos in ant.toml fail loudly.
    write(&dir.path().join("ant.toml"), "adaptr = \"cargo\"\n");
    let (code, _, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("invalid ant.toml") && stderr.contains("adaptr"),
        "{stderr:?}"
    );
}

#[test]
fn ant_build_runs_an_npm_project_without_configuration() {
    let dir = TempDir::new("npm");
    write(
        &dir.path().join("package.json"),
        "{\"name\":\"webby\",\"version\":\"1.0.0\",\"scripts\":{\"build\":\"node build.js\"}}",
    );
    write(&dir.path().join("src/index.js"), "console.log('webby');\n");
    write(
        &dir.path().join("build.js"),
        "const fs=require('fs');fs.mkdirSync('dist',{recursive:true});fs.copyFileSync('src/index.js','dist/main.js');\n",
    );
    let (code, stdout, stderr) = ant_in(dir.path(), &["build"]);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");
    assert!(stdout.contains("ant build · npm · 1 step"), "{stdout:?}");
    assert!(stdout.contains("wrote its output in place"), "{stdout:?}");
    assert!(dir.path().join("dist").join("main.js").is_file());
}
