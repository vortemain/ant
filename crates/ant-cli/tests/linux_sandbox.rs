//! Linux sandbox (bubblewrap, decision D-018) end-to-end tests. They run only
//! on Linux; with `ANT_REQUIRE_BWRAP=1` (set in CI) a missing or unusable
//! sandbox is a failure, otherwise the tests skip with a printed reason.
#![cfg(target_os = "linux")]

use serde_json::json;
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
            std::env::temp_dir().join(format!("ant-linux-{name}-{}-{unique}", std::process::id()));
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

fn run_ant(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
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

/// Whether the sandbox can run here. A missing `bwrap` skips the test unless
/// `ANT_REQUIRE_BWRAP` is set, in which case it fails.
fn sandbox_ready() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path).any(|dir| dir.join("bwrap").is_file());
    if !found {
        assert!(
            std::env::var_os("ANT_REQUIRE_BWRAP").is_none(),
            "ANT_REQUIRE_BWRAP is set but bwrap is not installed"
        );
        eprintln!("skipping: bubblewrap (bwrap) is not installed");
    }
    found
}

fn sh(workspace: &str, sandbox: bool, script: &str) -> (i32, String, String) {
    let mut args = vec!["run", "--workspace", workspace];
    if sandbox {
        args.push("--sandbox");
    }
    args.extend(["--", "sh", "-c", script]);
    run_ant(&args)
}

#[test]
fn linux_sandbox_confines_writes_and_has_no_network() {
    if !sandbox_ready() {
        return;
    }
    let dir = TempDir::new("confine");
    let ws = dir.path().to_string_lossy().into_owned();

    // Writes inside the workspace succeed (even though it lives under /tmp,
    // which is otherwise a private tmpfs inside the sandbox).
    let (code, stdout, stderr) = sh(&ws, true, "echo confined > marker.txt");
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert_eq!(
        fs::read_to_string(dir.path().join("marker.txt"))
            .expect("marker")
            .trim(),
        "confined"
    );
    // The sandbox truthfully declares both capabilities: only plan-level
    // blockers remain for this ad-hoc action.
    assert!(
        stdout.contains("blockers=[PolicyNever, MissingToolchainIdentity]"),
        "{stdout:?}"
    );

    // Writes outside the workspace fail and leave nothing behind.
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let outside = home.join(format!(
        "ant-escape-{}-{}.txt",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let (code, _, _) = sh(
        &ws,
        true,
        &format!("echo escaped > '{}'", outside.display()),
    );
    assert_ne!(code, 0, "writing outside the workspace must fail");
    assert!(!outside.exists(), "no escape file may appear");

    // No network: the same command connects without the sandbox (control)
    // and fails inside it; name resolution fails too (no resolver).
    let fetch = "curl -sS -m 10 -o /dev/null https://1.1.1.1/";
    let (code, _, stderr) = sh(&ws, false, fetch);
    assert_eq!(
        code, 0,
        "control: the runner must reach the network: {stderr:?}"
    );
    let (code, _, _) = sh(&ws, true, fetch);
    assert_ne!(code, 0, "the sandbox must have no network");
    let (code, _, _) = sh(&ws, true, "getent hosts example.com");
    assert_ne!(code, 0, "the sandbox must not resolve names");
}

#[test]
fn linux_sandbox_cache_reuses_and_invalidates() {
    if !sandbox_ready() {
        return;
    }
    let dir = TempDir::new("cache");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    fs::write(ws.join("in.txt"), "hello cache").expect("input");
    let shell = fs::canonicalize("/bin/sh").expect("resolve /bin/sh");
    let digest = build_digest::digest_file(&shell).expect("digest sh");
    let plan = json!({
        "schema_version": 1,
        "actions": [{
            "action": {
                "schema_version": 1,
                "id": "copy",
                "argv": [shell.to_string_lossy(), "-c", "cp in.txt out.txt"],
                "working_directory": ".",
                "inputs": [{ "path": "in.txt", "kind": "file" }],
                "outputs": [{ "path": "out.txt", "kind": "file" }],
                "environment": [],
                "cache_policy": "eligible",
                // Truthful identity: the real digest of the program that runs.
                "toolchain": { "name": "sh", "version": "system", "executable_digest": digest.to_string() },
                "timeout_ms": 60000
            },
            "dependencies": []
        }]
    });
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_vec_pretty(&plan).expect("json")).expect("plan");
    let (ws_s, plan_s) = (
        ws.to_string_lossy().into_owned(),
        plan_path.to_string_lossy().into_owned(),
    );
    let cache = dir.path().join("cache").to_string_lossy().into_owned();
    let run = |sandbox: bool| {
        let mut args = vec![
            "run",
            "--workspace",
            &ws_s,
            "--plan",
            &plan_s,
            "--cache-dir",
            &cache,
        ];
        if sandbox {
            args.push("--sandbox");
        }
        run_ant(&args)
    };

    // Control: unsandboxed, the gate stays closed.
    let (code, stdout, _) = run(false);
    assert_eq!(code, 0);
    assert!(stdout.contains("NetworkIsolationUnavailable"), "{stdout:?}");

    let (code, stdout, stderr) = run(true);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert!(
        stdout.contains("action=copy status=succeeded")
            && stdout.contains("cache_eligible=true blockers=[]"),
        "{stdout:?}"
    );

    fs::remove_file(ws.join("out.txt")).expect("delete output");
    let (code, stdout, _) = run(true);
    assert_eq!(code, 0);
    assert!(stdout.contains("action=copy status=reused"), "{stdout:?}");
    assert_eq!(
        fs::read_to_string(ws.join("out.txt")).expect("restored"),
        "hello cache"
    );

    fs::write(ws.join("in.txt"), "changed input").expect("change");
    let (code, stdout, _) = run(true);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("action=copy status=succeeded"),
        "a changed input must rebuild: {stdout:?}"
    );
    assert_eq!(
        fs::read_to_string(ws.join("out.txt")).expect("rebuilt"),
        "changed input"
    );
}

/// Kills a spawned server/worker when dropped.
struct Guard(std::process::Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The shared remote cache (D-019): a sandboxed Linux worker proves a
/// result once; afterwards the same action is reused from the coordinator —
/// including from a different checkout at a different path with different
/// file timestamps — and changed content executes again.
#[test]
fn remote_cache_reuses_across_workspaces_and_timestamps() {
    use std::io::BufRead as _;
    if !sandbox_ready() {
        return;
    }
    let dir = TempDir::new("remote-cache");
    let token = |name: &str, value: &str| {
        let path = dir.path().join(name);
        fs::write(&path, value).expect("token");
        path
    };
    let client_token = token(
        "client.token",
        "client-token-0123456789abcdefghijklmnopqrstuv",
    );
    let worker_token = token(
        "worker.token",
        "worker-token-0123456789abcdefghijklmnopqrstuv",
    );
    let mut coordinator = Command::new(env!("CARGO_BIN_EXE_ant"))
        .args(["coordinator", "--listen", "127.0.0.1:0", "--data-dir"])
        .arg(dir.path().join("coord"))
        .arg("--client-token-file")
        .arg(&client_token)
        .arg("--worker-token-file")
        .arg(&worker_token)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("coordinator");
    let mut line = String::new();
    std::io::BufReader::new(coordinator.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("listening line");
    let _coordinator = Guard(coordinator);
    let addr = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix("listening="))
        .expect("address");
    let url = format!("http://{addr}");
    let _worker = Guard(
        Command::new(env!("CARGO_BIN_EXE_ant"))
            .args([
                "worker",
                "--coordinator",
                &url,
                "--id",
                "linux-worker",
                "--sandbox",
                "--token-file",
            ])
            .arg(&worker_token)
            .arg("--work-dir")
            .arg(dir.path().join("work"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("worker"),
    );

    let shell = fs::canonicalize("/bin/sh").expect("resolve /bin/sh");
    let digest = build_digest::digest_file(&shell).expect("digest sh");
    let plan = json!({
        "schema_version": 1,
        "actions": [{
            "action": {
                "schema_version": 1, "id": "copy",
                "argv": [shell.to_string_lossy(), "-c", "cp in.txt out.txt"],
                "working_directory": ".",
                "inputs": [{ "path": "in.txt", "kind": "file" }],
                "outputs": [{ "path": "out.txt", "kind": "file" }],
                "environment": [], "cache_policy": "eligible",
                "toolchain": { "name": "sh", "version": "system", "executable_digest": digest.to_string() },
                "timeout_ms": 60000
            },
            "dependencies": []
        }]
    });
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_vec_pretty(&plan).expect("json")).expect("plan");
    let remote_run = |ws: &Path| {
        run_ant(&[
            "remote",
            "run",
            "--coordinator",
            &url,
            "--token-file",
            &client_token.to_string_lossy(),
            "--workspace",
            &ws.to_string_lossy(),
            "--plan",
            &plan_path.to_string_lossy(),
        ])
    };

    let ws_a = dir.path().join("checkout-a");
    fs::create_dir_all(&ws_a).expect("ws a");
    fs::write(ws_a.join("in.txt"), "hello remote cache").expect("input");
    let (code, stdout, stderr) = remote_run(&ws_a);
    assert_eq!(code, 0, "stdout {stdout:?} stderr {stderr:?}");
    assert!(
        stdout.contains("action=copy status=succeeded"),
        "first build executes: {stdout:?}"
    );

    fs::remove_file(ws_a.join("out.txt")).expect("delete output");
    let (code, stdout, _) = remote_run(&ws_a);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("action=copy status=reused"),
        "same workspace reuses: {stdout:?}"
    );
    assert_eq!(
        fs::read_to_string(ws_a.join("out.txt")).expect("restored"),
        "hello remote cache"
    );

    // A different checkout: other path, files written later (new mtimes).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let ws_b = dir.path().join("elsewhere").join("checkout-b");
    fs::create_dir_all(&ws_b).expect("ws b");
    fs::write(ws_b.join("in.txt"), "hello remote cache").expect("input");
    let (code, stdout, _) = remote_run(&ws_b);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("action=copy status=reused"),
        "a different path and timestamps must still reuse: {stdout:?}"
    );
    assert_eq!(
        fs::read_to_string(ws_b.join("out.txt")).expect("restored"),
        "hello remote cache"
    );

    fs::write(ws_b.join("in.txt"), "different content").expect("change");
    let (code, stdout, _) = remote_run(&ws_b);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("action=copy status=succeeded"),
        "changed content executes: {stdout:?}"
    );
    assert_eq!(
        fs::read_to_string(ws_b.join("out.txt")).expect("rebuilt"),
        "different content"
    );
}
