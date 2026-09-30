#!/usr/bin/env bash
#
# Pre-merge gate: the full verification an agent (or a human) must pass green before merging.
# Runs the three checks as a fail-closed chain — cargo tests, clippy with warnings-as-errors,
# and the web build. It is deliberately un-piped: each stage runs to the terminal so a failure
# is never hidden, and `set -euo pipefail` aborts on the first non-zero exit with that exit code.
#
# Run it INSIDE the flake devShell (which pins the toolchain — the host PATH node may be too old
# for the web build):
#
#     nix develop -c scripts/gate.sh
#
# Exit code is authoritative: 0 = all three passed, non-zero = something failed. Never decide
# "green" by eyeballing a truncated tail of the output — trust the exit code (that is the whole
# point of this script; piping each stage to `tail` masks the real exit status).
set -euo pipefail

echo "== gate: cargo test =="
cargo test

echo "== gate: cargo clippy (warnings = errors) =="
cargo clippy --all-targets -- -D warnings

echo "== gate: web build =="
(cd web && npm run build)

echo "== gate: PASS =="
