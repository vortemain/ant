# Remote protocol v1

**Status:** implemented (T4.1–T4.3, decision D-014) and verified end to end on one Windows machine over loopback. Not yet exercised across a real network or on a VPS. Not REAPI: Ant's own minimal protocol; REAPI interoperability stays a separate target (D-002).

## Roles and transport

- **Coordinator** (`ant coordinator`): content-addressed blob store on disk + in-memory job queue behind an HTTP API.
- **Worker** (`ant worker`): leases jobs, materializes inputs, executes with Ant's local executor (optionally `--sandbox`), uploads outputs, reports.
- **Client** (`ant remote run`): runs a plan by submitting one job per action in the plan's deterministic topological order.

JSON documents over HTTP/1.1 under `/v1`. Every job and lease document carries `protocol_version: 1`; a coordinator rejects other versions with 400. Plain HTTP is only acceptable on loopback: the coordinator binds loopback unless `--allow-non-loopback`, and clients refuse to send a token over `http://` to a non-loopback host unless `--allow-insecure-http`. Production deployments terminate TLS in a reverse proxy (see [operations](operations.md)).

## Authentication

`Authorization: Bearer <token>`. Two tokens, read from files (≥ 32 characters, no whitespace), compared in constant time, never logged:

| Role | May call |
|---|---|
| client | blobs, `POST /v1/jobs`, `GET /v1/jobs/{id}`, `GET /v1/jobs`, `GET /v1/workers` |
| viewer (optional, `--viewer-token-file`) | read-only: `GET /v1/jobs`, `GET /v1/jobs/{id}`, `GET /v1/workers` |
| worker | blobs, `POST /v1/lease`, `POST /v1/jobs/{id}/result` (only for a job it holds the lease on) |

Missing/invalid token → 401; valid token, wrong role → 403. `GET /v1/health` is unauthenticated and returns `{"protocol_version":1}`.

## Content addressing

Blobs are named by Ant's canonical file digest, `sha256:` + hex of `SHA-256("ant-file-v1\0" ‖ content)` (`build_digest::digest_bytes`, identical to `digest_file`), so a transferred file and the same file digested locally always agree. The coordinator re-hashes every upload and rejects mismatches (400), re-verifies on every read (corruption → 500), and every downloader verifies again.

| Method | Path | Body → Response |
|---|---|---|
| POST | `/v1/blobs/missing` | `{"digests":[…]}` → `{"missing":[…]}` |
| PUT | `/v1/blobs/{digest}` | raw bytes → 201 (413 above `--max-body-mb`, default 512) |
| GET | `/v1/blobs/{digest}` | → raw bytes, 404 if absent |
| POST | `/v1/blobs/batch-put` | framed blobs → 201 `{stored}`; every blob re-hashed, one mismatch rejects the whole batch (nothing stored) |
| POST | `/v1/blobs/batch-get` | `{"digests":[…]}` (≤ 1024) → framed blobs in request order; 404 if any is missing (never a partial set) |

Batch framing (D-023): for each blob, a big-endian `u32` digest length, the digest text, a big-endian `u64` data length, then the data. Senders split batches at 1024 blobs or ~32 MiB; receivers verify every blob against its digest, as with single-blob transfers. Clients upload missing inputs in batches, workers download inputs 256 per batch, and clients fetch outputs in one verified batch.

## Entries and path safety

A transferred set is a list of entries `{ "path", "kind": "file"|"dir", "digest"? }`. Paths are workspace-relative and `/`-separated; files carry a digest, directories none. **Every party validates every path** (`protocol::validate_relative`): non-empty, no leading/trailing `/`, no empty, `.` or `..` components, no `\`, `:`, or control characters; duplicates rejected. The client additionally accepts only outputs at or under the action's declared outputs, and replaces each declared output wholesale before installing.

## Jobs

`POST /v1/jobs` (client) with a `JobSpec`:

```json
{ "protocol_version": 1, "action": { …canonical Ant action… }, "inputs": [ …entries… ] }
```

The coordinator validates the action, the entries, and that every input blob is already stored (409 otherwise), then queues it → `201 {"job_id":"job-N"}`. Inputs are exactly the action's declared inputs (trees expanded); **undeclared files do not exist on the worker**, so remote execution enforces input declarations by construction.

`POST /v1/lease` (worker) with `{"protocol_version":1,"worker_id":"…","os":"…"}` → `200 {"job_id","spec"}` or `204` when the queue is empty. The lease lasts the action's `timeout_ms` (or one hour) plus 120 s grace; an expired lease requeues the job once, and a second expiry fails it (`state: failed`, `error: lease expired …`). Jobs are not yet routed by OS or capability.

`POST /v1/jobs/{id}/result` (worker holding the lease) with a `JobResult` `{worker_id, exit_code, timed_out, error, outputs}`; output blobs must be uploaded first (409 otherwise).

`GET /v1/jobs/{id}` (client) → `{job_id, state: queued|running|completed|failed, attempts, result, error}`.

## Worker execution

Fresh workspace per job (`<work-dir>/<job-id>`, removed afterwards), plus an `.ant/tmp` staging directory; each input blob downloaded and verified before writing; the action runs with Ant's local executor (`prepare_with_metadata` + `execute_prepared`, or the restricted-token sandbox with `--sandbox`), which re-verifies inputs and checks declared outputs; on success every declared output file is uploaded (trees expanded).

## Control-plane read model (T5.1)

`GET /v1/jobs` (client/viewer) → `{coordinator_started_unix_ms, jobs:[JobSummary]}`, newest first. A `JobSummary` is `{job_id, action_id, state, attempts, worker_id, submitted_unix_ms, started_unix_ms, finished_unix_ms, exit_code, succeeded, error, input_count, output_count}`. Timestamps are the coordinator's wall clock in Unix milliseconds recorded when the event happened (measured, never estimated); `succeeded` is `true` only for a completed job whose action exited 0 without error or timeout.

`GET /v1/workers` (client/viewer) → `{now_unix_ms, workers:[{worker_id, os, last_seen_unix_ms, current_job, jobs_reported}]}`. "Last seen" is the last lease poll or result report; a busy worker does not poll, so its last-seen time ages while it runs a job.

Both lists reflect coordinator memory since it started (see limits below).

## Dashboard (T5.1)

`GET /`, `GET /ui`, `GET /ui/app.js`, `GET /ui/app.css` serve a static, data-free dashboard. The page asks for a viewer or client token (kept only in that browser tab's `sessionStorage`), then polls the two read-model routes every 3 s. Every coordinator response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and `Cache-Control: no-store`. All data is rendered with `textContent`, never as markup.

## Shared remote cache (P2.3, D-019)

Only a **sandboxed Linux worker** (`ant worker --sandbox` on Linux: bubblewrap, no network) produces cacheable results. Before executing a job it:

1. writes every input with one normalized modification time (2000-01-01 UTC), so file metadata is a function of content;
2. mounts the job's workspace at the fixed path `/tmp/ant/ws` inside the sandbox, so paths the action sees (and may embed in outputs) are the same for every job and machine;
3. computes the action key from content digests with that fingerprint (`ExecutionFingerprint::remote_worker_sandboxed`); the key is eligible only when the action is fully declared (`cache_policy: eligible`, toolchain identity) — the same blocker rules as local runs.

| Method | Path | Role | Body → Response |
|---|---|---|---|
| GET | `/v1/cache/{key}` | client, worker | → `CacheEntry {outputs, worker_id, created_unix_ms}`; 404 on a miss or when any referenced blob is gone |
| PUT | `/v1/cache/{key}` | **worker only** | `CacheStoreRequest {worker_id, outputs}` → 201 stored / 200 already present (first entry wins); 409 when an output blob is missing |

On a hit the worker reports the stored outputs without executing (`JobResult.reused = true`, plus `cache_key`); the client prints `status=reused` and installs the outputs exactly as for an executed job; the dashboard marks the job "reused from the shared cache". Viewers cannot read or write the cache. `JobResult.reused`/`cache_key` and `JobSummary.reused` are additive fields with defaults.

**Trust model:** a cache entry is as trustworthy as the worker host that stored it — clients can never write entries, so a compromised *client* cannot poison the cache, but a compromised *worker* (or worker token) can. Keep worker tokens on dedicated, controlled machines. Entries are keyed by the full action key (platform version, toolchain digest, inputs, environment allowlist), so workers with different OS versions or toolchains do not share entries.

## Known v1 limits

- Jobs live in coordinator memory: a restart loses queued/running jobs (clients fail with 404); blobs persist.
- No stdout/stderr transfer: action output appears on the worker's console only.
- No cancellation propagation from client to worker; a client that gives up leaves the job to run.
- No job routing by OS/toolchain, no fairness between clients, no blob garbage collection, single coordinator.
- The client runs actions sequentially (no remote parallelism yet) and does not consult the local cache.
