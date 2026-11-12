#!/bin/bash
# The conventions' native library (aconv.c) for test/libs/native-libs: as an
# LP64 wasm library that replaces libaconv.so.1 (toolchain/native-libs: its
# runtime, gen.py's adapters for the exports and the GexA64Icall pass's
# entry thunks for the pointers conv_table() hands out), as the arm64
# libaconv.so.1, and the arm64 program aconvt against it (rpath /t).
#
#   build-aconv.sh OUT   (GEXCC_PREFIX; A64_SYSROOT optional, as lib.sh)
set -eu
F=$(cd "$(dirname "$0")" && pwd); H=$(cd "$F/../../../../toolchain/native-libs" && pwd)
O=$(realpath -m "$1"); mkdir -p "$O/aconv"
OPT=${GEXCC_PREFIX:?}/sysroot/binaryen/bin/wasm-opt
. "$H/lib.sh"
lib_setup "$H" "$O/aconv"
"$H/a64icall/build.sh" "$O/aconv"
clang $CF -c "$H/gexa.c" -o "$O/aconv/gexa.o" &
GEXA_ICALL_STATS=1 clang $CF -fpass-plugin="$O/aconv/GexA64Icall.so" -I"$F" -c "$F/aconv.c" -o "$O/aconv/aconv.o" &
python3 "$H/gen.py" conv - "$F/aconv.h" > "$O/aconv/adapters.c"
clang $CF -I"$H" -I"$F" -c "$O/aconv/adapters.c" -o "$O/aconv/adapters.o" &
wait
lib_link "$O" libaconv.so.1 "$O"/aconv/{aconv,adapters,gexa}.o
clang --target=aarch64-linux-gnu -O2 -fPIC -ffreestanding -nostdlib -shared -fuse-ld=lld -Wl,-soname,libaconv.so.1 \
  "$F/aconv.c" -o "$O/libaconv.so.1"
clang --target=aarch64-linux-gnu -O2 -ffreestanding -fno-builtin -fPIE -c "$F/aconvt.c" -o "$O/aconv/aconvt.o"
ld.lld -pie -dynamic-linker /lib/ld-linux-aarch64.so.1 -e _start -rpath /t -o "$O/aconvt" "$O/aconv/aconvt.o" "$O/libaconv.so.1"
