#!/bin/sh
# The wasm32 sysroot Wine's Unix side builds against (docs/wine-native.md §3.2):
# musl with the gex `wasm32` arch (fork: wt/musl-wasm, branch gex-wasm32) and
# compiler-rt's builtins, both built with the system clang.
#
#   tools/wine-wasm/build-sysroot.sh OUT [MUSL_SRC]
#
# OUT/sysroot gets include/, lib/libc.a, lib/crt1.o and
# lib/libclang_rt.builtins-wasm32.a. Everything is -fPIC, shared-memory
# (atomics) code: a program is a position-independent module instantiated
# once per thread.
set -eu
OUT=$(realpath -m "$1")
MUSL=$(realpath "${2:-$(dirname "$0")/../../../musl-wasm}")
CRT=${GEX_CRT_SRC:-/usr/lib/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins}
JOBS=${JOBS:-4}
. "$(dirname "$0")/flags.sh"
mkdir -p "$OUT/musl-build" "$OUT/crt-build" "$OUT/sysroot/lib"

cd "$OUT/musl-build"
if [ ! -f config.mak ]; then
  CC=clang CFLAGS="$WASM_CFLAGS" AR=llvm-ar RANLIB=llvm-ranlib \
    "$MUSL/configure" --target=wasm32-unknown-linux-musl --prefix="$OUT/sysroot" \
    --disable-shared --enable-static >/dev/null
fi
make -s -j"$JOBS" install-headers install-libs >/dev/null

cd "$OUT/crt-build"
objs=
for f in "$CRT"/*.c; do
  b=$(basename "$f" .c)
  case $b in
    apple_versioning|clear_cache|enable_execute_stack|trampoline_setup|emutls|\
    gcc_personality_v0|os_version_check|atomic*|*xf*|*xc3|*bf2|crtbegin|crtend) continue ;;
  esac
  [ "$b.o" -nt "$f" ] || clang $WASM_CFLAGS -Wno-everything -c "$f" -o "$b.o" &
  objs="$objs $b.o"
  [ $(jobs -p | wc -l) -ge "$JOBS" ] && wait -n 2>/dev/null || true
done
wait
rm -f "$OUT/sysroot/lib/libclang_rt.builtins-wasm32.a"
llvm-ar crs "$OUT/sysroot/lib/libclang_rt.builtins-wasm32.a" $objs
echo "sysroot: $OUT/sysroot"
