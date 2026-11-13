# Initial threat model

**Status:** Initial qualitative model. No sandbox or security control has been implemented or validated yet.

## Assets

Source trees; dependency/toolchain inputs; developer and service credentials; secret values; build logs; cache metadata and blobs; artifacts and provenance; tenant/project metadata; worker and coordinator integrity; availability and resource capacity.

## Actors and trust zones

1. Developer and local CLI: user-controlled machine and workspace; local account is trusted by its owner but project content may be hostile.
2. Coordinator/control plane: trusted policy and scheduling service; high-value target.
3. Worker host: organization-controlled infrastructure but executes untrusted project commands.
4. Action sandbox: hostile workload zone with only declared files, permitted network, bounded resources, and narrowly scoped credentials.
5. Cache/object storage: integrity-sensitive shared service; contents may originate from untrusted or compromised workers.
6. External registries/repositories: dependencies and source acquisition are untrusted until integrity and policy checks pass.

## Threats and required controls

| Threat | Potential impact | Required design control | Validation evidence before production |
|---|---|---|---|
| Malicious build command | host compromise, data theft | OS-appropriate sandbox, least privilege, resource and network policy | escape testing and threat-specific review per supported OS |
| Path traversal or symlink race | read/write outside declared workspace | normalized paths, descriptor-based safe operations where possible, symlink policy | adversarial path corpus and race tests |
| Secret exfiltration through network/log/output | credential compromise | short-lived scoped secret injection, masking limits, egress policy, no secret-bearing cache | deliberate exfiltration scenarios; secret values absent from logs/artifacts/cache |
| Cache poisoning or tampering | incorrect or malicious artifact | content digest verification, authenticated metadata, scoped namespaces, conservative eligibility | corrupted blob and malicious metadata tests |
| Cross-project/tenant cache leakage | source/artifact disclosure | namespace authorization and policy-aware sharing | isolation tests across identities and cache operations |
| Worker compromise | fleet pivot, forged outputs | worker identity, minimal credentials, ephemeral workloads, coordinator verification | compromised-worker assumptions and recovery exercise |
| Dependency substitution | malicious build input | integrity-checked lockfiles/artifacts, registry policy, provenance | changed digest/signature rejection scenarios |
| Denial of service | unavailable workers/coordinator | quotas, bounded queues, CPU/memory/disk/time limits, admission control | load/stress and resource exhaustion tests |
| Coordinator compromise | fleet-wide execution/data access | reduce worker authority, rotate credentials, audit/control separation | architecture review, incident and key rotation exercise |
| Artifact tampering after build | unsafe release | digest verification, signed provenance where configured | modified artifact rejection and verification-chain tests |

## Security invariants

- An action receives only declared inputs and outputs within its assigned workspace boundary.
- Secret values never enter ordinary action serialization or cache keys.
- A cache hit is accepted only after key compatibility and content integrity checks.
- Cancellation, timeout, and worker loss cannot silently mark incomplete output successful.
- Every privileged policy decision and credential lifecycle event is auditable.

## Open questions

Sandbox options differ by operating system and deployment privileges. Network denial, filesystem isolation, process/job controls, nested virtualization, macOS signing constraints, Windows job objects, Linux namespaces/seccomp, and container-runtime assumptions require platform-specific research. Do not advertise a security level until controls and escape tests are concrete.
