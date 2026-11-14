# ADR-0002: Cargo metadata cache for cheaper plan generation

- Status: accepted
- Date: 2026-11-13

## Context

Plan generation in the cargo adapter calls `cargo metadata --format-version 1` to resolve the workspace dependency graph. This costs ~0.3–0.4 s per invocation — the dominant remaining cost on a no-change build after P3.1–P3.3. On a developer's edit loop, the dependency graph rarely changes between consecutive `ant build` calls, so the same expensive resolution runs again with the same result. The goal is to skip `cargo metadata` when nothing that affects the graph has changed, without weakening any honesty guarantee.

## Alternatives considered

1. Cache the entire generated `PlanFile`. Rejected: the tree walk (`declare_project`) is what detects new, removed, or modified project files; caching the whole plan would skip it and miss a file added or changed since the last run. The tree walk is cheap (pure directory I/O), so it should keep running.
2. Parallelize `cargo --version` with `cargo metadata`. Rejected as the sole optimization: it saves only ~0.1 s (the version call's startup), leaving the ~0.3–0.4 s metadata call. Useful as a follow-up but insufficient alone.
3. Cache the `cargo metadata` JSON keyed on manifest + lock digests + toolchain identity + options. Chosen: it elides the expensive call entirely on a cache hit while the tree walk still runs, so file-level changes are still detected.

## Decision

Add a per-user metadata cache (`<user cache>/cargo-metadata.json`, `ANT_CACHE_HOME` overrides the directory). Each entry stores the parsed `cargo metadata` JSON plus its validation key: canonical workspace root, adapter options hash, `cargo --version` output, cargo executable SHA-256 digest, content digests of every `Cargo.toml` reported by `cargo metadata`, and the `Cargo.lock` digest. On a cache hit (`try_metadata_cache`), the adapter skips `cargo metadata` and uses the cached JSON. On a miss or any validation failure, it runs `cargo metadata` and writes the result (`write_metadata_cache`).

Safety properties:
- The tree walk still runs on every invocation, so new, removed, or modified project files (non-manifest sources, build scripts, config) are detected — the cache only elides the dependency-graph resolution.
- `cargo --version` still runs on every invocation as the toolchain-identity safety net (a binary replaced with identical size and preserved mtime is not re-hashed by the toolchain digest cache; the version call catches it).
- Any cache problem (missing file, stale entry, unreadable manifest, changed lock, different toolchain, different options) falls back to a live `cargo metadata` call.
- The cache write is best-effort: failures are silently ignored, so the next run simply re-runs `cargo metadata`.

## Consequences

On a no-change build, plan generation skips the ~0.3–0.4 s `cargo metadata` call, reducing the plan-generation phase from ~0.5 s to ~0.1 s (the `cargo --version` call plus manifest re-digesting). The cache is per-user (not in the workspace, so it can never become a declared input or affect cache keys). Multiple projects and option sets coexist in the same file (keyed on workspace root + options hash). The cache file is portable across adapter versions only if the schema stays stable; a schema mismatch falls back to a live call (serde deserialization failure → cache miss).

## Risks

A manifest file that changes without a content-digest change (same content, different path) would be a false hit. This cannot happen because the manifest paths are stored in the cache entry and re-digested on each hit; a path that no longer exists produces an empty digest (cache miss). A `Cargo.lock` that is deleted between runs would also miss (the stored non-empty digest no longer matches the current empty one). The cache file could grow unbounded if many projects are built; a future cleanup or size limit may be needed.

## Reversal conditions

Remove the `try_metadata_cache`/`write_metadata_cache` calls from `CargoAdapter::generate` (reverts to always running `cargo metadata`). The cache file can be deleted at any time with no effect on correctness.
