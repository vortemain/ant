# First local execution slice

## What works

The `ant run` command can turn one command line into an action or load a multi-action JSON plan. Standard output and error stream directly to the terminal. In plan mode, the full DAG is validated before execution. By default actions run sequentially in deterministic topological order and stop on the first process failure or cancellation. With `--jobs N`, up to `N` actions run concurrently: dependencies still complete successfully before their dependents start, and the first failure, timeout, or cancellation stops *new* actions from starting while already-running actions terminate; actions that never started are reported as `status=skipped`. Among simultaneously ready actions the start order is not deterministic when `--jobs` is greater than one. See [the plan format](build-plan-format.md).

```powershell
ant run --workspace C:\src\my-project --cwd . -- cargo build
```

Set a maximum runtime for the direct command with `--timeout-ms`:

```powershell
ant run --timeout-ms 600000 -- cargo build
```

Timeout stops the direct child, reports `status=timed_out`, and returns exit code 124. In a JSON plan, use the action's `timeout_ms` field.

To request controlled cancellation from another process, supply a marker path and create that file while the action is running:

Terminal 1:

```powershell
ant run --cancel-file .ant-cancel -- cargo build
```

Terminal 2, while the action is running:

```powershell
New-Item .ant-cancel -ItemType File
```

The runner polls the marker while waiting for the direct child, kills that child, reports `status=cancelled`, returns exit code 130, and does not start later plan actions. Remove the marker before reusing the path.

Ctrl+C is now handled cooperatively: pressing Ctrl+C while `ant run` executes cancels the running action, terminates its process tree, reports `status=cancelled`, and returns exit code 130 instead of killing the runner abruptly. If the handler cannot be installed, Ant prints a warning and falls back to `--cancel-file` for cooperative cancellation. Pressing Ctrl+C again does not force-quit; create the cancel marker or terminate the process if a hard stop is needed.

On Windows, every action's process tree is placed in a kill-on-close job object. When the action finishes — normally, on timeout, or on cancellation — any processes still running in that job (including grandchildren started by the action) are terminated. This prevents cancelled builds from leaving orphaned compilers or daemons behind. Descendant cleanup on non-Windows platforms remains best-effort and only covers the direct child.

## `--sandbox`: Windows restricted-token filesystem confinement

On Windows, `ant run --sandbox` executes each action inside a write-confined sandbox instead of the direct local executor. The design is documented in [docs/sandbox/windows-design.md](../sandbox/windows-design.md).

```powershell
ant run --sandbox --workspace C:\src\my-project -- cargo build
ant run --sandbox --workspace C:\src\my-project --plan build.json --jobs 4
```

What the sandbox does:

- Prepares a deterministic capability SID from the canonical workspace path and grants it write access on the workspace root (idempotent — repeated runs do not pile up ACEs).
- Spawns the action under a restricted token derived from your own token: privileges dropped, LUA-filtered, and write access restricted to that capability SID. Reads and execution stay unrestricted so real toolchains work; writes succeed only where a DACL grants the capability SID (the workspace root) or one of the inherently allowed restricting SIDs (logon/everyone).
- Inherits your stdout/stderr so output still streams live. Uses the same environment allowlist and declared values, the same input-unchanged and output-coverage checks, the same cancellation/timeout polling, and the same kill-on-close job for descendants.

What the sandbox does **not** do (honest limitations):

- **It does not block network.** Restricted tokens cannot prevent network access for same-user code. `network-isolation-unavailable` stays a blocker, so Ant never reuses cache entries from `--sandbox` runs — the cache mechanics are wired behind the blocker gate (decision D-003, T2.8), and the gate stays closed until a real network-isolation mechanism exists.
- **World-writable locations remain writable** (because the Everyone SID is a restricting SID). `%TEMP%` is in the environment allowlist and is typically Everyone-writable.
- **It is not an adversarial security boundary.** It is a defense against accidental escape (build scripts writing outside the workspace). Job breakaway, WMI, `ShellExecute`, and other escape vectors are not yet hardened.
- Filesystems without persistent ACLs (FAT, exFAT, some network shares) are detected by a post-grant verification and **fail closed** — the action is not executed unsandboxed.

Non-Windows: `--sandbox` fails closed with `sandbox unavailable`. It never falls back to unsandboxed execution.

The reported blockers under `--sandbox` drop `local-executor-unsandboxed` (the sandbox truthfully declares the `sandboxed` capability) while `network-isolation-unavailable` remains. `ant plan explain` is unaffected: its action key is executor-agnostic; only the reported blockers differ between run modes.

## `--network-isolated`: write confinement plus blocked network (Windows)

```powershell
ant sandbox setup --dry-run   # list the machine changes (no admin needed)
ant sandbox setup             # once, from an elevated terminal, as the user who runs Ant
ant sandbox status            # non-elevated verification; must end with ready=true
ant run --workspace C:\src\proj --plan build.json --network-isolated --cache-dir C:\ant-cache
```

`--network-isolated` (plan runs only; implies `--sandbox`) runs each action as the dedicated `AntSandboxOffline` account, whose traffic alone is blocked by firewall rules, while keeping the restricted-token write confinement. The setup is re-verified before every run and the run fails closed when it is not ready. This is the only executor that truthfully declares network isolation, so it is the one under which `--cache-dir` actually reuses results. Design, verification evidence, and limits (DNS resolution still leaves the machine; per-user toolchains such as `~/.cargo` are unreadable to the account; declare workspace-relative `TEMP`/`TMP`) are in [network isolation](../sandbox/network-isolation.md) and decision D-013. `ant sandbox teardown` (elevated) removes the account, rules, and stored credential.

## `--cache-dir`: cache participation behind the blocker gate

Plan runs can opt into cache participation:

```powershell
ant run --workspace C:\src\my-project --plan build.json --cache-dir C:\ant-cache
```

The mechanics are wired — Ant prepares each action once, computes its action-key candidate from the pre-run input digests (the same candidate printed as `key_candidate=`), and, only when that candidate reported **no blockers**, looks up and restores a verified entry instead of spawning the process (`status=reused`), or stores the verified outputs after a successful build.

Reuse happens only under `--network-isolated` (D-013): it is the one executor that removes `network-isolation-unavailable`, so a fully declared action (cache policy `eligible` + truthful toolchain identity) gets a blocker-free key, is stored after a successful run, and is reused later with its outputs restored and re-digested (proven end to end: store, delete output, `status=reused` with correct content, changed input rebuilds). Under the direct executor or `--sandbox` the network blocker remains, so nothing is stored or reused — also proven end to end.

Failure behavior: a cache lookup, restore, or integrity problem is treated as a miss and the action executes normally; a store problem is printed as a warning and the build continues. Only an unusable `--cache-dir` (or a preparation/execution failure) is fatal. Corrupted cache content is never trusted: restores re-digest every file and remove partial output on mismatch.

`--cache-dir` applies only to `ant run --plan`. The single-command form is rejected because its action is always `cache_policy: "never"`, so participation there could only be a silent no-op. `ant plan explain --cache-dir` remains presence-only diagnostics.

To record a machine-readable event stream, pass `--events PATH`. The file receives one JSON object per line (schema version 2, `build-events` contract): `run_started` (with execution mode and `--jobs` value), `action_started`, `action_finished` (status — including `reused` for an action satisfied from a verified cache entry — exit code, observed digests, action-key candidate, error text), `action_skipped`, and `run_finished` (outcome plus executed/skipped counts), all correlated by a per-invocation `run_id`. Events carry measured facts and no secret values. If the events file cannot be created the run aborts; if a write fails mid-run Ant prints a warning and suppresses further event writes.

If running from the project directory, `--workspace` may be omitted:

```powershell
cargo run -p ant-cli -- run -- cargo --version
```

## Current boundaries

- By default this is direct local process execution, not a sandbox. The process runs as the current user and can access everything that user can access. On Windows, `ant run --sandbox` enables write-confinement to the workspace (see above) but does not block network and is not an adversarial boundary.
- The working directory is canonicalized and must resolve beneath the workspace. This is a basic boundary check, not protection from races or filesystem access performed by the command.
- Child environment is cleared and repopulated from a small OS runtime allowlist plus action-declared variables. The Windows allowlist also includes the well-known machine root variables (`SystemDrive`, `ProgramFiles`, `ProgramFiles(x86)`, `ProgramW6432`) because toolchains consult them when locating their own linkers at spawn time (verified: `rustc` MSVC detection requires `ProgramFiles(x86)` and `SystemDrive`). Tool lookup relies on the host `PATH`.
- The CLI executes sequential JSON plans. Declared file inputs and tree inputs are SHA-256 digested before launch and checked again after successful execution; a changed input fails the action. Declared outputs are checked for existence and file/tree type, digested after success, and their digests are printed. Tree hashing is deterministic for UTF-8 paths and rejects symlinks. Digests do not include file metadata such as permissions or timestamps. Ant prints an explainable action-key candidate after success. With `--cache-dir`, cache lookup/store/restore runs behind the blocker gate (see above); keys are blocker-free, and therefore reusable, only under `--network-isolated`. These checks do not restrict filesystem access or prevent concurrent mutation races. Adapter-generated plans are available through `ant adapt` for Cargo and npm projects (see [build adapters](adapters.md)); generated cargo plans are sandbox-compatible — they declare workspace-relative `TMP`/`TEMP` pointing at an `.ant/tmp` staging directory generation creates, so `ant run --sandbox` can compile and link real cargo builds (path-dependency workspaces; registry-dependency builds still write to `~/.cargo` and fail under confinement). Dependency discovery, daemon, and remote execution are not implemented yet.
- Multi-action plans are supported through the versioned JSON plan format. The default is a sequential executor; `--jobs N` schedules actions concurrently with dependencies enforced, using `N` as a CPU-slot capacity against per-action `resources` requests (plus an optional `--memory-mb` budget) with strict FIFO admission so heavy actions are not starved (see [the plan format](build-plan-format.md#resource-weighted-scheduling)). Requests are admission weights unless `--enforce-memory` (Windows) turns declared `memory_mb` into a hard job-wide cap; job-tracked runs report measured peak memory. `--keep-going` keeps running independent actions after a failure (see [the plan format](build-plan-format.md#keep-going)). There is no cross-run fairness policy, CPU-time limit, or non-Windows enforcement yet. Declared file inputs/outputs are not connected to edges or enforced.
- The one-command CLI form declares no inputs or outputs. In JSON plans, declared inputs and outputs are validated and digested, but the executor does not restrict the process to those paths.
- Nonzero child exits are reported as failure, with CLI status 1. Signal termination is reported as `signal`.
- Cancellation is polled while waiting on the direct child. On Windows, descendant processes are cleaned up through a kill-on-close job object; on other platforms only the direct child is terminated. Cancellation during preflight/digest calculation is observed only before process launch or at the next wait poll.

Do not treat this command as safe for untrusted multi-tenant source. A future isolated worker requires a separate OS-specific sandbox implementation and security validation.



