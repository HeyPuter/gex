#!/bin/sh
# The LP64 libc item 3's libraries build against: musl
# with the gex `wasm64a64` arch (branch gex-wasm64-a64, from gex-wasm64):
# wasm64 code with aarch64's LP64 layout and
# kernel ABI, aarch64 Linux syscalls through gex.syscall(ctx) on the
# calling guest thread's context; and compiler-rt's builtins (long double
# is IEEE quad, as on aarch64). The libraries are then lowered to the
# guest's 32-bit memory (build.sh).
#
#   sysroot.sh OUT [MUSL_SRC]   (MUSL_SRC, argument or environment: required)
#
# OUT/sysroot gets include/, lib/libc.a and lib/libclang_rt.builtins-wasm64.a.
set -eu
OUT=$(realpath -m "$1")
MUSL=$(realpath "${2:-${MUSL_SRC:?musl, branch gex-wasm64-a64}}")
CRT=${GEX_CRT_SRC:-/usr/lib/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins}
JOBS=${JOBS:-8}
. "$(dirname "$0")/flags.sh"
mkdir -p "$OUT/musl-build" "$OUT/crt-build" "$OUT/sysroot/lib"

cd "$OUT/musl-build"
if [ ! -f config.mak ]; then
  CC=clang CFLAGS="$A64W_CFLAGS" AR=llvm-ar RANLIB=llvm-ranlib \
    "$MUSL/configure" --target=wasm64a64-unknown-linux-musl --prefix="$OUT/sysroot" \
    --disable-shared --enable-static >/dev/null
fi
make -s -j"$JOBS" install-headers install-libs >/dev/null

cd "$OUT/crt-build"
# The comparisons (__lttf2 and the rest) return int, as clang's wasm64 calls
# expect (compiler-rt types them long for every other LP64 target but
# aarch64): a patched copy of their sources.
mkdir -p cmp
sed 's/^typedef long CMP_RESULT;/typedef int CMP_RESULT;/' "$CRT/fp_compare_impl.inc" > cmp/fp_compare_impl.inc
cp "$CRT"/compare?f2.c cmp/
objs=
for f in cmp/compare?f2.c $(ls "$CRT"/*.c | grep -v '/compare.f2\.c$'); do
  b=$(basename "$f" .c)
  case $b in
    apple_versioning|clear_cache|enable_execute_stack|trampoline_setup|emutls|\
    gcc_personality_v0|os_version_check|atomic*|*xf*|*xc3|*bf2|crtbegin|crtend) continue ;;
  esac
  [ "$b.o" -nt "$f" ] || clang $A64W_CFLAGS -nostdinc -isystem "$OUT/sysroot/include" -isystem "$(clang -print-resource-dir)/include" -Wno-everything -I"$CRT" -c "$f" -o "$b.o" &
  objs="$objs $b.o"
  [ $(jobs -p | wc -l) -ge "$JOBS" ] && wait -n 2>/dev/null || true
done
wait
rm -f "$OUT/sysroot/lib/libclang_rt.builtins-wasm64.a"
llvm-ar crs "$OUT/sysroot/lib/libclang_rt.builtins-wasm64.a" $objs
echo "sysroot: $OUT/sysroot"
