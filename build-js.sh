#!/bin/bash
# Build the engine WITH the secret yousj-js integration.
#
# Public builds must NOT use this script (public clones lack js-engine/;
# plain `cargo build --release` in engine/ stays clean by design).
#
# Two steps, because yousj-js is wired via RUSTFLAGS --extern rather than a
# cargo dependency (an optional path dep would break public builds: cargo
# fails to resolve the manifest when the directory is absent, even with the
# feature off).
set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

echo "== step 1: yousj-js -> rlib =="
cargo build --release --manifest-path "$ROOT/js-engine/Cargo.toml" 2>&1 | tail -1
RLIB="$ROOT/js-engine/target/release/libyousj_js.rlib"
[ -f "$RLIB" ] || { echo "FATAL: rlib not found at $RLIB"; exit 1; }

echo "== step 2: engine cdylib with --features js =="
cd "$ROOT/engine"
RUSTFLAGS="--extern yousj_js=$RLIB" cargo build --release --features js 2>&1 | tail -2
echo "OK: $ROOT/engine/target/release/libyousj_engine.so"
