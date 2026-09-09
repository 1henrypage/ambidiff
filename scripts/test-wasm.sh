#!/usr/bin/env bash
# Run the conformance and projection corpora plus the core's unit tests
# under wasm32 (the drift firewall's second leg).
#
# Drives cargo + wasm-bindgen-test-runner directly (wasm-pack does not
# forward feature flags). Requires: the wasm32-unknown-unknown target,
# `cargo install wasm-bindgen-cli` at the locked version, and node.
# Fixture embeds stay fresh through crates/core/build.rs (rerun-if-changed
# on the fixture directories), so nothing is touched here.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib.sh
. scripts/lib.sh
check_wasm_target
check_wasm_bindgen
require node "install node (the wasm test runner needs it)"

cargo test -p ambidiff-core \
    --target wasm32-unknown-unknown \
    --no-default-features --features wasm \
    --lib --test conformance --test projections "$@"
