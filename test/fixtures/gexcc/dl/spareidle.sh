#!/bin/sh
# A spare thread Worker the page started for a process lets go after five
# seconds with no thread to take: a program starts three threads (each taken
# by a waiting spare), sleeps eight seconds, then starts a fourth, which
# starts on a Worker of its own. Every thread runs, in order, and the idle
# spare's release is logged. In the browser runtime (run-node, GEX_ROOT's
# x86 shim).
#
#   test/fixtures/gexcc/dl/spareidle.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                   with page/public/gex built for x86; DIR:
#                                   where gex can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "skip spareidle: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
SEED=$ROOT/page/public/gex/seed.x86.shared.wasm
[ -f "$SEED" ] || { echo "skip spareidle: no $SEED"; exit 0; }
W=$(mktemp -d "${1:?DIR}/spareidle.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/s.c" <<'X'
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
static void *say(void *a) { printf("thread %ld\n", (long)a); fflush(stdout); return 0; }
static void run(long n) { pthread_t t; pthread_create(&t, 0, say, (void *)n); pthread_join(t, 0); }
int main(void) {
    run(1); run(2); run(3);
    sleep(8);
    run(4);
    return 0;
}
X
gexcc -pthread -o "$W/s" "$W/s.c" || { echo "FAIL spareidle: build"; exit 1; }
out=$(GEX_ARCH=x86 GEX_SHARED=1 GEX_LOG=spare GEX_TEST_SPARE_MODULES=1 timeout 120 \
    node "$ROOT/hosts/browser/js/run-node.mjs" --file /t/s="$W/s" --argv0 /t/s "$SEED" "$W/s" 2>&1)
rc=$?
threads=$(printf '%s\n' "$out" | grep '^thread ' | tr '\n' ' ')
fail=0
[ $rc -eq 0 ] || { echo "FAIL spareidle: exit $rc"; fail=1; }
[ "$threads" = "thread 1 thread 2 thread 3 thread 4 " ] || { echo "FAIL spareidle: threads [$threads]"; fail=1; }
printf '%s\n' "$out" | grep -q 'let go, no thread in 5 s' || { echo "FAIL spareidle: no idle spare let go"; fail=1; }
[ $fail -eq 0 ] && echo "ok spareidle" || printf '%s\n' "$out" | tail -20
exit $fail
