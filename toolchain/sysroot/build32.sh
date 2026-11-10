#!/bin/sh
# The wasm32 sysroot Wine's Unix side builds against:
# musl with the gex `wasm32` arch (the fork toolchain/pins names) and
# compiler-rt's builtins, both built with the system clang.
#
#   toolchain/sysroot/build32.sh OUT [MUSL_SRC]
#
# OUT/sysroot gets include/, lib/libc.a, lib/crt1.o and
# lib/libclang_rt.builtins-wasm32.a. Everything is -fPIC, shared-memory
# (atomics) code: a program is a position-independent module instantiated
# once per thread.
set -eu
OUT=$(realpath -m "$1")
# One build of a sysroot at a time: two at once replace each other's
# compiler-rt archive and binaryen install half way through.
mkdir -p "$(dirname "$OUT")"
exec 9>"$OUT.lock"
flock 9
HERE=$(dirname "$(realpath "$0")")
MUSL=$(realpath "${2:-$HERE/../../../musl-wasm}")
CRT=${GEX_CRT_SRC:-/usr/lib/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins}
JOBS=${JOBS:-4}
. "$HERE/flags.sh"
# GEX_SNAPSHOT=1: the snapshot flavour, for pages that save and restore a
# session (the libc's GEX_SNAPSHOT hooks and libgex-snap/: threads stopped
# at a call that can block and resumed by asyncify). wcc sees the
# lib/gex-snapshot it leaves and instruments what reaches gex.syscall. Its
# libc is asyncify's too, so it takes the legacy exception instructions.
if [ -n "${GEX_SNAPSHOT:-}" ]; then
  GEX_WASM_LEGACY_EH=1
  GEX_WASM_EXTRA_CFLAGS="${GEX_WASM_EXTRA_CFLAGS:+$GEX_WASM_EXTRA_CFLAGS }-DGEX_SNAPSHOT"
fi
# GEX_WASM_LEGACY_EH=1 (gexcc): the libc's setjmp lowers onto the legacy
# exception instructions, as programs' does (wcc), so binaryen's asyncify
# can instrument a libc function a fork can reach (timer_create's thread,
# which setjmps); the link translates them to the standard ones.
if [ -n "${GEX_WASM_LEGACY_EH:-}" ]; then
  WASM_CFLAGS=$(printf '%s' "$WASM_CFLAGS" | sed 's/-wasm-use-legacy-eh=false/-wasm-use-legacy-eh=true/')
fi
# GEX_WASM_EXTRA_CFLAGS (gexcc: -fvisibility=default, so the same objects
# make libc.so): appended to every compile here.
WASM_CFLAGS="$WASM_CFLAGS${GEX_WASM_EXTRA_CFLAGS:+ $GEX_WASM_EXTRA_CFLAGS}"
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

# libgex: what gex's wasm32 programs need beyond the libc and is gex's
# protocol rather than musl's: the setjmp
# runtime, dlopen over the linked registry, signal frames, vfork. wcc links
# it before libc.
mkdir -p "$OUT/libgex-build"
gobjs=
for f in "$HERE"/libgex/*.c "$HERE"/libgex/*.s; do
  o="$OUT/libgex-build/$(basename "$f").o"
  clang $WASM_CFLAGS --sysroot="$OUT/sysroot" -Wall -Wno-unused-command-line-argument -c "$f" -o "$o"
  gobjs="$gobjs $o"
done
rm -f "$OUT/sysroot/lib/libgex.a" "$OUT/sysroot/lib/libgex-snap.a"
llvm-ar crs "$OUT/sysroot/lib/libgex.a" $gobjs
# The snapshot flavour's part (libgex-snap/) only in its sysroot, an archive
# of its own that the link takes last: its data and table slots come after
# the default build's, which keep their places.
if [ -n "${GEX_SNAPSHOT:-}" ]; then
  sobjs=
  for f in "$HERE/libgex-snap/call.c" "$HERE/libgex-snap/drive32.c"; do
    o="$OUT/libgex-build/$(basename "$f").o"
    clang $WASM_CFLAGS --sysroot="$OUT/sysroot" -Wall -Wno-unused-command-line-argument -c "$f" -o "$o"
    sobjs="$sobjs $o"
  done
  llvm-ar crs "$OUT/sysroot/lib/libgex-snap.a" $sobjs
fi

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
# objects of other flags are built again (a build directory copied from one
# made without gexcc's -fvisibility=default kept its hidden builtins, and
# libc.so then exported none of them)
if [ "$(cat .gex-flags 2>/dev/null)" != "$CRT $WASM_CFLAGS" ]; then
  rm -f ./*.o
  printf '%s' "$CRT $WASM_CFLAGS" > .gex-flags
fi
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
if [ -n "${GEX_SNAPSHOT:-}" ]; then touch "$OUT/sysroot/lib/gex-snapshot"; else rm -f "$OUT/sysroot/lib/gex-snapshot"; fi
echo "sysroot: $OUT/sysroot"
