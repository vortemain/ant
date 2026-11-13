# Ant — product vision

**Owner's statement (2026-11-13):** the vision is having *everything* covered.

**Owner's clarification (2026-11-13, D-026): Ant is local-first.** Most developers do not have a VPS; they build on their own laptop, often an old one. Ant must make *local* builds dramatically faster and simpler first. Running builds on a VPS or in the cloud is an optional extension for those who have one, never a requirement and never the headline.

## What Ant is

Ant is to building and shipping software what **Bun** is to JavaScript tooling and what **Expo** is to mobile apps — a fast, **local-first** tool for **every platform and language**, that can optionally scale out to your own servers:

- **One tool, very few commands, very fast.** A developer runs `ant build` and the system works out the rest: what the project is, how to build it, what can be reused, and where to run it. No plan files, no flags, no wiring.
- **Builds everything, ready to ship.** Android (APK/AAB), iOS (IPA), web, desktop (installers for Windows/macOS/Linux), backend services (container images), libraries and CLIs — in any language (JavaScript/TypeScript, Rust, Go, Java/Kotlin, Swift, C/C++, C#/.NET, Python, Dart/Flutter, …). Output is *packaged*, signed where the platform requires it, and downloadable.
- **Fast on the machine you already have.** Local builds reuse every result Ant can prove correct, keep the laptop's own work proportional to what changed, and use all its cores. This is the default and the main product.
- **Optionally scales onto your own servers.** Developers who have a VPS can point Ant at it once (`ant server deploy user@host`) and builds offload automatically — in parallel, with a shared cache — with no vendor cloud and no lock-in. Nobody needs a server to get value from Ant.
- **The system handles itself.** Project detection, toolchain provisioning on workers, caching, credentials and signing (keystores, certificates, provisioning profiles), TLS for servers, worker health, retries — managed by Ant, not by the user.
- **A polished dashboard at Expo-level quality.** Projects, builds, live logs, artifacts (scan a QR code to install a build on your phone), servers and workers, team access.
- **Trustworthy.** Every speed claim is measured; a result is reused only when Ant can prove it is correct; nothing is called supported until it works end to end (see `AGENTS.md`).

## The developer experience we are aiming for

```bash
ant init                         # once: detect the project, write a minimal ant.toml only if needed
ant build                        # build for the detected platform(s); fast, cached, packaged into dist/
ant build android ios            # explicit targets
# optional, only for those with a server:
ant server deploy root@my-vps    # once: install + TLS + start a self-hosted build server over SSH
ant login https://build.my.dev   # link this machine to the server; later builds offload automatically
ant open                         # open the dashboard
```

Everything built so far (action model, plans, adapters, sandbox, cache, coordinator/worker protocol) is the **engine underneath**; its plumbing commands remain for power users and debugging, but they are not the product's front door.

## Hard facts that shape the plan

- **iOS needs a Mac.** Xcode only runs on macOS, and Apple's license requires Apple hardware. Self-hosted iOS builds therefore need a Mac worker (owned Mac mini or rented cloud Mac). Ant makes adding one a single command; it cannot remove the need for one.
- **Self-hosting means Linux.** Most VPSs run Linux, so the optional server story runs on Linux (the Linux build compiles and is tested in CI since P2.1).
- **"Fast" must be proven on real projects.** Today real adapter builds never hit the cache (dependency sources are undeclared); fixing that is a core deliverable, not polish.
- **Signing credentials are secrets.** Keystores, certificates and passwords must be stored encrypted, scoped, never logged, and never part of cache keys or plan files.

## Open questions (record answers here as they come)

- Which Mac (if any) will serve iOS builds?
- Which VPS provider/OS for the first deployment (assumed: Ubuntu LTS on a common provider)?
- Expo Go-style live preview / over-the-air updates: in scope as a later phase (assumed yes, after build and ship work end to end).
