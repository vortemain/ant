# ADR-0005: Flutter web adapter with truthful bundled-Dart toolchain identity

- Status: accepted
- Date: 2026-11-14
- Decision: D-038

## Context

The roadmap (P6 "Everything else") calls for a verified adapter per ecosystem.
Flutter is a high-leverage ecosystem (Bun/Expo-class tool, per the vision) and
its `web` target has one property that makes it a good first slice: the build
output layout (`build/web/index.html`, `build/web/main.dart.js`) is
deterministic and portable across machines, so the declared outputs are real
things Ant can verify rather than guess.

The front door (`ant build`) already supports cargo, npm, CMake, and Gradle.
Flutter needed three new pieces:

1. **Detection** — distinguishing a Flutter app/plugin from a plain Dart
   package (CLI, server), since Ant does not yet build non-Flutter Dart.
2. **Truthful toolchain identity** — `flutter` is a launcher script
   (`flutter.bat` on Windows), not a standalone executable. Digesting the
   launcher script would capture someone's shell wrapper, not the runtime
   the build actually runs on.
3. **Mode handling** — Flutter's build mode (release/debug/profile) is a real
   choice, like cargo's profile, so the `ant build` summary must always label
   it; the front door's existing `--dev`/`--release` flags had no Flutter
   meaning.

## Alternatives

1. **Digest the `flutter` launcher script** for toolchain identity. Rejected:
   `flutter` is a batch/shell shim that delegates to the bundled Dart VM. The
   launcher can be edited, moved, or replaced without changing what runs a
   build. The npm adapter already established the right precedent: digest the
   runtime, not the script (`node`, not `npm`). For Flutter the runtime is the
   bundled Dart executable at `<flutter_bin>/cache/dart-sdk/bin/dart<EXE>`.

2. **Declare pub package sources as inputs.** Rejected: pub sources live in
   the pub cache (`~/.pub-cache` / `PUB_CACHE`) outside the workspace. They are
   the derived product of `flutter pub get` against the registry, resolved
   from `pubspec.lock`, not source committed to the project. Declaring them
   would require digesting the pub cache and would make keys depend on a
   host-only directory. This is the same honest stance the Gradle adapter
   takes when its dependencies are not locally vendored.

3. **Make Flutter actions share-cache-eligible (`cache_policy: Eligible`).**
   Rejected: with pub sources undeclared, the action has undeclared inputs, so
   it can never honestly claim shared cache eligibility (D-003/D-010). Local
   reuse (D-028) is safe because `pubspec.lock` pins versions on this machine,
   so `cache_policy: Local` lets the developer reuse their own result without
   ever sharing a possibly-wrong artifact.

4. **Mark Flutter actions path-independent (D-030).** Rejected: the Dart
   compiler and bundler write absolute source paths into source maps and the
   generated `main.dart.js`, so outputs embed this clone's path. A shared
   cache could restore a wrong binary into a second clone. Flutter keys
   therefore keep the workspace path and rebuild in a second folder, like npm
   and CMake.

## Decision

Add a `flutter` adapter (`build-adapter::flutter`) and wire it through the
front door:

- **Detect** a `pubspec.yaml` whose `dependencies` (or `dev_dependencies`)
  reference the Flutter SDK (`sdk: flutter`). A plain Dart package without
  that reference is not recognized.
- **Generate** one action: `flutter build web --<mode>` (release default;
  `--option mode=debug|profile`). Only `target=web` is supported in this
  phase; any other target fails closed.
- **Declared inputs:** `pubspec.yaml`, `pubspec.lock` when present, and the
  `lib` (Dart sources), `web` (web template), and `assets` trees when
  present. Pub package sources are intentionally not declared.
- **Declared outputs:** `build/web/index.html` and `build/web/main.dart.js`,
  verified not trusted — the executor fails the action if one is not created.
- **Toolchain identity:** `flutter --version` for the version, and the SHA-256
  digest of the bundled Dart executable the build runs (`<flutter_bin>/cache/
  dart-sdk/bin/dart<EXE_SUFFIX>`). `flutter` itself is a launcher script.
- **Windows invocation:** `flutter` is `flutter.bat`, and `CreateProcess`
  cannot launch batch scripts directly, so the argv is
  `cmd /C flutter build ...` on Windows and `flutter build ...` elsewhere.
- **Caching:** `cache_policy: Local` (D-028); never shared, because pub
  sources are undeclared. No path-independence marker (D-030).
- **Front door:** `--dev` selects `mode=debug`, `--release` restores
  `mode=release`; the summary always labels the mode
  (`ant build · flutter · release · 1 step`, or `debug`/`profile`).

## Consequences

- A Flutter web project builds with `ant build` alone: detect → build →
  collect `build/web/*` into `dist/` → reuse the local result on a no-change
  rebuild. Verified end to end through a stub SDK
  (`ant-cli/tests/flutter_build.rs`), so the test needs no Flutter install,
  Dart SDK, or network.
- The adapter is honestly scoped: only `web` today; `apk`/`ios`/desktop
  targets (platform-SDK-dependent output layouts) are a later phase. Pub
  sources stay undeclared, so a missing dependency still fails and the action
  is never shared-cache-eligible.
- The bundled-Dart-VM-digest precedent is reusable: any future adapter whose
  build tool is a launcher script over a bundled VM (or a fixed runtime)
  should digest the runtime, not the launcher.

## Risks

- The bundled Dart executable path (`<flutter_bin>/cache/dart-sdk/bin/dart`)
  is a Flutter-SDK-internal layout. If a future Flutter release reorganizes
  the SDK, `flutter_toolchain` would fail to resolve it; the error message
  names the expected path and asks whether Flutter is installed.
- `flutter --version` can download artifacts on first run (Dart SDK,
  dependencies). Ant runs it only for the version banner; if that side effect
  changes, toolchain probing could have network dependence. Mitigated today
  by running it against the project root with the cleared child environment
  the other adapters use.
- Local-only reuse means a second clone rebuilds (no shared cache). This is
  the honest trade-off until pub sources can be declared or the build is
  proven path-independent; it matches the npm and Gradle adapters' stance.

## Reversal

Drop the `flutter` module registration in `build-adapter::lib` and the front-
door mode arms in `front.rs`; the adapter is self-contained and no other
adapter depends on it. Re-enable by re-registering. The `target`/`mode`
options are adapter-defined strings, so adding more targets later changes no
CLI grammar.
