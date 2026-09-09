#!/usr/bin/env bash
# Build the browser frontend: wasm via cargo + wasm-bindgen (NOT wasm-pack:
# it does not forward feature flags), the TS painter bundled by bun, then
# the static assets. Output goes to web/dist (committed, embedded into the
# binary at compile time) or to --out DIR for an isolated comparison build.
# The build is deterministic: the same inputs produce byte-identical files,
# which scripts/check.sh relies on.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib.sh
. scripts/lib.sh

out="web/dist"
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    *) echo "usage: scripts/build-web.sh [--out DIR]" >&2; exit 2 ;;
  esac
done

check_wasm_target
check_wasm_bindgen
require bun "install bun from https://bun.sh"

echo "== wasm build"
cargo build -p ambidiff-core --target wasm32-unknown-unknown --release \
    --no-default-features --features wasm --locked
wasm-bindgen target/wasm32-unknown-unknown/release/ambidiff_core.wasm \
    --target web --out-dir web/pkg

echo "== bundle"
mkdir -p "$out"
(cd web && bun install --frozen-lockfile --silent)
bun build web/src/main.ts --outdir "$out" --minify --sourcemap=none

echo "== assets"
cp web/index.html web/styles.css "$out/"
cp web/pkg/ambidiff_core_bg.wasm "$out/"

echo "== done"
ls -la "$out/"
