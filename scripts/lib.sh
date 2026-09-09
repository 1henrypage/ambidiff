#!/usr/bin/env bash
# Shared toolchain resolution for every script in scripts/. Source it after
# `set -euo pipefail`; it puts the active rustup toolchain first on PATH,
# applies the macOS rust-lld library workaround, and provides prerequisite
# helpers. Portable: no machine-specific paths.

# rustup's proxies do not reliably beat Homebrew's rust on PATH here, so put
# the active toolchain's bin dir first. AMBIDIFF_TOOLCHAIN_BIN overrides.
if [ -n "${AMBIDIFF_TOOLCHAIN_BIN:-}" ]; then
  toolchain_bin="$AMBIDIFF_TOOLCHAIN_BIN"
elif command -v rustup >/dev/null 2>&1; then
  toolchain_bin="$(dirname "$(rustup which rustc)")"
else
  echo "missing rustup: install from https://rustup.rs (or set AMBIDIFF_TOOLCHAIN_BIN)" >&2
  exit 2
fi
export PATH="$toolchain_bin:$HOME/.cargo/bin:$HOME/.local/share/cargo/bin:$PATH"

# rust-lld on macOS looks for libLLVM.dylib beside itself; some installs keep
# it in the sysroot lib dir instead.
sysroot="$(rustc --print sysroot)"
if [ "$(uname -s)" = Darwin ] && [ -f "$sysroot/lib/libLLVM.dylib" ]; then
  export DYLD_FALLBACK_LIBRARY_PATH="$sysroot/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
fi
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner

# require NAME HINT: fail with an install hint when NAME is not on PATH.
require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing $1: $2" >&2
    exit 2
  }
}

# The wasm-bindgen CLI must match the wasm-bindgen crate in Cargo.lock, or
# the generated bindings do not load.
check_wasm_bindgen() {
  require wasm-bindgen "cargo install wasm-bindgen-cli --version <locked version>"
  local locked installed
  locked="$(awk '/^name = "wasm-bindgen"$/ { getline; sub(/^version = /, ""); gsub(/"/, ""); print; exit }' Cargo.lock)"
  installed="$(wasm-bindgen --version | awk '{ print $2 }')"
  if [ "$locked" != "$installed" ]; then
    echo "wasm-bindgen-cli $installed does not match Cargo.lock ($locked): cargo install wasm-bindgen-cli --version $locked --force" >&2
    exit 2
  fi
}

check_wasm_target() {
  rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown || {
    echo "missing wasm32 target: rustup target add wasm32-unknown-unknown" >&2
    exit 2
  }
}

# Git 2.31+ (for --path-format=absolute and --end-of-options).
check_git_version() {
  require git "install git 2.31 or newer"
  local v
  v="$(git --version | awk '{ print $3 }')"
  local major minor
  major="${v%%.*}"
  minor="${v#*.}"
  minor="${minor%%.*}"
  if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 31 ]; }; then
    echo "git $v is too old: 2.31 or newer is required" >&2
    exit 2
  fi
}
