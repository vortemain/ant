# Network isolation options (Windows)

**Status:** investigation complete (2026-11-13). Decision recorded in `docs/plan.md` D-009: Ant keeps `network_isolation = false`, so cache reuse cannot happen (D-003; since the T2.8 addendum the lookup/store/restore mechanics are wired behind the blocker gate, and the gate stays closed exactly because this blocker is live). No mechanism evaluated here can be truthfully declared by the current restricted-token executor, and no code was added that pretends otherwise.

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

None of this is implemented. Until it is — or option B/C matures — `network_isolation` stays false, `network-isolation-unavailable` remains a live blocker, and the cache gate stays closed per D-003.

## References

- `FwpmFilterAdd0` access requirements: <https://learn.microsoft.com/en-us/windows/win32/api/fwpmu/nf-fwpmu-fwpmfilteradd0> (accessed 2026-11-13)
- WFP management/filtering layers: <https://learn.microsoft.com/en-us/windows/desktop/FWP/management-filtering-layer-identifiers->
- Codex `firewall.rs` (studied): <https://github.com/openai/codex/blob/63f8f547c0b95800f3c61ee172a2a833b75faf3f/codex-rs/windows-sandbox-rs/src/bin/setup_main/win/firewall.rs>
- Codex `sandbox_users.rs` (studied): <https://github.com/openai/codex/blob/63f8f547c0b95800f3c61ee172a2a833b75faf3f/codex-rs/windows-sandbox-rs/src/bin/setup_main/win/sandbox_users.rs>
- `zagens-windows-sandbox` 0.8.9 source (read 2026-11-13, rejected as dependency per D-008): <https://crates.io/crates/zagens-windows-sandbox>
