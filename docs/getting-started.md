# Getting started

Ant builds your project with one command. No configuration file is required.

## Build

```bash
cd my-project
ant build
```

Ant detects the project, builds it in release mode using all your cores, and copies the deliverables into `dist/`:

```
ant build · cargo · release · 1 step · 8 parallel
  ✓ workspace  1.6s

Built in 2.0s →
  dist\greeter.exe
```

- `ant build --dev` builds in debug mode.
- `ant build --release` in an Android project builds the signed release APK and the AAB for Google Play (see below).
- `ant build --fresh` ignores results and plans Ant saved earlier and builds everything again.
- `ant build --verbose` also prints the engine's detailed status lines.
- The build tool's own output (compiler messages, warnings, errors) appears as usual.

## Record the setup (optional)

```bash
ant init
```

This writes a short, commented `ant.toml` so every machine builds the project the same way, and adds `.ant/` and `dist/` to `.gitignore` in git projects:

```toml
adapter = "cargo"
out_dir = "dist"   # where `ant build` collects deliverables

[options]
# profile (dev|release): cargo build profile
profile = "release"
```

All fields are optional. Unknown keys are rejected, so typos fail loudly instead of being ignored.

## What Ant builds today

| Project | Detected by | Deliverables collected into `dist/` |
|---|---|---|
| Rust (Cargo, including workspaces) | `Cargo.toml` | the binaries |
| JavaScript/TypeScript (npm, including workspaces) | `package.json` with a `build` script | none by default (your build script writes its own output); set `outputs = "entry-points"` under `[options]` to have Ant verify and collect the entries named in `package.json` |
| C/C++ (CMake) | `CMakeLists.txt` | executables and shared libraries (from CMake's own target list); Visual Studio's bundled CMake is found automatically on Windows |
| Go (modules) | `go.mod` | one executable per `main` package (release builds are trimmed: no paths or symbols) |
| Flutter (web) | `pubspec.yaml` using the Flutter SDK | `index.html` and `main.dart.js` |
| iOS (Xcode, on a Mac) | `*.xcodeproj` / `*.xcworkspace` | none yet (the build lands in `.ant/build`); `ant build ios` |
| Java (Maven) | `pom.xml` | the `target/` folder (your JAR/WAR) |
| Python packages | `pyproject.toml` with `[build-system]`, or `setup.py` | the `dist/` folder (wheel and/or sdist) |
| Desktop apps (Tauri, Electron) | `src-tauri/tauri.conf.json`; an npm project using `electron` | the installer folder (`src-tauri/target/release/bundle/`, `dist/`) |
| Container images (Docker) | a `Dockerfile` (alone, or `adapter = "docker"` in `ant.toml`) | the image in Docker, or `dist/image.tar` with `output = "tar"` |

Vite and Next.js projects (npm) report their `dist/` / `.next/` folder automatically.
| Android (Gradle wrapper) | `settings.gradle(.kts)` + `gradlew` | each app module's debug APK, signed with your usual debug key; Ant finds the Android SDK in its default location if `ANDROID_HOME` is not set, uses your own Gradle cache, and keeps Gradle's daemon warm between builds |

iOS, web bundlers, desktop installers, containers, and more languages are on the roadmap ([vision](vision.md), [plan](plan.md)).

## Signed Android releases

`ant build` in an Android project makes a debug APK you can install right away. For the Play Store:

```
ant build --release
```

The first time, Ant creates an upload key for your app and tells you where it lives; after that every release is signed with it. Back the key up once (keep the folder in a password manager, never in git):

```
ant signing export --out D:\backups\my-app-key
```

Already have a key? Bring it before your first release: set `ANT_SIGNING_STORE_PASSWORD` (and `ANT_SIGNING_KEY_PASSWORD` if it differs), then run `ant signing import --keystore my.jks --alias my-alias`. `ant signing status` shows which key a project uses. Passwords never appear in Ant's output, plan files, or caches; on Windows they are encrypted for your user account.

## Put the app on your phone

```
ant install
```

Ant builds (instantly if nothing changed) and installs the APK on every phone connected over USB with USB debugging on. With no phone plugged in, it shares the app over your Wi-Fi instead and prints a QR code: scan it with the phone's camera, tap the link, and allow your browser to install apps when Android asks. Press Ctrl+C to stop sharing (or pass `--once`). `ant install --release` installs the signed release build; `--qr` shares over Wi-Fi even when a phone is plugged in. The link is only for your local network, and only someone who has the link can download the app.

## Good to know

- Ant never overwrites a `dist/` directory it did not create. If your project already uses `dist/`, set `out_dir` in `ant.toml`.
- If a directory matches several build systems (for example both `Cargo.toml` and `package.json`), Ant asks you to choose once: `ant init --adapter cargo`.
- Ant reuses results it built earlier on this machine: if you switch back to code it has already built (another branch, an undone change), the deliverables are restored instead of rebuilt. A change to any project file, the lock file, the toolchain, or the environment means a real build. These saved results stay on your machine. Restoring them is nearly instant: on the same drive Ant links the files from its store instead of copying them. Use `ant build --fresh` if you ever suspect a stale result.
- Ant remembers each file's fingerprint between builds and only re-reads files whose size or timestamp changed, so a no-change build barely touches the disk. The record lives in your user cache folder, not the project (set `ANT_NO_DIGEST_CACHE=1` to turn it off).
- Ant also remembers the build plan it worked out for your project and reuses it while nothing that shapes it has changed (project files, the toolchain rustup selects, cargo configuration, or `CARGO_*`/`RUSTUP_*` environment), so it does not ask cargo about your workspace on every build.
- Set `ANT_PROFILE=1` to see where an `ant build` spends its time.
- Ant's working files live in `.ant/` (for example `.ant/plan.json`, the exact plan it ran); you can delete them at any time.
- Building on your own servers, caching across machines, and the dashboard are described in [remote operations](remote/operations.md); a one-command setup (`ant server deploy`) is the next phase of the roadmap.
