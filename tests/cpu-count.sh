#!/bin/sh
# The guest sees the host's processors, natively and under the browser
# runtime, the same way at every layer: sched_getaffinity (musl's
# sysconf(_SC_NPROCESSORS_ONLN)), /proc/stat's cpu lines (Boehm GC),
# /proc/cpuinfo, and /sys/devices/system/cpu (online, which glibc's
# get_nprocs reads, and cpuN/topology, which Wine's
# GetLogicalProcessorInformation reads). Guest threads run in parallel
# (host threads, page Workers), and gex said one processor everywhere, so
# Wine reported NumberOfProcessors 1 and engines ran one job worker.
#
#   tests/cpu-count.sh [a64|x86]
#
# Natively GEX_CPUS=3 must give 3. In the browser runtime (node) the count
# must be navigator.hardwareConcurrency (at most 64). Needs zig, node, the
# guest's gexc and the shared shim in web/public/gex.
set -eu
ARCH="${1:-a64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-cpu-count-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86]" >&2; exit 2 ;;
esac
mkdir -p "$WORK"
trap 'rm -rf "$WORK"' EXIT
cat > "$WORK/nc.c" <<'EOF2'
#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static int lines(const char *path, const char *prefix, int skip_aggregate) {
    char b[512]; int n = 0; FILE *f = fopen(path, "r");
    if (!f) return -1;
    while (fgets(b, sizeof b, f))
        if (!strncmp(b, prefix, strlen(prefix)) && !(skip_aggregate && b[3] == ' ')) n++;
    fclose(f);
    return n;
}
int main(void) {
    cpu_set_t s; CPU_ZERO(&s);
    int aff = sched_getaffinity(0, sizeof s, &s) == 0 ? CPU_COUNT(&s) : -1;
    /* /sys/devices/system/cpu: the online list, and one topology entry per processor. */
    int lo = -1, hi = -1, sys = -1, topo = 0;
    FILE *f = fopen("/sys/devices/system/cpu/online", "r");
    if (f) { int k = fscanf(f, "%d-%d", &lo, &hi); fclose(f); sys = k == 2 ? hi - lo + 1 : k == 1 ? 1 : -1; }
    for (int i = 0; i < 1024; i++) {
        char p[96]; snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu%d/topology/core_id", i);
        if (!(f = fopen(p, "r"))) break;
        fclose(f); topo++;
    }
    printf("affinity=%d sysconf=%ld stat=%d cpuinfo=%d sys=%d topology=%d\n", aff, sysconf(_SC_NPROCESSORS_ONLN),
           lines("/proc/stat", "cpu", 1), lines("/proc/cpuinfo", "processor", 0), sys, topo);
    return 0;
}
EOF2
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/nc" "$WORK/nc.c"
fail=0
check() { # WHAT WANT GOT
    if [ "$3" = "affinity=$2 sysconf=$2 stat=$2 cpuinfo=$2 sys=$2 topology=$2" ]; then echo "ok $1: $3"
    else echo "FAIL $1: want $2 everywhere, got '$3'"; fail=1; fi
}
got=$(GEX_CPUS=3 "$GEXC" run "$WORK/nc" 2>/dev/null | grep '^affinity=' || true)
check native 3 "$got"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
want=$(node -e 'console.log(Math.min(64, navigator.hardwareConcurrency || 1))')
got=$(GEX_SHARED=1 timeout "${TMO:-120}" node "$ROOT/shims/browser/js/run-node.mjs" "$WORK/seed.wasm" "$WORK/nc" 2>/dev/null | grep '^affinity=' || true)
check browser "$want" "$got"
exit $fail
