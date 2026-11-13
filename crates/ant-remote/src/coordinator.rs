//! The coordinator: blob store + job queue behind an authenticated HTTP API.
//!
//! Routes (protocol v1; see `docs/remote/protocol-v1.md`):
//!
//! | Method | Path                      | Role          |
//! |--------|---------------------------|---------------|
//! | GET    | `/v1/health`              | none          |
//! | POST   | `/v1/blobs/missing`       | client/worker |
//! | PUT    | `/v1/blobs/{digest}`      | client/worker |
//! | GET    | `/v1/blobs/{digest}`      | client/worker |
//! | POST   | `/v1/jobs`                | client        |
//! | GET    | `/v1/jobs/{id}`           | client        |
//! | POST   | `/v1/lease`               | worker        |
//! | POST   | `/v1/jobs/{id}/result`    | worker (the lease holder) |
//! | GET    | `/v1/cache/{key}`         | client/worker |
//! | PUT    | `/v1/cache/{key}`         | worker (D-019: only workers write) |
//! | GET    | `/v1/task-cache/{tool}/{key}` | client/worker (D-025) |
//! | PUT    | `/v1/task-cache/{tool}/{key}` | worker (D-025: only workers write) |
//! | GET    | `/v1/jobs`                | client/viewer |
//! | GET    | `/v1/workers`             | client/viewer |
//! | GET    | `/`, `/ui`, `/ui/app.*`   | none (static dashboard; contains no data) |
//!
//! The optional viewer token is read-only: list and status routes only. Every
//! response carries a restrictive Content-Security-Policy and related
//! headers because browsers talk to the coordinator through the dashboard.
//!
//! Jobs live in memory: a coordinator restart loses queued and running jobs
//! (blobs persist on disk). A worker that stops reporting loses its lease
//! after the action's timeout (or one hour) plus a grace period; the job is
//! requeued once and then failed.

use crate::auth::Token;
use crate::blobs::BlobStore;
use crate::protocol::{
    CacheEntry, CacheStoreRequest, Health, JobList, JobResult, JobSpec, JobState, JobStatus,
    JobSummary, LeaseRequest, LeaseResponse, MissingRequest, MissingResponse, PROTOCOL_VERSION,
    SubmitResponse, WorkerList, WorkerSummary, validate_entries,
};
use build_digest::ContentDigest;
use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct CoordinatorConfig {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub client_token: Token,
    pub worker_token: Token,
    /// Optional read-only token for the dashboard and monitoring.
    pub viewer_token: Option<Token>,
    /// Largest accepted upload (blob or JSON document), in bytes.
    pub max_body_bytes: u64,
    /// Binding a non-loopback address is refused unless this is set: tokens
    /// travel in plain HTTP, so a public bind must sit behind a TLS proxy.
    pub allow_non_loopback: bool,
    pub threads: usize,
}

const MAX_ATTEMPTS: u32 = 2;
const LEASE_GRACE: Duration = Duration::from_secs(120);
const DEFAULT_LEASE: Duration = Duration::from_secs(3600);

struct JobRecord {
    spec: JobSpec,
    state: JobState,
    attempts: u32,
    lease: Option<(String, Instant)>,
    result: Option<JobResult>,
    error: Option<String>,
    submitted_ms: u64,
    started_ms: Option<u64>,
    finished_ms: Option<u64>,
}

struct WorkerRecord {
    os: String,
    last_seen_ms: u64,
    current_job: Option<String>,
    jobs_reported: u64,
}

/// Coordinator wall clock in Unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[derive(Default)]
struct Jobs {
    next_id: u64,
    records: HashMap<String, JobRecord>,
    queue: VecDeque<String>,
    workers: HashMap<String, WorkerRecord>,
}

impl Jobs {
    /// Requeue or fail running jobs whose lease deadline passed.
    fn expire_leases(&mut self, now: Instant) {
        let mut requeue = Vec::new();
        for (id, record) in &mut self.records {
            if record.state != JobState::Running {
                continue;
            }
            if let Some((_, deadline)) = &record.lease
                && *deadline <= now
            {
                record.lease = None;
                if record.attempts < MAX_ATTEMPTS {
                    record.state = JobState::Queued;
                    requeue.push(id.clone());
                } else {
                    record.state = JobState::Failed;
                    record.finished_ms = Some(now_ms());
                    record.error =
                        Some(format!("lease expired {MAX_ATTEMPTS} times (worker lost)"));
                }
            }
        }
        requeue.sort();
        self.queue.extend(requeue);
    }

    fn summaries(&self) -> Vec<JobSummary> {
        let mut jobs: Vec<JobSummary> = self
            .records
            .iter()
            .map(|(id, record)| JobSummary {
                job_id: id.clone(),
                action_id: record.spec.action.id.clone(),
                state: record.state,
                attempts: record.attempts,
                worker_id: record
                    .lease
                    .as_ref()
                    .map(|(worker, _)| worker.clone())
                    .or_else(|| record.result.as_ref().map(|r| r.worker_id.clone())),
                submitted_unix_ms: record.submitted_ms,
                started_unix_ms: record.started_ms,
                finished_unix_ms: record.finished_ms,
                exit_code: record.result.as_ref().and_then(|r| r.exit_code),
                succeeded: match record.state {
                    JobState::Completed => record.result.as_ref().map(JobResult::succeeded),
                    JobState::Failed => Some(false),
                    _ => None,
                },
                error: record
                    .error
                    .clone()
                    .or_else(|| record.result.as_ref().and_then(|r| r.error.clone())),
                input_count: record.spec.inputs.len(),
                output_count: record.result.as_ref().map_or(0, |r| r.outputs.len()),
                reused: record.result.as_ref().is_some_and(|r| r.reused),
            })
            .collect();
        // Newest first; job ids are `job-N` with increasing N.
        jobs.sort_by_key(|job| {
            std::cmp::Reverse(
                job.job_id
                    .trim_start_matches("job-")
                    .parse::<u64>()
                    .unwrap_or(0),
            )
        });
        jobs
    }

    fn status(&self, id: &str) -> Option<JobStatus> {
        self.records.get(id).map(|record| JobStatus {
            job_id: id.to_owned(),
            state: record.state,
            attempts: record.attempts,
            result: record.result.clone(),
            error: record.error.clone(),
        })
    }
}

struct State {
    config: CoordinatorConfig,
    blobs: BlobStore,
    jobs: Mutex<Jobs>,
    started_ms: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Client,
    Worker,
    Viewer,
}

type Reply = (u16, Vec<u8>, &'static str);

fn json_reply<T: serde::Serialize>(code: u16, value: &T) -> Reply {
    (
        code,
        serde_json::to_vec(value).expect("serializable"),
        "application/json",
    )
}

fn error_reply(code: u16, message: &str) -> Reply {
    json_reply(code, &serde_json::json!({ "error": message }))
}

/// Start the coordinator. `on_bound` receives the actual listening address
/// (useful with port 0). Blocks forever serving requests.
pub fn serve(config: CoordinatorConfig, on_bound: impl FnOnce(SocketAddr)) -> Result<(), String> {
    if !config.listen.ip().is_loopback() && !config.allow_non_loopback {
        return Err(format!(
            "refusing to listen on non-loopback address {} without --allow-non-loopback: \
             bearer tokens travel in plain HTTP, so a public coordinator must sit behind a \
             TLS-terminating reverse proxy (see docs/remote/operations.md)",
            config.listen
        ));
    }
    let blobs = BlobStore::open(config.data_dir.join("blobs"))
        .map_err(|error| format!("cannot open blob store: {error}"))?;
    let server = tiny_http::Server::http(config.listen)
        .map_err(|error| format!("cannot listen: {error}"))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or("listener has no IP address")?;
    let threads = config.threads.max(1);
    let state = Arc::new(State {
        config,
        blobs,
        jobs: Mutex::new(Jobs::default()),
        started_ms: now_ms(),
    });
    let server = Arc::new(server);
    on_bound(bound);
    let mut handles = Vec::new();
    for _ in 0..threads {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            while let Ok(request) = server.recv() {
                handle(&state, request);
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

fn handle(state: &State, mut request: tiny_http::Request) {
    let reply = route(state, &mut request);
    let (code, body, content_type) = reply;
    let mut response = tiny_http::Response::from_data(body).with_status_code(code);
    for (name, value) in [
        ("Content-Type", content_type),
        // The dashboard loads only its own script/style and talks only to
        // this origin; nothing may frame it or leak referrers.
        (
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
             img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        ("Cache-Control", "no-store"),
    ] {
        response.add_header(
            tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
                .expect("static header"),
        );
    }
    let _ = request.respond(response);
}

fn authenticate(state: &State, request: &tiny_http::Request) -> Option<Role> {
    let presented = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .map(|header| header.value.as_str().to_owned())?;
    if state.config.client_token.matches_header(&presented) {
        Some(Role::Client)
    } else if state.config.worker_token.matches_header(&presented) {
        Some(Role::Worker)
    } else if state
        .config
        .viewer_token
        .as_ref()
        .is_some_and(|token| token.matches_header(&presented))
    {
        Some(Role::Viewer)
    } else {
        None
    }
}

fn read_body(state: &State, request: &mut tiny_http::Request) -> Result<Vec<u8>, Reply> {
    let limit = state.config.max_body_bytes;
    let mut body = Vec::new();
    request
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|error| error_reply(400, &format!("cannot read body: {error}")))?;
    if body.len() as u64 > limit {
        return Err(error_reply(413, &format!("body exceeds {limit} bytes")));
    }
    Ok(body)
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Reply> {
    serde_json::from_slice(body)
        .map_err(|error| error_reply(400, &format!("invalid JSON: {error}")))
}

fn route(state: &State, request: &mut tiny_http::Request) -> Reply {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("").to_owned();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if method == tiny_http::Method::Get {
        // Static dashboard assets: public, data-free.
        let asset = match segments.as_slice() {
            [""] | ["ui"] | ["ui", ""] => Some((crate::ui::INDEX_HTML, "text/html; charset=utf-8")),
            ["ui", "app.js"] => Some((crate::ui::APP_JS, "text/javascript; charset=utf-8")),
            ["ui", "app.css"] => Some((crate::ui::APP_CSS, "text/css; charset=utf-8")),
            _ => None,
        };
        if let Some((body, content_type)) = asset {
            return (200, body.as_bytes().to_vec(), content_type);
        }
    }
    if method == tiny_http::Method::Get && segments == ["v1", "health"] {
        return json_reply(
            200,
            &Health {
                protocol_version: PROTOCOL_VERSION,
            },
        );
    }
    let Some(role) = authenticate(state, request) else {
        return error_reply(401, "missing or invalid bearer token");
    };
    let result = match (&method, segments.as_slice()) {
        (tiny_http::Method::Get, ["v1", "jobs"]) => require_reader(role).map(|_| list_jobs(state)),
        (tiny_http::Method::Get, ["v1", "workers"]) => {
            require_reader(role).map(|_| list_workers(state))
        }
        (_, ["v1", "blobs" | "cache" | "task-cache", ..]) if role == Role::Viewer => {
            Err(error_reply(403, "viewer tokens are read-only"))
        }
        (tiny_http::Method::Get, ["v1", "cache", key]) => cache_get(state, key),
        (tiny_http::Method::Put, ["v1", "cache", key]) => {
            require(role, Role::Worker).and_then(|_| cache_put(state, request, key))
        }
        (tiny_http::Method::Get, ["v1", "task-cache", tool, key]) => {
            task_cache_get(state, tool, key)
        }
        (tiny_http::Method::Put, ["v1", "task-cache", tool, key]) => {
            require(role, Role::Worker).and_then(|_| task_cache_put(state, request, tool, key))
        }
        (tiny_http::Method::Post, ["v1", "blobs", "missing"]) => missing(state, request),
        (tiny_http::Method::Post, ["v1", "blobs", "batch-get"]) => batch_get(state, request),
        (tiny_http::Method::Post, ["v1", "blobs", "batch-put"]) => batch_put(state, request),
        (tiny_http::Method::Put, ["v1", "blobs", digest]) => put_blob(state, request, digest),
        (tiny_http::Method::Get, ["v1", "blobs", digest]) => get_blob(state, digest),
        (tiny_http::Method::Post, ["v1", "jobs"]) => {
            require(role, Role::Client).and_then(|_| submit(state, request))
        }
        (tiny_http::Method::Get, ["v1", "jobs", id]) => {
            require_reader(role).and_then(|_| job_status(state, id))
        }
        (tiny_http::Method::Post, ["v1", "lease"]) => {
            require(role, Role::Worker).and_then(|_| lease(state, request))
        }
        (tiny_http::Method::Post, ["v1", "jobs", id, "result"]) => {
            require(role, Role::Worker).and_then(|_| post_result(state, request, id))
        }
        _ => Err(error_reply(404, "no such route")),
    };
    result.unwrap_or_else(|reply| reply)
}

fn cache_path(state: &State, key: &str) -> Result<PathBuf, Reply> {
    let digest = parse_digest(key)?;
    let hex = digest.to_string();
    Ok(state
        .config
        .data_dir
        .join("cache")
        .join(hex.trim_start_matches("sha256:")))
}

/// A hit only when the entry parses and every output blob is still stored.
fn cache_get(state: &State, key: &str) -> Result<Reply, Reply> {
    let path = cache_path(state, key)?;
    let Ok(bytes) = std::fs::read(&path) else {
        return Err(error_reply(404, "no cache entry"));
    };
    let entry: CacheEntry =
        serde_json::from_slice(&bytes).map_err(|_| error_reply(404, "unreadable cache entry"))?;
    for output in &entry.outputs {
        if let Some(digest) = &output.digest
            && !state.blobs.contains(&parse_digest(digest)?)
        {
            return Err(error_reply(404, "cache entry refers to a missing blob"));
        }
    }
    Ok(json_reply(200, &entry))
}

/// Store a worker-proven result. The first stored entry for a key wins (a
/// key determines its outputs); every referenced blob must already exist.
fn cache_put(state: &State, request: &mut tiny_http::Request, key: &str) -> Result<Reply, Reply> {
    let path = cache_path(state, key)?;
    let body = read_body(state, request)?;
    let store: CacheStoreRequest = parse_json(&body)?;
    validate_entries(&store.outputs).map_err(|error| error_reply(400, &error))?;
    for output in &store.outputs {
        if let Some(digest) = &output.digest
            && !state.blobs.contains(&parse_digest(digest)?)
        {
            return Err(error_reply(
                409,
                &format!("output blob {digest} not uploaded"),
            ));
        }
    }
    if path.is_file() {
        return Ok(json_reply(200, &serde_json::json!({ "stored": false })));
    }
    let entry = CacheEntry {
        outputs: store.outputs,
        worker_id: store.worker_id,
        created_unix_ms: now_ms(),
    };
    let dir = path.parent().expect("cache dir");
    std::fs::create_dir_all(dir).map_err(|error| error_reply(500, &error.to_string()))?;
    let temp = dir.join(format!(".{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec(&entry).expect("serializable"))
        .and_then(|()| std::fs::rename(&temp, &path))
        .map_err(|error| error_reply(500, &error.to_string()))?;
    Ok(json_reply(201, &serde_json::json!({ "stored": true })))
}

/// Index file for a tool-level task cache entry (D-025). Only known tools
/// and plain hex keys (as Gradle's build cache uses) are accepted.
fn task_cache_path(state: &State, tool: &str, key: &str) -> Result<PathBuf, Reply> {
    if !TASK_CACHE_TOOLS.contains(&tool) {
        return Err(error_reply(404, "unknown task cache"));
    }
    if !(8..=128).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error_reply(400, "task cache keys are 8-128 hex characters"));
    }
    Ok(state
        .config
        .data_dir
        .join("task-cache")
        .join(tool)
        .join(key.to_ascii_lowercase()))
}

/// Tools whose own build caches the coordinator serves (D-025).
const TASK_CACHE_TOOLS: &[&str] = &["gradle"];

/// An entry is an index file naming a stored blob; a hit only when the blob
/// is still stored (and it is re-verified on read like every blob).
fn task_cache_get(state: &State, tool: &str, key: &str) -> Result<Reply, Reply> {
    let path = task_cache_path(state, tool, key)?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Err(error_reply(404, "no task cache entry"));
    };
    let digest = parse_digest(text.trim())?;
    match state.blobs.get(&digest) {
        Ok(Some(bytes)) => Ok((200, bytes, "application/octet-stream")),
        Ok(None) => Err(error_reply(
            404,
            "task cache entry refers to a missing blob",
        )),
        Err(error) => Err(error_reply(500, &error)),
    }
}

/// Store a task output produced on a worker. Later writes for a key replace
/// earlier ones (the tool owns its key semantics).
fn task_cache_put(
    state: &State,
    request: &mut tiny_http::Request,
    tool: &str,
    key: &str,
) -> Result<Reply, Reply> {
    let path = task_cache_path(state, tool, key)?;
    let body = read_body(state, request)?;
    let digest = crate::blobs::digest_bytes(&body);
    state
        .blobs
        .put(&digest, &body)
        .map_err(|error| error_reply(500, &error))?;
    let dir = path.parent().expect("task cache dir");
    std::fs::create_dir_all(dir).map_err(|error| error_reply(500, &error.to_string()))?;
    let temp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    std::fs::write(&temp, digest.to_string())
        .and_then(|()| std::fs::rename(&temp, &path))
        .map_err(|error| error_reply(500, &error.to_string()))?;
    Ok(json_reply(201, &serde_json::json!({ "stored": true })))
}

/// Read-model routes: clients and viewers.
fn require_reader(actual: Role) -> Result<(), Reply> {
    if matches!(actual, Role::Client | Role::Viewer) {
        Ok(())
    } else {
        Err(error_reply(403, "token role not permitted for this route"))
    }
}

fn list_jobs(state: &State) -> Reply {
    let mut jobs = state.jobs.lock().expect("jobs lock");
    jobs.expire_leases(Instant::now());
    json_reply(
        200,
        &JobList {
            coordinator_started_unix_ms: state.started_ms,
            jobs: jobs.summaries(),
        },
    )
}

fn list_workers(state: &State) -> Reply {
    let jobs = state.jobs.lock().expect("jobs lock");
    let mut workers: Vec<WorkerSummary> = jobs
        .workers
        .iter()
        .map(|(id, worker)| WorkerSummary {
            worker_id: id.clone(),
            os: worker.os.clone(),
            last_seen_unix_ms: worker.last_seen_ms,
            current_job: worker.current_job.clone(),
            jobs_reported: worker.jobs_reported,
        })
        .collect();
    workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    json_reply(
        200,
        &WorkerList {
            now_unix_ms: now_ms(),
            workers,
        },
    )
}

fn require(actual: Role, needed: Role) -> Result<(), Reply> {
    if actual == needed {
        Ok(())
    } else {
        Err(error_reply(403, "token role not permitted for this route"))
    }
}

fn parse_digest(text: &str) -> Result<ContentDigest, Reply> {
    text.parse::<ContentDigest>()
        .map_err(|error| error_reply(400, &error))
}

fn missing(state: &State, request: &mut tiny_http::Request) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let query: MissingRequest = parse_json(&body)?;
    let mut missing = Vec::new();
    for text in query.digests {
        let digest = parse_digest(&text)?;
        if !state.blobs.contains(&digest) {
            missing.push(text);
        }
    }
    Ok(json_reply(200, &MissingResponse { missing }))
}

/// Return many blobs in one response, framed (D-023). Every requested blob
/// must exist and is re-verified on read, exactly like `GET /v1/blobs/{d}`.
fn batch_get(state: &State, request: &mut tiny_http::Request) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let query: crate::protocol::BatchGetRequest = parse_json(&body)?;
    if query.digests.len() > crate::protocol::MAX_BATCH_BLOBS {
        return Err(error_reply(
            400,
            &format!(
                "at most {} digests per batch",
                crate::protocol::MAX_BATCH_BLOBS
            ),
        ));
    }
    let mut blobs = Vec::with_capacity(query.digests.len());
    for text in &query.digests {
        let digest = parse_digest(text)?;
        match state.blobs.get(&digest) {
            Ok(Some(bytes)) => blobs.push(bytes),
            Ok(None) => return Err(error_reply(404, &format!("no such blob {text}"))),
            Err(error) => return Err(error_reply(500, &error)),
        }
    }
    let frames: Vec<(&str, &[u8])> = query
        .digests
        .iter()
        .zip(&blobs)
        .map(|(digest, bytes)| (digest.as_str(), bytes.as_slice()))
        .collect();
    Ok((
        200,
        crate::protocol::encode_frames(&frames),
        "application/octet-stream",
    ))
}

/// Store many blobs from one framed body (D-023). Each blob is re-hashed
/// against its stated digest; one mismatch rejects the whole batch.
fn batch_put(state: &State, request: &mut tiny_http::Request) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let frames = crate::protocol::decode_frames(&body).map_err(|error| error_reply(400, &error))?;
    let mut parsed = Vec::with_capacity(frames.len());
    for (text, bytes) in &frames {
        let digest = parse_digest(text)?;
        if crate::blobs::digest_bytes(bytes) != digest {
            return Err(error_reply(
                400,
                &format!("blob content does not match {text}"),
            ));
        }
        parsed.push((digest, bytes));
    }
    for (digest, bytes) in parsed {
        state
            .blobs
            .put(&digest, bytes)
            .map_err(|error| error_reply(500, &error))?;
    }
    Ok(json_reply(
        201,
        &serde_json::json!({ "stored": frames.len() }),
    ))
}

fn put_blob(state: &State, request: &mut tiny_http::Request, digest: &str) -> Result<Reply, Reply> {
    let digest = parse_digest(digest)?;
    let body = read_body(state, request)?;
    state
        .blobs
        .put(&digest, &body)
        .map_err(|error| error_reply(400, &error))?;
    Ok(json_reply(
        201,
        &serde_json::json!({ "stored": digest.to_string() }),
    ))
}

fn get_blob(state: &State, digest: &str) -> Result<Reply, Reply> {
    let digest = parse_digest(digest)?;
    match state.blobs.get(&digest) {
        Ok(Some(bytes)) => Ok((200, bytes, "application/octet-stream")),
        Ok(None) => Err(error_reply(404, "no such blob")),
        Err(error) => Err(error_reply(500, &error)),
    }
}

fn submit(state: &State, request: &mut tiny_http::Request) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let spec: JobSpec = parse_json(&body)?;
    if spec.protocol_version != PROTOCOL_VERSION {
        return Err(error_reply(
            400,
            &format!("unsupported protocol version {}", spec.protocol_version),
        ));
    }
    spec.action
        .validate()
        .map_err(|error| error_reply(400, &format!("invalid action: {error:?}")))?;
    validate_entries(&spec.inputs).map_err(|error| error_reply(400, &error))?;
    // Every input blob must already be uploaded, so a leased job is runnable.
    for entry in &spec.inputs {
        if let Some(digest) = &entry.digest
            && !state.blobs.contains(&parse_digest(digest)?)
        {
            return Err(error_reply(
                409,
                &format!("input blob {digest} for '{}' not uploaded", entry.path),
            ));
        }
    }
    let mut jobs = state.jobs.lock().expect("jobs lock");
    jobs.next_id += 1;
    let job_id = format!("job-{}", jobs.next_id);
    jobs.records.insert(
        job_id.clone(),
        JobRecord {
            spec,
            state: JobState::Queued,
            attempts: 0,
            lease: None,
            result: None,
            error: None,
            submitted_ms: now_ms(),
            started_ms: None,
            finished_ms: None,
        },
    );
    jobs.queue.push_back(job_id.clone());
    Ok(json_reply(201, &SubmitResponse { job_id }))
}

fn job_status(state: &State, id: &str) -> Result<Reply, Reply> {
    let mut jobs = state.jobs.lock().expect("jobs lock");
    jobs.expire_leases(Instant::now());
    jobs.status(id)
        .map(|status| json_reply(200, &status))
        .ok_or_else(|| error_reply(404, "no such job"))
}

fn lease(state: &State, request: &mut tiny_http::Request) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let lease: LeaseRequest = parse_json(&body)?;
    if lease.protocol_version != PROTOCOL_VERSION {
        return Err(error_reply(
            400,
            &format!("unsupported protocol version {}", lease.protocol_version),
        ));
    }
    if lease.worker_id.trim().is_empty() || lease.worker_id.len() > 128 {
        return Err(error_reply(400, "worker_id must be 1-128 characters"));
    }
    let now = Instant::now();
    let mut jobs = state.jobs.lock().expect("jobs lock");
    jobs.expire_leases(now);
    // Every poll is a heartbeat, including empty ones.
    let seen = now_ms();
    let worker = jobs
        .workers
        .entry(lease.worker_id.clone())
        .or_insert(WorkerRecord {
            os: lease.os.clone(),
            last_seen_ms: seen,
            current_job: None,
            jobs_reported: 0,
        });
    worker.os = lease.os.clone();
    worker.last_seen_ms = seen;
    worker.current_job = None;
    let Some(job_id) = jobs.queue.pop_front() else {
        return Ok((204, Vec::new(), "application/json"));
    };
    if let Some(worker) = jobs.workers.get_mut(&lease.worker_id) {
        worker.current_job = Some(job_id.clone());
    }
    let record = jobs.records.get_mut(&job_id).expect("queued job exists");
    let lease_for = record
        .spec
        .action
        .timeout_ms
        .map_or(DEFAULT_LEASE, Duration::from_millis)
        + LEASE_GRACE;
    record.state = JobState::Running;
    record.attempts += 1;
    record.started_ms = Some(seen);
    record.lease = Some((lease.worker_id, now + lease_for));
    let spec = record.spec.clone();
    Ok(json_reply(200, &LeaseResponse { job_id, spec }))
}

fn post_result(state: &State, request: &mut tiny_http::Request, id: &str) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let result: JobResult = parse_json(&body)?;
    validate_entries(&result.outputs).map_err(|error| error_reply(400, &error))?;
    for entry in &result.outputs {
        if let Some(digest) = &entry.digest
            && !state.blobs.contains(&parse_digest(digest)?)
        {
            return Err(error_reply(
                409,
                &format!("output blob {digest} for '{}' not uploaded", entry.path),
            ));
        }
    }
    let mut jobs = state.jobs.lock().expect("jobs lock");
    let record = jobs
        .records
        .get_mut(id)
        .ok_or_else(|| error_reply(404, "no such job"))?;
    match &record.lease {
        Some((holder, _)) if *holder == result.worker_id && record.state == JobState::Running => {}
        _ => {
            return Err(error_reply(
                409,
                "this worker does not hold the job's lease",
            ));
        }
    }
    record.state = JobState::Completed;
    record.lease = None;
    let finished = now_ms();
    record.finished_ms = Some(finished);
    let worker_id = result.worker_id.clone();
    record.result = Some(result);
    if let Some(worker) = jobs.workers.get_mut(&worker_id) {
        worker.last_seen_ms = finished;
        worker.current_job = None;
        worker.jobs_reported += 1;
    }
    Ok(json_reply(200, &serde_json::json!({ "accepted": id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(timeout_state: JobState) -> JobRecord {
        let action: build_action::Action = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "id": "a", "argv": ["x"], "working_directory": ".",
            "inputs": [], "outputs": [], "environment": [], "cache_policy": "never"
        }))
        .expect("action");
        JobRecord {
            spec: JobSpec {
                protocol_version: 1,
                action,
                inputs: Vec::new(),
            },
            state: timeout_state,
            attempts: 1,
            lease: Some(("w".into(), Instant::now())),
            result: None,
            error: None,
            submitted_ms: 1,
            started_ms: Some(2),
            finished_ms: None,
        }
    }

    #[test]
    fn expired_leases_requeue_once_then_fail() {
        let mut jobs = Jobs::default();
        jobs.records
            .insert("job-1".into(), record(JobState::Running));
        let later = Instant::now() + Duration::from_millis(1);
        jobs.expire_leases(later);
        assert_eq!(
            jobs.status("job-1").expect("status").state,
            JobState::Queued
        );
        assert_eq!(jobs.queue.len(), 1);

        let job = jobs.records.get_mut("job-1").expect("job");
        job.state = JobState::Running;
        job.attempts = MAX_ATTEMPTS;
        job.lease = Some(("w".into(), Instant::now()));
        jobs.queue.clear();
        jobs.expire_leases(Instant::now() + Duration::from_millis(1));
        let status = jobs.status("job-1").expect("status");
        assert_eq!(status.state, JobState::Failed);
        assert!(status.error.expect("error").contains("lease expired"));
        assert!(jobs.queue.is_empty());
    }
}
