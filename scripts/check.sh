#!/usr/bin/env sh
# Ant quality gate (POSIX shell). Mirrors scripts/check.ps1.
#
# Hard gates: clippy with warnings denied across all targets, and the full
# workspace test suite. Formatting is advisory until the one-time format
# commit lands (decision D-012); pass --strict-format to make it a hard gate.
set -u
cd "$(dirname "$0")/.."
strict_format=0
[ "${1:-}" = "--strict-format" ] && strict_format=1
failed=""

echo "==> cargo fmt --check"
if ! cargo fmt --all -- --check >/dev/null; then
    if [ "$strict_format" -eq 1 ]; then failed="$failed fmt"; else echo "ADVISORY: formatting differences (not failing the gate)"; fi
fi
echo "==> cargo clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings || failed="$failed clippy"
echo "==> cargo test"
cargo test --workspace || failed="$failed test"

if [ -n "$failed" ]; then
    echo "Quality gate FAILED:$failed"
    exit 1
fi
echo "Quality gate passed."
