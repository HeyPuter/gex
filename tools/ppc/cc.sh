#!/bin/sh
# clang for gex's PowerPC guest: a static powerpc-linux-musl executable against
# the sysroot tools/ppc/build-sysroot.sh builds. Extra arguments go to clang.
#
#   PPC_SYSROOT   the sysroot (default: ../ppc-sysroot beside the worktree)
#   PPC_CPU       the baseline CPU (default: ppc, no AltiVec)
#
# Linked at 0x1000000 (gex's PIE_BASE): lld's default for ppc32 is 0x10000000,
# which works too, but a low image keeps the process's memory small.
# Non-PIC, as a static executable is: clang otherwise defaults to PIE code,
# reaching every global through a GOT pointer in r30.
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
SYSROOT=${PPC_SYSROOT:-$(dirname "$root")/ppc-sysroot}
exec clang --target=powerpc-linux-musl -mcpu="${PPC_CPU:-ppc}" -mno-altivec -fno-pic \
    --sysroot="$SYSROOT" -nostdinc -isystem "$SYSROOT/include" \
    -isystem "$(clang -print-resource-dir)/include" \
    -fuse-ld=lld -static -nostdlib \
    "$SYSROOT/lib/crt1.o" "$SYSROOT/lib/crti.o" \
    "$@" \
    -L"$SYSROOT/lib" -lc "$SYSROOT/lib/libclang_rt.builtins.a" "$SYSROOT/lib/crtn.o" \
    -Wl,--image-base=0x1000000
