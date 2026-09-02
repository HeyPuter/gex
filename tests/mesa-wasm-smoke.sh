#!/bin/sh
# Fail closed unless Mesa's public desktop GL API reaches Gallium under Wasm
# and returns the exact pixel written by the test framebuffer clear.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
SMOKE=$OUT/build/src/gallium/targets/gex-wasm/gex-mesa-smoke.js

"$ROOT/web/build-mesa-wasm.sh" "$OUT"
RESULT=$(node "$SMOKE")
printf '%s\n' "$RESULT"

printf '%s\n' "$RESULT" |
    grep -F 'PASS gex-mesa-wasm version=' >/dev/null
printf '%s\n' "$RESULT" |
    grep -F 'pixel=64,128,191,255' >/dev/null
