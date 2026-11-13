# Network isolation options (Windows)

**Status:** option A **implemented and verified on Windows 11** (2026-11-13, T2.9–T2.10, decision D-013) as the opt-in `ant run --plan --network-isolated` mode. The plain restricted-token executor (`--sandbox`) still cannot declare network isolation and stays gated. The investigation below (D-009) is kept as the rationale.

## Implementation (D-013)

**One-time elevated setup** — `ant sandbox setup` (embeds `scripts/sandbox/setup.ps1`; `--dry-run` lists the changes without admin; `ant sandbox teardown` reverts everything):

- local group `AntSandboxUsers` and local account `AntSandboxOffline` (CSPRNG 32-char password, never expires, member of Users so it may log on, hidden from the sign-in screen via `SpecialAccounts\UserList`);
- Windows Firewall rules `AntSandbox-BlockOutbound` / `AntSandbox-BlockInbound`: block, all protocols, all profiles, scoped with `-LocalUser "O:LSD:(A;;CC;;;<account SID>)"` so they match **only** that account's traffic; read back after creation;
- refuses to proceed when a group policy overrides local rules (`LocalPolicyModifyState != 0`) or any firewall profile is disabled;
- the password is stored only as a current-user DPAPI blob (`%LOCALAPPDATA%\Ant\sandbox\offline.cred`, via `ConvertFrom-SecureString`) — never printed, logged, or put in plans, keys, or events; a versioned marker (`setup.json`) records the account SID and the Windows user it was installed for.

**Per-run verification (fail closed)** — every `--network-isolated` run first executes the embedded `status.ps1` (non-elevated): marker version, installed-for-this-user, account present and enabled with the recorded SID, credential decryptable, no policy override, all profiles enabled, both rules present as enabled blocks scoped to the SID. Any failure aborts the run before an action starts.

**Run path** — a non-elevated process cannot hand another user's token to `CreateProcessAsUserW` (needs `SeAssignPrimaryTokenPrivilege`), so Ant starts **its own runner** (`ant __sandbox-runner`) as the account through the Secondary Logon service (`CreateProcessWithLogonW`, profile loaded), **suspended**, joins it to a kill-on-close job (plus the `--enforce-memory` cap when requested), and only then resumes it. The runner restricts *its own* token for the workspace (the same `WRITE_RESTRICT` capability-SID token as `--sandbox`; allowed for any caller) and runs the action. Before launch, the workspace owner grants the per-workspace capability SID and the account SID write access to the workspace root. The action's key is computed with `ExecutionFingerprint::local_network_isolated` (sandboxed + network-isolated + metadata fingerprinting).

**Verified on this machine** (end-to-end tests skip with a printed reason when the setup is absent):

- `whoami` inside the action reports `antsandboxoffline`; the same plan without the flag reports the real user;
- `curl.exe https://1.1.1.1/` connects without the flag (HTTP 301) and fails under it in ~1 ms with `WSAEACCES` ("Bad access") — a local firewall refusal, not a timeout;
- write confinement still holds (writing `C:\Projects\…` outside the workspace is denied even though `Authenticated Users` may write there);
- the action key reports no executor blockers, so the cache gate opens: store → delete output → `status=reused` with the output restored and integrity-checked → changed input rebuilds; the same plan under `--sandbox` alone stays gated on `network-isolation-unavailable`.

**Honest limits**

- **DNS resolution still leaves the machine.** Name lookups go through the system DNS Client service, which runs as another account and is not matched by the account-scoped rules (verified: `example.com` resolves under isolation, then the connect is refused). Data could be exfiltrated or fetched through DNS queries/responses. D-013 accepts this residual channel for cache gating because it requires deliberately crafted DNS tunnelling; it is not an adversarial boundary (consistent with `windows-design.md`).
- **Loopback traffic is not verified as blocked** (Windows Firewall does not filter loopback by default); local services on the machine may be reachable.
- **Toolchains in the user profile are unreadable** to the account (for example `~/.cargo`, `~/.rustup`, per-user Node installs), so most adapter-generated plans fail under this mode unless their toolchains are installed machine-wide. Proven workloads use System32 tools.
- **Windows PowerShell falls into Constrained Language Mode** as the account unless `TEMP`/`TMP` point at a writable location (declare workspace-relative `TEMP`/`TMP`, as adapter plans do).
- Network granularity is **per machine** (one shared account); write granularity stays per workspace. Single-command `ant run` does not support the mode yet.

## The requirement

`build-digest::ExecutorCapabilities::network_isolation` may only be set by an executor that actually prevents (or fully records) network access for the action's process tree. The cache gate (D-003) opens only when the same executor also truthfully declares `sandboxed`. Today's sandbox (`ant-sandbox-win`, `docs/sandbox/windows-design.md`) confines **writes** via restricted tokens; it cannot confine network access — restricted tokens do not create a network identity distinguishable from the user's normal processes.

## What the proven reference implementations actually do

Both shipped Windows sandboxes isolate network with the same primitive: **Windows Firewall / WFP rules scoped to a dedicated local user account**, created by an elevated one-time setup. Neither blocks network through the restricted token itself.

### OpenAI Codex (`codex-rs/windows-sandbox-rs`, Apache-2.0, studied 2026-11-12/13)

- Elevated `setup_main` binary: creates an `CodexSandboxUsers` local group and two local accounts (`offline`/`online`) via `NetUserAdd` with random passwords, DPAPI-protected and stored under the Codex home; a versioned, ACL-protected `setup_marker.json` makes readiness checkable and fails closed while setup is in progress.
- Network enforcement (`bin/setup_main/win/firewall.rs`): firewall rules via `INetFwPolicy2` COM, scoped with `INetFwRule3::LocalUserAuthorizedList = "O:LSD:(A;;CC;;;{offline_sid})"` — i.e. the rule applies **only to processes running as the offline sandbox account**. Block-outbound rule for all non-loopback remote addresses (all IP protocols), plus loopback TCP/UDP blocks narrowed to an allowed proxy-port complement for its local proxy IPC.
- Fail-closed validation: `INetFwPolicy2::LocalPolicyModifyState` must be `NET_FW_MODIFY_STATE_OK` on all profiles; a group-policy override or partial profile coverage is a setup failure, not a silent no-op. Rules are re-applied idempotently with a read-back verification of the SID scope.
- At run time the child runs **as the offline user** (LogonUser + CreateProcessAsUser), so the firewall rule matches it.

### `zagens-windows-sandbox` 0.8.9 (source read for research on 2026-11-13; still rejected as a dependency — D-008)

- Same shape: elevated setup provisions dedicated sandbox local users (`src/bin/setup_main/win/sandbox_users.rs`).
- Native WFP (`src/wfp.rs`, `src/wfp/filter_specs.rs`): persistent provider + sublayer, `FwpmFilterAdd0` block filters on `FWPM_LAYER_ALE_AUTH_CONNECT_V4/V6` and `FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4/V6`, conditions combining `FWPM_CONDITION_ALE_USER_ID` (a security-descriptor blob matching the sandbox account, `FWP_SECURITY_DESCRIPTOR_TYPE`) with protocol/remote-port/loopback-address conditions. Permits for loopback IPC are weighted above the blocks. Notably it also blocks DNS (ports 53 and 853) explicitly and documents that `FWPM_LAYER_NAME_RESOLUTION_CACHE` filters did not validate (`FWP_E_OUT_OF_BOUNDS`) for static filter shapes — a reminder that name-resolution caches can leak DNS unless the connect-layer blocks make them moot.

### Why the restricted token cannot match these rules

The `ALE_USER_ID` / `LocalUserAuthorizedList` scope matches the **user SID of the connecting token**. `CreateRestrictedToken` keeps the original user SID; restricting/capability SIDs are *additional* checks, not a change of network identity. A firewall rule scoped to the real user would therefore block the user's own unrelated processes, and there is no condition type for our per-workspace capability SIDs (WFP has `FWPM_CONDITION_ALE_PACKAGE_ID` for AppContainer package SIDs only).

## Privilege requirement (Microsoft-documented)

`FwpmFilterAdd0` requires `FWPM_ACTRL_ADD` on the filter container and `FWPM_ACTRL_ADD_LINK` on provider/layer/sublayer (`learn.microsoft.com/en-us/windows/win32/api/fwpmu/nf-fwpmu-fwpmfilteradd0`, accessed 2026-11-13). The default WFP access-control model grants those to administrators; the firewall COM API (`INetFwPolicy2::Rules`) likewise requires elevation. **Per-run, non-elevated WFP/firewall configuration is therefore not possible.**

## Options evaluated

| Option | Enforcement quality | Elevation needed | Runtime compatibility | Verdict |
|---|---|---|---|---|
| **A. Dedicated sandbox user + firewall/WFP rules** (Codex/zagens model) | Kernel-enforced, block outbound all protocols (+ optional DNS specifics); loopback allows for local IPC | One-time elevated setup; per-run `LogonUser` | Proven by two shipped implementations; child runs as another user, so profile/TEMP/ACLs must be granted to that user (Ant's workspace capability-SID grants already work for any token containing the SID) | **Intended path.** Highest honesty and effort; changes machine state; documented in D-009 as the future `ant sandbox setup` slice |
| **B. AppContainer / LPAC** | Per-process network denied by default (no `internetClient` capability granted) | None | Changes named-object namespace, profile paths, COM/RPC; LPAC needs grants for nearly every runtime dep; poor fit for arbitrary toolchains (see `windows-design.md`) | Future opt-in defense-in-depth mode, not the cache-gate opener |
| **C. `Experimental_CreateProcessInSandbox` (Bound File System)** | Promises network policy + filesystem bound | None documented (Windows 11 24H2+) | Windows 11-only, schema `0.1.0`, Microsoft: preview profiles are *not yet* security boundaries | Re-evaluate before M4; not declarable today |
| **D. Per-run non-elevated WFP** | — | — | Impossible: filter addition is admin-gated | Rejected |
| **E. Policy-only controls** (hosts file, `HTTP_PROXY`/`NO_PROXY`, firewall UI guidance, job-object limits) | None — code can bypass all of them | None | — | **Rejected as dishonest**: could never support a truthful `network_isolation = true` |
| **F. VM/container execution modes** (Windows Sandbox, WSL, Docker) | Real isolation | Varies | Heavy lifecycle (images, startup latency), different output/artifact semantics | Out of scope for local mode; candidate for remote/hybrid milestones |

## What option A would mean for Ant (future slice sketch)

1. `ant sandbox setup` (elevated, once per machine): create `AntSandboxUsers` group + one offline user; DPAPI-protect and store credentials locally (never in plan files, logs, cache keys, or event streams); add idempotent, named, read-back-verified block rules scoped to the offline SID; validate `LocalPolicyModifyState` fail-closed; write a versioned setup marker.
2. Run path (only when setup is present and valid): `LogonUser(offline)` → `CreateRestrictedToken` from that token with the workspace capability SID (filesystem confinement unchanged) → `CreateProcessAsUserW`; grant the workspace ACL to a SID the sandbox token contains (already deterministic per workspace).
3. Honest consequences to design for: the offline user's profile/TEMP are different paths (environment allowlist must not leak the real user's TEMP); the same offline user is shared across workspaces, so network granularity is per-machine, while write granularity stays per-workspace; uninstall/teardown must remove users and rules; `LogonUser` failure or a missing/invalid setup marker fails closed.

This sketch is now implemented as described in the Implementation section above (D-013). For every executor other than `--network-isolated`, `network_isolation` stays false and the cache gate stays closed per D-003.

## References

- `FwpmFilterAdd0` access requirements: <https://learn.microsoft.com/en-us/windows/win32/api/fwpmu/nf-fwpmu-fwpmfilteradd0> (accessed 2026-11-13)
- WFP management/filtering layers: <https://learn.microsoft.com/en-us/windows/desktop/FWP/management-filtering-layer-identifiers->
- Codex `firewall.rs` (studied): <https://github.com/openai/codex/blob/63f8f547c0b95800f3c61ee172a2a833b75faf3f/codex-rs/windows-sandbox-rs/src/bin/setup_main/win/firewall.rs>
- Codex `sandbox_users.rs` (studied): <https://github.com/openai/codex/blob/63f8f547c0b95800f3c61ee172a2a833b75faf3f/codex-rs/windows-sandbox-rs/src/bin/setup_main/win/sandbox_users.rs>
- `zagens-windows-sandbox` 0.8.9 source (read 2026-11-13, rejected as dependency per D-008): <https://crates.io/crates/zagens-windows-sandbox>
