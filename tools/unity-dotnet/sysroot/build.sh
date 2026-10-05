#!/bin/sh
# The wasm64 sysroot the Mono provider builds against (docs/unity-dotnet.md):
# musl with the gex `wasm64` arch (fork: wt/musl-wasm64, branch gex-wasm64,
# from stream W's gex-wasm32) and compiler-rt's builtins, with the system
# clang. Syscalls are the x86-64 Linux ABI through gex.syscall(ctx).
#
#   tools/unity-dotnet/sysroot/build.sh OUT [MUSL_SRC]
#
# OUT/sysroot gets include/ (musl plus the host's Linux UAPI headers),
# lib/libc.a and lib/libclang_rt.builtins-wasm64.a. Everything is -fPIC,
# shared-memory code: a provider is a position-independent module
# instantiated once per thread.
set -eu
OUT=$(realpath -m "$1")
MUSL=$(realpath "${2:-$(dirname "$0")/../../../../musl-wasm64}")
CRT=${GEX_CRT_SRC:-/usr/lib/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins}
JOBS=${JOBS:-4}
. "$(dirname "$0")/flags.sh"
mkdir -p "$OUT/musl-build" "$OUT/crt-build" "$OUT/sysroot/lib"

cd "$OUT/musl-build"
if [ ! -f config.mak ]; then
  CC=clang CFLAGS="$WASM64_CFLAGS" AR=llvm-ar RANLIB=llvm-ranlib \
    "$MUSL/configure" --target=wasm64-unknown-linux-musl --prefix="$OUT/sysroot" \
    --disable-shared --enable-static >/dev/null
fi
make -s -j"$JOBS" install-headers install-libs >/dev/null

# Linux UAPI headers: the host's x86 set, whose non-i386 branches are
# x86-64's, which is the ABI gex-kernel serves here.
SR="$OUT/sysroot/include"
rm -rf "$SR/linux" "$SR/asm-generic" "$SR/asm"
cp -r /usr/include/linux /usr/include/asm-generic "$SR/"
cp -r /usr/include/asm "$SR/asm"

cd "$OUT/crt-build"
objs=
for f in "$CRT"/*.c; do
  b=$(basename "$f" .c)
  case $b in
    apple_versioning|clear_cache|enable_execute_stack|trampoline_setup|emutls|\
    gcc_personality_v0|os_version_check|atomic*|*xf*|*xc3|*bf2|crtbegin|crtend) continue ;;
  esac
  [ "$b.o" -nt "$f" ] || clang $WASM64_CFLAGS -nostdinc -isystem "$OUT/sysroot/include" -isystem "$(clang -print-resource-dir)/include" -Wno-everything -c "$f" -o "$b.o" &
  objs="$objs $b.o"
  [ $(jobs -p | wc -l) -ge "$JOBS" ] && wait -n 2>/dev/null || true
done
wait
rm -f "$OUT/sysroot/lib/libclang_rt.builtins-wasm64.a"
llvm-ar crs "$OUT/sysroot/lib/libclang_rt.builtins-wasm64.a" $objs
echo "sysroot: $OUT/sysroot"
