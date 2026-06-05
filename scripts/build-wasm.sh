#!/usr/bin/env bash
# Build the cascade-wasm crate into a .wasm module and generate JS glue.
#
# Produces cascade_wasm.js and cascade_wasm_bg.wasm (plus .d.ts type stubs)
# in crates/cascade-wasm/pkg/.
#
# Idempotent: safe to re-run in CI or local dev.

set -euo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

# Use the rustup-managed cargo so the wasm32-unknown-unknown target is
# available.  Homebrew's standalone cargo does not carry rustup targets.
CARGO="${HOME}/.cargo/bin/cargo"
WASM_BINDGEN="${HOME}/.cargo/bin/wasm-bindgen"

TARGET_DIR="target/wasm32-unknown-unknown/release"
WASM_INPUT="${TARGET_DIR}/cascade_wasm.wasm"
OUT_DIR="crates/cascade-wasm/pkg"

echo "Building cascade-wasm (wasm32-unknown-unknown, release)..."
"$CARGO" build -p cascade-wasm --target wasm32-unknown-unknown --release

echo "Running wasm-bindgen (target web)..."
mkdir -p "$OUT_DIR"
"$WASM_BINDGEN" --target web --out-dir "$OUT_DIR" "$WASM_INPUT"

echo "WASM artefacts:"
ls -lh "$OUT_DIR"
echo "Done."
