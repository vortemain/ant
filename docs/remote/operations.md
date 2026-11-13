# Operating a self-hosted Ant coordinator and workers

**Status:** the procedures below follow from the implemented behavior (protocol v1, D-014), but **have not yet been exercised on a real VPS** — only on one Windows machine over loopback. Treat this as the deployment plan to verify, not as proven operations.

## Topology

```
clients ──https──▶ reverse proxy (TLS) ──http──▶ ant coordinator (127.0.0.1:8742)
workers ──https──▶            "                      blobs on disk, jobs in memory
```

The coordinator speaks plain HTTP and refuses non-loopback binds by default. Put it behind a TLS-terminating reverse proxy on the same host (or a private network such as WireGuard, then use `--allow-insecure-http` on clients/workers knowingly).

## Install

Build a release binary on (or for) each machine: `cargo build --release -p ant-cli` → `target/release/ant` (`ant.exe` on Windows). The same binary is coordinator, worker, and client.

## Tokens

Generate two independent random tokens (≥ 32 characters), one for clients and one for workers, and store each in a file readable only by the service account:

```bash
umask 077
head -c 48 /dev/urandom | base64 | tr -d '/+=' > /etc/ant/client.token
head -c 48 /dev/urandom | base64 | tr -d '/+=' > /etc/ant/worker.token
```

Never pass tokens on the command line or in environment variables visible to other users. **Rotation:** write new token files and restart the coordinator, then update clients/workers; running jobs keep their state across a worker token change only if the coordinator was not restarted (jobs are in memory).

## Coordinator (Linux, systemd)

```ini
# /etc/systemd/system/ant-coordinator.service
[Unit]
Description=Ant coordinator
After=network-online.target

[Service]
User=ant
ExecStart=/usr/local/bin/ant coordinator --listen 127.0.0.1:8742 --data-dir /var/lib/ant \
  --client-token-file /etc/ant/client.token --worker-token-file /etc/ant/worker.token --max-body-mb 512
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/ant

[Install]
WantedBy=multi-user.target
```

The first stdout line is `coordinator listening=ADDR protocol_version=1`. Health check: `curl -fsS http://127.0.0.1:8742/v1/health`.

> The Linux build of Ant has not been compiled or tested yet (only the Windows target is installed in the development environment); verify it before relying on this unit.

## TLS reverse proxy (Caddy example)

```
build.example.com {
    reverse_proxy 127.0.0.1:8742
    request_body {
        max_size 512MB
    }
}
```

Caddy obtains and renews certificates automatically. Keep the proxy's body limit ≥ `--max-body-mb`.

## Workers

```bash
ant worker --coordinator https://build.example.com --token-file /etc/ant/worker.token \
  --work-dir /var/lib/ant-worker --id "$(hostname)"
```

Workers execute untrusted build code with the worker account's privileges. Run each worker as a dedicated, unprivileged account on a dedicated machine; on Windows add `--sandbox` (restricted-token write confinement, verified with the remote path). Toolchains referenced by plans (absolute paths or `PATH`) must exist on the worker. Several workers may share one coordinator; each processes one job at a time.

## Clients

```bash
ant remote run --coordinator https://build.example.com --token-file ~/.config/ant/client.token \
  --workspace . --plan build-plan.json
```

## Backup and restore

State on disk is only the blob store, `--data-dir/blobs` (content-addressed, immutable files; partial writes never appear because uploads are written to `blobs/tmp` and renamed). Back it up with any file-level tool while the coordinator runs; restore by copying it back before starting the coordinator. Queued/running jobs are **not** persisted: after a restart, clients re-run their plan.

## Upgrade

Stop workers, stop the coordinator, replace the binary, start the coordinator, then workers. Clients and workers must speak the same `protocol_version` as the coordinator (mismatches are rejected with 400).

## Dashboard and viewer token

Create a third token for read-only access and start the coordinator with `--viewer-token-file /etc/ant/viewer.token`. Open `https://build.example.com/ui` and paste the viewer token: the page shows job counts, every job (state, worker, attempts, submitted time, duration, exit code/error), and the workers seen since the coordinator started. The viewer token can read the control plane only — it cannot upload, submit, or lease. Share it with people who need visibility; keep client and worker tokens for machines. The dashboard is served by the coordinator itself (same origin as the API), so the reverse proxy needs no extra configuration.

## Monitoring and limits

- Health: `GET /v1/health`. Per-job state: `GET /v1/jobs/{id}` (client token).
- Disk: the blob store grows without bound (no garbage collection yet) — monitor `--data-dir` and prune manually only while stopped.
- A worker that disappears loses its lease after the action timeout (or one hour) + 120 s; the job is retried once on another lease, then fails.
