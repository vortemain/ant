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
const VIEWER_TOKEN: &str = "viewer-token-0123456789abcdefghijklmnopqrstuv";

struct Cluster {
    dir: TempDir,
    url: String,
    _coordinator: Guard,
    workers: Vec<Guard>,
}

impl Cluster {
    fn start(name: &str, with_worker: bool) -> Self {
        let dir = TempDir::new(name);
        fs::write(dir.path().join("client.token"), CLIENT_TOKEN).expect("token");
        fs::write(dir.path().join("worker.token"), WORKER_TOKEN).expect("token");
        fs::write(dir.path().join("viewer.token"), VIEWER_TOKEN).expect("token");
        let mut coordinator = Command::new(env!("CARGO_BIN_EXE_ant"))
            .args(["coordinator", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(dir.path().join("coord"))
            .arg("--client-token-file")
            .arg(dir.path().join("client.token"))
            .arg("--worker-token-file")
            .arg(dir.path().join("worker.token"))
            .arg("--viewer-token-file")
            .arg(dir.path().join("viewer.token"))
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
        let mut cluster = Cluster {
            dir,
            url,
            _coordinator: coordinator,
            workers: Vec::new(),
        };
        if with_worker {
            cluster.add_worker("test-worker", "work");
        }
        cluster
    }

    fn add_worker(&mut self, id: &str, work_dir: &str) {
        self.workers.push(Guard(
            Command::new(env!("CARGO_BIN_EXE_ant"))
                .args([
                    "worker",
                    "--coordinator",
                    &self.url,
                    "--id",
                    id,
                    "--token-file",
                ])
                .arg(self.dir.path().join("worker.token"))
                .arg("--work-dir")
                .arg(self.dir.path().join(work_dir))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn worker"),
        ));
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
            executable: false,
        }],
        workspace_key: None,
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

#[test]
fn control_plane_read_model_and_viewer_role() {
    let cluster = Cluster::start("control-plane", true);
    let ws = cluster.dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    fs::write(ws.join("in.txt"), "x").expect("input");
    let copy = if cfg!(windows) {
        "copy /Y in.txt out.txt >nul"
    } else {
        "cp in.txt out.txt"
    };
    let plan = write_plan(
        cluster.dir.path(),
        &[action(
            "copy",
            copy,
            json!([{ "path": "in.txt", "kind": "file" }]),
            json!([{ "path": "out.txt", "kind": "file" }]),
            &[],
        )],
    );
    let (code, stdout, stderr) = cluster.remote_run(&ws, &plan);
    assert_eq!(code, 0, "stderr {stderr:?} stdout {stdout:?}");

    let viewer = cluster.client(VIEWER_TOKEN);
    let jobs: ant_remote::protocol::JobList = viewer
        .get_json("/v1/jobs", "list jobs")
        .expect("viewer lists jobs");
    assert_eq!(jobs.jobs.len(), 1, "{jobs:?}");
    let job = &jobs.jobs[0];
    assert_eq!(job.action_id, "copy");
    assert_eq!(job.state, ant_remote::protocol::JobState::Completed);
    assert_eq!(job.succeeded, Some(true));
    assert_eq!(job.exit_code, Some(0));
    assert_eq!(job.worker_id.as_deref(), Some("test-worker"));
    assert_eq!((job.input_count, job.output_count), (1, 1));
    let (submitted, started, finished) = (
        job.submitted_unix_ms,
        job.started_unix_ms.expect("started"),
        job.finished_unix_ms.expect("finished"),
    );
    assert!(
        jobs.coordinator_started_unix_ms <= submitted
            && submitted <= started
            && started <= finished,
        "{job:?}"
    );

    let workers: ant_remote::protocol::WorkerList = viewer
        .get_json("/v1/workers", "list workers")
        .expect("viewer lists workers");
    assert_eq!(workers.workers.len(), 1);
    assert_eq!(workers.workers[0].worker_id, "test-worker");
    assert_eq!(workers.workers[0].os, std::env::consts::OS);
    assert_eq!(workers.workers[0].jobs_reported, 1);
    assert!(workers.workers[0].last_seen_unix_ms <= workers.now_unix_ms);

    let status: ant_remote::protocol::JobStatus = viewer
        .get_json(&format!("/v1/jobs/{}", job.job_id), "status")
        .expect("viewer reads status");
    assert_eq!(status.state, ant_remote::protocol::JobState::Completed);

    // The viewer is strictly read-only.
    let error = viewer
        .missing(vec![format!("sha256:{}", "0".repeat(64))])
        .expect_err("blobs");
    assert!(error.contains("403"), "{error}");
    let error = viewer.put_blob(b"x").expect_err("upload");
    assert!(error.contains("403"), "{error}");
    let lease = json!({ "protocol_version": 1, "worker_id": "v", "os": "x" });
    let error = viewer
        .post_json::<_, serde_json::Value>("/v1/lease", &lease, "lease")
        .expect_err("lease");
    assert!(error.contains("403"), "{error}");
    let error = viewer
        .post_json::<_, serde_json::Value>("/v1/jobs", &json!({}), "submit")
        .expect_err("submit");
    assert!(error.contains("403"), "{error}");
    // Workers cannot read the control plane.
    let error = cluster
        .client(WORKER_TOKEN)
        .get_json::<serde_json::Value>("/v1/jobs", "list")
        .expect_err("worker");
    assert!(error.contains("403"), "{error}");
}

#[test]
fn dashboard_is_static_data_free_and_locked_down() {
    let cluster = Cluster::start("dashboard", false);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    for (path, content_type, marker) in [
        ("/ui", "text/html", "<title>Ant coordinator</title>"),
        ("/", "text/html", "/ui/app.js"),
        ("/ui/app.js", "text/javascript", "textContent"),
        ("/ui/app.css", "text/css", "prefers-color-scheme"),
    ] {
        let mut response = agent
            .get(&format!("{}{path}", cluster.url))
            .call()
            .expect("get");
        assert_eq!(response.status().as_u16(), 200, "{path}");
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        assert!(header("content-type").starts_with(content_type), "{path}");
        let csp = header("content-security-policy");
        assert!(
            csp.contains("default-src 'none'")
                && csp.contains("script-src 'self'")
                && csp.contains("frame-ancestors 'none'"),
            "{path}: {csp}"
        );
        assert_eq!(header("x-content-type-options"), "nosniff");
        let body = response.body_mut().read_to_string().expect("body");
        assert!(body.contains(marker), "{path}");
        assert!(
            !body.contains(CLIENT_TOKEN) && !body.contains("job-"),
            "{path} must not embed data"
        );
    }
    // Without a token the data routes stay closed.
    let response = agent
        .get(&format!("{}/v1/jobs", cluster.url))
        .call()
        .expect("get");
    assert_eq!(response.status().as_u16(), 401);
}

/// Batched blob routes (D-023) keep every guarantee of the single-blob
/// routes: content is verified per blob (one bad blob rejects the whole
/// batch and stores nothing), downloads never return partial sets, framing
/// is validated, and viewers cannot use them.
#[test]
fn batched_blob_routes_verify_everything() {
    use ant_remote::protocol::{BatchGetRequest, encode_frames};
    let cluster = Cluster::start("batch", false);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let post = |path: &str, token: &str, body: Vec<u8>| {
        let mut response = agent
            .post(&format!("{}{path}", cluster.url))
            .header("Authorization", &format!("Bearer {token}"))
            .send(&body[..])
            .expect("post");
        let status = response.status().as_u16();
        let bytes = response.body_mut().read_to_vec().expect("body");
        (status, bytes)
    };
    let good = ant_remote::blobs::digest_bytes(b"good").to_string();
    let other = ant_remote::blobs::digest_bytes(b"other").to_string();

    // One mismatched blob rejects the batch; the good one is not stored.
    let tampered = encode_frames(&[(good.as_str(), b"good"), (other.as_str(), b"not other")]);
    let (status, _) = post("/v1/blobs/batch-put", CLIENT_TOKEN, tampered);
    assert_eq!(status, 400);
    let client = cluster.client(CLIENT_TOKEN);
    assert_eq!(
        client.missing(vec![good.clone()]).expect("missing"),
        vec![good.clone()]
    );

    // Garbage framing and viewers are refused.
    let (status, _) = post("/v1/blobs/batch-put", CLIENT_TOKEN, vec![0, 0, 0]);
    assert_eq!(status, 400);
    let (status, _) = post("/v1/blobs/batch-put", VIEWER_TOKEN, encode_frames(&[]));
    assert_eq!(status, 403);

    // A valid batch round-trips byte for byte, in request order.
    client
        .put_blobs(&[b"good".to_vec(), b"other".to_vec()])
        .expect("batch put");
    let blobs = client
        .get_blobs(&[other.clone(), good.clone()])
        .expect("batch get");
    assert_eq!(blobs, vec![b"other".to_vec(), b"good".to_vec()]);

    // A download naming a missing blob fails instead of returning a subset.
    let absent = ant_remote::blobs::digest_bytes(b"absent").to_string();
    let request = serde_json::to_vec(&BatchGetRequest {
        digests: vec![good.clone(), absent],
    })
    .expect("json");
    let (status, _) = post("/v1/blobs/batch-get", CLIENT_TOKEN, request.clone());
    assert_eq!(status, 404);
    let (status, _) = post("/v1/blobs/batch-get", VIEWER_TOKEN, request);
    assert_eq!(status, 403);
}

/// The Gradle task cache (D-025): only workers write, anyone with a client
/// or worker token reads, viewers are refused, keys and tools are checked.
#[test]
fn task_cache_routes_are_worker_written() {
    let cluster = Cluster::start("task-cache", false);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let call = |method: &str, path: &str, token: &str, body: &[u8]| {
        let url = format!("{}{path}", cluster.url);
        let auth = format!("Bearer {token}");
        let mut response = match method {
            "PUT" => agent
                .put(&url)
                .header("Authorization", &auth)
                .send(body)
                .expect("put"),
            _ => agent
                .get(&url)
                .header("Authorization", &auth)
                .call()
                .expect("get"),
        };
        let status = response.status().as_u16();
        (status, response.body_mut().read_to_vec().expect("body"))
    };
    let path = "/v1/task-cache/gradle/0123456789abcdef";
    assert_eq!(call("GET", path, CLIENT_TOKEN, b"").0, 404, "miss");
    assert_eq!(
        call("PUT", path, CLIENT_TOKEN, b"poison").0,
        403,
        "clients never write"
    );
    assert_eq!(
        call("PUT", path, VIEWER_TOKEN, b"poison").0,
        403,
        "viewers never write"
    );
    assert_eq!(call("PUT", path, WORKER_TOKEN, b"task output").0, 201);
    assert_eq!(
        call("GET", path, CLIENT_TOKEN, b""),
        (200, b"task output".to_vec())
    );
    assert_eq!(
        call("GET", path, VIEWER_TOKEN, b"").0,
        403,
        "viewers never read blobs"
    );
    assert_eq!(
        call("PUT", "/v1/task-cache/gradle/not-hex", WORKER_TOKEN, b"x").0,
        400
    );
    assert_eq!(
        call(
            "PUT",
            "/v1/task-cache/other/0123456789abcdef",
            WORKER_TOKEN,
            b"x"
        )
        .0,
        404,
        "only known tools"
    );
    // The typed client used by workers round-trips too.
    let worker = cluster.client(WORKER_TOKEN);
    worker
        .task_cache_put("gradle", "fedcba9876543210", b"second")
        .expect("put");
    assert_eq!(
        worker
            .task_cache_get("gradle", "fedcba9876543210")
            .expect("get"),
        Some(b"second".to_vec())
    );
    assert_eq!(
        worker
            .task_cache_get("gradle", "aaaaaaaaaaaa")
            .expect("miss"),
        None
    );
}

/// D-048: with a workspace key the worker keeps the project's workspace
/// between jobs (undeclared state such as build caches survives, changed
/// inputs still arrive); without one every job starts fresh.
#[test]
fn kept_workspaces_preserve_incremental_state_between_jobs() {
    let cluster = Cluster::start("sticky", true);
    let project = TempDir::new("sticky-project");
    fs::write(project.path().join("in.txt"), "first\n").expect("input");
    let command = if cfg!(windows) {
        "type in.txt >> state.txt & copy /Y state.txt out.txt >nul"
    } else {
        "cat in.txt >> state.txt; cp state.txt out.txt"
    };
    let plan: build_graph::PlanFile = serde_json::from_value(json!({
        "schema_version": 1,
        "actions": [action(
            "accumulate",
            command,
            json!([{ "path": "in.txt", "kind": "file" }]),
            json!([{ "path": "out.txt", "kind": "file" }]),
            &[],
        )]
    }))
    .expect("plan");
    let graph = plan.to_graph().expect("graph");
    let client = cluster.client(CLIENT_TOKEN);
    let workspace = project.path().canonicalize().expect("workspace");
    let run = |key: Option<&str>| {
        let code = ant_remote::client::run_plan_in(
            &client,
            &graph,
            &workspace,
            std::time::Duration::from_millis(50),
            key,
            None,
        )
        .expect("run");
        assert_eq!(code, 0);
        fs::read_to_string(project.path().join("out.txt"))
            .expect("output")
            .lines()
            .map(|line| line.trim().to_owned())
            .collect::<Vec<_>>()
    };
    let key = Some("0123456789abcdef0123456789abcdef");
    assert_eq!(run(key), vec!["first"]);
    assert_eq!(run(key), vec!["first", "first"], "state kept between jobs");
    fs::write(project.path().join("in.txt"), "edited\n").expect("edit");
    assert_eq!(
        run(key),
        vec!["first", "first", "edited"],
        "a changed input reaches the kept workspace"
    );
    assert_eq!(run(None), vec!["edited"], "no key: a fresh workspace");
    let refused = ant_remote::client::run_plan_in(
        &client,
        &graph,
        &workspace,
        std::time::Duration::from_millis(50),
        Some("../../escape"),
        None,
    )
    .expect("run");
    assert_ne!(refused, 0, "an invalid key is refused by the worker");
}

/// D-049: a job's tool output streams from the worker through the
/// coordinator to the client while the job runs, in order and complete.
#[test]
fn job_output_streams_to_the_client() {
    let cluster = Cluster::start("live-output", true);
    let project = TempDir::new("live-output-project");
    let command = if cfg!(windows) {
        "echo first line & ping -n 2 127.0.0.1 >nul & echo second line 1>&2 & echo done> out.txt"
    } else {
        "echo first line; sleep 1; echo second line >&2; echo done > out.txt"
    };
    let plan: build_graph::PlanFile = serde_json::from_value(json!({
        "schema_version": 1,
        "actions": [action(
            "talk",
            command,
            json!([]),
            json!([{ "path": "out.txt", "kind": "file" }]),
            &[],
        )]
    }))
    .expect("plan");
    let graph = plan.to_graph().expect("graph");
    let client = cluster.client(CLIENT_TOKEN);
    let workspace = project.path().canonicalize().expect("workspace");
    let seen = std::sync::Mutex::new(Vec::<String>::new());
    let handler = |text: &str| seen.lock().expect("lock").push(text.to_owned());
    let code = ant_remote::client::run_plan_in(
        &client,
        &graph,
        &workspace,
        std::time::Duration::from_millis(50),
        None,
        Some(&handler),
    )
    .expect("run");
    assert_eq!(code, 0);
    let chunks = seen.into_inner().expect("lock");
    let all: String = chunks.concat();
    let first = all
        .find("first line")
        .unwrap_or_else(|| panic!("first line in {all:?}"));
    let second = all
        .find("second line")
        .unwrap_or_else(|| panic!("second line in {all:?}"));
    assert!(first < second, "{all:?}");
    assert!(
        chunks.len() >= 2,
        "output arrived while the job ran, not only at the end: {chunks:?}"
    ); // Only the worker holding a running job's lease may add output; any
    // reader may read it.
    let jobs: ant_remote::protocol::JobList = client.get_json("/v1/jobs", "jobs").expect("jobs");
    let job = &jobs.jobs[0].job_id;
    let chunk = ant_remote::protocol::LogChunk {
        worker_id: "intruder".into(),
        text: "forged".into(),
    };
    let path = format!("/v1/jobs/{job}/log");
    let error = client
        .post_json::<_, serde_json::Value>(&path, &chunk, "log")
        .expect_err("clients cannot write output");
    assert!(error.contains("403"), "{error}");
    let error = cluster
        .client(WORKER_TOKEN)
        .post_json::<_, serde_json::Value>(&path, &chunk, "log")
        .expect_err("a finished job takes no more output");
    assert!(error.contains("409"), "{error}");
    let log: ant_remote::protocol::JobLog = cluster
        .client(VIEWER_TOKEN)
        .get_json(&format!("{path}?from=0"), "log")
        .expect("viewers read output");
    assert!(log.text.contains("second line") && !log.text.contains("forged"));
    let error = client
        .get_json::<ant_remote::protocol::JobLog>(&format!("{path}?from=x"), "log")
        .expect_err("bad offset");
    assert!(error.contains("400"), "{error}");
}

/// D-050: independent actions run on different workers at the same time;
/// a dependent action waits for both and sees their outputs; with several
/// actions each output line names its action.
#[test]
fn independent_actions_run_in_parallel_on_several_workers() {
    let mut cluster = Cluster::start("parallel", true);
    cluster.add_worker("second-worker", "work-2");
    let project = TempDir::new("parallel-project");
    let (slow_a, slow_b, join) = if cfg!(windows) {
        (
            "ping -n 4 127.0.0.1 >nul & echo from a> a.txt & echo a done",
            "ping -n 4 127.0.0.1 >nul & echo from b> b.txt & echo b done",
            "copy /b a.txt+b.txt both.txt >nul",
        )
    } else {
        (
            "sleep 3; echo from a > a.txt; echo a done",
            "sleep 3; echo from b > b.txt; echo b done",
            "cat a.txt b.txt > both.txt",
        )
    };
    let plan: build_graph::PlanFile = serde_json::from_value(json!({
        "schema_version": 1,
        "actions": [
            action("a", slow_a, json!([]), json!([{ "path": "a.txt", "kind": "file" }]), &[]),
            action("b", slow_b, json!([]), json!([{ "path": "b.txt", "kind": "file" }]), &[]),
            action(
                "join",
                join,
                json!([{ "path": "a.txt", "kind": "file" }, { "path": "b.txt", "kind": "file" }]),
                json!([{ "path": "both.txt", "kind": "file" }]),
                &["a", "b"],
            ),
        ]
    }))
    .expect("plan");
    let graph = plan.to_graph().expect("graph");
    let client = cluster.client(CLIENT_TOKEN);
    // Both workers registered before timing anything.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let workers: serde_json::Value =
            client.get_json("/v1/workers", "workers").expect("workers");
        if workers["workers"].as_array().map_or(0, Vec::len) >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "workers never registered"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let workspace = project.path().canonicalize().expect("workspace");
    let seen = std::sync::Mutex::new(String::new());
    let handler = |text: &str| seen.lock().expect("lock").push_str(text);
    let started = std::time::Instant::now();
    let code = ant_remote::client::run_plan_in(
        &client,
        &graph,
        &workspace,
        std::time::Duration::from_millis(50),
        None,
        Some(&handler),
    )
    .expect("run");
    let elapsed = started.elapsed();
    assert_eq!(code, 0);
    let both = fs::read_to_string(project.path().join("both.txt")).expect("joined");
    assert!(
        both.contains("from a") && both.contains("from b"),
        "{both:?}"
    );
    let jobs: ant_remote::protocol::JobList = client.get_json("/v1/jobs", "jobs").expect("jobs");
    let worker_of = |id: &str| {
        jobs.jobs
            .iter()
            .find(|job| job.action_id == id)
            .and_then(|job| job.worker_id.clone())
            .expect("worker")
    };
    assert_ne!(
        worker_of("a"),
        worker_of("b"),
        "a and b ran on different workers"
    );
    // Sequentially a and b alone take 6 s.
    assert!(
        elapsed < std::time::Duration::from_millis(5500),
        "a and b overlapped: {elapsed:?}"
    );
    let seen = seen.into_inner().expect("lock");
    assert!(
        seen.contains("[a] a done") && seen.contains("[b] b done"),
        "{seen:?}"
    );
}

/// D-050: in a kept workspace, syncing one action's inputs never deletes
/// another action's inputs.
#[test]
fn kept_workspaces_keep_other_actions_inputs() {
    let cluster = Cluster::start("sticky-actions", true);
    let project = TempDir::new("sticky-actions-project");
    fs::write(project.path().join("a.txt"), "a\n").expect("input");
    fs::write(project.path().join("b.txt"), "b\n").expect("input");
    let check = if cfg!(windows) {
        "if exist a.txt (copy /Y b.txt out-b.txt >nul) else (exit 3)"
    } else {
        "test -f a.txt && cp b.txt out-b.txt"
    };
    let copy_a = if cfg!(windows) {
        "copy /Y a.txt out-a.txt >nul"
    } else {
        "cp a.txt out-a.txt"
    };
    let plan: build_graph::PlanFile = serde_json::from_value(json!({
        "schema_version": 1,
        "actions": [
            action("a", copy_a, json!([{ "path": "a.txt", "kind": "file" }]),
                json!([{ "path": "out-a.txt", "kind": "file" }]), &[]),
            action("b", check, json!([{ "path": "b.txt", "kind": "file" }]),
                json!([{ "path": "out-b.txt", "kind": "file" }]), &["a"]),
        ]
    }))
    .expect("plan");
    let graph = plan.to_graph().expect("graph");
    let client = cluster.client(CLIENT_TOKEN);
    let workspace = project.path().canonicalize().expect("workspace");
    for _ in 0..2 {
        let code = ant_remote::client::run_plan_in(
            &client,
            &graph,
            &workspace,
            std::time::Duration::from_millis(50),
            Some("00112233445566778899aabbccddeeff"),
            None,
        )
        .expect("run");
        assert_eq!(code, 0, "b still finds a's input in the kept workspace");
    }
}
