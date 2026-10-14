#!/bin/bash
# 10: the wasm32 sysroot Wine's Unix side and the W3 programs build against:
# musl-wasm (MUSL_SRC), libgex, compiler-rt's builtins, the icall plugin and
# binaryen (tools/wine-wasm/build-sysroot.sh). Output: $W7_OUT/sysroot.
. "$(dirname "$0")/lib.sh"; need MUSL_SRC
step "sysroot from $MUSL_SRC"
JOBS=$JOBS "$GEX_SRC/tools/wine-wasm/build-sysroot.sh" "$W7_OUT/sysroot" "$MUSL_SRC" > "$W7_OUT/logs/sysroot.log" 2>&1 ||
  { tail -20 "$W7_OUT/logs/sysroot.log"; exit 1; }
ls "$GEX_WASM_SYSROOT/lib/libc.a" "$GEX_WASM_SYSROOT/lib/GexIcall.so" "$GEX_WASM_SYSROOT/binaryen/bin/wasm-opt" > /dev/null
step "done: $GEX_WASM_SYSROOT"
