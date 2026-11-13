# Shared remote cache vs. plain cargo (2026-11-13)

**Scope:** first measurements of Ant's shared remote cache on a real project. One project, one GitHub-hosted Linux runner per run, loopback coordinator and one sandboxed worker on the same machine (no real network). Characterizes where time goes; not a general performance claim.

## Method

- Workflow: [`.github/workflows/bench.yml`](../../.github/workflows/bench.yml); script: [`benchmarks/remote-cache/run.sh`](../../benchmarks/remote-cache/run.sh) (committed before each run).
- Subject: this repository at the run's commit (Ant itself: 10 workspace crates), dependencies vendored with `cargo vendor` so every source is a declared input (D-020): 291 MB, 6,430 files.
- Baseline: `cargo build --release --offline` — cold (fresh target directory), no-change (×3), and a fresh second checkout (cold again: plain cargo has no cross-checkout reuse).
- Ant: `ant adapt --adapter cargo --option profile=release --option granularity=workspace`, then `ant remote run` against a loopback coordinator with one `ant worker --sandbox` (bubblewrap, no network): cold (empty shared cache), no-change (×3), and fresh checkouts at other paths with new file timestamps (×3).
- Raw data (every run kept): [`before`](../../benchmarks/remote-cache/results/2026-11-13-before/results.csv) (run 43452066592, commit e503db9), [`after`](../../benchmarks/remote-cache/results/2026-11-13-after/results.csv) (run 43458111072, commit df4b946, key-before-download D-022), and [`batched`](../../benchmarks/remote-cache/results/2026-11-13-batched/results.csv) (run 43461407410, commit 5e6129c, batched transfers D-023), with environment files. **Each run landed on a different CPU model** (GitHub assigns runners), so compare Ant with plain cargo *within* a run. Runner: 4 cores, 15.6 GiB RAM, Ubuntu 26.04, Rust 1.95.0, bubblewrap 0.11.1 (CPU model differed between runs: Intel Xeon 8573C / AMD EPYC 9V74).

## Results (seconds; plain cargo measured in the same run as each Ant column)

| Scenario | Run 1: cargo / Ant | Run 2 (D-022): cargo / Ant | Run 3 (D-023): cargo / **Ant** |
|---|---|---|---|
| Cold (empty cache / fresh target) | 30.96 / 256.34 | 29.73 / 254.47 | 24.26 / **33.25** |
| No change, same checkout (×3) | 0.08 0.08 0.08 / 216.64 216.44 216.65 | 0.08 0.36 0.09 / 1.23 1.18 1.07 | 0.06 0.06 0.06 / **0.54 0.73 0.53** |
| Fresh checkout, other path, new timestamps | 29.22 (×1) / 216.84 216.29 215.77 | 29.94 (×1) / 1.03 1.08 1.27 | 25.23 (×1) / **0.56 0.74 0.54** |

Every Ant re-run in both runs executed nothing and was served from the shared cache (executed=0, reused=1), including the fresh checkouts — the path- and time-independent keys (D-019) work on a real project. The remote-built CLI installed from the cache is executable and runs (checked by the "after" run).

## Reading

- **A fresh checkout builds ~40× faster through Ant than with plain cargo (≈0.6 s vs ≈25 s, run 3)** once the shared cache holds the result. This is the case a shared cache exists for: CI runners, new clones, teammates.
- **Cold builds now cost ~9 s more than plain cargo (33.3 s vs 24.3 s, run 3)**, down from ~225 s of overhead: batching (D-023) removed the per-file transfer cost; the worker's compile itself took 25.9 s.
- **Before D-022, a hit cost ~216 s:** the worker downloaded, wrote, stamped, and re-hashed all 6,430 inputs before consulting the cache. Computing the key from the job's own digests first (D-022) removed that; the remaining ~1.1 s is the client hashing the project, one submission, the lookup, and downloading the outputs.
- **Unchanged checkouts are still slower than cargo's own no-change check (≈0.6 s vs ≈0.06 s, run 3):** the client re-hashes 291 MB every run. Caching client-side digests by file metadata is a clear next step.
- **Before D-023, cold builds were ~8.5× slower than plain cargo (≈255 s vs ≈30 s):** the compile took 28.7 s; the rest was 6,430 files moved one HTTP request each, both ways. Batched transfers fixed it (run 3).
- Single worker on the same machine: no real network latency or bandwidth limits, no parallelism across workers. Numbers on a VPS will differ.
