# ADR-0007: Go, Python, and npm framework-aware adapters

- Status: accepted
- Date: 2026-11-15
- Decision: D-040, D-041, D-042

## Context

The roadmap (P6 Everything else) calls for verified adapters for every ecosystem:
web (Vite/Next…), desktop installers, container images, Go, Python, Java, .NET,
Flutter… — each a real project → packaged artifact. The Flutter web adapter
(D-038) and the Xcode (iOS) adapter (D-039) are done. The next ecosystems in
the P6 column are Go, Python, and the npm adapter's framework-aware outputs for
Vite and Next.js.

## Decision

Ship three adapters (or adapter extensions) now, each verified by `build-adapter`
unit tests.

### D-040: Go adapter

1. **Detection.** A root `go.mod` is the Go module marker. The adapter reads the
   module path and derives the binary name from its last segment (e.g.
   `github.com/user/myapp` → `myapp`).

2. **Plan.** One action runs `go build -o dist/<name><EXE_SUFFIX> .` — the root
   main package compiled to a deterministic binary path. The `--option goal=vet`
   mode runs `go vet ./...` (checks all packages, produces no binary). Declared
   inputs: `go.mod`, `go.sum` when present, `vendor/` when present, and `*.go`
   source files at the root. Declared output: `dist/<name>` as a `File`.

3. **Cache policy `Never`.** The Go module cache (`~/go/pkg/mod`) is outside the
   workspace and undeclared, so actions never claim cache eligibility (D-003,
   D-010) — the same stance as cargo and npm for registry dependency sources.

4. **Toolchain identity.** The version from `go version` and the SHA-256 digest
   of the resolved `go` executable. `go` is a standalone executable
   (`go.exe` on Windows), not a batch shim, so no `cmd /C` wrapper.

5. **Honest limits.** Only the root main package is built; a `cmd/` subdirectory
   layout arrives in a later phase. A library module (no `func main`) produces
   a valid plan but the action fails at execution time when the package has no
   main — the adapter does not guess.

6. **Verification.** 10 `build-adapter` unit tests: detect recognizes/rejects,
   goal validation, non-Go rejection, unknown option keys, module-name parsing
   with comments, binary-name derivation, build and vet argv assembly, declared
   inputs.

### D-041: Python adapter

1. **Detection.** A `pyproject.toml` with a `[build-system]` table (PEP 518) or
   a legacy `setup.py`. A `pyproject.toml` without `[build-system]` may be
   tooling-only config (ruff, black) and is not recognized unless `setup.py` is
   also present.

2. **Plan.** One action runs `python -m build [--wheel | --sdist]` — the PEP 517
   backend declared in `[build-system]` writes a wheel and/or an sdist into
   `dist/`. `--option format=wheel` (default) builds only the wheel, `--sdist`
   only the source distribution, `all` both (no flag). Declared inputs:
   `pyproject.toml`, `setup.py`, `setup.cfg`, `requirements.txt` when present,
   and `src/` when present. Declared output: `dist/` as a `Tree`.

3. **Tree output, not a guessed filename.** A wheel's filename encodes the
   version, platform, and Python tag — all deterministic for a given project but
   not a fixed name the adapter can hardcode. The `Tree` output covers the wheel
   (`dist/*.whl`) and the sdist (`dist/*.tar.gz`) without guessing either name.

4. **Cache policy `Never`.** PEP 517 build dependencies and the pip registry are
   outside the workspace and undeclared, so actions never claim cache eligibility
   (D-003, D-010).

5. **Toolchain identity.** The version from `python --version` and the SHA-256
   digest of the resolved `python` executable (standalone, not a batch shim).

6. **Verification.** 12 `build-adapter` unit tests: detect recognizes
   pyproject/setup.py, rejects tooling-only pyproject, format validation, non-
   Python rejection, unknown option keys, build argv for wheel/sdist/all, declared
   inputs, staging environment.

### D-042: npm framework-aware outputs

1. **Automatic framework output declaration.** The npm adapter now declares
   deterministic framework output directories automatically: when `vite` is in
   `dependencies`/`devDependencies` the action declares `dist/` as a `Tree`
   output; when `next` is in `dependencies` it declares `.next/` as a `Tree`
   output. No user option is needed — these are deterministic output layouts
   (Vite and Vite-based meta-frameworks always write to `dist`; Next.js always
   writes to `.next`).

2. **Entry-point outputs on top.** Entry-point outputs (`main`, `types`, `bin`)
   are still declared only with `--option outputs=entry-points` because they name
   where consumers load from, not what a build produces. When requested, they are
   declared on top of the framework output.

3. **Detection from the real `package.json`.** The framework is detected from the
   real `package.json` at generation time; no toolchain or network is needed.

4. **Verification.** 4 new `build-adapter` unit tests: Vite single-package
   declares `dist/` as `Tree`, Next.js single-package declares `.next/` as `Tree`,
   framework output combines with entry-point `File` output, and a Vite workspace
   package declares workspace-relative `dist/`. Existing npm adapter tests pass
   unchanged (empty options produce the same plan).

## Consequences

- `ant build` detects a Go module, a Python project, and a Vite/Next.js npm
  project, and generates the appropriate plan with declared inputs, outputs,
  cache policy, and truthful toolchain identity.
- The `go` and `python` adapters are registered in `available()` and reachable
  through the front door's auto-selection and `--adapter` override.
- The npm adapter's plan changes only when a framework is detected: empty options
  produce the same plan as before (the framework outputs are additive, not
  replacing the existing outputs).
- What is **not** done: a real Go/Python build end-to-end (needs the toolchains
  installed and a real project); `cmd/` subdirectory layouts for Go; Go module
  proxy pinning; a signed wheel; and the remaining P6 ecosystems (Java, .NET,
  desktop installers, container images).

## Alternatives considered

- **Wait for all P6 ecosystems at once.** The roadmap says "each ecosystem" —
  shipping one verified adapter at a time keeps the surface reviewable and the
  adapter contract exercised.
- **Hardcode the wheel filename.** A wheel's filename encodes version, platform,
  and Python tag — all deterministic for a given project, but not knowable
  without parsing the project metadata and resolving the platform. The `Tree`
  output (`dist/`) covers it without guessing, matching the CMake adapter's
  build-directory output.
- **Declare framework outputs only with an option.** Vite's `dist/` and Next.js's
  `.next/` are deterministic output layouts — every Vite build writes to `dist/`,
  every Next.js build to `.next/` — so declaring them automatically is honest,
  unlike entry-point outputs which name where consumers load from. The option
  stays for entry points; the framework output needs no option.