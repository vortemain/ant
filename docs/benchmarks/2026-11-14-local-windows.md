# Local `ant build` vs plain cargo on an ordinary laptop (2026-11-14)

**Scope:** the first local-first measurement (P3, D-026): plain `ant build` on the developer's own machine, no server, against plain `cargo build`. One project and one machine; three repetitions per warm scenario. It shows where Ant stands on a real laptop, not a general performance claim.

## Method

- Script: [`benchmarks/local/run.sh`](../../benchmarks/local/run.sh). Raw data, every observation kept: [`results.csv`](../../benchmarks/local/results/2026-11-14-windows/results.csv), [`environment.txt`](../../benchmarks/local/results/2026-11-14-windows/environment.txt).
- Machine: Intel Core i5-8365U (2019 laptop CPU, 4 cores / 8 threads, 1.6 GHz base), 7.8 GiB RAM, SSD, Windows 11 Pro 10.0.26300, Windows Defender on, Rust 1.95.0. Ant: release build of this branch (P3.3).
- Subject: this repository (10 workspace crates), two commits: `6e4d1fc` ("old") and `ec1d845` ("new", 13 files differ). Each tool builds the debug profile of the whole workspace in its own fresh clone: `cargo build --workspace` vs `ant build --dev`, with Ant's per-user cache in a fresh directory.
- Scenarios: **cold** (fresh clone); **no change** (×3); **switch to old / switch back** (×3): `git checkout` the other commit, then build. Both commits were built once before measuring, so each tool has seen both.
- cargo ran first, then Ant, in the same session.

## Results (seconds)

| Scenario | cargo | **ant build** |
|---|---|---|
| Cold, fresh clone | 128.09 | 74.29 (see reading) |
| No change (×3) | 0.42 0.39 0.39 | **0.16 0.21 0.14** |
| Switch to old (×3) | 14.58 15.30 13.75 | **1.88 0.52 0.53** |
| Switch back (×3) | 15.63 12.90 19.10 | **0.38 0.39 0.43** |

Every Ant warm run executed nothing: the plan came from the plan cache (D-029) and the outputs were restored from this machine's local results (D-028) by hard link (D-030).

## Reading

- **Switching between already-built code: ≈0.4–0.5 s with Ant vs 13–19 s with cargo (~30× faster).** The first switch took 1.9 s because each stored blob is read and verified once; after that, verification costs a stat. Cargo has no record of earlier builds of the same crate, so a branch switch recompiles the changed crates and relinks.
- **Nothing changed: 0.14–0.21 s vs 0.39–0.42 s (~2–3× faster than cargo's own no-change check).** Before this work it was 0.55 s (D-028); the plan cache removed ≈0.4 s of `cargo --version` + `cargo metadata`.
- **Where the remaining switch time goes** (`ANT_PROFILE=1`): ≈0.33 s is Windows Defender scanning the 13 freshly checked-out files on first read (plain `cat` of the same files takes the same time; cargo pays it too). Restore is ≈0.014 s; the rest is process start and change detection.
- **The cold numbers are not a fair comparison.** Cargo ran first, so Ant's cold build found registry sources and the OS file cache warm. Ant does not speed up a cold build: it runs the same `cargo build`. In an earlier cold run on this machine, cargo reported 78 s inside an 82 s `ant build`: about 4 s of overhead for planning, hashing, and copying outputs into the local store. Treat cold as "cargo plus a few seconds".
- **Not measured here:** a one-file edit loop (Ant runs cargo, and after a restore cargo's incremental state belongs to the other commit, so the first edit can recompile more than plain cargo would; see next actions), npm/CMake/Android projects, Linux laptops, and a second clone in another folder (rebuilds by decision D-031).
