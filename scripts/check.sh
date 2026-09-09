#!/usr/bin/env bash
# The local gate: every leg the repository relies on, in one command.
#
#   scripts/check.sh             everything (browser journeys and the
#                                sibling nvim journey included)
#   scripts/check.sh --fast      skip the browser and nvim legs
#   scripts/check.sh --require-nvim
#                                fail when ../ambidiff-nvim is absent
#
# Missing prerequisites fail with the install command instead of silently
# skipping a leg. The committed-asset check builds into a temporary
# directory and compares; it never writes into the tree.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib.sh
. scripts/lib.sh

fast=0
require_nvim=0
for arg in "$@"; do
  case "$arg" in
    --fast) fast=1 ;;
    --require-nvim) require_nvim=1 ;;
    *) echo "usage: scripts/check.sh [--fast] [--require-nvim]" >&2; exit 2 ;;
  esac
done

leg() { printf '\n== %s\n' "$*"; }

leg "prerequisites"
require rustup "install from https://rustup.rs"
check_wasm_target
check_wasm_bindgen
require bun "install bun from https://bun.sh"
require node "install node (the wasm test runner and playwright need it)"
check_git_version
echo "rustc $(rustc --version | awk '{print $2}'), wasm-bindgen $(wasm-bindgen --version | awk '{print $2}'), bun $(bun --version), node $(node --version), git $(git --version | awk '{print $3}')"

leg "cargo fmt --check"
cargo fmt --all -- --check

leg "cargo clippy (deny warnings)"
cargo clippy --workspace --all-targets --locked -- -D warnings

leg "cargo test (workspace, includes doc tests)"
cargo test --workspace --locked

leg "wasm feature build (no native deps)"
cargo build -p ambidiff-core --target wasm32-unknown-unknown --no-default-features --features wasm --locked

leg "wasm tests (lib, conformance, projections)"
cargo test -p ambidiff-core --target wasm32-unknown-unknown --no-default-features --features wasm --locked \
    --lib --test conformance --test projections

leg "browser: install, wasm bindings, typecheck, unit tests"
cargo build -p ambidiff-core --target wasm32-unknown-unknown --release --no-default-features --features wasm --locked
wasm-bindgen target/wasm32-unknown-unknown/release/ambidiff_core.wasm --target web --out-dir web/pkg
(cd web && bun install --frozen-lockfile --silent && bun run typecheck && bun test src)

leg "committed browser assets are fresh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
scripts/build-web.sh --out "$tmp/dist" >/dev/null
if ! diff -rq "$tmp/dist" web/dist; then
  echo "web/dist is stale: run scripts/build-web.sh and commit the result" >&2
  exit 1
fi
# A second build must reproduce the first byte for byte, or the check
# above cannot be trusted; report which artefact drifted.
scripts/build-web.sh --out "$tmp/dist2" >/dev/null
for f in main.js ambidiff_core_bg.wasm; do
  cmp -s "$tmp/dist/$f" "$tmp/dist2/$f" || { echo "build is not deterministic: $f differs between two builds" >&2; exit 1; }
done
echo "web/dist matches a fresh deterministic build"

if [ "$fast" -eq 1 ]; then
  echo
  echo "check.sh --fast: all legs passed (browser journeys and nvim skipped)"
  exit 0
fi

leg "browser journeys (playwright)"
cargo build -p ambidiff --locked
(cd web && bunx playwright test)

leg "sibling nvim journey"
if [ -x ../ambidiff-nvim/tests/run.sh ]; then
  require nvim "install neovim"
  AMBIDIFF_BIN="$PWD/target/debug/ambidiff" ../ambidiff-nvim/tests/run.sh
elif [ "$require_nvim" -eq 1 ]; then
  echo "../ambidiff-nvim/tests/run.sh not found (required by --require-nvim)" >&2
  exit 1
else
  echo "../ambidiff-nvim not checked out; skipped (pass --require-nvim to fail instead)"
fi

echo
echo "check.sh: all legs passed"
