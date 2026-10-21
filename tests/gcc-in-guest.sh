#!/bin/sh
# Debian's gcc, compiling and running a program, inside the guest.
#
#   tests/gcc-in-guest.sh [rootfs-dir]
#
# This is the most demanding thing gex runs. `gcc` is a driver: it forks and
# execs `cc1`, then `as`, then `collect2` and `ld`, each a separate aarch64
# ELF that gex has to translate. Between them they exercise fork, execve with
# handlers reset, wait4 and its status encoding, pipes, temporary files,
# vectored I/O, and a shell driving the whole thing. Then the binary they
# produce -- machine code that did not exist when the run started -- is itself
# translated and run.
#
# It also happens to be the case that found several of the kernel bugs the
# suite now guards: the wait status a killed child reports, the signal
# dispositions an execve must reset, and the partial vectored transfers that
# a compiler's pipe traffic hits constantly.
#
# The root filesystem is 257 MB and is not built by default. To make one:
#
#   GEX_DEBIAN_ROOTS="libc-bin base-files base-passwd dash coreutils gcc \
#       binutils libc6-dev make grep sed findutils" \
#     rootfs/build-debian.sh /tmp/gcc-rootfs.tar /tmp/gex-gcc
#
# (rootfs/build-debian.sh is in the gex-debian repository.)
set -eu

ROOT="${1:-/tmp/gex-gcc/root}"
GEXC="${GEXC:-target/release/gexc}"

if [ ! -x "$ROOT/usr/bin/gcc" ]; then
    echo "skip: no gcc rootfs at $ROOT (see the header for how to build one)"
    exit 0
fi

mkdir -p "$ROOT/work"
cat > "$ROOT/work/hello.c" <<'PROGRAM'
/* Compiled by Debian's own gcc, inside the guest, by translated aarch64. */
#include <stdio.h>
#include <string.h>
static int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
int main(void) {
    char msg[64];
    snprintf(msg, sizeof msg, "fib(24)=%d", fib(24));
    printf("%s len=%zu\n", msg, strlen(msg));
    return 0;
}
PROGRAM

cat > "$ROOT/build.sh" <<'SCRIPT'
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
cd /work
rm -f hello
gcc --version | head -1
gcc -O2 -o hello hello.c || { echo "COMPILE FAILED"; exit 1; }
./hello
SCRIPT

out=$("$GEXC" run --sysroot "$ROOT" --uid 0 --argv0 /bin/sh \
    "$ROOT/bin/dash" /build.sh 2>&1 | grep -v '^\[gex\]')

echo "$out" | sed 's/^/     /'
if echo "$out" | grep -q '^gcc (Debian' && echo "$out" | grep -q '^fib(24)=46368 len=13$'; then
    echo "ok   gcc-in-guest"
    exit 0
fi
echo "FAIL gcc-in-guest: the toolchain did not compile and run a program"
exit 1
