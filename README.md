<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
  <img alt="Ant: build everything. Fast. Right on your machine." src="docs/assets/banner-light.svg" width="760">
</picture>

**One small tool that builds any project with a single command, on the laptop you already have,
and makes the next build nearly instant by reusing every result it can prove correct.**
<br><sub>No server needed. If you do have one, Ant can use it too.</sub>

[![CI](https://github.com/vortemain/ant/actions/workflows/ci.yml/badge.svg)](https://github.com/vortemain/ant/actions/workflows/ci.yml)
[![Android benchmark](https://github.com/vortemain/ant/actions/workflows/bench-android.yml/badge.svg)](https://github.com/vortemain/ant/actions/workflows/bench-android.yml)
[![Rust 1.95](https://img.shields.io/badge/rust-1.95-orange?logo=rust)](rust-toolchain.toml)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-0d1117)](#what-works-today)
[![Status](https://img.shields.io/badge/status-early%20preview-ff8a1f)](docs/plan.md)

[Quick start](#quick-start) · [Benchmarks](#measured-not-promised) · [How it works](#how-it-works) · [Optional servers](#optional-use-your-own-servers) · [Roadmap](#roadmap) · [Docs](#documentation)

</div>

---

```bash
cd my-project
ant build
```

```
ant build · cargo · release · 2 steps · 8 parallel
  ✓ greet-core  1.0s
  ✓ greeter  0.7s

Built in 2.9s →
  dist/greeter
```

No config file. Ant detects the project, builds it on every core, and puts the deliverables in `dist/`.

## Measured, not promised

Every number below comes from a public CI run, with its raw data in this repository. Each one ran on **a single 4-core machine**: no extra hardware and no network. They're single-machine measurements, not general claims.

| Project | Scenario | Native tool | **Ant** | |
|---|---|---:|---:|---|
| **Android**: JetNews (Jetpack Compose) | fresh checkout | Gradle, warm daemon + build cache: 4.0–4.6 s | **0.65–0.85 s** | ~5× |
| | nothing changed | Gradle, warm daemon: 1.6–2.2 s | **0.66–0.86 s** | ~2.5× |
| | first build ever | Gradle: 156.9 s | 159.0 s | tie |
| **Rust**: Ant itself (10 crates, 291 MB vendored) | fresh checkout | cargo: 25.2 s | **0.54–0.74 s** | ~40× |
| | first build ever | cargo: 24.3 s | 33.3 s | |

Reports: [Android](docs/benchmarks/2026-11-13-android.md) · [Rust](docs/benchmarks/2026-11-13-remote-cache.md).
A "fresh checkout" is a new clone or branch folder, or a teammate's copy. Native tools rebuild from scratch there, while Ant reuses the result it already proved. Today these numbers come from Ant's cache engine run as a local service (`ant remote run` against `127.0.0.1`). Making plain `ant build` do this automatically is the current phase (P3). Where Ant still loses, for example the one-file edit loop against a warm Gradle daemon, the reports say so.

## Why Ant

- **Made for your own machine.** Most developers build on a laptop, often an old one. Ant's job is to make that laptop feel fast: reuse what's already built and keep the laptop's own work proportional to what you changed.
- **One command per job.** `ant build` builds, `ant init` records the setup, and that's all most projects need.
- **Builds are proven, not trusted.** A result is shared only when the build ran in a sandbox with no network, every input was declared and hashed, and the toolchain was identified. Anything less runs every time. You never get a stale result.
- **Path- and time-independent cache keys.** A clone at another path with fresh timestamps still hits the cache.
- **Servers are optional.** If you have a VPS or spare machines, Ant can send builds there and share results across a team. There's no vendor cloud and no lock-in, and none of it is required.
- **Honest by design.** Every feature lists its limits, every decision is logged ([`docs/plan.md`](docs/plan.md)), and nothing is called "supported" until it works end to end.

## Quick start

Ant is in early preview. Build it from source (Rust 1.95, pinned in `rust-toolchain.toml`):

```bash
git clone https://github.com/vortemain/ant.git
cd ant
cargo install --path crates/ant-cli
```

Then, in any supported project:

```bash
ant build          # release build, deliverables in dist/
ant build --dev    # debug build
ant init           # optional: write ant.toml so every machine builds the same way
```

See [Getting started](docs/getting-started.md).

## What works today

| Ecosystem | `ant build` | Proven result reuse (cache engine) |
|---|---|---|
| Rust / Cargo (incl. workspaces) | ✅ binaries into `dist/` | ✅ measured, with vendored dependencies |
| JavaScript / TypeScript / npm (incl. workspaces) | ✅ | not yet (`node_modules` is undeclared) |
| C / C++ / CMake | ✅ | not yet |
| Android / Gradle | via `ant adapt` (P3 brings it to `ant build`) | ✅ measured on JetNews, plus Gradle's own task cache |
| iOS, web bundlers, desktop installers, containers | roadmap | roadmap |

| Platform | Status |
|---|---|
| Linux | bubblewrap sandbox with no network; proven results are cached |
| Windows | restricted-token sandbox, job objects, memory caps, network-isolated account |
| macOS | not yet (needed for iOS) |

## How it works

```mermaid
flowchart LR
    P[Your project] -->|detect| A[Adapter<br/>cargo · npm · cmake · gradle]
    A -->|plan| G[Action graph<br/>declared inputs, outputs, toolchain]
    G --> K{Key from content,<br/>toolchain, platform}
    K -->|hit| C[(Result cache<br/>on your machine, or a shared server)]
    K -->|miss| W[Sandboxed build<br/>no network]
    W -->|proven result| C
    C -->|verified outputs| D[dist/]
```

1. **Adapters** read your build system (Cargo, npm, CMake, Gradle) and produce a plan: each action's command, declared inputs, outputs, and toolchain.
2. **Keys** come from content digests, the toolchain identity, and the execution platform, never from paths or timestamps.
3. **Builds run in a sandbox** with no network, on your machine, or on a worker if you have one. Only sandboxed builds can write to the cache, and every blob is re-hashed on read and write.
4. **Results** come back verified and land in your workspace and `dist/`.

## Optional: use your own servers

None of this is needed to use Ant. If you have a VPS or spare machines, you can send builds there and share proven results across a team:

```bash
# On your server
ant coordinator --data-dir /var/lib/ant --client-token-file client.token --worker-token-file worker.token

# On each build machine (Linux)
ant worker --coordinator https://builds.example.com --token-file worker.token --work-dir /var/tmp/ant --sandbox

# From your laptop or CI
ant adapt --out plan.json
ant remote run --coordinator https://builds.example.com --token-file client.token --plan plan.json
```

The coordinator also serves a read-only dashboard at `/ui` (viewer token). See [operations](docs/remote/operations.md) and [protocol v1](docs/remote/protocol-v1.md). A one-command `ant server deploy` is planned for P8.

## Roadmap

| Phase | Goal | State |
|---|---|---|
| P1 Front door | `ant build`, `ant init`, zero config | ✅ shipped |
| P2 Speed engine | proven result cache, measured on Rust and Android | ✅ shipped |
| P3 Local-first speed | all of that inside plain `ant build` on a laptop, with no server: cache on by default, fast change detection, a fast edit loop | 🚧 next |
| P4 Android | Gradle builds, signing, install-by-QR | 🚧 builds and caches APKs, Gradle task cache shipped |
| P5 iOS | Xcode builds on your Mac workers | planned |
| P6 Everything else | web, desktop, containers, more languages | planned |
| P7 Dashboard | Expo-quality UI: builds, logs, artifacts, QR installs | seeded (`/ui`) |
| P8 Optional servers | `ant server deploy`, offload to your VPS, team-wide cache | engine exists, one-command setup planned |
| P9 Ship | store submission, live preview | planned |

The full plan, acceptance criteria, and decision log are in [`docs/plan.md`](docs/plan.md). The product vision is in [`docs/vision.md`](docs/vision.md).

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | `ant build` and `ant init` |
| [Build adapters](docs/cli/adapters.md) | Cargo, npm, CMake, Gradle: what's declared and the honest limits |
| [Cache keys](docs/cli/cache-and-keys.md) | what goes into a key and why results are reused |
| [Optional servers](docs/remote/operations.md) | coordinator, workers, tokens, dashboard |
| [Architecture](docs/architecture.md) | crates, contracts, trust boundaries |
| [Security model](docs/security/threat-model.md) | what's protected, from whom, and what isn't yet |
| [Benchmarks](docs/benchmarks/) | methodology and every recorded run |

## Contributing

```bash
./scripts/check.sh      # Linux/macOS: rustfmt, clippy -D warnings, all tests
./scripts/check.ps1     # Windows
```

All three are hard gates, and CI runs them on Linux and Windows for every push. Read [`AGENTS.md`](AGENTS.md) before changing a contract. It explains how work is planned, logged, and handed off.

<div align="center">
<sub>Built in Rust · Local-first · Servers optional · Every claim backed by a CI run</sub>
</div>
