# Cold vs. warm cache under network isolation (2026-11-13)

**Scope:** first measured data for Ant. One synthetic workload on one laptop. It characterizes overheads; it is **not** a general speedup claim and not a comparison with other build systems.

## Method

- Script: [`benchmarks/cache-cold-warm/run.ps1`](../../benchmarks/cache-cold-warm/run.ps1) (committed before the run; `ant_dirty=false`).
- Raw data: [`results.csv`](../../benchmarks/cache-cold-warm/results.csv) (every run, warmups flagged, none discarded), [`status-check.csv`](../../benchmarks/cache-cold-warm/status-check.csv), environment: [`environment.json`](../../benchmarks/cache-cold-warm/environment.json).
- Environment: Intel Core i5-8365U (4 cores / 8 threads), 7.8 GiB RAM, Toshiba NVMe SSD, Windows 11 Pro build 26300, Balanced power plan, release build of commit `09c986c`, Rust 1.95.0. Other desktop software was running (not a quiet lab machine).
- Workload: 8 independent actions, each `tar.exe -czf out-i.tgz -C src-i .` over a deterministic generated tree of 100 × 100 KiB text files (≈10 MiB per action, ≈80 MiB total); declared tree input, declared file output, `cache_policy: eligible`, truthful toolchain identity (SHA-256 of System32 `tar.exe`); `--jobs 4`.
- Each scenario: 1 warmup + 5 measured runs; outputs deleted before every run; wall time measured around the whole `ant run` invocation.

| Scenario | Flags | Executed / reused per run |
|---|---|---|
| direct-nocache | — | 8 / 0 |
| sandbox-nocache | `--sandbox` | 8 / 0 |
| isolated-nocache | `--network-isolated` | 8 / 0 |
| isolated-cold | `--network-isolated --cache-dir` (emptied before each run) | 8 / 0 (stores 8) |
| isolated-warm | `--network-isolated --cache-dir` (populated) | 0 / 8 |

All 30 runs exited 0 with exactly the expected executed/reused counts.

## Results (wall ms, 5 measured runs)

| Scenario | Median | Min | Max |
|---|---:|---:|---:|
| direct-nocache | 3344 | 2552 | 3422 |
| sandbox-nocache | 2859 | 2237 | 3980 |
| isolated-nocache | 10204 | 10001 | 11456 |
| isolated-cold | 8342 | 8252 | 8845 |
| isolated-warm | 5521 | 4728 | 6020 |

Separately measured: the per-run fail-closed setup verification (`ant sandbox status`, the same script `--network-isolated` runs first) took **5005–5738 ms, median 5170 ms** (5 runs).

## Reading

- **Cache reuse works and is cheap; verification dominates.** A warm run restores and re-digests all 8 outputs (≈80 MiB of inputs digested with metadata, 8 archives restored) in about 5.5 s, of which ≈5.2 s is the setup verification, leaving roughly 0.3–0.4 s for lookup, restore, and digesting.
- **Network isolation is expensive per run today.** isolated-nocache costs ≈7 s more than sandbox-nocache: ≈5.2 s verification plus ≈2 s for launching 8 runners through Secondary Logon with profile load.
- **Warm under isolation is still slower than direct execution without any cache** (5.5 s vs. 3.3 s median) on this light workload. A warm cache only pays off against isolated execution, or once actions are much more expensive than the ≈5 s fixed verification cost.
- **Unexplained:** isolated-cold (8.3 s) ran faster than isolated-nocache (10.2 s) although it does strictly more work (store). Scenario order (nocache ran first; filesystem/antivirus warm-up) is a plausible confound; the runs were not interleaved or randomized, so this is not attributed.
- sandbox-nocache vs direct-nocache differ by less than their spread; no conclusion.

## Follow-ups this data motivates

1. Make verification cheaper without weakening fail-closed semantics (e.g. query the firewall rules through the `INetFwPolicy2` COM API in-process instead of `Get-NetFirewallRule`/CIM, which dominates the 5 s).
2. Randomize/interleave scenario order and repeat on an idle machine before quoting cold-vs-cold differences.
3. A real-compiler workload once toolchains are installed machine-wide (profile-local toolchains are unreadable to the sandbox account).
