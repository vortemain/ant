# npm and Android, plain `ant build` vs the native tool (2026-11-16)

Local-first (P3): no server, the developer's own machine. Script:
[`benchmarks/ecosystems/run.sh`](../../benchmarks/ecosystems/run.sh), workflow
`bench-ecosystems`, run [43818529430](https://github.com/vortemain/ant/actions/runs/43818529430)
on Ant commit `1f989b3`. Raw data: [`benchmarks/ecosystems/results/2026-11-16-linux-ci/`](../../benchmarks/ecosystems/results/2026-11-16-linux-ci/).

Each subject ran on its own 4-core GitHub Ubuntu runner (npm: Intel Xeon
6975P-C; Android: AMD EPYC 9V45), so compare the two tools **within** a
subject, not across subjects. Each tool builds its own copy of the project.
Dependencies (npm packages, Maven artifacts, the Gradle distribution) were
fetched once before measuring, so the numbers are building, not downloading.
Three repetitions per scenario; every observation is in the CSVs.

## npm: a fresh Vite + React + TypeScript app (`create-vite@6`, `react-ts`)

`npm run build` vs `ant build`.

| Scenario | npm | Ant | |
|---|---|---|---|
| cold | 1.72 s | 2.03 s | Ant slower (first build also records what it may reuse) |
| no change | 1.63–1.65 s | **0.017–0.018 s** | ~95× faster |
| one-file edit (`src/App.tsx`) | 1.62–1.64 s | 1.66–1.71 s | equal (Ant rebuilds, as it must) |
| edit undone | 1.64–1.66 s | **0.018 s** | ~90× faster |

Before D-056 (run 43817191278, same script) Ant matched npm everywhere
(1.88–1.98 s) because it never reused npm results.

## Android: JetNews (`android/compose-samples@25631e8`, `assembleDebug`)

`./gradlew assembleDebug` with its daemon warm vs `ant build`.

| Scenario | Gradle | Ant | |
|---|---|---|---|
| cold | 2.67 s | 11.39 s | **Ant slower**: Gradle reused the daemon and build cache left warm by the dependency prefetch; Ant started its own daemon |
| no change | 1.05–1.15 s | **0.047–0.077 s** | ~20× faster |
| one-file edit | 1.39–2.06 s | 1.28–1.50 s | equal or slightly faster (Ant's warm daemon, D-032) |
| edit undone | 1.04–1.15 s | **0.034–0.053 s** | ~25× faster |

The earlier run (43817191278) agrees: no change 0.055–0.089 s vs 1.35–1.42 s,
undo 0.036–0.047 s vs 1.30–1.62 s, edit 1.68–1.82 s vs 1.85–2.75 s, cold
15.8 s vs 3.7 s.

## What this says about the 10× goal

- Met for "nothing changed" and "back to something already built" on npm
  (~90×) and Android (~20–25×), the builds a developer repeats most.
- Not a win for a real edit: Ant runs the same tool, so it costs the same.
- Cold builds are slower with Ant when the native tool already has a warm
  daemon or cache; making Ant reuse the developer's existing Gradle daemon is
  open work.
