# Cache keys and explainability

Ant computes a deterministic **action-key candidate** for every action from its full semantic content: command and arguments, working directory, timeout, sorted declared environment, declared input paths with their content digests, declared outputs, toolchain identity, OS/architecture, the canonical workspace path, and the inherited process environment. The key is a `sha256:` digest and is stable across machines for the same workspace content.

The key is called a *candidate* because it is only a candidate for cache addressing. Whether that address may actually be used is decided by **blockers**, and Ant defaults to blocking.

## Blockers

A candidate is cacheable only when it reports zero blockers. Blockers come in two kinds:

**Declared-content blockers** (fixable by declaring more in the plan):

- `policy-never` — the action sets `cache_policy: "never"` (the default).
- `missing-toolchain-identity` — no `toolchain` identity is declared.
- `missing-input-digest=PATH` — a declared input was not observed.
- `unexpected-input-digest=PATH` — an undeclared input was observed; declare it or fix the drift.
- `duplicate-input-digest=PATH` — the same input was provided twice.
- `platform-version-unspecified` — the executor did not pin an OS platform version.

**Executor-capability blockers** (fixable only by an executor that enforces them; there is no CLI override):

- `network-isolation-unavailable` — the executor did not prevent (or fully record) network access.
- `local-executor-unsandboxed` — the executor did not isolate the action from the host filesystem and OS identity.

Ant's direct local executor truthfully declares neither capability, so every local candidate reports both and is ineligible. This is deliberate: storing or reusing results whose execution was not isolated could serve stale, influenced, or poisoned outputs as if they were fresh ones. The intended Windows sandbox design is documented in [`docs/sandbox/windows-design.md`](../sandbox/windows-design.md).

Two former blockers are resolved truthfully by the current implementation and never appear anymore:

- `platform-version-unspecified` — resolved: the executor captures the running OS version into the key.
- `filesystem-metadata-not-fingerprintable` — resolved: declared input digests include filesystem metadata.

## Input digest mode: content plus metadata

The executor's default digest mode hashes each declared input's **content, size, and last-modification time**. Consequences you can observe:

- A timestamp-only change to a declared input produces a different action key (and would prevent cache reuse). This is intentional: a build step can read input timestamps to decide whether to regenerate, so metadata is semantically load-bearing.
- Fresh clones or checkouts typically re-set modification times, so their keys differ from the original workspace's even for identical content. Conservative by design.
- Declared **outputs** are always digested by content only, so a cache can verify a restored artifact without reproducing timestamps.

## Inspecting a plan: `ant plan explain`

`ant plan explain` reports the same facts without executing anything:

```powershell
ant plan explain --plan build-plan.json [--workspace PATH] [--cache-dir DIR]
```

Example output:

```text
plan=C:\dev\proj\plan.json mode=explain actions=1 cache=C:\dev\proj\.ant-cache
action=build status=prepared key_candidate=sha256:19df533e…baf1de7 cache_eligible=false
  input=in.txt digest=sha256:3b6ec9a1…c1b88d
  blocker=policy-never
  blocker=missing-toolchain-identity
  blocker=network-isolation-unavailable
  blocker=local-executor-unsandboxed
  cache_entry=absent
plan=C:\dev\proj\plan.json explain=complete result=prepared
```

- `status=prepared` — every declared input exists, has the declared type, and is inside the workspace; the digests shown are exactly what a subsequent `ant run` would verify against.
- `status=prepare_failed error=…` — the action could not be prepared (typically a missing input). The command exits 1 after reporting all actions.
- `blocker=…` — one line per cacheability blocker, with stable labels.
- `cache_entry=present|absent` — reported only when `--cache-dir` is given, and only a fact about that directory: `ant run --plan --cache-dir` performs lookup, storage, and reuse only for blocker-free keys; the only executor that can produce them is `--network-isolated` (decision D-013; see [local execution](local-execution.md)). `plan explain` itself never stores or reuses.

`--cache-dir` and `--workspace` are accepted only by `plan explain` and are rejected elsewhere.
