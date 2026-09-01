#!/bin/sh
# Instrument Mesa plus the WebGPU Gallium driver, then execute the same semantic
# corpus against softpipe and a real browser WebGPU device.  This is a memory-
# safety gate, not a rendering benchmark and not an STK smoke run.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_SANITIZE_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm-sanitize}
SOFTPIPE=$OUT/build/src/gallium/targets/gex-wasm/gex-gallium-softpipe-conformance.js

GEX_MESA_SANITIZE=1 "$ROOT/web/build-mesa-wasm.sh" "$OUT"
ninja -C "$OUT/build" \
  src/gallium/targets/gex-wasm/gex-gallium-softpipe-conformance.js

# Leak checking is not useful for Emscripten's process-lifetime Mesa singletons;
# all other ASan and UBSan findings are fatal and retain symbolized stacks.
ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=0:halt_on_error=1} \
UBSAN_OPTIONS=${UBSAN_OPTIONS:-halt_on_error=1:print_stacktrace=1} \
  node "$SOFTPIPE"

GEX_MESA_WASM_OUT="$OUT" \
GEX_MESA_SKIP_BUILD=1 \
GEX_WGPU_TIMEOUT_MS=${GEX_WGPU_TIMEOUT_MS:-900000} \
  "$ROOT/tests/mesa-wgpu-browser.sh"

echo "PASS gex-wgpu-gallium asan-ubsan softpipe-browser"
