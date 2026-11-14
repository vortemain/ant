# Ant vs. plain cargo on Ant itself, local build states (2026-11-14)

**Scope:** `ant build` against plain `cargo build --release --workspace` on the ant repository, on one laptop, in the states a developer actually hits: nothing changed, one file edited, switching back to already-built code, and a clean checkout. First-party data for the local-reuse decisions (D-028 local result store, D-029 switchback restore, D-031 metadata cache); **not** a general speedup claim against other build systems, and single-machine medians only.

## Method

- Script: [`benchmarks/local-rust/run.ps1`](../../benchmarks/local-rust/run.ps1) (committed before the run). ant was rebuilt from the working tree at the start; the manifest records commit `ec1d845` with uncommitted changes (`ant_dirty=true`).
- Raw data: [`results.csv`](../../benchmarks/local-rust/results/2026-11-14/results.csv) — every run including warmups, none discarded; executed/reused counts and ant's self-reported time are recorded per row. Environment: [`environment.json`](../../benchmarks/local-rust/results/2026-11-14/environment.json).
- Environment: Intel Core i5-8365U (4 cores / 8 threads), 7.8 GiB RAM, Toshiba NVMe SSD, Windows 11 Pro build 26300, Balanced power plan, Rust 1.95.0, other desktop software running.
- Workload: this repository, one workspace action (`cargo build --workspace --release`, ~110 crates incl. dependencies). ant sees the whole project as declared inputs and 11 declared outputs; plain cargo is the identical command line.
- Scenarios: no-change (5 measured reps + warmup), one-file edit (unique comment appended to `crates/build-digest/src/lib.rs` per rep; 4 + warmup), switchback (edit → untimed build → revert → timed build; 2–3 + warmup), cold (`cargo clean` + full rebuild; 2 reps). The timed command is the whole invocation, process start included. The edit is restored byte-for-byte and verified after every scenario.
- The run was split across invocations (cold reps, ~2 min each, ran last); `results.csv` rows were appended and every scenario ran in listed order.

## Results (wall ms, measured reps)

| Scenario | ant median | plain cargo median | Ratio | ant min–max | cargo min–max | n ant/cargo |
|---|---:|---:|---:|---|---|---:|
| no-change (warm target) | **170** | 604 | **3.6×** | 156–248 | 468–1028 | 5/5 |
| no-change, metadata cache cleared | 471 | — | — | 464–864 | — | 5/– |
| one-file edit | 40 600 | 36 593 | 0.90× | 37 939–41 272 | 35 882–43 385 | 4/4 |
| switchback to built code | **275** | **28 420** | **103×** | 274–297 | 28 420–29 157 | 3/2 |
| clean checkout, full rebuild | 109 620 | 106 401 | 0.97× | 109 620–113 724 | 106 401–107 353 | 2/2 |

Every run exited 0; the executed/reused columns in the raw data confirm each rep took the intended path (ant no-change and switchback reps show `reused=1, executed=0`; edit and cold reps show `executed=1`).

## Reading

- **Switchback is where local reuse pays off, and it is not a corner case.** Returning to code that was already built — revert a change, checkout a branch, `git stash pop` — costs plain cargo a full 28 s rebuild here (mtime invalidation), while ant restores the 11 outputs from the local result store in 0.28 s: **103× on this data** (D-029). Repeated-experiment builds often look exactly like this.
- **Even the no-change case beats plain cargo's no-op.** cargo's own warm no-op costs ~0.5–1 s (fingerprint walk, process start); ant's result-store hit is ~0.17 s — 3.6×. Clearing the cargo-metadata cache (D-031) raises ant to 471 ms, so that cache alone saves ~300 ms per build; ant's steady state remains faster than the cargo no-op it replaces.
- **Honest overhead on real work.** When something actually changed, ant cost +4.0 s (+11%) over plain cargo: whole-project input digests, 11 output digests, the store write, and installing artifacts to `dist\`. On a clean checkout the overhead is ~+3 s (~3%) — smaller because the compile dominates. This is the price of the guarantees (content-addressed reuse, no mtime trust); whether it is worth it depends on how often a developer's day is actually a no-change or switchback build.
- **Absolute numbers are fragile on this machine; ratios are the signal.** The no-change cargo reps spread 468–1028 ms, one-file reps 36–43 s. Informally, the first clean build of the day (before this harness existed) took 3 m 08 s, while the harness's clean-checkout reps ran 106–114 s on an already-busy, page-cache-warm machine — a ~2× swing from machine state alone. Compare ant and cargo within a row, not across rows; treat single-machine absolute values as indicative only.
- All scenarios share one `ANT_CACHE_HOME` under the benchmark's temp directory; the machine's real cache and remote policies were never in play (`cache_policy` for a local, unsandboxed run keeps the store local — the raw `--verbose` output shows `PolicyLocalOnly` among blockers on executed runs, i.e. no remote cache was consulted or populated).

## Follow-ups this data motivates

1. The +11% on a real edit is mostly digesting a whole project (~450 files) and writing ~50 MiB of outputs to the store on every executed build. Candidates: parallel input digesting, digesting only files whose (size, mtime) changed with a metadata cache (the same trust question D-027 answers for change detection), and skipping store writes for actions whose outputs are already present with matching digests.
2. A larger project would amortize the fixed costs differently; repeat on a second repository and a second machine before quoting ratios as expected values.
3. The switchback gap on `dist\` restore (0.28 s for 11 outputs) should hold as artifacts grow, but measure a project with large binaries (e.g. the Android benchmark workload) to confirm restore is not O(artifact size) in a way that erodes the 103×.

## Addendum (same day, after D-034 dev-profile path independence)

D-034 changed the default dev profile: every dev action now sets `CARGO_INCREMENTAL=0` and remaps the workspace root out of debug info (`CARGO_ENCODED_RUSTFLAGS`) so two clones build byte-identical outputs. Because that trades away cargo's incremental compilation — exactly what a one-file edit loop relies on — the edit scenario was re-measured the same day, ant rebuilt from the current tree, in one back-to-back A/B (rows appended to `results.csv` above; both sides ran consecutively under identical machine state):

| Scenario (post-D-034) | ant median | plain cargo median | Ratio | ant min–max | cargo min–max | n ant/cargo |
|---|---:|---:|---:|---|---|---:|
| one-file edit | 28 641 | 27 566 | 0.96× | 27 034–29 593 | 26 683–28 437 | 3/4 |

- **Parity, not regression.** With incremental compilation off, ant's one-file edit is ~4% slower than cargo's warm *incremental* rebuild on this machine — within this machine's run-to-run noise (see the fragility note above). The pre-D-034 session had measured +11%; that number is superseded for dev builds. Disabling incremental cost less than feared because the workspace action rebuilds the edited crate and its dependents either way; incremental mostly helped within a single crate, and its session-id-named codegen units were what blocked path-independent dev outputs.
- The cold/no-change/switchback rows are unaffected by D-034's `CARGO_INCREMENTAL=0`: no-change and switchback reps are pure reuse paths (no compilation), and cold reps compile from scratch with or without incremental state. Only the edit row was exposed to the change.
- Raw rows for this addendum are in the same `results.csv` (the two `ant-onefile` warmup rows are from separate harness invocations used to stay within per-invocation time limits; every measured row is included, none discarded).
