//! End-to-end tests for self-hosted remote execution: a real coordinator
//! process on an ephemeral loopback port, a real worker process, and the
//! `ant remote run` client — plus the protocol's security properties.

use ant_remote::auth::Token;
use ant_remote::http::Coordinator;
use ant_remote::protocol::{Entry, EntryKind, JobSpec, PROTOCOL_VERSION};
use serde_json::json;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ant-remote-{name}-{}-{unique}", std::process::id()));
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

/// Kills the process when dropped so failing tests never leak servers.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const CLIENT_TOKEN: &str = "client-token-0123456789abcdefghijklmnopqrstuv";
const WORKER_TOKEN: &str = "worker-token-0123456789abcdefghijklmnopqrstuv";

struct Cluster {
    dir: TempDir,
    url: String,
    _coordinator: Guard,
    _worker: Option<Guard>,
}

impl Cluster {
    fn start(name: &str, with_worker: bool) -> Self {
        let dir = TempDir::new(name);
        fs::write(dir.path().join("client.token"), CLIENT_TOKEN).expect("token");
        fs::write(dir.path().join("worker.token"), WORKER_TOKEN).expect("token");
        let mut coordinator = Command::new(env!("CARGO_BIN_EXE_ant"))
            .args(["coordinator", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(dir.path().join("coord"))
            .arg("--client-token-file")
            .arg(dir.path().join("client.token"))
            .arg("--worker-token-file")
            .arg(dir.path().join("worker.token"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn coordinator");
        let mut line = String::new();
        BufReader::new(coordinator.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read");
        let addr = line
            .split_whitespace()
            .find_map(|part| part.strip_prefix("listening="))
            .unwrap_or_else(|| panic!("no listening line: {line:?}"))
            .to_owned();
        let url = format!("http://{addr}");
        let coordinator = Guard(coordinator);
        let worker = with_worker.then(|| {
            Guard(
                Command::new(env!("CARGO_BIN_EXE_ant"))
                    .args([
                        "worker",
                        "--coordinator",
                        &url,
                        "--id",
                        "test-worker",
                        "--token-file",
                    ])
                    .arg(dir.path().join("worker.token"))
                    .arg("--work-dir")
                    .arg(dir.path().join("work"))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn worker"),
            )
        });
        Cluster {
            dir,
            url,
            _coordinator: coordinator,
            _worker: worker,
        }
    }

    fn client(&self, token: &str) -> Coordinator {
        Coordinator::new(&self.url, Token::new(token).expect("token"), false).expect("client")
    }

    fn remote_run(&self, workspace: &Path, plan: &Path) -> (i32, String, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_ant"))
            .args(["remote", "run", "--coordinator", &self.url, "--token-file"])
            .arg(self.dir.path().join("client.token"))
            .arg("--workspace")
            .arg(workspace)
            .arg("--plan")
            .arg(plan)
            .output()
            .expect("run client");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

fn shell(command: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), command.into()]
    } else {
        vec!["sh".into(), "-c".into(), command.into()]
    }
}

fn action(
    id: &str,
    command: &str,
    inputs: serde_json::Value,
    outputs: serde_json::Value,
    deps: &[&str],
) -> serde_json::Value {
    json!({
        "action": {
            "schema_version": 1, "id": id, "argv": shell(command), "working_directory": ".",
            "inputs": inputs, "outputs": outputs, "environment": [], "cache_policy": "never", "timeout_ms": 60000
        },
        "dependencies": deps
    })
}

fn write_plan(dir: &Path, actions: &[serde_json::Value]) -> PathBuf {
    let path = dir.join("plan.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({ "schema_version": 1, "actions": actions }))
            .expect("json"),
    )
    .expect("plan");
    path
}

#[test]
fn remote_run_executes_on_worker_and_installs_verified_outputs() {
    let cluster = Cluster::start("e2e", true);
    let ws = cluster.dir.path().join("ws");
    fs::create_dir_all(ws.join("src").join("nested")).expect("src");
    fs::write(ws.join("src").join("a.txt"), "alpha").expect("a");
    fs::write(ws.join("src").join("nested").join("b.txt"), "beta").expect("b");
    let (concat, stamp) = if cfg!(windows) {
        (
            "mkdir build 2>nul & copy /b src\\a.txt+src\\nested\\b.txt build\\joined.txt >nul",
            "copy /b build\\joined.txt+build\\joined.txt twice.txt >nul",
        )
    } else {
        (
            "mkdir -p build && cat src/a.txt src/nested/b.txt > build/joined.txt",
            "cat build/joined.txt build/joined.txt > twice.txt",
        )
    };
    let plan = write_plan(
        cluster.dir.path(),
        &[
            action(
                "concat",
                concat,
                json!([{ "path": "src", "kind": "tree" }]),
                json!([{ "path": "build", "kind": "tree" }]),
                &[],
            ),
            action(
                "twice",
                stamp,
                json!([{ "path": "build/joined.txt", "kind": "file" }]),
                json!([{ "path": "twice.txt", "kind": "file" }]),
                &["concat"],
            ),
        ],
    );
    let (code, stdout, stderr) = cluster.remote_run(&ws, &plan);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");
    assert!(
        stdout.contains(
            "action=concat status=succeeded exit_code=0 remote_job=job-1 worker=test-worker"
        ),
        "{stdout:?}"
    );
    assert!(
        stdout.contains("action=twice status=succeeded"),
        "{stdout:?}"
    );
    assert_eq!(
        fs::read_to_string(ws.join("build").join("joined.txt")).expect("joined"),
        "alphabeta"
    );
    assert_eq!(
        fs::read_to_string(ws.join("twice.txt")).expect("twice"),
        "alphabetaalphabeta"
    );
    // The reported digest is Ant's canonical file digest of the installed file.
    let digest = build_digest::digest_file(&ws.join("twice.txt")).expect("digest");
    assert!(
        stdout.contains(&format!("output=twice.txt digest={digest}")),
        "{stdout:?}"
    );
    // The worker leaves no job workspaces behind.
    assert_eq!(
        fs::read_dir(cluster.dir.path().join("work"))
            .expect("work")
            .count(),
        0
    );
}

#[test]
fn remote_execution_enforces_input_declarations() {
    // Locally the undeclared file is visible; remotely it does not exist.
    let cluster = Cluster::start("undeclared", true);
    let ws = cluster.dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    fs::write(ws.join("declared.txt"), "ok").expect("declared");
    fs::write(ws.join("secret.txt"), "undeclared").expect("undeclared");
    let read = if cfg!(windows) {
        "type declared.txt >nul && type secret.txt > out.txt"
    } else {
        "cat declared.txt >/dev/null && cat secret.txt > out.txt"
    };
    let plan = write_plan(
        cluster.dir.path(),
        &[action(
            "sneaky",
            read,
            json!([{ "path": "declared.txt", "kind": "file" }]),
            json!([{ "path": "out.txt", "kind": "file" }]),
            &[],
        )],
    );
    let (code, stdout, _) = cluster.remote_run(&ws, &plan);
    assert_eq!(
        code, 1,
        "the undeclared input must be absent on the worker: {stdout:?}"
    );
    assert!(stdout.contains("action=sneaky status=failed"), "{stdout:?}");
    assert!(!ws.join("out.txt").exists());
}

#[test]
fn coordinator_enforces_auth_roles_and_safe_paths() {
    let cluster = Cluster::start("security", false);
    let digest = format!("sha256:{}", "0".repeat(64));

    // Health is public; everything else needs a valid token.
    let anonymous = cluster.client(&format!("{}-wrong-but-long-enough", "x".repeat(20)));
    let error = anonymous
        .missing(vec![digest.clone()])
        .expect_err("bad token");
    assert!(error.contains("401"), "{error}");

    // A worker token cannot submit jobs; a client token cannot lease work.
    let spec = |path: &str| JobSpec {
        protocol_version: PROTOCOL_VERSION,
        action: serde_json::from_value(json!({
            "schema_version": 1, "id": "x", "argv": ["tool"], "working_directory": ".",
            "inputs": [], "outputs": [], "environment": [], "cache_policy": "never"
        }))
        .expect("action"),
        inputs: vec![Entry {
            path: path.to_owned(),
            kind: EntryKind::Dir,
            digest: None,
        }],
    };
    let worker = cluster.client(WORKER_TOKEN);
    let error = worker
        .post_json::<_, serde_json::Value>("/v1/jobs", &spec("ok"), "submit")
        .expect_err("role");
    assert!(error.contains("403"), "{error}");
    let client = cluster.client(CLIENT_TOKEN);
    let lease = json!({ "protocol_version": 1, "worker_id": "w", "os": "x" });
    let error = client
        .post_json::<_, serde_json::Value>("/v1/lease", &lease, "lease")
        .expect_err("role");
    assert!(error.contains("403"), "{error}");

    // Path traversal and absolute paths are rejected at submission.
    for bad in ["../escape", "/etc", "a/../../b", "C:/x"] {
        let error = client
            .post_json::<_, serde_json::Value>("/v1/jobs", &spec(bad), "submit")
            .expect_err(bad);
        assert!(error.contains("400"), "{bad}: {error}");
    }
    // A well-formed job is accepted (the rejections above are not blanket).
    let accepted = client.post_json::<_, serde_json::Value>("/v1/jobs", &spec("ok"), "submit");
    assert!(accepted.is_ok(), "a valid job is accepted: {accepted:?}");

    // A blob corrupted on the coordinator's disk is never served as valid:
    // the coordinator re-verifies on read (and the client verifies again).
    let stored = client.put_blob(b"precious bytes").expect("upload");
    assert_eq!(
        client.get_blob(&stored.to_string()).expect("download"),
        b"precious bytes"
    );
    let hex = stored.to_string().trim_start_matches("sha256:").to_owned();
    let on_disk = cluster
        .dir
        .path()
        .join("coord")
        .join("blobs")
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::write(&on_disk, b"tampered bytes").expect("tamper");
    let error = client
        .get_blob(&stored.to_string())
        .expect_err("tampered blob");
    assert!(
        error.contains("500") && error.contains("corrupt"),
        "{error}"
    );
}

#[test]
fn transport_guards_refuse_unsafe_configurations() {
    let dir = TempDir::new("guards");
    fs::write(dir.path().join("t"), CLIENT_TOKEN).expect("token");
    let token = dir.path().join("t");
    // The client will not send a token over plain HTTP to a non-loopback host.
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .args([
            "remote",
            "run",
            "--coordinator",
            "http://192.0.2.10:8742",
            "--plan",
            "p.json",
            "--token-file",
        ])
        .arg(&token)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("refusing to send a token over plain HTTP")
    );
    // The coordinator will not bind a public address without the explicit flag.
    let output = Command::new(env!("CARGO_BIN_EXE_ant"))
        .args(["coordinator", "--listen", "0.0.0.0:0", "--data-dir"])
        .arg(dir.path().join("coord"))
        .arg("--client-token-file")
        .arg(&token)
        .arg("--worker-token-file")
        .arg(&token)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to listen on non-loopback"));
}
