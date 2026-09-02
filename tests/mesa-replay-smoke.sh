#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
REPLAY=$OUT/build/src/gallium/targets/gex-wasm/gex-mesa-replay.js

"$ROOT/web/build-mesa-wasm.sh" "$OUT"
node "$ROOT/tests/mesa-replay-smoke.mjs" "$REPLAY"
