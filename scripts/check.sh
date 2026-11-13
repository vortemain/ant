#!/usr/bin/env sh
# Ant quality gate (POSIX shell). Mirrors scripts/check.ps1.
#
# Hard gates: rustfmt, clippy with warnings denied across all targets, and
# the full workspace test suite (decision D-012).
set -u
cd "$(dirname "$0")/.."
failed=""

echo "==> cargo fmt --check"
cargo fmt --all -- --check || failed="$failed fmt"
echo "==> cargo clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings || failed="$failed clippy"
echo "==> cargo test"
cargo test --workspace || failed="$failed test"

if [ -n "$failed" ]; then
    echo "Quality gate FAILED:$failed"
    exit 1
fi
echo "Quality gate passed."
