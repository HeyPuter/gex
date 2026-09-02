#!/bin/sh
# Run the Mesa Gallium/WebGPU clear+readback test in the persistent CDP browser.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
BUILD=$OUT/build/src/gallium/targets/gex-wasm
PUBLIC=$ROOT/web/public/gex
DIST=$ROOT/web/dist/gex

if [ "${GEX_MESA_SKIP_BUILD:-0}" != 1 ]; then
  "$ROOT/web/build-mesa-wasm.sh" "$OUT"
fi
mkdir -p "$PUBLIC" "$DIST"
cp "$BUILD/gex-wgpu-smoke.js" "$PUBLIC/gex-wgpu-smoke.js"
cp "$BUILD/gex-wgpu-smoke.wasm" "$PUBLIC/gex-wgpu-smoke.wasm"
cp "$BUILD/gex-wgpu-smoke.js" "$DIST/gex-wgpu-smoke.js"
cp "$BUILD/gex-wgpu-smoke.wasm" "$DIST/gex-wgpu-smoke.wasm"
cp "$ROOT/web/public/mesa-wgpu-smoke.html" "$ROOT/web/dist/mesa-wgpu-smoke.html"

GEX_CHROME_PORT=${GEX_CHROME_PORT:-9335} \
GEX_SERVE_PORT=${GEX_SERVE_PORT:-8091} \
node "$ROOT/tests/mesa-wgpu-browser.mjs"
