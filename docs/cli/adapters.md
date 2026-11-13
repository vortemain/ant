# Build adapters (`ant adapt`)

Adapters turn a real build-system project into an Ant plan. The adapter
contract lives in the `build-adapter` crate: `detect` (a non-destructive
compatibility check) and `generate` (inspection-only plan generation).
Generated plans are the **same canonical versioned document** `ant run --plan`
accepts — one action model, one validation path — and generation re-validates
the full plan through the build graph before writing anything.

## Usage

```powershell
ant adapt --adapter cargo --project C:\src\my-project --out build-plan.json
ant run --workspace C:\src\my-project --plan build-plan.json
```

`--project` defaults to the current directory; `--out` is required, so adapter
output never clobbers anything implicitly. The status line reports the adapter,
how it was selected (`selection=explicit` or `selection=auto`), the project,
and the generated action count. An unknown adapter fails with the available
list; an unrecognized project fails with the adapter's reason; a project
directory that does not exist fails before any adapter runs.

### Automatic selection and detection diagnostics

```powershell
ant adapt --project C:\src\my-project --out build-plan.json
ant adapt --detect --project C:\src\my-project
```

Without `--adapter`, `ant adapt` runs every registered adapter's `detect` and
uses the **single** adapter that recognizes the project. It never invents a
priority between ecosystems: when several adapters match (for example a
project with both `Cargo.toml` and `package.json`) it fails and asks for
`--adapter`; when none match it fails listing every adapter's reason; when any
adapter's detection itself errors it fails closed rather than skipping that
adapter. No plan file is written on any selection failure.

`ant adapt --detect` prints one `detect adapter=NAME supported=true|false|error
[reason=...]` line per registered adapter plus a summary
`detect project=PATH supported_adapters=N` line. It is inspection-only (it
does not create the `.ant/tmp` staging directory), cannot be combined with
`--adapter` or `--out`, and exits 0 when at least one adapter recognizes the
project, 1 otherwise. Detection is a marker-file check (`Cargo.toml`,
`package.json`); a positive detection does not guarantee generation succeeds
(for example an npm project without a `build` script is detected but rejected
at generation).

## The `cargo` adapter

The first adapter, chosen for feasibility: `cargo metadata` is a stable,
offline-friendly JSON API, and real end-to-end verification (adapt → run →
real artifact) is possible against real cargo builds.

Generation runs `cargo metadata --format-version 1` in the project root and
emits one `cargo build --package <name>` action per workspace member package,
sorted by package name, with:

- **Dependency edges** between member packages from the metadata dependency
  lists (dev- and build-dependencies included conservatively; redundant reruns
  are never wrong, missed reruns would be).
- **Inputs per member**: its `Cargo.toml`, its `src` tree, `build.rs` when
  present, plus the workspace `Cargo.toml` (when distinct) and `Cargo.lock`
  when present.
- **Outputs** mapped deterministically into the default `target/debug` layout:
  `bin` targets become `target/debug/<name><.exe>`, `lib` targets become
  `target/debug/lib<crate_name>.rlib`. Other target kinds (proc-macro, test,
  bench, example, custom-build) declare no outputs.
- **Truthful toolchain identity**: the resolved PATH `cargo` executable's
  SHA-256 digest (rustup shim symlinks are resolved to their backing file) and
  its reported version. Generation fails when cargo cannot be resolved.
- **Workspace-relative scratch environment**: every generated action sets
  `TMP`/`TEMP` (plus `TMPDIR` on non-Windows, unverified there) to
  `.ant/tmp`, a staging directory generation creates idempotently inside the
  project. This is the one deliberate exception to inspection-only
  generation, documented in the `build-adapter` crate contract: toolchains
  write scratch files (the MSVC linker's response files, rustc temporaries)
  into the system temp directory during compilation and linking, and
  redirecting those writes into the workspace is what makes generated plans
  runnable under `ant run --sandbox`, where writes outside the workspace are
  confined. The staging directory is not a declared input or output and is
  safe to delete at any time.

## The `npm` adapter

A second adapter, chosen for feasibility: `node`/`npm` are present in the
development environment, so real adapt → run → real-artifact verification is
possible (it is verified against a real TypeScript project). The npm adapter
proves the adapter contract generalizes beyond cargo without inventing a
second, weaker action model.

Generation reads `package.json` directly (no `npm` invocation for metadata)
and emits a single `npm run build` action, with:

- **The project's own `scripts.build` is the command.** If `package.json` has
  no `build` script, generation fails honestly rather than guessing a
  command. v1 generates one action for the root package.
- **Inputs declared**: `package.json`, `package-lock.json` when present, the
  `src` tree when present, and `tsconfig.json` when present (a common build
  driver). The installed `node_modules` tree is intentionally **not** declared
  — it is the derived product of `npm install` against the registry, not
  source; declaring the whole tree is out of scope for v1, so generated
  actions are `cache_policy: never` (under-declared inputs never claim
  eligibility, D-003/D-010). This mirrors the cargo adapter's stance on
  registry dependency sources.
- **Outputs are not declared in v1.** npm has no standard output layout
  (`dist/` is conventional but not universal; the bundler config decides the
  real location), so guessing would be dishonest (D-010). The build still
  runs and produces real artifacts; they are just not verified by the plan.
- **Truthful toolchain identity** from the real `node` runtime: the resolved
  PATH `node` executable's SHA-256 digest and `node --version`. The build is
  driven by `npm`, but `npm` is a script (`npm.cmd` on Windows, a JS file
  elsewhere), not a standalone executable; `node` is the truthful runtime
  identity. Generation fails when `node` cannot be resolved.
- **Windows invocation**: `npm` is `npm.cmd` (a batch shim) on Windows, and
  `CreateProcess` cannot execute batch scripts directly, so the generated
  argv is `cmd /C npm run build` on Windows and `npm run build` elsewhere.
- **Workspace-relative scratch environment**: like the cargo adapter, every
  generated action sets `TMP`/`TEMP` (plus `TMPDIR` on non-Windows) to
  `.ant/tmp` so generated plans run under `ant run --sandbox`.

## Sandbox compatibility

Generated plans run under `ant run --sandbox` on Windows. This is verified
end to end: adapting a two-member path-dependency workspace and running it
with `--sandbox` builds both members in DAG order and produces the declared
artifacts (`libcore_lib.rlib`, `app-bin.exe`), with the confined child
compiling and linking successfully — before the staging redirect, the same
run failed with the MSVC linker's `LNK1104` on its `%TEMP%` response file.

Honest limits of sandbox compatibility:

- **Path-dependency workspaces only (for fetching).** Path-only cargo builds
  need no writes to `~/.cargo`, so they work fully confined. A workspace with
  **registry dependencies** writes to the cargo package cache under `~/.cargo`
  during the build, which `--sandbox` blocks; such builds fail under
  confinement. Vendor dependencies or pre-warm outside the sandbox if you
  need both registry crates and confinement.
- **The staging directory is a shared scratch space.** `.ant/tmp` is
  world-accessible within the confined process; it is not an adversarial
  boundary (see `docs/sandbox/windows-design.md` — the sandbox confines
  writes, it is not a security boundary).
- **Relative values resolve against the child's cwd.** `TMP=.ant/tmp` works
  because generated actions run with `working_directory: "."` at the workspace
  root. If you edit a generated plan to change working directories, the
  staging values no longer resolve as intended.

## Honest v1 limitations

These limitations apply per adapter; see each adapter's section for the
specifics.

- **`cache_policy: never` by construction.** Where a dependency's content
  cannot be declared as an input — registry dependency sources for cargo live
  outside the workspace; the installed `node_modules` tree for npm is derived,
  not source — generated actions never claim cache eligibility (decision
  D-003/D-010). The action key still records the toolchain identity and the
  printed blockers show `policy-never` alongside the usual capability blockers.
- **Default target layout assumed (cargo).** A custom `CARGO_TARGET_DIR` or
  build-config `target-dir` makes declared outputs wrong; the executor will
  then fail the action on output coverage instead of guessing. Mapping
  alternate layouts is future work.
- **No outputs declared (npm).** npm has no standard output layout, so v1
  declares none; the executor's output coverage is trivially satisfied and the
  build's real artifacts are not verified by the plan. Mapping common bundler
  output locations (`dist/`, `build/`) is future work.
- **Full dependency resolution required (cargo).** Generation runs the
  complete `cargo metadata`, which resolves the dependency graph; a project
  whose dependencies are neither locked nor vendored may need network for that
  step.
- **npm workspaces (monorepos) and per-script selection are not supported in
  v1.** The npm adapter generates one `npm run build` action for the root
  package only.
- **`npm install` is a precondition (npm).** The npm adapter generates the
  build action; installing dependencies (`npm install`) is the user's
  responsibility and requires network/registry access. Under `ant run --sandbox`,
  a project whose build writes to `~/.npm` or other outside-workspace caches
  will fail; pre-warm those outside the sandbox.
- **Platform-native path separators.** Generated paths use the generating
  host's separators; cross-platform plan portability remains an open contract
  question (`docs/architecture.md`).
