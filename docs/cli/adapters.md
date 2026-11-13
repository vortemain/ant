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
output never clobbers anything implicitly. `--option KEY=VALUE` (repeatable)
passes adapter-specific settings:

| Adapter | Option | Values | Effect |
|---|---|---|---|
| cargo | `profile` | `dev` (default), `release` | `release` adds `--release`; outputs map to `<target-dir>/release` |
| npm | `outputs` | `none` (default), `entry-points` | declare each package's `main`/`types`/`bin` entries as outputs |
| cmake | `config` | `Debug` (default), `Release` | `CMAKE_BUILD_TYPE` at configure, `--config` at build |

Options are adapter-defined strings, so new options never change the CLI
grammar or the adapter contract. An unknown key (naming the adapter's
accepted keys), an unsupported value, a malformed `KEY=VALUE`, or a key given
twice fails before anything is written (no plan file, no staging directory).
`--option` cannot be combined with `--detect`. With no options every adapter
produces exactly the plans it produced before options existed. The status line reports the adapter,
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
project, 1 otherwise. Detection is a marker-file check (`Cargo.toml`, `CMakeLists.txt`,
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
- **Outputs** mapped deterministically into `<target-dir>/<profile-dir>`:
  `bin` targets become `<target-dir>/<profile-dir>/<name><.exe>`, `lib`
  targets `<target-dir>/<profile-dir>/lib<crate_name>.rlib`. Other target
  kinds (proc-macro, test, bench, example, custom-build) declare no outputs.
  - **Profile** (T3.6): `debug` for the default `dev` profile, `release` for
    `--option profile=release` (which adds `--release`). Custom profiles
    (`--profile NAME`, output `target/NAME`) are rejected: whether a profile
    is defined cannot be checked through `cargo metadata`, so the adapter
    refuses rather than emit a plan naming a profile that may not exist.
  - **Target directory** (T3.7): the `target_directory` `cargo metadata`
    reports, which already reflects `CARGO_TARGET_DIR` and
    `build.target-dir`. A target directory **outside the project fails
    closed** (outputs are workspace-relative by contract; `../x` escapes are
    caught component-wise, not by string prefix). A non-default directory
    inside the project is **pinned** on every action as a workspace-relative
    `CARGO_TARGET_DIR`, because the executor clears the child environment and
    an adapt-time variable would otherwise not reach the build. The default
    `target` pins nothing, so default plans are unchanged.
- **Workspace root required**: `--project` must be the cargo workspace root
  (every declared path is relative to it); pointing at a member directory
  fails with a message naming the workspace root.
- **Project cargo configuration**: `.cargo/config.toml` (or legacy
  `.cargo/config`) in the project is a declared input, since it can change
  the build (source replacement, flags, target dir).
- **In-project dependency sources** (T3.8): every non-member package whose
  manifest lives inside the project — vendored crates under `cargo vendor`
  source replacement, `exclude`d path dependencies — becomes a tree input of
  each member that transitively depends on it (from the `cargo metadata`
  resolve graph), and only of those members. Sources outside the project
  (the `~/.cargo` registry cache, git checkouts, path dependencies elsewhere)
  cannot be declared, which is one reason policy stays `never`.
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

- **The project's own `scripts.build` is the command.** If a single-package
  `package.json` has no `build` script, generation fails honestly rather than
  guessing a command.
- **npm workspaces** (T3.9): when the
  root `package.json` has a `workspaces` array, the adapter emits one
  `npm run build` action per workspace package that has a `build` script
  (the set `npm run build --workspaces --if-present` would run), with the
  package name as the action id (scoped names such as `@demo/core` work) and
  the package directory as `working_directory`. Patterns: literal paths and
  `*` within a segment (`packages/*`, `apps/web-*`); `**`, negation, braces,
  character classes, absolute paths, and `..` fail closed; hidden directories
  and `node_modules` never match; a wildcard match without `package.json` is
  skipped, a literal one fails. Every package needs a unique `name`.
  **Edges** come from `dependencies`/`devDependencies`/`peerDependencies`/
  `optionalDependencies` naming another workspace package: a building
  dependency is a DAG edge; a non-building one contributes its
  `package.json` and `src` as inputs and is walked through, so a building
  package behind it still becomes an edge. Cycles fail plan validation.
  Inputs also include the root `package.json`, `package-lock.json`, and
  `tsconfig.json` when present. **The root's own `build` script is not run**
  in workspace mode: monorepo root scripts conventionally orchestrate the
  workspaces (`npm run build --workspaces`), so running it as well would
  build everything twice. Verified end to end: a three-package monorepo
  (`core` builds, `util` has no build script and depends on `core`, `app`
  builds, depends on `util`, and reads `core`'s build output) adapts to two
  actions with `@demo/app → @demo/core`, runs under `--sandbox` in that
  order, and produces working artifacts.
- **Inputs declared**: `package.json`, `package-lock.json` when present, the
  `src` tree when present, and `tsconfig.json` when present (a common build
  driver). The installed `node_modules` tree is intentionally **not** declared
  — it is the derived product of `npm install` against the registry, not
  source; declaring the whole tree is out of scope for v1, so generated
  actions are `cache_policy: never` (under-declared inputs never claim
  eligibility, D-003/D-010). This mirrors the cargo adapter's stance on
  registry dependency sources.
- **Outputs are not declared by default.** npm has no standard output
  layout, and — the reasoning behind the T3.10 decision — **no `package.json`
  field states what a build produces.** `main`, `types`/`typings`, and `bin`
  state where consumers load the package from, which is often a committed
  source file no build writes (`"main": "index.js"`); `files` lists publish
  contents (sources, READMEs, globs); `exports` is a conditional resolution
  map whose targets are routinely sources. Declaring any of them
  automatically would turn sources into "outputs". With
  `--option outputs=entry-points` the user asserts that `main`,
  `types`/`typings`, and every `bin` entry are build products; they are then
  declared as outputs and **verified, not trusted**: the executor fails the
  action when one is not created (e2e-tested). Entries that are not plain
  paths inside the package, or that fall under a declared input (e.g.
  `src/index.js`), are rejected at generation; requesting entry points on a
  package that declares none fails rather than silently declaring nothing.
  An implicit default `main` (`index.js` when absent) is never declared.
  `files` and `exports` are never used.
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
  `.ant/tmp`, resolved against the action's working directory; for workspace
  actions generation prepares `.ant/tmp` inside each building package.

## The `cmake` adapter (T3.11)

Added because real verification is possible here: CMake 3.31 ships with the
installed Visual Studio Build Tools 2022 (not on PATH; the adapter requires
`cmake` on PATH), and with the Visual Studio generator MSBuild locates the
MSVC toolchain itself, so builds work under Ant's cleared child environment
(no `vcvars` environment needed). Verified end to end: adapt → validate → run
builds a two-directory C project in Debug and Release and the built program
runs. Nothing was installed.

Generation reads no CMake output (it never configures the project) and emits
two actions:

- `configure`: `cmake -S . -B build -DCMAKE_BUILD_TYPE=<config>`, declaring
  `build/CMakeCache.txt` as its one output (CMake always writes it).
- `build`: `cmake --build build --config <config>`, depending on `configure`.

`<config>` is `Debug` or `Release` (`--option config=...`), applied both ways
so single- and multi-config generators agree. The generator is CMake's
default for the machine (the cleared environment drops `CMAKE_GENERATOR`).
Inputs: every `CMakeLists.txt` and `*.cmake` file, skipping `build/`, hidden
directories, `node_modules`, and other build trees (directories holding a
`CMakeCache.txt`). Honest limits: **source files are not declared** (which
files a target compiles is only known after configuring, via the CMake File
API — future work), so actions are `cache_policy: never`; **build artifacts
are not declared** (their location depends on the generator —
`build/<Config>/` vs `build/` — and on target properties only a configured
code model reports); the compiler identity is not captured (toolchain
identity is the `cmake` executable digest and version line); and CMake plans
do not run under `--sandbox` with the Visual Studio generator (see below).
The CMake e2e test locates `cmake` on PATH or through `vswhere`, and prints a
skip notice on machines with neither.

## Sandbox compatibility

Generated cargo and npm plans run under `ant run --sandbox` on Windows. This
is verified end to end: adapting a two-member path-dependency workspace and
running it with `--sandbox` builds both members in DAG order and produces the
declared artifacts (`libcore_lib.rlib`, `app-bin.exe`), with the confined
child compiling and linking successfully — before the staging redirect, the
same run failed with the MSVC linker's `LNK1104` on its `%TEMP%` response
file. npm workspace plans also run confined (verified, see below).

### Registry dependencies under `--sandbox` (T3.8)

What actually happens was probed on this machine (cargo 1.95, Windows 11)
rather than assumed:

- **The confined build itself never needs network or `~/.cargo` writes once
  the sources are extracted.** A cargo build reads registry sources from
  `~/.cargo/registry/src`; it must *write* there only to download or extract
  a crate, and confinement denies that write. Observed: with a crate cached
  as a `.crate` but not extracted, `ant run --sandbox` fails with `failed to
  unpack package ... failed to create directory
  C:\Users\...\.cargo\registry\src\...` — the honest, fail-closed outcome.
- **`ant adapt` pre-warms as a side effect of cargo itself.** Generation runs
  the full `cargo metadata` unsandboxed, and cargo downloads (network, when
  not cached) and extracts every locked package to answer it. Observed: a
  crate that was cached but unextracted became extracted by `ant adapt`, and
  the subsequent `ant run --sandbox` succeeded. So adapt → sandboxed run
  works for registry dependencies **as long as the extracted sources are
  still present** at run time; if they are removed (e.g. by cargo's cache
  garbage collection, or a cleared `~/.cargo`), the confined build fails as
  above. Re-run `ant adapt` or `cargo fetch` outside the sandbox to re-warm.
  This path reads undeclared sources from `~/.cargo`, so it is not
  hermetic; it is live-smoke evidence only, not an automated test (it
  depends on the machine's registry cache).
- **The hermetic path is vendoring, and it is verified end to end.** Run
  `cargo vendor` once (it works offline from a warm cache:
  `cargo vendor --offline`) and add the source replacement it prints to
  `.cargo/config.toml`. Cargo then resolves crates.io dependencies from
  `vendor/` inside the project: the adapter declares each vendored crate
  directory and `.cargo/config.toml` as inputs, and the confined build reads
  only workspace files — no `~/.cargo` writes, no network. Verified live with
  a real `cargo vendor --offline` of `adler2` (sandboxed build succeeded with
  the crate deliberately absent from `~/.cargo/registry/src`, and it was not
  re-extracted) and by an automated e2e test using a hand-built directory
  source in the exact `cargo vendor` layout (network- and cache-independent).
- **Why not a workspace-local `CARGO_HOME` by default:** it would make every
  generated plan re-download the registry into each project (network at run
  time, outside the plan's control) and ignore user cargo configuration;
  vendoring gives the same confinement benefit with declared inputs. Not
  implemented.

Honest limits of sandbox compatibility:

- **Registry sources must be vendored or pre-extracted** (see above).
- **CMake with the Visual Studio generator does not run confined.** MSBuild
  writes `%LOCALAPPDATA%\Temp\MSBuildTemp` regardless of `TMP`/`TEMP`
  (verified: relative and absolute workspace `TMP`/`TEMP`,
  `MSBUILDDISABLENODEREUSE=1`, and workspace-local `LOCALAPPDATA`/`APPDATA`/
  `USERPROFILE` all still fail with `UnauthorizedAccessException` on that
  path), which confinement denies. CMake plans run unsandboxed.
- **The staging directory is a shared scratch space.** `.ant/tmp` is
  world-accessible within the confined process; it is not an adversarial
  boundary (see `docs/sandbox/windows-design.md` — the sandbox confines
  writes, it is not a security boundary).
- **Relative values resolve against the child's cwd.** `TMP=.ant/tmp` works
  because each action's staging directory is prepared inside its working
  directory (the workspace root, or the package directory for npm workspace
  actions). If you edit a generated plan to change working directories, the
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
- **Target layout (cargo).** Custom target directories inside the project
  and the `release` profile are mapped; target directories outside the
  project fail closed; custom profiles are rejected. A configured default
  `build.target` triple (layout `<target-dir>/<triple>/<profile>`) is not
  reported by `cargo metadata`, so declared outputs would be wrong and the
  executor fails the action on output coverage instead of guessing.
- **Outputs only on request (npm).** See the npm section: no
  `package.json` field states what a build produces, so outputs are declared
  only with `--option outputs=entry-points`.
- **Full dependency resolution required (cargo).** Generation runs the
  complete `cargo metadata`, which resolves the dependency graph; a project
  whose dependencies are neither locked nor vendored may need network for that
  step.
- **npm workspace patterns are a small subset.** Literal paths and `*`
  within a segment only; `**`, negation, braces, and character classes fail
  closed. Per-script selection (anything other than `build`) is not
  supported.
- **`npm install` is a precondition (npm).** The npm adapter generates the
  build action; installing dependencies (`npm install`) is the user's
  responsibility and requires network/registry access. Under `ant run --sandbox`,
  a project whose build writes to `~/.npm` or other outside-workspace caches
  will fail; pre-warm those outside the sandbox.
- **Platform-native path separators.** Generated paths use the generating
  host's separators; cross-platform plan portability remains an open contract
  question (`docs/architecture.md`).
