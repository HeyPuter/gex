#!/bin/sh
# W0(c) benchmark module: gex-cgen's unit flags (crates/gex-cgen/src/toolchain.rs).
set -e
cd "$(dirname "$0")"
OUT=${1:-.}
clang --target=wasm32 -O2 -mtail-call -msimd128 -matomics -mbulk-memory -mnontrapping-fptoint -msign-ext \
  -fno-strict-aliasing -ffreestanding -nostdlib -c boundary.c -o "$OUT/boundary.o"
wasm-ld --no-entry --export-dynamic --import-memory --shared-memory --max-memory=67108864 \
  --global-base=4194304 -z stack-size=65536 --export-table \
  --export=setup --export=bench_native_direct --export=bench_native_indirect --export=bench_native_icall --export=bench_native_icall2 --export=bench_native_to_guest2 --export=bench_native_to_guest --export=bench_guest_to_guest --export=bench_guest_to_native  \
  "$OUT/boundary.o" -o "$OUT/boundary.wasm"
