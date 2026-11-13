# Windows sandbox design

**Status:** design groundwork, **with the first implementation landed and wired into `ant run --sandbox`**. Ant's sandboxed executor (`ant-sandbox-win` + `local-executor::execute_sandboxed_cancellable`) truthfully declares `sandboxed: true` and `metadata_fingerprinting: true`, removing the `local-executor-unsandboxed` blocker when `--sandbox` is in use. `network-isolation-unavailable` remains: restricted tokens cannot prevent network access for same-user code. Cache lookup/store/reuse mechanics are now wired into `ant run --plan --cache-dir` behind exactly that blocker gate (D-003 addendum, T2.8), so nothing is reused while the network blocker exists. This document records what was researched (2026-11-12), what was implemented (2026-11-12), the intended approach, and what must still be true before cache reuse can actually happen.

## Implementation status (2026-11-12)

The `ant-sandbox-win` crate (workspace member, deliberately not inheriting `[lints] workspace = true` per D-008) realizes the design below:

- **Deterministic capability SID** (`src/sid.rs`): `S-1-5-21-{a}-{b}-{c}-{d}` from `SHA-256("ant-sandbox-cap-v1\0" || canonical workspace path)`. Same workspace → same SID → idempotent ACL grant; one ACE per workspace; no state file.
- **ACL grant + fail-closed verification** (`src/acl.rs`): `GetNamedSecurityInfoW` → idempotent check → `SetEntriesInAclW` → `SetNamedSecurityInfoW`, then a re-fetch verifying the grant actually persists (FAT/exFAT/network shares fail closed with `ConfinementUnavailable`). `WRITE_ALLOW_MASK = FILE_GENERIC_READ | WRITE | EXECUTE | DELETE | FILE_DELETE_CHILD`, inherited `CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE`.
- **Restricted token** (`src/token.rs`, adapted from Codex `token.rs`, Apache-2.0): `CreateRestrictedToken(base, DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICT, …, restricting = [capability, logon, everyone])`; permissive default DACL (`SetEntriesInAclW` of `GENERIC_ALL` for those three SIDs); `SeChangeNotifyPrivilege` enabled so the confined process can walk readable directories.
- **Spawn** (`src/process.rs`): `CreateProcessAsUserW` with inherited std handles (`STARTF_USESTDHANDLES`), `CREATE_UNICODE_ENVIRONMENT`, `lpDesktop = "winsta0\\default"` (avoids `STATUS_DLL_INIT_FAILED` under restricted tokens), kill-on-close job object assigned right after spawn.
- **Public safe API** (`src/lib.rs`): `Sandbox::prepare(workspace)` and `Sandbox::spawn(argv, cwd, env)` returning a `Child` with `try_wait`/`terminate`/`exit_code`. Non-Windows builds compile (a `stub` module) and fail closed with `SandboxError::UnsupportedPlatform`.
- **Executor wiring** (`crates/local-executor/src/lib.rs`): `execute_sandboxed_cancellable` reuses `prepare_with_metadata`, prepares the sandbox (fail closed), spawns the confined process with the Ant environment allowlist plus declared values, runs the 40ms poll loop (cancel → terminate + cancelled, timeout → terminate + timed_out), and runs the shared `verify_inputs_and_digest_outputs` (refactored out of `execute_prepared`). `ExitStatus::from_raw` carries the confined exit code. The spawn cwd strips the `\\?\` verbatim prefix because `cmd.exe` rejects verbatim paths as a cwd ("UNC paths are not supported") and would fall back to the Windows directory, which the restricted token cannot write.
- **Fingerprint** (`crates/build-digest`): `ExecutionFingerprint::local_sandboxed` sets `sandboxed: true` + `metadata_fingerprinting: true`, keeps `network_isolation: false`.
- **CLI** (`crates/ant-cli`): `--sandbox` flag on both `ant run` forms, threaded through `execute_plan`/`run_plan`/`run_sequential`/`run_parallel`/`worker_loop`/`run_task` and `print_action_key` (which picks `local_sandboxed` vs `local_metadata_fingerprinting`). `ant plan explain` is intentionally unchanged — its key is executor-agnostic; only the reported blockers differ.
- **Verification**: 9 new tests (6 unit in `ant-sandbox-win`, 3 confinement integration spawning real confined `cmd` processes on Windows; 3 e2e in `ant-cli` proving inside-write success, outside-write fails with no escape file, and `--sandbox` appears in usage). Full suite 111 tests pass; clippy clean across all targets; live `ant run --sandbox` smoke run confirmed.

## Honest limitations (this is not a hardened security boundary)

This is a first-line write-confinement mechanism against accidental escape paths (build scripts writing outside the workspace). Adversarial escape analysis is **not** claimed:

- **Network is unrestricted.** Restricted tokens cannot prevent same-user network access; `network_isolation` stays false and cache reuse stays gated (D-003).
- **World-writable locations remain writable.** Because the `everyone` SID participates in the restricted write check (it is a restricting SID so ACLs granting Everyone keep working), any location whose DACL grants write to Everyone is still writable by the confined process.
- **Not an adversarial boundary.** Job breakaway, WMI, `ShellExecute`, and similar escape vectors are not yet analyzed; hardening them is future work before this capability is used to gate cache reuse end-to-end.
- **Reads and execution stay unrestricted** by design (real toolchains need to read system headers, run compilers, etc.); only write access is confined.
- **TEMP/TMP** are in the Ant environment allowlist on Windows, and `%TEMP%` is typically world-writable, so confined writes there can succeed. A future tightening can drop these from the allowlist or redirect them under the workspace.

## What was resolved before this design

Two of the original blockers are resolved truthfully by the current implementation and no longer require OS features:

- `platform-version-unspecified`: the executor captures the running OS version (`os_info`, the same source Cargo uses for its user agent) into the action key.
- `filesystem-metadata-not-fingerprintable`: declared input digests include filesystem metadata (size + last-modification time) in the executor's default digest mode, proven end-to-end by a test where a timestamp-only input change produces a different action key.

The remaining blockers require OS confinement primitives.

## Research findings

### The correct Windows primitive: restricted tokens, not AppContainer (first)

A normal restricted token or low integrity level does **not** enforce a path allowlist when files are writable by the same user. The workable design adds a second access check through *restricting SIDs*:

1. `CreateRestrictedToken` with `DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICT`.
2. One synthetic restricting SID per writable root; the launch token contains only the SIDs for the current profile's roots.
3. Narrow root-scoped NTFS ACL grants for those SIDs (inert unless the token contains the matching SID), plus explicit deny ACEs for protected paths such as `.git`.
4. `CreateProcessAsUserW` with a controlled environment and explicit inherited handles.
5. Job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, assigned before untrusted code runs (Ant already has this piece).
6. Fail closed: if the token, ACL provisioning, or policy cannot be enforced, the action must not run unrestricted.

**Network is not covered by this approach.** Restricted tokens do not prevent network access for code running as the same user. The follow-up investigation (`docs/sandbox/network-isolation.md`, decision D-009) established with primary-source evidence that per-run non-elevated WFP/firewall configuration is impossible (admin-gated `FWPM_ACTRL_ADD`), and the proven mechanism — firewall/WFP rules scoped to a dedicated local sandbox user created by elevated setup — cannot scope to our restricted-token capability SIDs. Ant keeps `network_isolation = false`; the cache gate stays closed regardless of filesystem progress (D-003), and the dedicated-sandbox-user path is documented as the future `ant sandbox setup` slice.

### AppContainer / LPAC are not the first fit

AppContainer and LPAC provide stronger isolation (including network) but change registry, credentials, COM/RPC, named-object, and network behavior; LPAC requires explicit access grants for nearly every runtime dependency. That is a poor fit for arbitrary shells, compilers, and package managers. They remain a future defense-in-depth mode.

### Microsoft MXC / `Experimental_CreateProcessInSandbox`

Microsoft's new `Experimental_CreateProcessInSandbox` with Bound File System policy is the likely long-term replacement for allow-side ACL grants, but it is experimental, Windows 11-only, uses schema version `0.1.0`, and Microsoft states its preview profiles are not yet security boundaries. Re-evaluate before M4.

## Ant's intended implementation shape

- A dedicated low-level FFI boundary crate (for example `ant-sandbox-win`) may carry carefully audited `unsafe`, excluded from the workspace-wide `unsafe_code = "forbid"` inheritance by explicit decision; the workspace discipline is audit-driven, not blanket-permissive.
- The sandboxed executor implements the same `prepare`/`execute_prepared` contract as the direct local executor, digests inputs in metadata mode, and declares `ExecutorCapabilities { sandboxed: true, network_isolation: <only when enforced>, metadata_fingerprinting: true }`.
- Provisioned ACLs must be journaled, inspectable, and removable; per-command recursive ACL mutation is rejected.
- Fail closed. No silent fallback to an unrestricted process, and no CLI flag may override capability reporting.

## Acceptance before wiring cache reuse

Met by the current implementation:
- Project writes succeed under the restricted token; same-user writes outside the workspace fail (verified end-to-end: a confined `cmd /C "echo x > abs\out.txt"` to an absolute path outside the workspace exits non-zero and creates no escape file).
- The confined process and its job-tracked descendants stay in the restricted token; kill-on-close job enforces cleanup on cancel/timeout/drop.

Not yet met (cache reuse stays gated by D-003):
- Junctions, symlinks, drive-relative paths, short names, and case variants cannot expand authority — partial (the executor rejects declared input/output symlink escapes and canonicalizes the workspace), but a full adversarial path-equivalence audit is not done.
- Child and grandchild processes cannot escape via job breakaway — not yet hardened.
- Network access is actually prevented (or fully recorded) by a demonstrated mechanism, not by policy intent — **open** (restricted tokens cannot block network).
- Change scenarios prove correct rebuild/reuse end-to-end, including a corrupted-cache miss — requires wiring the cache, which is intentionally inert until network isolation exists.

Until then, cache reuse stays impossible in `ant run` even though the lookup/store/restore mechanics are wired (D-003 addendum, T2.8): a key is consulted only when its candidate reported no blockers, and `network-isolation-unavailable` always applies today. `ant plan explain` continues to report the live blockers (`network-isolation-unavailable` always; `local-executor-unsandboxed` only when `--sandbox` is not the run mode).

## Primary references

- [CreateRestrictedToken](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken)
- [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
- [Launch an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)
- [CreateProcessInSandbox (experimental)](https://learn.microsoft.com/en-us/windows/win32/secauthz/createprocessinsandbox)
- [OpenAI Codex Windows token implementation](https://github.com/openai/codex/blob/63f8f547c0b95800f3c61ee172a2a833b75faf3f/codex-rs/windows-sandbox-rs/src/token.rs)
- [Kilo Windows sandbox analysis (issue 11542)](https://github.com/Kilo-Org/kilocode/issues/11542)
