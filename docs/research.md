# Research register and competitive study

**Updated:** 2026-11-12. This is a research queue and evidence ledger, not a completed competitive dossier. The full specification requires dozens of product-specific investigations; each finding must be checked against current official docs/source before it informs a major decision.

## Initial primary-source evidence

| Topic | Source / access date | Evidence captured | Confidence / implication |
|---|---|---|---|
| Cargo workspaces | [Cargo Book: Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), accessed 2026-11-12 | Shared lockfile/output and workspace-level metadata/dependency settings; members can be organized under crates | High for Cargo behavior. Use a workspace but keep members responsibility-based; toolchain is still unknown. |
| Remote execution protocols | [bazelbuild/remote-apis](https://github.com/bazelbuild/remote-apis), accessed 2026-11-12 | API collection covers content-addressed input upload, execution, result monitoring, and remote cache; also includes adjacent remote asset/logstream APIs | High for stated project scope. Map via an adapter; do not infer full interoperability from protocol names. |
| REAPI schema | [remote_execution.proto](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto), accessed 2026-11-12 | Authoritative protocol schema available; full feature inventory and license review remain | Medium pending detailed schema/conformance study. |

## Dossier backlog

The master spec lists these minimum studies. No dossier is complete yet. Split each into source-backed product dossiers and capture version/access date, architecture, execution, cache, environments, isolation, UX, scale, pricing/license, limitations/trade-offs, and lessons.

- Build systems/orchestration: Gradle, Bazel, Buck2, Pants, Nix, Ninja, CMake, Meson, Make, MSBuild, Cargo, Please, Earthly, Dagger.
- Bundlers/toolchains: esbuild, Vite, Rollup, Webpack, Rspack, Rolldown, Parcel, Metro, Turbopack, SWC, Babel, TypeScript, Flutter, Android Gradle Plugin, Xcode, Go, Python packaging, .NET/MSBuild, C/C++.
- Build infrastructure/cache/execution: BuildKit, Buildbarn, BuildGrid, BuildBuddy, EngFlow, REAPI implementations, cache systems, OCI runtimes, Nix remote builders.
- CI/mobile cloud: GitHub Actions, GitLab CI, Buildkite, CircleCI, Jenkins, TeamCity, Bitrise, Codemagic, Expo EAS, Appcircle, Xcode Cloud.

## Initial architectural hypotheses (not facts)

1. Canonical action contracts plus adapter-specific planners can preserve existing developer workflows. Validate with first target ecosystems and their real task graphs.
2. REAPI is a useful interoperability boundary for content-addressed remote execution, but hybrid placement, secrets, richer events, and policy may need internal extensions. Validate by schema/conformance analysis.
3. A local-first vertical slice is the lowest-risk starting point because the repository is empty and no cloud credentials/environment are present. Revisit after target user/workload selection.

## Required next research

- Choose initial adapter based on actual user demand and feasibility; the spec's candidate list is not itself priority evidence.
- Compare sandbox choices across Windows, macOS, Linux, including escape risks and operational constraints.
- Study REAPI semantics, supported optional features, compatibility tests, and licenses of candidate server/worker implementations.
- Compare local metadata and remote persistence choices, storage failure models, and tenant isolation.
- Perform full license and dependency audit before adopting dependencies or selecting project license.
- Benchmark native baseline workloads only after workload/source revisions and supported toolchains are available.
