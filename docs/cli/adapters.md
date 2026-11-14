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

## The `go` adapter (P6, D-035)

Detects a Go module (`go.mod` at the project root) and emits one action,
`go build -o .ant/go/bin/ ./...`: every package is compiled and each `main`
package becomes `.ant/go/bin/<name>[.exe]` (named like `go build` names it;
`ant build` collects them into `dist/`).

- **Options:** `profile=dev|release` (`ant build` uses `release`, which adds
  `-trimpath -ldflags=-s -w`; `--dev` gives a plain, debuggable build).
- **Inputs:** the whole project except `.ant/`, `.git/`, and output
  directories Ant created.
- **Environment:** the executor clears the child environment, so the plan
  declares Go's effective settings from `go env -json` (module and build
  caches, proxy and checksum settings, `GOFLAGS`, `GOTOOLCHAIN`,
  `CGO_ENABLED`, `GOOS`, `GOARCH`) with `GOENV=off`, and workspace-relative
  temp directories. The absolute cache paths make results machine-local.
- **Caching:** `cache_policy: local` (modules are pinned by `go.sum`);
  `never` when a `replace` points at a directory outside the project or the
  project contains a symlink.
- **Honest limits:** with cgo, the C compiler is not part of the toolchain
  identity; cross-machine reuse is not offered.

## The `flutter` adapter (P6, D-037)

Detects a Flutter app or plugin: a `pubspec.yaml` whose dependencies reference
the Flutter SDK (`sdk: flutter`). A plain Dart package (CLI, server) has no
such reference and is not recognized — Ant does not yet build non-Flutter
Dart. Generation emits one `flutter build web --<mode>` action and re-validates
the plan through the build graph before any side effect.

- **Target (T3.13):** only `web` is supported in this phase (`--option
  target=web`, the default). Its output layout (`build/web/index.html`,
  `build/web/main.dart.js`) is deterministic and portable, so the declared
  outputs are real. Other targets (`apk`, `ios`, desktop) have
  platform-SDK-dependent layouts and arrive in a later roadmap phase;
  `--option target=` anything but `web` fails closed.
- **Mode:** `release` (default), `debug`, or `profile` (`--option mode=...`).
  Flutter's build mode is a real choice (like cargo's profile), so the summary
  always labels it: `ant build · flutter · release · 1 step` (or `debug`/`profile`).
  `--dev` selects debug, `--release` restores release.
- **Inputs declared:** `pubspec.yaml`, `pubspec.lock` when present, and the
  `lib` (Dart sources), `web` (web template) and `assets` trees when present.
  Pub package sources live in the pub cache (`~/.pub-cache` / `PUB_CACHE`)
  outside the workspace and are intentionally not declared — they are the
  derived product of `flutter pub get` against the registry, not source.
- **Outputs declared:** the deterministic web files `build/web/index.html` and
  `build/web/main.dart.js`, **verified, not trusted** (the executor fails the
  action when one is not created).
- **Caching:** because pub package sources and the bundled Dart VM are
  undeclared inputs, generated actions are `cache_policy: Local` (D-028): this
  machine may reuse its own result (pubspec pins versions locally) but it is
  never shared — the same honest stance the Gradle adapter takes when its
  dependencies are not locally declared.
- **Truthful toolchain identity** from the real Flutter SDK: `flutter
  --version` for the version, and the SHA-256 digest of the bundled Dart
  executable the build actually runs (`<flutter_bin>/cache/dart-sdk/bin/
  dart<EXE_SUFFIX>`). `flutter` itself is a launcher script (`flutter.bat` on
  Windows), not a standalone executable; the bundled Dart VM is the truthful
  runtime identity, the way `node` is for npm.
- **Windows invocation:** `flutter` is `flutter.bat` and `CreateProcess`
  cannot launch batch scripts directly, so the argv is
  `cmd /C flutter build ...` on Windows and `flutter build ...` elsewhere.
- **Workspace-relative scratch environment:** like the other adapters, every
  generated action sets `TMP`/`TEMP` (plus `TMPDIR` on non-Windows) to `.ant/tmp`,
  resolved against the action's working directory.
- **Honest limits:** Flutter actions are honestly **path-dependent** (local keys stay
  path-specific, D-031): the Dart compiler and bundler write absolute source paths
  into source maps and the generated `main.dart.js`, so outputs may embed this
  clone's path and a shared cache could restore a wrong binary into a second
  clone. Keys therefore keep the workspace path and a second clone in another
  folder rebuilds; the marker exists only for adapters whose outputs Ant can
  prove path-independent. The Flutter SDK itself is a host installation,
  identified by its launcher location and the bundled Dart digest, not a
  workspace input. Verified end to end: adapt → run builds a Flutter web project
  through a stub SDK (no Flutter install, Dart SDK, or network needed), the
  declared web files flow from `lib/main.dart` to `dist/`, and a no-change
  rebuild reuses the local result without running Flutter again
  (`ant-cli/tests/flutter_build.rs`).

## The `xcode` adapter (P5, D-038) — `ant build ios`

Detects an iOS app by a root-level `*.xcodeproj` or `*.xcworkspace` (a pure iOS
app keeps one at the project root; a Flutter/React Native project keeps its iOS
project in an `ios/` subfolder, so there is no collision with the Flutter
adapter's `pubspec.yaml` detection). The single build action runs
`xcodebuild -scheme <scheme> -configuration <config> -sdk <sdk>
-derivedDataPath .ant/build build` — `-scheme` is omitted when unspecified
(xcodebuild uses the project default), `-configuration` is `Release` by
default (`--dev` for `Debug`), and `-sdk` is `iphoneos` (the device, the
deliverable) by default (`iphonesimulator` for a credential-free simulator
build).

The declared inputs are the project package, `Package.swift`, `Podfile`,
`Podfile.lock`, and `Sources`/`App` trees when present; the declared output is
the `.ant/build` derived-data directory. The `.app` product name is not known
without parsing the project, so — like the CMake adapter — no deliverable is
claimed and the build directory is the only declared output.

- **Honest limits:** only the **build** verb is supported in this phase. A
  signed `.ipa` export (the full P5 success criterion) needs an Apple Developer
  identity and a provisioning profile the owner must supply, so a signed export
  is a later step; unsigned simulator and development builds need no
  credentials. Mac workers and certificate/provisioning-profile management are
  not yet implemented.
- **Cache policy `Local`:** SPM clone sources and the Pods cache live outside
  the workspace and are undeclared inputs, so these actions never claim shared
  cache eligibility. Local reuse is allowed: `Package.resolved` / `Podfile.lock`
  pin versions and this machine may reuse its own result (D-028).
- **Truthful toolchain identity** from the real Xcode: the version from
  `xcodebuild -version` and the SHA-256 digest of the resolved `xcodebuild`
  executable. `xcodebuild` is a real executable (an `xcrun` shim on macOS), so
  no `cmd /C` wrapper is needed; on a non-Mac it is simply absent and
  `generate` fails closed with a toolchain error.
- **Verified:** the adapter unit tests skip honestly where `xcodebuild` is
  absent (no Xcode on Linux/Windows) so the main CI matrix stays green, and run
  for real on a macOS CI runner (`verify-ios.yml`, `macos-latest`) where real
  Xcode is present — exercising the genuine toolchain capture and plan
  assembly on Apple hardware without needing an Apple Developer identity or a
  sample app (the adapter builds a plan; it does not run `xcodebuild build`).

## The `python` adapter (P6, D-039)

A Python project has a `pyproject.toml` (PEP 518/621, with a `[build-system]`
table) or a legacy `setup.py`. The adapter generates one action that runs
`python -m build`, declaring the workspace inputs it can honestly cover and
the `dist/` tree it produces.

- **Build:** `python -m build [--wheel | --sdist]` — calls the PEP 517 backend
declared in `pyproject.toml`'s `[build-system]` (or the legacy
  `setup.py`/`setup.cfg` metadata) and writes a wheel and an sdist into
  `dist/`. `--option format=wheel` (default) builds only the wheel,
  `--option format=sdist` only the source distribution, `--option format=all`
  both (no flag).
- **Detection:** a `pyproject.toml` with a `[build-system]` table is a
  buildable package (PEP 518). One without it may be tooling-only config
  (ruff, black, etc.) and is not recognized unless `setup.py` is also present.
  A bare `setup.py` is recognized (legacy).
- **Declared inputs:** `pyproject.toml`, `setup.py`, `setup.cfg`, and
  `requirements.txt` when present, and the `src/` source tree when present.
  PEP 517 build dependencies and the pip registry are outside the workspace
  and intentionally not declared.
- **Declared output:** `dist/` (`OutputKind::Tree`). A wheel's filename
  encodes the version, platform, and Python tag — all deterministic for a given
  project, but not a fixed name the adapter can hardcode. The tree output
  covers the wheel (`dist/*.whl`) and the sdist (`dist/*.tar.gz`) without
  guessing either name.
- **Cache policy `Never`:** PEP 517 build dependencies and the pip registry are
  undeclared inputs (they live outside the workspace), so generated actions
  never claim cache eligibility (D-003, D-010).
- **Truthful toolchain identity** from the real `python` runtime: the resolved
  PATH executable's SHA-256 digest and `python --version`. `python` is a
  standalone executable (`python3` on Linux, `python.exe` on Windows), not a
  batch shim, so no `cmd /C` wrapper is needed on Windows.
- **Workspace-relative scratch:** `TMP`/`TEMP` (plus `TMPDIR` off Windows)
  point at `.ant/tmp`, the staging pattern shared with the other adapters.

## npm framework outputs (P6, D-040)

The npm adapter declares a framework's output directory automatically when it
finds the framework in `package.json`: `vite` in `dependencies` or
`devDependencies` declares `dist/`, `next` in `dependencies` declares `.next/`
(both as tree outputs, in each workspace package that uses them). Entry-point
outputs (`outputs=entry-points`) are added on top when requested. Actions stay
`cache_policy: never` (`node_modules` is an undeclared input).

## The `maven` adapter (P6, D-041)

A `pom.xml` at the root: one action `mvn package -DskipTests`
(`--option goal=package|verify|compile`; `verify` runs the tests). Declares
`target/` as a tree output (the artifact name encodes `artifactId` and
`version`). `cache_policy: never`: dependencies resolve from Maven Central into
`~/.m2`, outside the project.

## The `docker` adapter (P6, D-041)

A `Dockerfile` at the root: `docker build -t <tag> .` (`--option tag=NAME`,
default the directory name). With the default `output=image` the image goes to
the Docker daemon and no file is declared; `--option output=tar` writes a
portable image tar to `.ant/docker/image.tar` (BuildKit), which `ant build`
collects into `dist/image.tar`. `Dockerfile` and `.dockerignore` are declared
inputs; the rest of the build context and pulled base images are not, so
`cache_policy: never`. Docker is **packaging-only** for project detection: a
Rust, Go, or other project that also has a `Dockerfile` keeps building with its
own adapter; Docker is chosen when the `Dockerfile` is all there is, or when
`ant.toml` says `adapter = "docker"` (D-036).

## The `tauri` adapter (P6, D-041)

`src-tauri/tauri.conf.json`: one action `npm run tauri build`, which compiles
the Rust backend with cargo, bundles the frontend, and writes installers to
`src-tauri/target/release/bundle/` (declared as a tree output). npm and cargo
registry dependencies are undeclared, so `cache_policy: never`. A Tauri app is
also an npm project and contains a cargo crate; the Tauri adapter supersedes
both in detection (D-036).

## The `electron` adapter (P6, D-041)

An npm project with `electron` in `dependencies` or `devDependencies`: one
action `npx electron-builder` (`cmd /C npx` on Windows), installers in `dist/`
(declared as a tree output). `cache_policy: never` (`node_modules`). Electron
supersedes npm in detection (D-036).

## The `gradle` adapter (P4, D-024)

Detects a Gradle **wrapper** project (`settings.gradle[.kts]` plus `gradlew`
/ `gradlew.bat` at the project root) and emits one action that runs the
wrapper task (`--option task=assembleDebug|assembleRelease|release`, default
`assembleDebug`; `release` runs `assembleRelease bundleRelease` and declares the
signed `<module>-release.apk` and `.aab`, so it succeeds only when signing is
injected, as `ant build --release` does, D-033) with `--console=plain`, plus `--no-daemon` unless
`--option home=user`.

- **Homes (`--option home=project|user`):** `project` (the default for
  `ant adapt`, sandboxed runs, and workers) keeps Gradle's and Android's
  homes in `.ant/` as described under Environment. `user` (what `ant build`
  passes, D-032) uses the developer's own `~/.gradle` and `~/.android` (or
  `GRADLE_USER_HOME`/`ANDROID_USER_HOME` when set), so local builds reuse
  already-downloaded dependencies and the usual debug signing key, and it
  declares `JAVA_HOME` and the Android SDK (`ANDROID_HOME`/`ANDROID_SDK_ROOT`,
  else `sdk.dir` in `local.properties`, else the default install location
  when it exists). These absolute values are key material, so such results
  are reused only on this machine. `user` plans also keep Gradle's daemon
  warm: they drop `--no-daemon` and set `background_services` so the daemon
  survives the action (on Windows it leaves the action's job object; see
  the action contract).

- **Inputs:** the whole project except Gradle's outputs and caches. Every
  module directory (one with `build.gradle[.kts]`) is listed child by child
  without its `build/`; `.gradle/`, `.kotlin/`, `.idea/`, `.cxx/`, `.ant/`,
  and `.git/` are never inputs. So keys do not change when a previous build
  left `build/` directories behind.
- **Outputs:** for `assembleDebug`, each Android application module's
  (`android.application` plugin) debug APK at
  `<module>/build/outputs/apk/debug/<module>-debug.apk`. Product flavors or
  renamed outputs fail with "declared output was not created". Other tasks
  declare no outputs.
- **Environment:** `GRADLE_USER_HOME=.ant/gradle-home`,
  `ANDROID_USER_HOME=.ant/android-home`, temp dirs under `.ant/tmp`, all
  workspace-relative (made absolute at launch by a small `sh` wrapper on
  Unix) so nothing is written outside the workspace and keys stay
  path-independent. `JAVA_HOME`, `ANDROID_HOME`, and `ANDROID_SDK_ROOT` pass
  through on Unix and their values are key material.
- **Caching:** Gradle downloads dependencies and its own distribution, which
  a sandboxed worker (no network) cannot do and which would be undeclared
  inputs. The action is `cache_policy: eligible` only when both are inside
  the project: a read-only dependency cache at `.gradle-deps/` (a copy of a
  Gradle home's `caches/modules-2` without lock files; passed as
  `GRADLE_RO_DEP_CACHE`) and a local `distributionUrl` (for example the
  distribution zip next to `gradle-wrapper.properties`). Then the action runs
  with the Kotlin compiler in-process and `--build-cache`, and asks workers
  for the shared Gradle task cache (`ANT_GRADLE_TASK_CACHE=1`, D-025). It
  does not pass `--offline` (Gradle turns its remote cache off in offline
  mode); the sandbox has no network, so a missing dependency still fails.
  Otherwise it is `cache_policy: never`.
- **Honest limits:** the JDK and Android SDK are host installations,
  identified only by the environment values above (not digested); the
  toolchain identity is the wrapper jar digest, the distribution, and
  `java --version`. Only the single-root wrapper layout is supported.
  Verified locally on Windows (2026-11-14): a minimal plain-Java app (AGP
  8.11.0, Gradle 8.14.3, compileSdk 35) builds with `ant build` alone, the
  debug APK lands in `dist/`, and `apksigner` verifies it. Plans are cached
  (`plan_cache_probe`: JDK on `PATH`, `JAVA_*`/`ANDROID_*`/`GRADLE_*` and
  home environment, default SDK locations).

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
