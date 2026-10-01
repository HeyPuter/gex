#!/bin/sh
# Build a static powerpc-linux-musl sysroot with clang, for gex's PowerPC
# (32-bit, big-endian) guest: musl's libc.a and crt files, and compiler-rt's
# builtins (64-bit division, float conversions: what libgcc would give).
#
# zig can't provide libc for powerpc-linux-musl, so the guest toolchain is
# clang + lld against this sysroot:
#
#   tools/ppc/cc.sh -O2 hello.c -o hello      (see cc.sh)
#
# The baseline CPU is `ppc` (no AltiVec, no SPE): gex fails loudly on vector
# instructions, and the script checks libc.a has none.
#
# Usage: tools/ppc/build-sysroot.sh [SYSROOT]   (default: ../ppc-sysroot beside
# the worktree, i.e. outside git). Needs clang, llvm-ar, llvm-ranlib, curl.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
SYSROOT=${1:-$(dirname "$root")/ppc-sysroot}
MUSL_VER=1.2.5
MUSL_SHA=a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
CRT_VER=21.1.8
TARGET=powerpc-linux-musl
CPU=${PPC_CPU:-ppc}
JOBS=${JOBS:-4}
CFLAGS_T="--target=$TARGET -mcpu=$CPU -mno-altivec"

mkdir -p "$SYSROOT/src"
cd "$SYSROOT/src"
[ -f musl-$MUSL_VER.tar.gz ] || curl -sSLO https://musl.libc.org/releases/musl-$MUSL_VER.tar.gz
echo "$MUSL_SHA  musl-$MUSL_VER.tar.gz" | sha256sum -c -
[ -f compiler-rt-$CRT_VER.src.tar.xz ] || curl -sSLO \
    https://github.com/llvm/llvm-project/releases/download/llvmorg-$CRT_VER/compiler-rt-$CRT_VER.src.tar.xz

# -- musl ---------------------------------------------------------------------
rm -rf musl-$MUSL_VER && tar xzf musl-$MUSL_VER.tar.gz
cd musl-$MUSL_VER
CC="clang $CFLAGS_T" AR=llvm-ar RANLIB=llvm-ranlib \
    ./configure --target=$TARGET --prefix="$SYSROOT" --disable-shared \
    --syslibdir="$SYSROOT/lib" >/dev/null
make -j"$JOBS" >/dev/null
make install >/dev/null
cd ..

# -- compiler-rt builtins -------------------------------------------------------
# The generic C builtins only; ppc/ holds IBM long double helpers for ppc64,
# and musl's ppc32 long double is plain double.
rm -rf compiler-rt-$CRT_VER.src && tar xJf compiler-rt-$CRT_VER.src.tar.xz
b=compiler-rt-$CRT_VER.src/lib/builtins
rm -rf crt-obj && mkdir crt-obj
for f in "$b"/*.c; do
    n=$(basename "$f" .c)
    case $n in
        # Host-specific or needing OS facilities a static guest lacks.
        emutls|enable_execute_stack|eprintf|gcc_personality_v0|clear_cache|os_version_check|atomic|cpu_model|trampoline_setup) continue ;;
    esac
    echo "$f"
done | xargs -P"$JOBS" -I{} sh -c 'clang '"$CFLAGS_T"' -O2 -fno-builtin -I'"$b"' -c {} \
    -o crt-obj/$(basename {} .c).o 2>/dev/null || true'
# Files for types this target lacks (bf16, x87 and quad floats) fail to build,
# which is expected; the ones a C program on ppc32 needs must not.
for n in udivdi3 divdi3 umoddi3 moddi3 udivmoddi4 ashldi3 lshrdi3 ashrdi3 muldi3 \
         floatdidf floatundidf fixdfdi fixunsdfdi floatdisf fixsfdi popcountsi2 clzdi2 ctzdi2; do
    [ -f crt-obj/$n.o ] || { echo "build-sysroot: compiler-rt $n failed to build" >&2; exit 1; }
done
mkdir -p "$SYSROOT/lib"
rm -f "$SYSROOT/lib/libclang_rt.builtins.a"
llvm-ar rcs "$SYSROOT/lib/libclang_rt.builtins.a" crt-obj/*.o

# -- checks -----------------------------------------------------------------------
if llvm-objdump -d --no-show-raw-insn "$SYSROOT/lib/libc.a" "$SYSROOT/lib/libclang_rt.builtins.a" |
        grep -E '^\s+[0-9a-f]+:\s+(v[a-z]+|lvx|stvx|lvsl|lvsr|mtvscr|mfvscr|ev[a-z]+|xx[a-z]+)\s' | head -3 | grep .; then
    echo "build-sysroot: vector instructions in the sysroot" >&2
    exit 1
fi
echo "sysroot ready: $SYSROOT"
