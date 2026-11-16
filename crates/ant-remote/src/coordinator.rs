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
    /// Single sign-on: ID tokens from this issuer act as client tokens
    /// (D-063).
    pub oidc: Option<Arc<crate::oidc::OidcVerifier>>,
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
    /// Tool output streamed by the worker (D-049), at most
    /// `MAX_JOB_LOG_BYTES`.
    log: String,
    log_truncated: bool,
    /// The client asked to stop this running job (D-054); the worker hears
    /// it on its next output heartbeat.
    cancel_requested: bool,
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
    /// `<workspace key>/<action id>` -> the worker that last ran it, whose
    /// kept workspace holds that action's incremental state (D-050).
    affinity: HashMap<String, String>,
}

/// How long a job waits for the worker that holds its kept workspace
/// before any worker may take it (D-050).
const AFFINITY_WAIT_MS: u64 = 5_000;
/// A worker that has not polled for this long is busy or gone.
const WORKER_ALIVE_MS: u64 = 10_000;

fn affinity_key(spec: &JobSpec) -> Option<String> {
    spec.workspace_key
        .as_ref()
        .map(|key| format!("{key}/{}", spec.action.id))
}

impl Jobs {
    /// The next queued job for `worker` at wall-clock `now_ms`: the oldest
    /// one, except that a job whose kept workspace lives on another live
    /// worker waits up to [`AFFINITY_WAIT_MS`] for that worker (D-050).
    fn take_next(&mut self, worker: &str, now_ms: u64) -> Option<String> {
        let position = self.queue.iter().position(|id| {
            let Some(record) = self.records.get(id) else {
                return true;
            };
            let Some(preferred) =
                affinity_key(&record.spec).and_then(|key| self.affinity.get(&key))
            else {
                return true;
            };
            if preferred == worker {
                return true;
            }
            let preferred_alive = self
                .workers
                .get(preferred)
                .is_some_and(|record| now_ms.saturating_sub(record.last_seen_ms) < WORKER_ALIVE_MS);
            !preferred_alive || now_ms.saturating_sub(record.submitted_ms) >= AFFINITY_WAIT_MS
        })?;
        let job_id = self.queue.remove(position)?;
        if let Some(key) = self
            .records
            .get(&job_id)
            .and_then(|record| affinity_key(&record.spec))
        {
            self.affinity.insert(key, worker.to_owned());
        }
        Some(job_id)
    }

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
    /// Append-only audit trail, `<data dir>/audit.jsonl` (D-059).
    audit: Mutex<Option<std::fs::File>>,
}

/// Lines `GET /v1/audit` returns at most (the newest).
const AUDIT_TAIL: usize = 1000;

/// Record one audit event: who (role, worker id, address) did what to
/// which job, when. Never tokens, environment values, or file contents.
fn audit(state: &State, mut event: serde_json::Value) {
    use std::io::Write as _;
    if let Some(fields) = event.as_object_mut() {
        fields.insert("time_ms".to_owned(), serde_json::json!(now_ms()));
    }
    let Ok(mut file) = state.audit.lock() else {
        return;
    };
    if let Some(file) = file.as_mut() {
        let _ = writeln!(file, "{event}");
    }
}

fn audit_tail(state: &State) -> Result<Reply, Reply> {
    let text =
        std::fs::read_to_string(state.config.data_dir.join("audit.jsonl")).unwrap_or_default();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .rev()
        .take(AUDIT_TAIL)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(json_reply(200, &serde_json::json!({ "events": lines })))
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
    let audit_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.data_dir.join("audit.jsonl"))
        .map_err(|error| format!("cannot open the audit log: {error}"))?;
    let state = Arc::new(State {
        config,
        blobs,
        jobs: Mutex::new(Jobs::default()),
        started_ms: now_ms(),
        audit: Mutex::new(Some(audit_file)),
    });
    audit(
        &state,
        serde_json::json!({ "event": "coordinator_started", "listen": bound.to_string() }),
    );
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

/// The caller's role, and for single sign-on the verified email (D-063).
fn authenticate(state: &State, request: &tiny_http::Request) -> Option<(Role, Option<String>)> {
    let presented = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .map(|header| header.value.as_str().to_owned())?;
    if state.config.client_token.matches_header(&presented) {
        Some((Role::Client, None))
    } else if state.config.worker_token.matches_header(&presented) {
        Some((Role::Worker, None))
    } else if state
        .config
        .viewer_token
        .as_ref()
        .is_some_and(|token| token.matches_header(&presented))
    {
        Some((Role::Viewer, None))
    } else {
        let oidc = state.config.oidc.as_ref()?;
        let jwt = presented.strip_prefix("Bearer ")?.trim();
        match oidc.verify(jwt, now_ms() / 1000) {
            Ok(email) => Some((Role::Client, Some(email))),
            Err(reason) => {
                audit(
                    state,
                    serde_json::json!({ "event": "sso_rejected", "reason": reason }),
                );
                None
            }
        }
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
    if method == tiny_http::Method::Get && segments == ["v1", "sso"] {
        return match &state.config.oidc {
            Some(oidc) => json_reply(200, &oidc.public_info()),
            None => error_reply(404, "single sign-on is not configured"),
        };
    }
    let Some((role, user)) = authenticate(state, request) else {
        audit(
            state,
            serde_json::json!({
                "event": "auth_failed",
                "method": method.as_str(),
                "path": path,
                "from": request.remote_addr().map(|address| address.ip().to_string()),
            }),
        );
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
            require(role, Role::Client).and_then(|_| submit(state, request, user.as_deref()))
        }
        (tiny_http::Method::Get, ["v1", "jobs", id]) => {
            require_reader(role).and_then(|_| job_status(state, id))
        }
        (tiny_http::Method::Post, ["v1", "lease"]) => {
            require(role, Role::Worker).and_then(|_| lease(state, request))
        }
        (tiny_http::Method::Get, ["v1", "audit"]) => {
            require_reader(role).and_then(|_| audit_tail(state))
        }
        (tiny_http::Method::Post, ["v1", "jobs", id, "cancel"]) => {
            require(role, Role::Client).and_then(|_| cancel_job(state, id, user.as_deref()))
        }
        (tiny_http::Method::Post, ["v1", "jobs", id, "log"]) => {
            require(role, Role::Worker).and_then(|_| post_log(state, request, id))
        }
        (tiny_http::Method::Get, ["v1", "jobs", id, "log"]) => {
            let from = request
                .url()
                .split_once('?')
                .and_then(|(_, query)| query.split('&').find_map(|pair| pair.strip_prefix("from=")))
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|_| error_reply(400, "from must be a byte offset"));
            require_reader(role)
                .and(from)
                .and_then(|from| job_log(state, id, from.unwrap_or(0)))
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

fn submit(
    state: &State,
    request: &mut tiny_http::Request,
    user: Option<&str>,
) -> Result<Reply, Reply> {
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
            log: String::new(),
            log_truncated: false,
            cancel_requested: false,
        },
    );
    jobs.queue.push_back(job_id.clone());
    let action = jobs.records[&job_id].spec.action.id.clone();
    drop(jobs);
    audit(
        state,
        serde_json::json!({
            "event": "job_submitted",
            "job": job_id,
            "action": action,
            "role": "client",
            "user": user,
            "from": request.remote_addr().map(|address| address.ip().to_string()),
        }),
    );
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
    let Some(job_id) = jobs.take_next(&lease.worker_id, seen) else {
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
    record.lease = Some((lease.worker_id.clone(), now + lease_for));
    let spec = record.spec.clone();
    drop(jobs);
    audit(
        state,
        serde_json::json!({
            "event": "job_leased",
            "job": job_id,
            "action": spec.action.id,
            "worker": lease.worker_id,
        }),
    );
    Ok(json_reply(200, &LeaseResponse { job_id, spec }))
}

/// Append a running job's output; only the worker holding its lease may.
fn post_log(state: &State, request: &mut tiny_http::Request, id: &str) -> Result<Reply, Reply> {
    let body = read_body(state, request)?;
    let chunk: crate::protocol::LogChunk = parse_json(&body)?;
    let mut jobs = state.jobs.lock().expect("jobs lock");
    let record = jobs
        .records
        .get_mut(id)
        .ok_or_else(|| error_reply(404, "no such job"))?;
    match &record.lease {
        Some((holder, _)) if *holder == chunk.worker_id && record.state == JobState::Running => {}
        _ => {
            return Err(error_reply(
                409,
                "this worker does not hold the job's lease",
            ));
        }
    }
    // An empty chunk is a heartbeat; the answer says whether to stop.
    let cancel = record.cancel_requested;
    let room = crate::protocol::MAX_JOB_LOG_BYTES.saturating_sub(record.log.len());
    if chunk.text.len() <= room {
        record.log.push_str(&chunk.text);
    } else {
        let mut cut = room;
        while !chunk.text.is_char_boundary(cut) {
            cut -= 1;
        }
        record.log.push_str(&chunk.text[..cut]);
        record.log_truncated = true;
    }
    if cancel {
        return Ok(json_reply(200, &serde_json::json!({ "cancel": true })));
    }
    Ok((204, Vec::new(), "text/plain"))
}

/// Stop a job (D-054): a queued job never starts; a running one is told to
/// stop through its worker's next heartbeat. Finished jobs are left alone.
fn cancel_job(state: &State, id: &str, user: Option<&str>) -> Result<Reply, Reply> {
    let mut jobs = state.jobs.lock().expect("jobs lock");
    let record = jobs
        .records
        .get_mut(id)
        .ok_or_else(|| error_reply(404, "no such job"))?;
    let action = record.spec.action.id.clone();
    match record.state {
        JobState::Queued => {
            record.state = JobState::Failed;
            record.error = Some("cancelled before it started".to_owned());
            record.finished_ms = Some(now_ms());
            jobs.queue.retain(|queued| queued != id);
        }
        JobState::Running => record.cancel_requested = true,
        JobState::Completed | JobState::Failed => {}
    }
    drop(jobs);
    audit(
        state,
        serde_json::json!({ "event": "job_cancelled", "job": id, "action": action, "role": "client", "user": user }),
    );
    Ok((204, Vec::new(), "text/plain"))
}

fn job_log(state: &State, id: &str, from: u64) -> Result<Reply, Reply> {
    let jobs = state.jobs.lock().expect("jobs lock");
    let record = jobs
        .records
        .get(id)
        .ok_or_else(|| error_reply(404, "no such job"))?;
    let mut start = usize::try_from(from)
        .unwrap_or(usize::MAX)
        .min(record.log.len());
    while !record.log.is_char_boundary(start) {
        start -= 1;
    }
    Ok(json_reply(
        200,
        &crate::protocol::JobLog {
            next: record.log.len() as u64,
            text: record.log[start..].to_owned(),
            truncated: record.log_truncated,
        },
    ))
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
    let succeeded = record
        .result
        .as_ref()
        .is_some_and(|result| result.succeeded());
    let action = record.spec.action.id.clone();
    if let Some(worker) = jobs.workers.get_mut(&worker_id) {
        worker.last_seen_ms = finished;
        worker.current_job = None;
        worker.jobs_reported += 1;
    }
    drop(jobs);
    audit(
        state,
        serde_json::json!({
            "event": "job_finished",
            "job": id,
            "action": action,
            "worker": worker_id,
            "succeeded": succeeded,
        }),
    );
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
                workspace_key: None,
            },
            state: timeout_state,
            attempts: 1,
            lease: Some(("w".into(), Instant::now())),
            result: None,
            error: None,
            submitted_ms: 1,
            started_ms: Some(2),
            finished_ms: None,
            log: String::new(),
            log_truncated: false,
            cancel_requested: false,
        }
    }

    #[test]
    fn kept_workspaces_prefer_their_worker_for_a_while() {
        fn queue(jobs: &mut Jobs, id: &str, key: Option<&str>, submitted_ms: u64) {
            let mut job = record(JobState::Queued);
            job.lease = None;
            job.submitted_ms = submitted_ms;
            job.spec.workspace_key = key.map(str::to_owned);
            jobs.records.insert(id.into(), job);
            jobs.queue.push_back(id.into());
        }
        let key = Some("0123456789abcdef");
        let mut jobs = Jobs::default();
        for worker in ["w1", "w2"] {
            jobs.workers.insert(
                worker.into(),
                WorkerRecord {
                    os: "linux".into(),
                    last_seen_ms: 1_000,
                    current_job: None,
                    jobs_reported: 0,
                },
            );
        }
        queue(&mut jobs, "first", key, 1_000);
        assert_eq!(jobs.take_next("w1", 1_000).as_deref(), Some("first"));
        // The project's next job waits for w1, whose workspace has its
        // state; w2 meanwhile takes work without a kept workspace.
        queue(&mut jobs, "again", key, 2_000);
        queue(&mut jobs, "other", None, 2_000);
        assert_eq!(jobs.take_next("w2", 2_000).as_deref(), Some("other"));
        assert_eq!(jobs.take_next("w2", 2_000), None);
        assert_eq!(jobs.take_next("w1", 2_000).as_deref(), Some("again"));
        // After the wait anyone may take it (and becomes preferred).
        queue(&mut jobs, "later", key, 3_000);
        assert_eq!(jobs.take_next("w2", 2_999 + AFFINITY_WAIT_MS), None);
        assert_eq!(
            jobs.take_next("w2", 3_000 + AFFINITY_WAIT_MS).as_deref(),
            Some("later")
        );
        // A preferred worker that stopped polling is not waited for.
        let now = 50_000;
        queue(&mut jobs, "stale", key, now);
        jobs.workers.get_mut("w2").expect("w2").last_seen_ms = now - WORKER_ALIVE_MS;
        assert_eq!(jobs.take_next("w1", now).as_deref(), Some("stale"));
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
