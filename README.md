# Ant

A local-first, remotely executable build platform. This repository is at project inception; see [the architecture](docs/architecture.md) and [the agent handoff guide](AGENTS.md) before making changes.

## Status

Stage 0 architecture validation is in progress. The initial Rust implementation includes a validated action model, a deterministic build DAG API, a JSON plan runner with sequential and bounded-parallel (`--jobs N`) execution with resource-weighted admission (per-action `resources` requests, optional `--memory-mb` budget), `--keep-going`, opt-in enforced Windows memory caps (`--enforce-memory`) with measured peak memory, a direct local process executor with a prepare/execute split, SHA-256 digests for declared files and trees (input digests include filesystem metadata), action-key candidates with capability-driven blockers (platform version captured), a content-addressed local cache library, `ant plan explain` cacheability diagnostics, cooperative cancellation/timeouts, Ctrl+C handling, Windows descendant-process cleanup, a schema-versioned JSONL event stream (`--events`, schema v2), a Windows restricted-token filesystem sandbox (`ant-sandbox-win`, wired into `ant run --sandbox`) that confines writes to the workspace, cache lookup/store/reuse wired behind the blocker gate (`ant run --plan --cache-dir DIR`; nothing is reused while blockers remain — proven end-to-end), and the first build-system adapters: `ant adapt --adapter cargo` generates validated plans for real Cargo workspaces (per-member build actions, dependency edges, truthful toolchain identity) and `ant adapt --adapter npm` generates a `npm run build` action for real npm projects (no guessed outputs, truthful `node` toolchain identity, `cmd /C` on Windows because `npm` is a batch shim); generated plans run under `ant run --sandbox` via a workspace-relative scratch directory; adapt → run → real artifact proven end-to-end for both; without `--adapter`, `ant adapt` uses the single adapter that recognizes the project (refusing to guess when several do) and `ant adapt --detect` reports per-adapter detection diagnostics. The workspace is covered by 175 passing tests (including seeded property tests) behind a local quality gate (`scripts/check.ps1`) (unit, confinement integration, and end-to-end CLI) and compiles clippy-warning-free. The cache gate stays closed because network isolation requires a dedicated sandbox user + elevated setup (per-run non-elevated WFP is admin-gated and cannot scope to restricted-token capability SIDs — investigation: `docs/sandbox/network-isolation.md`, D-009), so `network-isolation-unavailable` remains (design + implementation + honest limitations: `docs/sandbox/windows-design.md`); generated adapter actions are `cache_policy: never` because dependency sources cannot be declared; remote execution is not implemented.

## CLI

```powershell
cargo run -p ant-cli -- --version
cargo run -p ant-cli -- run --timeout-ms 600000 -- cargo build
cargo run -p ant-cli -- plan validate --plan build-plan.json
cargo run -p ant-cli -- plan describe --plan build-plan.json
cargo run -p ant-cli -- plan explain --plan build-plan.json --cache-dir .ant-cache
cargo run -p ant-cli -- adapt --adapter cargo --project path\to\cargo-project --out build-plan.json
```

See the [CLI guide](docs/cli/local-execution.md), [build plan format](docs/cli/build-plan-format.md), [cache keys and explainability](docs/cli/cache-and-keys.md), and [build adapters](docs/cli/adapters.md).

## Handoff entry points

- [Agent handoff rules](AGENTS.md)
- [Architecture and trust boundaries](docs/architecture.md)
- [Initial threat model](docs/security/threat-model.md)
- [Benchmark methodology](docs/benchmarks/methodology.md)
- [First local execution slice and limits](docs/cli/local-execution.md)
- [Local JSON build plan format](docs/cli/build-plan-format.md)
- [Cache keys and explainability](docs/cli/cache-and-keys.md)
- [Build adapters (`ant adapt`)](docs/cli/adapters.md)
- [Windows sandbox design (implementation + honest limitations)](docs/sandbox/windows-design.md)
- [Network isolation options (investigation + D-009)](docs/sandbox/network-isolation.md)
- [Implementation plan and decision log](docs/plan.md)
- [Requirements traceability](docs/requirements-traceability.md)
- [Research sources and open questions](docs/research.md)

## Current decisions

- Rust core in a Cargo workspace; keep members responsibility-driven.
- Start with a canonical action model and local execution semantics before remote transport.
- Treat actions as non-cacheable unless declared inputs and execution identity are complete.
- Remote execution API compatibility is an adapter boundary to investigate, not an assumption of full conformance.

See the plan for staged acceptance criteria and unresolved decisions.




