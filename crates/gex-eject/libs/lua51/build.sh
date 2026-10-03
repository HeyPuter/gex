#!/bin/sh
# Build the Lua provider (lua51.dll's exports on PUC Lua 5.1.5, natively in
# wasm): a position-independent module for gex's stateful providers
# (docs/eject-pe.md §5).
#
#   build.sh OUT.wasm GEN_DIR
#
# GEN_DIR holds gex_abi.h and provider.h (the project's gen/). Needs
# emscripten (emcc; its musl and compiler-rt sources supply strtod, printf,
# the math library and dlmalloc) and wasm-ld. EMCC and WASM_LD override.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$1
GEN=$2
EMCC=${EMCC:-emcc}
WASM_LD=${WASM_LD:-wasm-ld}
EM=$(dirname "$(readlink -f "$(command -v "$EMCC")")")
MUSL=$EM/system/lib/libc/musl
RT=$EM/system/lib/compiler-rt/lib/builtins
OBJ=$(dirname "$OUT")/lua51-obj
rm -rf "$OBJ" && mkdir -p "$OBJ"
FLAGS="-O2 -fPIC -pthread -msimd128 -fvisibility=hidden -fno-strict-aliasing -DGEX_PROVIDER"
cc() { "$EMCC" -c $FLAGS "$@"; }

for f in "$HERE"/lua/*.c; do
    cc -I"$HERE/lua" "$f" -o "$OBJ/lua_$(basename "$f" .c).o"
done
for f in lua51 lua51_libs; do
    cc -I"$HERE" -I"$GEN" "$HERE/$f.c" -o "$OBJ/$f.o"
done
cc -I"$MUSL/src/internal" -I"$MUSL/arch/emscripten" -I"$MUSL/arch/generic" -I"$MUSL/src/include" "$HERE/gexlibc.c" -o "$OBJ/gexlibc.o"
# musl: strings, math, strtod, printf (for string FILEs only).
for f in string/memchr string/memcmp string/strcat string/strchr string/strchrnul string/strcmp string/strcoll \
    string/strcspn string/strlen string/strncat string/strncmp string/strncpy string/strnlen string/strpbrk \
    string/strrchr string/strspn string/strstr string/strcpy string/stpcpy string/stpncpy string/memrchr \
    math/acos math/asin math/atan math/atan2 math/cos math/cosh math/exp math/exp_data math/fmod math/frexp \
    math/ldexp math/log math/log_data math/log10 math/modf math/pow math/pow_data math/sin math/sinh \
    math/tan math/tanh math/__cos math/__sin math/__tan math/__rem_pio2 math/__rem_pio2_large math/__expo2 \
    math/scalbn math/scalbnl math/fmodl math/expm1 math/frexpl math/copysignl math/fabsl \
    math/__math_divzero math/__math_invalid math/__math_oflow math/__math_uflow math/__math_xflow \
    math/__fpclassifyl math/__signbitl \
    stdio/vsnprintf stdio/vfprintf stdio/snprintf stdio/__towrite stdio/fwrite stdio/vsprintf \
    stdio/fprintf stdio/printf stdio/fputs stdio/fputc stdio/putc stdio/putchar stdio/puts stdio/__overflow \
    stdio/fgetc stdio/getc stdio/ungetc stdio/fgets stdio/fread stdio/__uflow stdio/__toread stdio/feof \
    stdio/ferror stdio/clearerr stdio/fseek stdio/ftell stdio/setvbuf stdio/getc_unlocked \
    stdio/fwrite_unlocked stdio/fputs_unlocked \
    internal/floatscan internal/shgetc internal/intscan stdlib/strtod stdlib/strtol; do
    [ -f "$MUSL/src/$f.c" ] || continue
    cc -I"$MUSL/src/internal" -I"$MUSL/arch/emscripten" -I"$MUSL/arch/generic" -I"$MUSL/src/include" -I"$EM/system/lib/pthread" \
        "$MUSL/src/$f.c" -o "$OBJ/musl_$(echo "$f" | tr / _).o"
done
# Soft float for musl's long double (128-bit on wasm32).
for f in addtf3 subtf3 multf3 divtf3 extenddftf2 extendsftf2 trunctfdf2 trunctfsf2 floatsitf floatunsitf \
    floatditf floatunditf fixtfsi fixunstfsi fixtfdi comparetf2 multi3 ashlti3 lshrti3 udivti3 umodti3 \
    udivmodti4 clzti2 fp_mode; do
    cc "$RT/$f.c" -o "$OBJ/rt_$f.o"
done
# dlmalloc, over gexlibc's sbrk; spin locks, no mmap.
cc -DUSE_LOCKS=1 -DUSE_SPIN_LOCKS=1 -DHAVE_MMAP=0 -DMORECORE_CONTIGUOUS=0 -Wno-macro-redefined \
    -U__EMSCRIPTEN_PTHREADS__ "$EM/system/lib/dlmalloc.c" -o "$OBJ/dlmalloc.o"

"$WASM_LD" --experimental-pic -shared --no-entry --import-memory --shared-memory --max-memory=4294901760 \
    --allow-undefined -O2 -o "$OUT" "$OBJ"/*.o
