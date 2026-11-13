# Local JSON build plans

The `ant run --plan PATH` form executes a small versioned JSON plan. Paths in actions are relative to the selected workspace. The plan file itself is read relative to the current process directory. Set `--workspace` when that is not the intended project root.

The envelope below is the canonical plan-file contract, owned by `build-graph` (`PLAN_SCHEMA_VERSION`, `PlanFile`): `ant run --plan` loads it and `ant adapt` writes it, so adapter output and hand-written plans share one format and one validation path.

Validate the plan without running any command:

```powershell
ant plan validate --plan build-plan.json
```

This prints the stable topological action order after validating the plan schema and dependency graph.

To inspect each action contract without executing it:

```powershell
ant plan describe --plan build-plan.json
```

The description includes command arguments, workspace directory, dependencies, declared inputs/outputs, cache policy, and timeout.

```json
{
  "schema_version": 1,
  "actions": [
    {
      "action": {
        "schema_version": 1,
        "id": "metadata",
        "argv": ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        "working_directory": ".",
        "inputs": [],
        "outputs": [],
        "environment": [],
        "cache_policy": "never",
        "timeout_ms": 600000
      },
      "dependencies": []
    },
    {
      "action": {
        "schema_version": 1,
        "id": "compile",
        "argv": ["cargo", "build"],
        "working_directory": ".",
        "inputs": [],
        "outputs": [],
        "environment": [],
        "cache_policy": "never",
        "timeout_ms": 600000,
        "resources": { "cpu": 4, "memory_mb": 2048 }
      },
      "dependencies": ["metadata"]
    }
  ]
}
```

Run it with:

```powershell
ant run --workspace C:\src\my-project --plan build-plan.json --jobs 4
```

The runner validates the entire plan (schema version, action contracts, IDs, dependency references, and acyclicity) before launching the first process. By default it executes one action at a time in deterministic action-ID order among ready actions. `--jobs N` allows up to `N` concurrently running actions; dependencies are still enforced, the first failure stops new starts (already-running actions finish), and unstarted actions are reported as `status=skipped`. Start order among concurrently ready actions is not deterministic with `--jobs` above one. The first process failure stops the plan; there is no keep-going option yet, and `--jobs` accepts only positive integers.

### Resource-weighted scheduling

The optional `resources` object is a scheduling request: `cpu` (CPU slots, integer ≥ 1, default 1) and `memory_mb` (expected peak memory in MiB, integer ≥ 1, default undeclared). Unknown fields and zero values are rejected at validation. With `--jobs N` above one, `N` is the CPU-slot capacity and `--memory-mb M` an optional memory budget; an action starts only when its request fits the remaining capacity, and the capacity is released when it finishes (on every outcome).

- **Strict FIFO admission.** Ready actions are admitted in queue order (initially ready actions by action ID, then in the order dependencies complete). When the head action does not fit, nothing behind it starts — a heavy action is never starved by a stream of light ones, at the cost of some idle capacity.
- **Clamping, not deadlock.** A request larger than the whole capacity is clamped to it and a `ant: note: action=… requests … clamped to …` line is printed before the run, so the action runs once enough capacity is free instead of blocking the plan forever.
- **Undeclared memory counts as zero** against `--memory-mb`; the budget only constrains actions that declare `memory_mb`. Without `--memory-mb`, declared memory is not constrained.
- **Admission weights, not limits.** Ant does not measure or cap a running process's CPU or memory; a request is a promise by the plan author. `--jobs 1` (the default) runs strictly sequentially, so weights have no effect there.
- **Not part of the action key.** Resources cannot change an action's outputs, so they are excluded from the action-key candidate (decision D-011); plans without `resources` serialize exactly as before.

The optional `timeout_ms` field limits each process runtime; zero is invalid and omitted/null means no time limit. The `dependencies` field expresses ordering only. Actions must be written so the dependency's successful process exit is a meaningful prerequisite; the runner does not verify the declared input/output relationship between actions. Declared inputs and outputs are hashed with SHA-256; inputs are checked for changes across successful action execution, and output digests are reported. This is integrity observation, not access control or a cache. Symlinks in declared file/tree paths are rejected; tree hashing also requires UTF-8 relative paths. `cache_policy` is descriptive only because no cache is connected. The local executor is unsandboxed; see [local execution limits](local-execution.md).

