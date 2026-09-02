#!/bin/sh
# Execute one Gallium semantic corpus against Mesa softpipe and real WebGPU.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
SOFTPIPE=$OUT/build/src/gallium/targets/gex-wasm/gex-gallium-softpipe-conformance.js

"$ROOT/web/build-mesa-wasm.sh" "$OUT"
ninja -C "$OUT/build" \
  src/gallium/targets/gex-wasm/gex-gallium-softpipe-conformance.js
node "$SOFTPIPE"
GEX_MESA_SKIP_BUILD=1 "$ROOT/tests/mesa-wgpu-browser.sh"
