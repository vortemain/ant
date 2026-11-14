# ADR-0006: Xcode (iOS) adapter with macOS CI verification

- Status: accepted
- Date: 2026-11-15
- Decision: D-039

## Context

The roadmap (P5 iOS) calls for an Xcode adapter, Mac workers, certificate and
provisioning-profile management, and a signed IPA from `ant build ios`. The
full P5 exit criterion is a real iOS app built signed on a Mac worker. This
machine is not a Mac and has no Xcode, so the toolchain capture and a signed
export cannot be exercised here; only the adapter's planning layer and its
unit tests can be written and verified locally.

## Decision

Ship the Xcode adapter's plan-generation and toolchain layer now, and verify
the parts that need a real Mac on a macOS CI runner.

1. **Detection.** A root-level `*.xcodeproj` or `*.xcworkspace` is the iOS app
   marker. A pure iOS app keeps one at the project root; a Flutter/React Native
   project keeps its iOS project in an `ios/` subfolder, so there is no
   collision with the Flutter adapter's `pubspec.yaml` detection.

2. **Plan.** One action runs `xcodebuild -scheme <scheme> -configuration <config>
   -sdk <sdk> -derivedDataPath .ant/build build`. `-scheme` is omitted when
   unspecified; `-configuration` is `Release` by default (`--dev` for `Debug`);
   `-sdk` is `iphoneos` by default (`iphonesimulator` for a credential-free
   simulator build). Declared inputs: the project package, `Package.swift`,
   `Podfile`, `Podfile.lock`, `Sources`/`App` when present. Declared output:
   the `.ant/build` derived-data directory. No deliverable is claimed — the
   `.app` product name is not known without parsing the project (same stance
   as the CMake adapter).

3. **Cache policy `Local`.** SPM clone sources and the Pods cache live outside
   the workspace and are undeclared inputs, so actions never claim shared cache
   eligibility; local reuse is allowed (D-028). This matches the Flutter and
   Gradle adapters' stance for undeclared dependency sources.

4. **Toolchain identity.** The version from `xcodebuild -version` and the
   SHA-256 digest of the resolved `xcodebuild` executable. `xcodebuild` is a
   real executable (an `xcrun` shim on macOS), so no `cmd /C` wrapper.

5. **macOS CI.** The toolchain-capture unit tests skip honestly where
   `xcodebuild` is absent (no Xcode on Linux/Windows) so the main CI matrix
   stays green, and run for real on a macOS runner (`verify-ios.yml`,
   `macos-latest`) where real Xcode is present.

## Consequences

- `ant build ios` routes to the Xcode adapter and refuses a non-Xcode project
  before adapter selection with an honest reason (ios is Xcode-specific, like
  `android` is Gradle-specific).
- The adapter's planning layer is verified on every platform; the toolchain
  capture is verified on Apple hardware only, on a dedicated CI job.
- What is **not** done: a signed `.ipa` export (needs an Apple Developer
  identity and a provisioning profile the owner must supply), Mac workers
  (`ant worker add` on a Mac), and certificate/provisioning-profile management.

## Alternatives considered

- **Wait for a Mac.** Blocked on hardware this machine does not have; the
  macOS CI runner is available now and verifies the layer that needs Xcode.
- **Run a real `xcodebuild build` in CI.** Needs a sample app and time on a
  macOS runner; the adapter builds a plan, not a product, so the unit tests
  suffice for the planning layer. An end-to-end build is a later step.
