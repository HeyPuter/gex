#!/bin/sh
# Dynamically linked glibc programs, i386 and x86-64, against the host's
# glibc (/usr/lib32 and /usr/lib): run natively and under gexc, output and
# exit status must match. The static musl programs beside this cannot see
# what ld.so and libc check at startup -- the CPU's x86 ISA level, IFUNC
# selection by cpuid -- which is how a cpuid change that broke every i386
# glibc program got past the gate.
#
#   tests/x86-programs/glibc/run.sh [gexc]
#
# Needs gcc with -m32 support (the host's lib32 glibc); a missing i386
# toolchain is a skip, not a pass.
set -u
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
WORK=${WORK:-$(mktemp -d)}
fail=0
for bits in 32 64; do
    bin="$WORK/hello$bits"
    if ! gcc -m$bits -O1 -o "$bin" "$HERE/hello.c" -lm 2>/dev/null; then
        echo "skip glibc-$bits (no gcc -m$bits)"
        continue
    fi
    native=$("$bin" arg); ns=$?
    gex=$("$GEXC" run "$bin" arg 2>&1); gs=$?
    if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
        echo "ok   glibc-$bits"
    else
        echo "FAIL glibc-$bits: native exit $ns, gex exit $gs"
        printf '  native: %s\n  gex:    %s\n' "$native" "$gex" | head -6
        fail=1
    fi
done
exit $fail
