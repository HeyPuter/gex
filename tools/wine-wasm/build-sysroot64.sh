#!/bin/sh
# W64's LP64 sysroot (docs/wine-native-x64.md §1.1): what Wine's Unix side for
# x64 programs builds against, for wcc64. musl with the gex `wasm64` arch (the
# x86-64 Linux syscall ABI through gex.syscall(ctx); fork: wt/musl-wasm64,
# branch w64/wasm64, from U's gex-wasm64), libgex (setjmp runtime, dlopen
# registry, x86-64 signal frames), compiler-rt's builtins and the host's x86
# UAPI headers (their 64-bit branches are x86-64's). Compiled for wasm64; wcc64
# lowers every program to wasm32 at link time.
#
#   tools/wine-wasm/build-sysroot64.sh OUT [MUSL_SRC]
#
# OUT/sysroot gets include/, lib/{libc.a,crt1.o,libgex.a,
# libclang_rt.builtins-wasm64.a} and binaryen/ (GEX_BINARYEN, or fetched).
set -eu
OUT=$(realpath -m "$1")
HERE=$(dirname "$(realpath "$0")")
MUSL=$(realpath "${2:-$HERE/../../../musl-w64}")
# compiler-rt's builtins sources: GEX_CRT_SRC, else an emsdk's
CRT=${GEX_CRT_SRC:-}
for c in "$HOME/emsdk" /usr/lib/emsdk; do
  [ -n "$CRT" ] || [ ! -d "$c/upstream/emscripten/system/lib/compiler-rt/lib/builtins" ] || CRT=$c/upstream/emscripten/system/lib/compiler-rt/lib/builtins
done
[ -n "$CRT" ] && [ -f "$CRT/udivti3.c" ] || { echo "build-sysroot64: no compiler-rt builtins sources (GEX_CRT_SRC)" >&2; exit 2; }
JOBS=${JOBS:-4}
. "$HERE/flags64.sh"
mkdir -p "$OUT/musl-build" "$OUT/crt-build" "$OUT/libgex-build" "$OUT/sysroot/lib"

cd "$OUT/musl-build"
if [ "$(cat .gex-flags 2>/dev/null)" != "$MUSL $WASM64_CFLAGS" ]; then
  rm -f config.mak
  [ -f Makefile ] && make -s clean >/dev/null 2>&1 || true
fi
if [ ! -f config.mak ]; then
  CC=clang CFLAGS="$WASM64_CFLAGS" AR=llvm-ar RANLIB=llvm-ranlib \
    "$MUSL/configure" --target=wasm64-unknown-linux-musl --prefix="$OUT/sysroot" \
    --disable-shared --enable-static >/dev/null
  printf '%s' "$MUSL $WASM64_CFLAGS" > .gex-flags
fi
if [ -f lib/libc.a ] && [ -n "$(find "$MUSL/arch" "$MUSL/include" "$MUSL/src/internal" -name '*.h' -newer lib/libc.a | head -1)" ]; then
  make -s clean
fi
make -s -j"$JOBS" install-headers install-libs >/dev/null

# Linux UAPI headers: the host's x86 set, whose non-i386 branches are x86-64's.
SR="$OUT/sysroot/include"
rm -rf "$SR/linux" "$SR/asm-generic" "$SR/asm"
cp -r /usr/include/linux /usr/include/asm-generic "$SR/"
cp -rL /usr/include/asm "$SR/asm"

# libgex: the wasm32 runtime's setjmp and dlopen (data-model neutral C) and
# the x86-64 signal-frame runner.
gobjs=
for f in "$HERE/libgex/setjmp.c" "$HERE/libgex/dl.c" "$HERE/libgex64/signal.c"; do
  o="$OUT/libgex-build/$(basename "$f").o"
  clang $WASM64_CFLAGS --sysroot="$OUT/sysroot" -isystem "$SR" -Wall -Wno-unused-command-line-argument -c "$f" -o "$o"
  gobjs="$gobjs $o"
done
rm -f "$OUT/sysroot/lib/libgex.a"
llvm-ar crs "$OUT/sysroot/lib/libgex.a" $gobjs

cd "$OUT/crt-build"
objs=
for f in "$CRT"/*.c; do
  b=$(basename "$f" .c)
  case $b in
    apple_versioning|clear_cache|enable_execute_stack|trampoline_setup|emutls|\
    gcc_personality_v0|os_version_check|atomic*|*xf*|*xc3|*bf2|crtbegin|crtend) continue ;;
  esac
  [ "$b.o" -nt "$f" ] || clang $WASM64_CFLAGS -nostdinc -isystem "$SR" -isystem "$(clang -print-resource-dir)/include" -Wno-everything -c "$f" -o "$b.o" &
  objs="$objs $b.o"
  [ $(jobs -p | wc -l) -ge "$JOBS" ] && wait -n 2>/dev/null || true
done
wait
rm -f "$OUT/sysroot/lib/libclang_rt.builtins-wasm64.a"
llvm-ar crs "$OUT/sysroot/lib/libclang_rt.builtins-wasm64.a" $objs

# binaryen 133 (--memory64-lowering of whole programs, exception handling)
BINARYEN_VERSION=133
if [ -n "${GEX_BINARYEN:-}" ]; then
  ln -sfn "$(realpath "$GEX_BINARYEN")" "$OUT/sysroot/binaryen"
else
  # fetched once per host into the cache, linked from every sysroot
  cache=${XDG_CACHE_HOME:-$HOME/.cache}/gex/binaryen-$BINARYEN_VERSION
  if [ ! -x "$cache/bin/wasm-opt" ]; then
    t=binaryen-version_$BINARYEN_VERSION
    mkdir -p "$(dirname "$cache")"
    tmp=$(mktemp -d "$(dirname "$cache")/.binaryen.XXXXXX")
    curl -sSfL "https://github.com/WebAssembly/binaryen/releases/download/version_$BINARYEN_VERSION/$t-x86_64-linux.tar.gz" | tar xz -C "$tmp"
    rm -rf "$cache"
    mv "$tmp/$t" "$cache"
    rm -rf "$tmp"
  fi
  rm -rf "$OUT/sysroot/binaryen"
  ln -sfn "$cache" "$OUT/sysroot/binaryen"
fi
echo "sysroot: $OUT/sysroot"
