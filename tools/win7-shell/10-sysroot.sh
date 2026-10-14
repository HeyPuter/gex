#!/bin/bash
# 10: the wasm32 sysroot Wine's Unix side and the W3 programs build against:
# musl-wasm (MUSL_SRC), libgex, compiler-rt's builtins, the icall plugin and
# binaryen (tools/wine-wasm/build-sysroot.sh). Output: $W7_OUT/sysroot.
# compiler-rt: GEX_CRT_SRC (its lib/builtins), or LLVM's 21.1.8 release
# (the one emsdk's copy is based on), downloaded.
. "$(dirname "$0")/lib.sh"; need MUSL_SRC
if [ -z "${GEX_CRT_SRC:-}" ]; then
  t=$(fetch https://github.com/llvm/llvm-project/releases/download/llvmorg-21.1.8/compiler-rt-21.1.8.src.tar.xz \
        dd54ae21aee1780fac59445b51ebff601ad016b31ac3a7de3b21126fd3ccb229)
  rm -rf "$W7_OUT/compiler-rt"; mkdir -p "$W7_OUT/compiler-rt"; tar -xf "$t" -C "$W7_OUT/compiler-rt" --strip-components=1
  export GEX_CRT_SRC=$W7_OUT/compiler-rt/lib/builtins
fi
step "sysroot from $MUSL_SRC"
JOBS=$JOBS "$GEX_SRC/tools/wine-wasm/build-sysroot.sh" "$W7_OUT/sysroot" "$MUSL_SRC" > "$W7_OUT/logs/sysroot.log" 2>&1 ||
  { tail -20 "$W7_OUT/logs/sysroot.log"; exit 1; }
ls "$GEX_WASM_SYSROOT/lib/libc.a" "$GEX_WASM_SYSROOT/lib/GexIcall.so" "$GEX_WASM_SYSROOT/binaryen/bin/wasm-opt" > /dev/null
step "done: $GEX_WASM_SYSROOT"
