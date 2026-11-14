//! End-to-end validation of path independence (decisions D-030 and D-034):
//! one project, two clones in different folders, one shared cache.
//!
//! The dev-profile action bakes its clone's absolute workspace path into
//! CARGO_ENCODED_RUSTFLAGS (the `--remap-path-prefix` root), and rustc embeds
//! that remapping in debug info. With the path-independence marker the action
//! key hashes that value with the workspace path normalized away, and the
//! remap itself keeps the two clones' binaries byte-identical — so the second
//! clone restores the first clone's results instead of rebuilding.

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
            std::env::temp_dir().join(format!("ant-pathin-{name}-{}-{unique}", std::process::id()));
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

/// Run `ant` with an explicit `ANT_CACHE_HOME` so several clones can share
/// one cache — the deployment shape the path-independence marker exists for.
fn ant_with_shared_cache(dir: &Path, cache_home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .current_dir(dir)
        .env("RUST_BACKTRACE", "0")
        .env("ANT_CACHE_HOME", cache_home)
        .args(args)
        .output()
        .expect("spawn ant");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_artifact(dir: &Path) -> String {
    let output = Command::new(dir.join("dist").join(exe("greeter")))
        .output()
        .expect("run artifact");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Copy a checked-out project to another folder: everything a second clone
/// would have, and none of Ant's or cargo's build state (`.ant`, `target`,
/// `dist`).
fn copy_clone(from: &Path, to: &Path) {
    fn step(from: &Path, to: &Path, top: bool) {
        for entry in fs::read_dir(from).expect("read dir") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if top && matches!(name.as_str(), ".ant" | "target" | "dist") {
                continue;
            }
            let target = to.join(&name);
            if entry.file_type().expect("file type").is_dir() {
                fs::create_dir_all(&target).expect("mkdir");
                step(&entry.path(), &target, false);
            } else {
                fs::copy(entry.path(), &target).expect("copy file");
            }
        }
    }
    fs::create_dir_all(to).expect("mkdir");
    step(from, to, true);
}

/// D-034: the dev action's CARGO_ENCODED_RUSTFLAGS value names this clone's
/// absolute path, so the second clone must still reuse the first clone's
/// results — the key normalizes the value — and its artifact must be the
/// same, byte for byte, as the one the first clone built.
#[test]
fn dev_builds_are_reused_across_two_clones_with_one_cache() {
    let cache = TempDir::new("shared-cache");
    let first = TempDir::new("clone-a");
    rust_workspace(first.path());
    let (code, stdout, stderr) =
        ant_with_shared_cache(first.path(), cache.path(), &["build", "--dev"]);
    assert_eq!(code, 0, "{stdout:?} {stderr:?}");
    assert!(
        !stdout.contains("reused"),
        "the first clone executes: {stdout:?}"
    );
    assert_eq!(run_artifact(first.path()), "hello, ant");

    let second = TempDir::new("clone-b");
    copy_clone(first.path(), second.path());
    assert_ne!(
        first
            .path()
            .canonicalize()
            .expect("canonical a")
            .to_string_lossy(),
        second
            .path()
            .canonicalize()
            .expect("canonical b")
            .to_string_lossy(),
        "the test only means something if the clones are in different folders"
    );
    let (code, stdout, stderr) =
        ant_with_shared_cache(second.path(), cache.path(), &["build", "--dev"]);
    assert_eq!(code, 0, "{stdout:?} {stderr:?}");
    assert!(
        stdout.contains("reused from cache"),
        "the second clone restores instead of rebuilding: {stdout:?}"
    );
    assert_eq!(run_artifact(second.path()), "hello, ant");

    let from_first = fs::read(first.path().join("dist").join(exe("greeter"))).expect("artifact a");
    let from_second =
        fs::read(second.path().join("dist").join(exe("greeter"))).expect("artifact b");
    assert_eq!(
        from_first, from_second,
        "the restored artifact must be byte-identical to the built one"
    );
}

/// D-030 had this for release already; keep proving it end to end so a
/// regression in either profile's marker or normalization is caught where it
/// matters, at the front door.
#[test]
fn release_builds_are_reused_across_two_clones_with_one_cache() {
    let cache = TempDir::new("shared-cache");
    let first = TempDir::new("clone-a");
    rust_workspace(first.path());
    let (code, stdout, stderr) = ant_with_shared_cache(first.path(), cache.path(), &["build"]);
    assert_eq!(code, 0, "{stdout:?} {stderr:?}");
    assert!(
        !stdout.contains("reused"),
        "the first clone executes: {stdout:?}"
    );
    assert_eq!(run_artifact(first.path()), "hello, ant");

    let second = TempDir::new("clone-b");
    copy_clone(first.path(), second.path());
    let (code, stdout, stderr) = ant_with_shared_cache(second.path(), cache.path(), &["build"]);
    assert_eq!(code, 0, "{stdout:?} {stderr:?}");
    assert!(
        stdout.contains("reused from cache"),
        "the second clone restores instead of rebuilding: {stdout:?}"
    );
    assert_eq!(run_artifact(second.path()), "hello, ant");
}
