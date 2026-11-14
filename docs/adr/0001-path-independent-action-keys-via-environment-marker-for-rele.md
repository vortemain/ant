# ADR-0001: Path-independent action keys via environment marker for release builds

- Status: accepted
- Date: 2026-11-13

## Context

Local cache keys include the canonical workspace path, so a second clone of the project in another folder always misses (noted as "not yet" in P3.2). Some builds — notably `cargo build --release` — do not embed the absolute workspace path in their outputs (release binaries have no debug info), so the path is unnecessarily conservative for them. Dev builds do embed absolute source paths in debug info, so they must stay path-dependent. The goal is to let adapters declare per-action that their outputs are path-independent, so a second clone restores instead of rebuilding.

## Alternatives considered

1. Add an `outputs_embed_workspace_path: bool` field to the Action struct (serde default true). Rejected: it would have broken every Action struct literal in the codebase, including in protected test files (build-graph/tests/properties.rs constructs PlanAction, build-action/tests construct Action, ant-cli e2e tests use serialized plans), and the PlanAction/BuildNode structs would also need the field. 2. Add `path_independent_outputs: bool` to ExecutionFingerprint. Rejected: the fingerprint is constructed by executor helpers and directly in protected test files (build-digest tests construct ExecutionFingerprint with all fields), so the field would need to be added at every construction site.

## Decision

Add a `build_digest::PATH_INDEPENDENT_MARKER` constant ("_ANT_PATH_INDEPENDENT"). When an adapter places this key in an action's `environment`, `action_key_candidate` excludes the workspace path from the key. The cargo adapter sets the marker for `--release` builds. Dev builds keep the workspace path in the key. The marker approach avoids adding a field to Action/PlanAction/BuildNode/ExecutionFingerprint (which would break all struct literals, including in protected test files).

## Consequences

Path-independent keys become opt-in per action via a single env-var push in the adapter; no core struct changes, no test-file breakage. The marker is hashed into the key (it is part of action.environment), so path-independent and path-dependent keys for the same action can never collide. Adapters can adopt this independently. The marker env var is visible in plans (e.g. `ant plan describe`), which makes the declaration auditable.

## Risks

Adapters that set the marker when outputs actually embed the path (e.g. user code with `env!("CARGO_MANIFEST_DIR")`) would produce stale restorations in another folder. Mitigated by: only the cargo adapter sets it, only for release, and this is a documented limitation. Dev builds (the default) are unaffected.

## Reversal conditions

Remove the marker from the cargo adapter (reverts release builds to path-dependent keys) or remove the marker check in action_key_candidate (reverts all to path-dependent). The constant can stay for future use.
