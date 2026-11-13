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
//!
//! Jobs live in memory: a coordinator restart loses queued and running jobs
//! (blobs persist on disk). A worker that stops reporting loses its lease
//! after the action's timeout (or one hour) plus a grace period; the job is
//! requeued once and then failed.

use crate::auth::Token;
use crate::blobs::BlobStore;
use crate::protocol::{
    Health, JobResult, JobSpec, JobState, JobStatus, LeaseRequest, LeaseResponse, MissingRequest,
    MissingResponse, PROTOCOL_VERSION, SubmitResponse, validate_entries,
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
}

#[derive(Default)]
struct Jobs {
    next_id: u64,
    records: HashMap<String, JobRecord>,
    queue: VecDeque<String>,
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
                    record.error =
                        Some(format!("lease expired {MAX_ATTEMPTS} times (worker lost)"));
                }
            }
        }
        requeue.sort();
        self.queue.extend(requeue);
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Client,
    Worker,
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
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("static header");
    let _ = request.respond(
        tiny_http::Response::from_data(body)
            .with_status_code(code)
            .with_header(header),
    );
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
        (tiny_http::Method::Post, ["v1", "blobs", "missing"]) => missing(state, request),
        (tiny_http::Method::Put, ["v1", "blobs", digest]) => put_blob(state, request, digest),
        (tiny_http::Method::Get, ["v1", "blobs", digest]) => get_blob(state, digest),
        (tiny_http::Method::Post, ["v1", "jobs"]) => {
            require(role, Role::Client).and_then(|_| submit(state, request))
        }
        (tiny_http::Method::Get, ["v1", "jobs", id]) => {
            require(role, Role::Client).and_then(|_| job_status(state, id))
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
    let Some(job_id) = jobs.queue.pop_front() else {
        return Ok((204, Vec::new(), "application/json"));
    };
    let record = jobs.records.get_mut(&job_id).expect("queued job exists");
    let lease_for = record
        .spec
        .action
        .timeout_ms
        .map_or(DEFAULT_LEASE, Duration::from_millis)
        + LEASE_GRACE;
    record.state = JobState::Running;
    record.attempts += 1;
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
    record.result = Some(result);
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
