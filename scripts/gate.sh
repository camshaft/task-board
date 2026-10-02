#!/usr/bin/env bash
#
# Pre-merge gate: the full verification an agent (or a human) must pass green before merging.
# Runs the checks as a fail-closed chain — rustfmt --check, cargo tests, clippy with
# warnings-as-errors, the web build, and (when the web deps changed) the nix web FOD. It is
# deliberately un-piped: each stage runs to the terminal so a failure is never hidden, and
# `set -euo pipefail` aborts on the first non-zero exit with that exit code.
#
# The toolchain is pinned by the flake devShell. This script SELF-PINS: if it is not already running
# inside the devShell (cargo-clippy not resolving into the nix store), it re-execs itself through
# `nix develop .#default -c`, so both of these run the pinned clippy/rustc/node and a host rustup
# clippy can never leak in (a floating clippy fires upstream lints the pinned toolchain never would):
#
#     scripts/gate.sh                  # self-pins into the devShell
#     nix develop -c scripts/gate.sh   # also fine (already pinned; guard falls through)
#
# Exit code is authoritative: 0 = all stages passed, non-zero = something failed. Never decide
# "green" by eyeballing a truncated tail of the output — trust the exit code (that is the whole
# point of this script; piping each stage to `tail` masks the real exit status).
set -euo pipefail

# Self-pin to the flake toolchain. A rustup clippy lives in ~/.cargo/bin (not a store path), so if
# cargo-clippy does not resolve into the nix store we are not in the pinned devShell -- re-exec the
# whole gate through it. After the re-exec cargo-clippy resolves to /nix/store and the guard falls
# through, so there is no loop. Closes the drift where a bare invocation inherited host clippy 1.98
# and fired lints the pinned 1.90 never would.
clippy_path="$(command -v cargo-clippy || true)"
case "$clippy_path" in
  /nix/store/*) : ;; # already pinned -- proceed
  *) exec nix develop .#default -c "$0" "$@" ;;
esac

echo "== gate: cargo fmt --check =="
cargo fmt --check

echo "== gate: cargo test =="
cargo test

echo "== gate: cargo clippy (warnings = errors) =="
cargo clippy --all-targets -- -D warnings

echo "== gate: web build =="
# `tsc`/`vite` live in web/node_modules/.bin, so a fresh worktree (no node_modules) would otherwise
# die with a bare "tsc: command not found" (task 698). Install deps first when they are absent —
# the lockfile is present and `npm ci` is ~5s — so the gate is self-sufficient on a clean checkout.
if [ ! -d web/node_modules ]; then
  echo "web/node_modules missing -- running (cd web && npm ci)"
  (cd web && npm ci)
fi
(cd web && npm run build)

# The GitOps deploy builds the web assets via a nix fixed-output derivation pinned by npmDepsHash
# (nix/package.nix). `npm run build` above uses the already-installed node_modules, so a dependency
# change that is not matched by an npmDepsHash bump passes the local gate yet BREAKS the deploy
# ("npmDepsHash is out of date") — task 700. Build the FOD here whenever the web deps changed vs the
# base, so a stale hash fails pre-merge instead of at deploy. Skipped for the common Rust-only change
# (keeps the inner loop fast) and when neither nix nor the base ref is available.
base="origin/main"
if command -v nix >/dev/null 2>&1 && git rev-parse --verify --quiet "$base" >/dev/null 2>&1; then
  if ! git diff --quiet "$base" -- web/package.json web/package-lock.json; then
    echo "== gate: web FOD (nix build, web deps changed vs $base) =="
    echo "if this fails with a hash mismatch, update npmDepsHash in nix/package.nix to the suggested value"
    nix build --no-link '.#task-board.web'
  else
    echo "== gate: web FOD skipped (web deps unchanged vs $base) =="
  fi
else
  echo "== gate: web FOD skipped (no nix or no $base ref; run 'nix build .#task-board.web' if you changed web deps) =="
fi

echo "== gate: PASS =="
