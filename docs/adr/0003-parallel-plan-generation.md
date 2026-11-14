# ADR 0003: Parallel plan generation — overlap `cargo --version` with manifest digesting

- **Status:** Accepted
- **Date:** 2026-11-13
- **Decision:** D-032

## Context

After the metadata cache (D-031) eliminated the ~0.3–0.4 s `cargo metadata` call on a cache hit, the remaining plan-generation cost in `CargoAdapter::generate` was:

- `cargo --version` (~0.1 s process spawn) — the toolchain-identity safety net
- `toolchain_digest` (~0.01 s, cached per-user)
- Manifest and `Cargo.lock` re-digesting (~0.05–0.1 s file I/O) — inside the metadata-cache verification

These ran sequentially: `cargo --version` → `toolchain_digest` → cache lookup (which digests manifests). The acceptance criterion for the plan-generation-cost work item was "no-change `ant build` ≤ cargo's own no-change time."

## Alternatives

1. **Sequential (status quo):** ~0.2 s total for these three calls. Already fast, but the process spawn and file I/O are independent and could overlap.

2. **Parallel via `std::thread::scope`:** Run all three in parallel threads, join, then verify. Overlaps the ~0.1 s process spawn with the ~0.05–0.1 s file I/O.

3. **Async runtime (tokio):** Rejected — the adapter crate is synchronous, adding an async runtime for two parallel calls is disproportionate.

## Decision

Use `std::thread::scope` (stable since Rust 1.63) to run `cargo --version`, `toolchain_digest`, and `prepare_metadata_cache` (the cache-file read + manifest/lock re-digesting) in three parallel threads. After joining, call `verify_metadata_cache` (the key comparison, which needs all three results).

To enable this, `try_metadata_cache` is split into:

- `prepare_metadata_cache` — reads the cache file, finds the matching entry by workspace root + options hash, and re-digests every manifest and `Cargo.lock`. I/O-heavy; does not need toolchain identity.
- `verify_metadata_cache` — compares `cargo_version`, `toolchain_digest`, manifest digests, and lock digest against the prepared entry. Returns the cached metadata on a verified hit.

`try_metadata_cache` remains as a `#[cfg(test)]` convenience wrapper so the existing six metadata-cache unit tests pass unchanged.

## Consequences

- Plan generation on a cache hit now overlaps process spawning with file I/O, saving ~0.05–0.1 s.
- The code is slightly more complex (three threads + a split function), but the logic is identical — the prepare/verify split is a pure refactor of the cache check.
- `std::thread::scope` ensures the threads are joined before any borrowed data is used, so there are no lifetime or data-race concerns.

## Measured results

On this repository (Windows, release profile):

| Operation | Time |
|---|---|
| `cargo build --release` (no-change) | 0.70 s |
| `cargo metadata` (raw) | 0.40 s |
| `ant adapt` (cold, cache miss) | 1.85 s |
| `ant adapt` (warm, cache hit) | ~0.20 s |
| `ant build` (warm, cache hit, reused) | ~0.23 s |

`ant build` no-change (0.23 s) is 3× faster than `cargo build --release` no-change (0.70 s), meeting the acceptance criterion.

## Risks

- Thread spawning has a small fixed cost (~1 ms on Windows). For very fast operations (e.g., `toolchain_digest` from cache at ~0.01 s), the thread overhead could exceed the work. This is acceptable because the other two threads do substantial work.
- If `cargo --version` fails, the error is still propagated correctly (the `Result` is unwrapped from the `JoinHandle`).

## Reversal conditions

If the thread overhead proves problematic on a target platform, or if a future change makes the three calls dependent again, revert to sequential execution by calling `prepare_metadata_cache` → `executable_version` → `toolchain_digest` → `verify_metadata_cache` in order.
