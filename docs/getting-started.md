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
| C/C++ (CMake) | `CMakeLists.txt` | none yet (products stay in `build/`); Visual Studio's bundled CMake is found automatically on Windows |

Android, iOS, web bundlers, desktop installers, containers, and more languages are on the roadmap ([vision](vision.md), [plan](plan.md)).

## Good to know

- Ant never overwrites a `dist/` directory it did not create. If your project already uses `dist/`, set `out_dir` in `ant.toml`.
- If a directory matches several build systems (for example both `Cargo.toml` and `package.json`), Ant asks you to choose once: `ant init --adapter cargo`.
- Ant remembers each file's fingerprint between builds and only re-reads files whose size or timestamp changed, so a no-change build barely touches the disk. The record lives in your user cache folder, not the project (set `ANT_NO_DIGEST_CACHE=1` to turn it off).
- Ant's working files live in `.ant/` (for example `.ant/plan.json`, the exact plan it ran); you can delete them at any time.
- Building on your own servers, caching across machines, and the dashboard are described in [remote operations](remote/operations.md); a one-command setup (`ant server deploy`) is the next phase of the roadmap.
