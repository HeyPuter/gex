#!/bin/sh
# A thread a spare Worker takes keeps no slot promised: its `clone` promised
# it one, and the spare holds one already. Four hundred threads started and
# joined one after another, every program given a spare
# (GEX_TEST_SPARE_MODULES=1), more than a process has slots. Each thread
# but the first is taken by the spare started for it, and every `clone` must
# succeed. In the browser runtime (run-node).
#
#   test/fixtures/gexcc/dl/sparepromise.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                      with page/public/gex built; DIR: where
#                                      gex can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/sparepromise.XXXXXX")
trap 'rm -rf "$W"' EXIT
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
cat > "$W/c.c" <<'X'
#include <pthread.h>
#include <stdio.h>
#include <string.h>
static void *f(void *a) { return a; }
int main(void) {
    for (int i = 0; i < 400; i++) {
        pthread_t t;
        int rc = pthread_create(&t, 0, f, (void *)(long)i);
        if (rc) { printf("thread %d: %s\n", i, strerror(rc)); return 1; }
        void *r;
        pthread_join(t, &r);
        if ((long)r != i) { printf("thread %d returned %ld\n", i, (long)r); return 1; }
    }
    puts("400 threads");
    return 0;
}
X
GEXCC_SHARED=1 gexcc -O1 -pthread -o "$W/c" "$W/c.c" || { echo "FAIL build"; exit 1; }
out=$(GEX_TEST_SPARE_MODULES=1 GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/hosts/browser/js/run-node.mjs" \
    --file "/usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so" --file "/t/c=$W/c" --argv0 /t/c "$SEED" "$W/c" 2>&1); s=$?
got=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
if [ "$got" = "400 threads" ] && [ $s = 0 ]; then echo "ok   sparepromise"; exit 0; fi
echo "FAIL sparepromise (exit $s): $got"; printf '%s\n' "$out" | tail -5
exit 1
