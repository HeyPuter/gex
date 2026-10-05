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
HERE=$(dirname "$(realpath "$0")")
MUSL=$(realpath "${2:-$HERE/../../../musl-wasm}")
CRT=${GEX_CRT_SRC:-/usr/lib/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins}
JOBS=${JOBS:-4}
. "$HERE/flags.sh"
mkdir -p "$OUT/musl-build" "$OUT/crt-build" "$OUT/sysroot/lib"

cd "$OUT/musl-build"
# reconfigure when the flags or the musl tree changed (configure bakes
# both in: the Makefile is a link into the source)
if [ "$(cat .gex-flags 2>/dev/null)" != "$MUSL $WASM_CFLAGS" ]; then
  rm -f config.mak
  [ -f Makefile ] && make -s clean >/dev/null 2>&1 || true
fi
if [ ! -f config.mak ]; then
  CC=clang CFLAGS="$WASM_CFLAGS" AR=llvm-ar RANLIB=llvm-ranlib \
    "$MUSL/configure" --target=wasm32-unknown-linux-musl --prefix="$OUT/sysroot" \
    --disable-shared --enable-static >/dev/null
  printf '%s' "$MUSL $WASM_CFLAGS" > .gex-flags
fi
# musl's makefile tracks no header dependencies: a changed header (the
# wasm32 syscall stub is one) means rebuilding everything.
if [ -f lib/libc.a ] && [ -n "$(find "$MUSL/arch" "$MUSL/include" "$MUSL/src/internal" -name '*.h' -newer lib/libc.a | head -1)" ]; then
  make -s clean
fi
make -s -j"$JOBS" install-headers install-libs >/dev/null

# Linux UAPI headers (Wine's Unix side includes linux/*.h). asm/ is the
# host's x86 set taken down its i386 branches: gex-kernel serves the i386
# ABI. swab.h keeps its generic C (its i386 branch is inline asm).
SR="$OUT/sysroot/include"
rm -rf "$SR/linux" "$SR/asm-generic" "$SR/asm"
cp -r /usr/include/linux /usr/include/asm-generic "$SR/"
cp -rL /usr/include/asm "$SR/asm"
for h in $(grep -l __i386__ "$SR"/asm/*.h); do
  case $h in */swab.h) continue ;; esac
  sed -i -e 's/defined(__i386__)/(defined(__i386__) || defined(__wasm32__))/g' \
         -e 's/^\(#[ ]*\)ifdef __i386__/\1if defined(__i386__) || defined(__wasm32__)/' \
         -e 's/^\(#[ ]*\)ifndef __i386__/\1if !defined(__i386__) \&\& !defined(__wasm32__)/' "$h"
done

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
# The ICALL pass (wcc applies it to Wine's PE-side code, -D_WIN32).
"$HERE/icall/build.sh" "$OUT/sysroot/lib"
# binaryen, for wcc's asyncify of programs that fork (wasm-opt >= 117 for
# exception handling): GEX_BINARYEN names an existing install.
BINARYEN_VERSION=133
if [ -n "${GEX_BINARYEN:-}" ]; then
  ln -sfn "$(realpath "$GEX_BINARYEN")" "$OUT/sysroot/binaryen"
elif [ ! -x "$OUT/sysroot/binaryen/bin/wasm-opt" ]; then
  t=binaryen-version_$BINARYEN_VERSION
  curl -sSL "https://github.com/WebAssembly/binaryen/releases/download/version_$BINARYEN_VERSION/$t-x86_64-linux.tar.gz" | tar xz -C "$OUT"
  rm -rf "$OUT/sysroot/binaryen"
  mv "$OUT/$t" "$OUT/sysroot/binaryen"
fi
echo "sysroot: $OUT/sysroot"
