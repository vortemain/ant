# ADR-0004: Install-by-QR with a minimal zero-dependency QR encoder

- Status: accepted
- Date: 2026-11-14

## Context

Roadmap P4 promises install-by-QR: a built APK is served over the LAN and the developer scans a QR code rendered in the terminal to install it. The renderer only needs small URLs (a 60-byte URL is version 4), and `ant-cli` is a zero-dependency crate with `unsafe_code = "forbid"`; pulling in a general QR crate (qrcode, image) adds dependencies and surface area far beyond the need. The risk is correctness: QR encoding is dense (Reed-Solomon over GF(256), block interleaving, mask selection, format/version information) and a subtly wrong matrix still *looks* like a QR code but scans as garbage or — worse — silently decodes after error correction hides 1-2 damaged codewords.

## Decision

Ship a minimal self-written encoder in `crates/ant-cli/src/qr.rs` (decision D-035): byte mode, error correction level M, versions 1-10, mask penalty rules, half-block terminal rendering — no dependencies, no unsafe. Correctness is anchored to three independent proofs: (1) format- and version-information strings are asserted against the published 32-string table; (2) every dumped matrix decodes with the independent jsQR decoder via `scripts/qr/verify-qr.js` (all 8 forced masks plus the real payloads, so a mask-selection bug cannot hide); (3) the forced-mask-4 rendering of "ant" is bit-for-bit identical to the production qrcode-generator reference — a diff of zero modules. The dump test writes the matrices to the temp dir for that external check.

## Consequences

`ant qr TEXT` renders a scannable code from a dependency-free, unsafe-free module; the CLI stays zero-dependency on the QR path. Codes are capped at 213-byte payloads (version 10, level M) — install URLs fit comfortably; longer payloads get a clear error instead of a wrong code. Future work (Kanji, other EC levels, versions 11+) stays out until a real need appears, at which point the encoder is extended or a vetted dependency replaces it and this record is superseded.

## Risks

A hand-written encoder can drift from the spec in ways unit tests sharing its assumptions won't catch — the bit-identity diff against an external reference and the out-of-tree jsQR decode exist precisely to close that loop; they must be re-run when the placement/masking code changes (the external script is the only part not in CI). jsQR silently error-corrects up to 2 damaged codewords, so "it decodes" alone is NOT proof of bit-correctness — the zero-diff check is the real gate. Terminal half-block rendering assumes a light background and a monospace font that keeps block glyphs square.
